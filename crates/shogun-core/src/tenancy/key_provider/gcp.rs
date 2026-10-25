//! GCP Cloud KMS as a [`KeyWrapper`] (E26): `cryptoKeys.encrypt` / `cryptoKeys.decrypt` on a
//! symmetric key, the data key drawn locally.
//!
//! - **Binding.** The context goes in `additionalAuthenticatedData`.
//! - **Rotation.** Cloud KMS decrypts with whichever version encrypted the blob and says whether
//!   that was the primary (`usedPrimary`). `false` means the key has rotated since: the data key is
//!   reported stale and re-wrapped under the new primary.
//! - **Credentials.** A bearer token from, in order: `SHOGUN_GCP_ACCESS_TOKEN_FILE` (read fresh on
//!   each call, for a token refresher sidecar), `SHOGUN_GCP_ACCESS_TOKEN` (static; development),
//!   else the instance metadata server (GKE Workload Identity, GCE), cached until a minute before
//!   it expires. Held in [`Secret`]; read (`.expose(`) only for the `Authorization` header —
//!   allowlisted in `scripts/check-secret-exposure.py`.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde_json::{json, Value};
use shogun_integrations::token_store::{DataKey, KeyWrapper, KmsError, Unwrapped, WrappedDataKey};
use shogun_redact::Secret;

use super::egress::{json as json_body, read_token_file, var, KmsHttp};
use super::TenancyError;
use crate::llm::transport::{HttpRequest, HttpTransport, Method};

/// The metadata server's token endpoint for the workload's default service account.
pub const METADATA_TOKEN_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";

/// Where the bearer token comes from.
pub enum TokenSource {
    File(String),
    Static(Secret),
    Metadata { url: String, cached: Mutex<Option<(Secret, Instant)>> },
}

/// The GCP Cloud KMS wrapper.
pub struct GcpKms<T> {
    http: KmsHttp<T>,
    /// `projects/<p>/locations/<l>/keyRings/<r>/cryptoKeys/<k>`.
    key: String,
    endpoint: String,
    token: TokenSource,
}

impl<T: HttpTransport + 'static> GcpKms<T> {
    /// From the environment (see `deploy/README.md`): `SHOGUN_GCP_KMS_KEY`, optional
    /// `SHOGUN_GCP_KMS_ENDPOINT`, and the token source.
    pub fn from_vars(vars: &HashMap<String, String>, http: KmsHttp<T>) -> Result<Self, TenancyError> {
        let key = var(vars, "SHOGUN_GCP_KMS_KEY").ok_or(TenancyError::KmsConfig("SHOGUN_GCP_KMS_KEY"))?;
        let token = if let Some(path) = var(vars, "SHOGUN_GCP_ACCESS_TOKEN_FILE") {
            TokenSource::File(path.to_string())
        } else if let Some(token) = var(vars, "SHOGUN_GCP_ACCESS_TOKEN") {
            TokenSource::Static(Secret::new(token))
        } else {
            TokenSource::Metadata { url: METADATA_TOKEN_URL.to_string(), cached: Mutex::new(None) }
        };
        let endpoint = var(vars, "SHOGUN_GCP_KMS_ENDPOINT").unwrap_or("https://cloudkms.googleapis.com");
        Self::new(http, key, endpoint, token)
    }

    pub fn new(http: KmsHttp<T>, key: &str, endpoint: &str, token: TokenSource) -> Result<Self, TenancyError> {
        let well_formed = key.starts_with("projects/")
            && key.contains("/locations/")
            && key.contains("/keyRings/")
            && key.contains("/cryptoKeys/")
            && !key.contains("/cryptoKeyVersions/")
            && key.bytes().all(|b| b.is_ascii_alphanumeric() || b"/-_.".contains(&b));
        if !well_formed {
            return Err(TenancyError::KmsConfig("SHOGUN_GCP_KMS_KEY"));
        }
        Ok(Self { http, key: key.to_string(), endpoint: endpoint.trim_end_matches('/').to_string(), token })
    }

    fn bearer(&self) -> Result<Secret, KmsError> {
        match &self.token {
            TokenSource::File(path) => read_token_file(path, "SHOGUN_GCP_ACCESS_TOKEN_FILE"),
            TokenSource::Static(token) => Ok(token.clone()),
            TokenSource::Metadata { url, cached } => {
                if let Some((token, until)) = cached.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
                    if Instant::now() < *until {
                        return Ok(token.clone());
                    }
                }
                let req = HttpRequest::cloud_metadata(url.as_str(), vec![("metadata-flavor".into(), "Google".into())])
                    .map_err(|_| KmsError::Config("the metadata server address"))?;
                let answer = json_body(&self.http.send("token", req)?)?;
                let token = answer.get("access_token").and_then(Value::as_str).ok_or(KmsError::Malformed)?;
                let lifetime = answer.get("expires_in").and_then(Value::as_u64).unwrap_or(300);
                let token = Secret::new(token);
                let until = Instant::now() + Duration::from_secs(lifetime.saturating_sub(60));
                *cached.lock().unwrap_or_else(PoisonError::into_inner) = Some((token.clone(), until));
                Ok(token)
            }
        }
    }

    fn call(&self, method: &'static str, body: &Value) -> Result<Value, KmsError> {
        let bearer = self.bearer()?;
        let url = format!("{}/v1/{}:{method}", self.endpoint, self.key);
        let req = HttpRequest::new(
            Method::Post,
            url,
            vec![
                ("content-type".into(), "application/json".into()),
                ("authorization".into(), format!("Bearer {}", bearer.expose())),
            ],
            Some(body.to_string()),
        )
        .map_err(|_| KmsError::Config("SHOGUN_GCP_KMS_ENDPOINT"))?
        .without_redirects();
        json_body(&self.http.send(method, req)?)
    }
}

impl<T: HttpTransport + 'static> KeyWrapper for GcpKms<T> {
    fn provider(&self) -> &'static str {
        "gcp-kms"
    }

    fn wrap(&self, key: &DataKey, context: &str) -> Result<WrappedDataKey, KmsError> {
        let plaintext = key.with_bytes(|b| STANDARD.encode(b));
        let answer = self.call(
            "encrypt",
            &json!({ "plaintext": plaintext, "additionalAuthenticatedData": STANDARD.encode(context) }),
        )?;
        let version = answer.get("name").and_then(Value::as_str).ok_or(KmsError::Malformed)?;
        let blob = answer.get("ciphertext").and_then(Value::as_str).ok_or(KmsError::Malformed)?;
        Ok(WrappedDataKey {
            key_version: version.to_string(),
            blob: STANDARD.decode(blob).map_err(|_| KmsError::Malformed)?,
        })
    }

    fn unwrap(&self, wrapped: &WrappedDataKey, context: &str) -> Result<Unwrapped, KmsError> {
        let answer = self.call(
            "decrypt",
            &json!({ "ciphertext": STANDARD.encode(&wrapped.blob), "additionalAuthenticatedData": STANDARD.encode(context) }),
        )?;
        let plain = answer.get("plaintext").and_then(Value::as_str).ok_or(KmsError::Malformed)?;
        let mut bytes = STANDARD.decode(plain).map_err(|_| KmsError::Malformed)?;
        let key = DataKey::from_slice(&mut bytes)?;
        let stale = answer.get("usedPrimary").and_then(Value::as_bool) == Some(false);
        Ok(Unwrapped { key, stale })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    use super::super::egress::testing::{header, ok, status, FakeKey};
    use super::super::egress::RecordingKmsAudit;
    use super::*;
    use crate::llm::transport::{HttpResponse, TransportError};

    const KEY: &str = "projects/acme/locations/europe-west1/keyRings/shogun/cryptoKeys/principals";

    struct FakeGcpKms {
        key: FakeKey,
    }

    impl FakeGcpKms {
        fn answer(&self, req: &HttpRequest) -> Result<HttpResponse, TransportError> {
            self.key.enter(req)?;
            if req.url.starts_with("http://metadata.google.internal/") {
                if header(req, "metadata-flavor") != Some("Google") {
                    return status(403);
                }
                return ok(json!({ "access_token": "metadata-token", "expires_in": 3599, "token_type": "Bearer" }));
            }
            if header(req, "authorization") != Some("Bearer metadata-token") {
                return status(401);
            }
            let body: Value = serde_json::from_str(req.body.as_deref().unwrap_or("{}")).unwrap();
            let aad = String::from_utf8(STANDARD.decode(body["additionalAuthenticatedData"].as_str().unwrap()).unwrap()).unwrap();
            if req.url == format!("https://cloudkms.googleapis.com/v1/{KEY}:encrypt") {
                let (v, blob) = self.key.encrypt(body["plaintext"].as_str().unwrap(), &aad).unwrap();
                ok(json!({ "name": format!("{KEY}/cryptoKeyVersions/{v}"), "ciphertext": blob }))
            } else if req.url == format!("https://cloudkms.googleapis.com/v1/{KEY}:decrypt") {
                match self.key.decrypt(body["ciphertext"].as_str().unwrap(), &aad) {
                    Some((v, plain)) => ok(json!({ "plaintext": plain, "usedPrimary": v == self.key.primary() })),
                    None => status(400),
                }
            } else {
                status(404)
            }
        }
    }

    impl HttpTransport for FakeGcpKms {
        fn send(&self, req: HttpRequest) -> impl std::future::Future<Output = Result<HttpResponse, TransportError>> + Send {
            std::future::ready(self.answer(&req))
        }
    }

    fn gcp(fake: Arc<FakeGcpKms>) -> GcpKms<FakeGcpKms> {
        let http = KmsHttp::new("gcp-kms", fake, Arc::new(RecordingKmsAudit::default()), Duration::from_secs(5));
        GcpKms::from_vars(&HashMap::from([("SHOGUN_GCP_KMS_KEY".to_string(), KEY.to_string())]), http).unwrap()
    }

    #[test]
    fn wrap_unwrap_and_rotation_through_the_metadata_token() {
        let fake = Arc::new(FakeGcpKms { key: FakeKey::new() });
        let kms = gcp(fake.clone());
        let (key, wrapped) = kms.generate("shogun/sqlcipher/v1/alice").unwrap();
        assert_eq!(wrapped.key_version, format!("{KEY}/cryptoKeyVersions/1"));
        let opened = kms.unwrap(&wrapped, "shogun/sqlcipher/v1/alice").unwrap();
        assert!(!opened.stale);
        assert_eq!(opened.key.with_bytes(|b| *b), key.with_bytes(|b| *b));
        assert_eq!(kms.unwrap(&wrapped, "shogun/sqlcipher/v1/bob").err(), Some(KmsError::Rejected(400)));

        fake.key.rotate();
        let opened = kms.unwrap(&wrapped, "shogun/sqlcipher/v1/alice").unwrap();
        assert!(opened.stale, "decrypted with a non-primary version");
        let rewrapped = kms.wrap(&opened.key, "shogun/sqlcipher/v1/alice").unwrap();
        assert_eq!(rewrapped.key_version, format!("{KEY}/cryptoKeyVersions/2"));

        let seen = fake.key.seen.lock().unwrap().clone();
        let metadata_calls = seen.iter().filter(|r| r.url.starts_with("http://metadata")).count();
        assert_eq!(metadata_calls, 1, "the workload token is cached");
        assert!(seen.iter().all(|r| !format!("{r:?}").contains("metadata-token")));
    }

    #[test]
    fn an_outage_and_a_bad_key_name_fail_closed() {
        let fake = Arc::new(FakeGcpKms { key: FakeKey::new() });
        let kms = gcp(fake.clone());
        let (_, wrapped) = kms.generate("c").unwrap();
        fake.key.down.store(true, Ordering::SeqCst);
        assert_eq!(kms.unwrap(&wrapped, "c").err(), Some(KmsError::Unreachable));

        let http = KmsHttp::new("gcp-kms", fake, Arc::new(RecordingKmsAudit::default()), Duration::from_secs(1));
        let bad = HashMap::from([("SHOGUN_GCP_KMS_KEY".to_string(), format!("{KEY}/cryptoKeyVersions/3"))]);
        assert!(matches!(GcpKms::from_vars(&bad, http), Err(TenancyError::KmsConfig("SHOGUN_GCP_KMS_KEY"))));
    }

    #[test]
    fn only_the_metadata_hosts_may_be_reached_over_http() {
        assert!(HttpRequest::cloud_metadata(METADATA_TOKEN_URL, vec![]).is_ok());
        assert!(HttpRequest::cloud_metadata("http://169.254.169.254/computeMetadata/v1/x", vec![]).is_ok());
        for bad in [
            "http://example.com/computeMetadata/v1/x",
            "http://metadata.google.internal/latest/meta-data/",
            "https://metadata.google.internal/computeMetadata/v1/x",
            "http://user@metadata.google.internal/computeMetadata/v1/x",
        ] {
            assert!(HttpRequest::cloud_metadata(bad, vec![]).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_token_file_is_read_on_each_call() {
        let dir = std::env::temp_dir().join(format!("shogun_kms_gcp_token_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token");
        std::fs::write(&path, "first\n").unwrap();
        let transport = Arc::new(crate::llm::transport::MockTransport::new([]));
        let http = KmsHttp::new("gcp-kms", transport, Arc::new(RecordingKmsAudit::default()), Duration::from_secs(1));
        let vars = HashMap::from([
            ("SHOGUN_GCP_KMS_KEY".to_string(), KEY.to_string()),
            ("SHOGUN_GCP_ACCESS_TOKEN_FILE".to_string(), path.display().to_string()),
        ]);
        let kms = GcpKms::from_vars(&vars, http).unwrap();
        assert_eq!(kms.bearer().unwrap(), Secret::new("first"));
        std::fs::write(&path, "second").unwrap();
        assert_eq!(kms.bearer().unwrap(), Secret::new("second"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
