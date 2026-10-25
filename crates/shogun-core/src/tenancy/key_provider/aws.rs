//! AWS KMS as a [`KeyWrapper`] (E26): `GenerateDataKey` for a new principal's key, `Decrypt` to
//! open it, `Encrypt` to re-wrap it, over the KMS JSON API signed with Signature Version 4.
//!
//! - **Binding.** Every call carries the encryption context `{"shogun:context": <context>}`, so a
//!   wrapped key copied to another principal does not decrypt there.
//! - **Rotation.** AWS automatic key rotation keeps the key ARN and decrypts old ciphertext
//!   transparently, so it needs no re-wrap. Moving to *another* key (a new key behind the
//!   configured alias, or a new `SHOGUN_AWS_KMS_KEY_ID`) does: `Decrypt` answers the ARN of the key
//!   that wrapped the blob, and when it differs from the configured key's ARN (`DescribeKey`, asked
//!   once per process) the data key is reported stale and re-wrapped under the configured key.
//! - **Credentials.** Static (`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` /
//!   `AWS_SESSION_TOKEN`), or IRSA web identity (`AWS_ROLE_ARN` + `AWS_WEB_IDENTITY_TOKEN_FILE`,
//!   which EKS injects): `sts:AssumeRoleWithWebIdentity`, cached until five minutes before expiry.
//!   Held in [`Secret`]; read (`.expose(`) only to sign — allowlisted in
//!   `scripts/check-secret-exposure.py`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use shogun_integrations::token_store::{DataKey, KeyWrapper, KmsError, Unwrapped, WrappedDataKey};
use shogun_redact::Secret;

use super::egress::{json as json_body, read_token_file, var, KmsHttp};
use super::TenancyError;
use crate::llm::transport::{HttpRequest, HttpTransport, Method};

/// The encryption-context key every call carries.
const CONTEXT_KEY: &str = "shogun:context";

/// One set of AWS credentials. The secret parts never render.
#[derive(Clone)]
pub struct AwsCredentials {
    pub access_key_id: String,
    pub secret_access_key: Secret,
    pub session_token: Option<Secret>,
    /// Unix seconds after which these must be refreshed (web identity); `None` for static ones.
    pub expires_at: Option<i64>,
}

impl std::fmt::Debug for AwsCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AwsCredentials")
            .field("access_key_id", &self.access_key_id)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// Where the credentials come from.
pub enum CredentialSource {
    Static(AwsCredentials),
    WebIdentity {
        role_arn: String,
        token_file: String,
        session_name: String,
        sts_endpoint: String,
        cached: Mutex<Option<AwsCredentials>>,
    },
}

type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// The AWS KMS wrapper.
pub struct AwsKms<T> {
    http: KmsHttp<T>,
    region: String,
    key_id: String,
    endpoint: String,
    host: String,
    credentials: CredentialSource,
    /// The configured key's ARN, once known (from `DescribeKey`, or a wrap's answer).
    current_arn: Mutex<Option<String>>,
    now: Clock,
}

fn wall_clock() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

impl<T: HttpTransport + 'static> AwsKms<T> {
    /// From the environment (see `deploy/README.md`): `SHOGUN_AWS_KMS_KEY_ID`, the region
    /// (`SHOGUN_AWS_KMS_REGION`, else `AWS_REGION`, else `AWS_DEFAULT_REGION`), optional
    /// `SHOGUN_AWS_KMS_ENDPOINT` / `SHOGUN_AWS_STS_ENDPOINT`, and the credentials.
    pub fn from_vars(vars: &HashMap<String, String>, http: KmsHttp<T>) -> Result<Self, TenancyError> {
        let key_id = var(vars, "SHOGUN_AWS_KMS_KEY_ID").ok_or(TenancyError::KmsConfig("SHOGUN_AWS_KMS_KEY_ID"))?;
        let region = var(vars, "SHOGUN_AWS_KMS_REGION")
            .or_else(|| var(vars, "AWS_REGION"))
            .or_else(|| var(vars, "AWS_DEFAULT_REGION"))
            .ok_or(TenancyError::KmsConfig("SHOGUN_AWS_KMS_REGION"))?;
        let credentials = if let (Some(role_arn), Some(token_file)) = (var(vars, "AWS_ROLE_ARN"), var(vars, "AWS_WEB_IDENTITY_TOKEN_FILE")) {
            CredentialSource::WebIdentity {
                role_arn: role_arn.to_string(),
                token_file: token_file.to_string(),
                session_name: var(vars, "AWS_ROLE_SESSION_NAME").unwrap_or("shogunagent-sidecar").to_string(),
                sts_endpoint: var(vars, "SHOGUN_AWS_STS_ENDPOINT")
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("https://sts.{region}.amazonaws.com/")),
                cached: Mutex::new(None),
            }
        } else {
            let access_key_id = var(vars, "AWS_ACCESS_KEY_ID").ok_or(TenancyError::KmsConfig("AWS_ACCESS_KEY_ID (or AWS_ROLE_ARN + AWS_WEB_IDENTITY_TOKEN_FILE)"))?;
            let secret = var(vars, "AWS_SECRET_ACCESS_KEY").ok_or(TenancyError::KmsConfig("AWS_SECRET_ACCESS_KEY"))?;
            CredentialSource::Static(AwsCredentials {
                access_key_id: access_key_id.to_string(),
                secret_access_key: Secret::new(secret),
                session_token: var(vars, "AWS_SESSION_TOKEN").map(Secret::new),
                expires_at: None,
            })
        };
        let endpoint = var(vars, "SHOGUN_AWS_KMS_ENDPOINT")
            .map(str::to_string)
            .unwrap_or_else(|| format!("https://kms.{region}.amazonaws.com/"));
        Self::new(http, region, key_id, endpoint, credentials, Arc::new(wall_clock))
    }

    pub fn new(
        http: KmsHttp<T>,
        region: impl Into<String>,
        key_id: impl Into<String>,
        endpoint: impl Into<String>,
        credentials: CredentialSource,
        now: Clock,
    ) -> Result<Self, TenancyError> {
        let endpoint = endpoint.into();
        let parsed = url::Url::parse(&endpoint).map_err(|_| TenancyError::KmsConfig("SHOGUN_AWS_KMS_ENDPOINT"))?;
        let host = match (parsed.host_str(), parsed.port()) {
            (Some(h), Some(p)) => format!("{h}:{p}"),
            (Some(h), None) => h.to_string(),
            (None, _) => return Err(TenancyError::KmsConfig("SHOGUN_AWS_KMS_ENDPOINT")),
        };
        Ok(Self {
            http,
            region: region.into(),
            key_id: key_id.into(),
            endpoint,
            host,
            credentials,
            current_arn: Mutex::new(None),
            now,
        })
    }

    fn credentials(&self) -> Result<AwsCredentials, KmsError> {
        match &self.credentials {
            CredentialSource::Static(c) => Ok(c.clone()),
            CredentialSource::WebIdentity { role_arn, token_file, session_name, sts_endpoint, cached } => {
                let now = (self.now)();
                if let Some(c) = cached.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
                    if c.expires_at.is_some_and(|at| at - 300 > now) {
                        return Ok(c.clone());
                    }
                }
                let token = read_token_file(token_file, "AWS_WEB_IDENTITY_TOKEN_FILE")?;
                let body = url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("Action", "AssumeRoleWithWebIdentity")
                    .append_pair("Version", "2011-06-15")
                    .append_pair("RoleArn", role_arn)
                    .append_pair("RoleSessionName", session_name)
                    .append_pair("WebIdentityToken", token.expose())
                    .finish();
                let req = HttpRequest::new(
                    Method::Post,
                    sts_endpoint.as_str(),
                    vec![("content-type".into(), "application/x-www-form-urlencoded".into())],
                    Some(body),
                )
                .map_err(|_| KmsError::Config("SHOGUN_AWS_STS_ENDPOINT"))?
                .without_redirects();
                let response = self.http.send("AssumeRoleWithWebIdentity", req)?;
                let fresh = parse_sts(&response.body)?;
                *cached.lock().unwrap_or_else(PoisonError::into_inner) = Some(fresh.clone());
                Ok(fresh)
            }
        }
    }

    /// One signed KMS JSON call.
    fn call(&self, target: &'static str, body: &Value) -> Result<Value, KmsError> {
        let creds = self.credentials()?;
        let body = body.to_string();
        let amz_date = amz_date((self.now)());
        let mut headers = vec![
            ("content-type".to_string(), "application/x-amz-json-1.1".to_string()),
            ("host".to_string(), self.host.clone()),
            ("x-amz-date".to_string(), amz_date.clone()),
            ("x-amz-target".to_string(), format!("TrentService.{target}")),
        ];
        if let Some(token) = &creds.session_token {
            headers.push(("x-amz-security-token".to_string(), token.expose().to_string()));
        }
        let authorization = authorization(&SignInput {
            method: "POST",
            path: "/",
            query: "",
            headers: &headers,
            body: body.as_bytes(),
            region: &self.region,
            service: "kms",
            amz_date: &amz_date,
            access_key_id: &creds.access_key_id,
            secret_access_key: &creds.secret_access_key,
        });
        // `host` is set by the transport from the URL; it must not be sent twice.
        let mut sent: Vec<(String, String)> = headers.into_iter().filter(|(k, _)| k != "host").collect();
        sent.push(("authorization".to_string(), authorization));
        let req = HttpRequest::new(Method::Post, self.endpoint.as_str(), sent, Some(body))
            .map_err(|_| KmsError::Config("SHOGUN_AWS_KMS_ENDPOINT"))?
            .without_redirects();
        json_body(&self.http.send(target, req)?)
    }

    fn context(context: &str) -> Value {
        json!({ CONTEXT_KEY: context })
    }

    fn remember_arn(&self, arn: &str) {
        *self.current_arn.lock().unwrap_or_else(PoisonError::into_inner) = Some(arn.to_string());
    }

    /// The configured key's ARN: remembered, else `DescribeKey` (a refusal reads as unknown, which
    /// never blocks an open: it only skips the re-wrap check).
    fn current_arn(&self) -> Option<String> {
        if let Some(arn) = self.current_arn.lock().unwrap_or_else(PoisonError::into_inner).clone() {
            return Some(arn);
        }
        let answer = self.call("DescribeKey", &json!({ "KeyId": self.key_id })).ok()?;
        let arn = answer.pointer("/KeyMetadata/Arn")?.as_str()?.to_string();
        self.remember_arn(&arn);
        Some(arn)
    }
}

fn b64_field(answer: &Value, field: &str) -> Result<Vec<u8>, KmsError> {
    let text = answer.get(field).and_then(Value::as_str).ok_or(KmsError::Malformed)?;
    STANDARD.decode(text).map_err(|_| KmsError::Malformed)
}

fn str_field(answer: &Value, field: &str) -> Result<String, KmsError> {
    answer.get(field).and_then(Value::as_str).map(str::to_string).ok_or(KmsError::Malformed)
}

impl<T: HttpTransport + 'static> KeyWrapper for AwsKms<T> {
    fn provider(&self) -> &'static str {
        "aws-kms"
    }

    fn generate(&self, context: &str) -> Result<(DataKey, WrappedDataKey), KmsError> {
        let answer = self.call(
            "GenerateDataKey",
            &json!({ "KeyId": self.key_id, "KeySpec": "AES_256", "EncryptionContext": Self::context(context) }),
        )?;
        let key = DataKey::from_slice(&mut b64_field(&answer, "Plaintext")?)?;
        let arn = str_field(&answer, "KeyId")?;
        self.remember_arn(&arn);
        Ok((key, WrappedDataKey { key_version: arn, blob: b64_field(&answer, "CiphertextBlob")? }))
    }

    fn wrap(&self, key: &DataKey, context: &str) -> Result<WrappedDataKey, KmsError> {
        let plaintext = key.with_bytes(|b| STANDARD.encode(b));
        let answer = self.call(
            "Encrypt",
            &json!({ "KeyId": self.key_id, "Plaintext": plaintext, "EncryptionContext": Self::context(context) }),
        )?;
        let arn = str_field(&answer, "KeyId")?;
        self.remember_arn(&arn);
        Ok(WrappedDataKey { key_version: arn, blob: b64_field(&answer, "CiphertextBlob")? })
    }

    fn unwrap(&self, wrapped: &WrappedDataKey, context: &str) -> Result<Unwrapped, KmsError> {
        let answer = self.call(
            "Decrypt",
            &json!({ "CiphertextBlob": STANDARD.encode(&wrapped.blob), "EncryptionContext": Self::context(context) }),
        )?;
        let key = DataKey::from_slice(&mut b64_field(&answer, "Plaintext")?)?;
        let used = str_field(&answer, "KeyId")?;
        let stale = self.current_arn().is_some_and(|current| current != used);
        Ok(Unwrapped { key, stale })
    }
}

/// `YYYYMMDDTHHMMSSZ` for unix seconds.
pub fn amz_date(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .unwrap_or_default()
        .format("%Y%m%dT%H%M%SZ")
        .to_string()
}

/// What SigV4 signs. `headers` are lower-case names, `host` and `x-amz-date` among them.
pub struct SignInput<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a str,
    pub headers: &'a [(String, String)],
    pub body: &'a [u8],
    pub region: &'a str,
    pub service: &'a str,
    pub amz_date: &'a str,
    pub access_key_id: &'a str,
    pub secret_access_key: &'a Secret,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    // HMAC accepts a key of any length; `new_from_slice` cannot fail for SHA-256.
    match Hmac::<Sha256>::new_from_slice(key) {
        Ok(mut mac) => {
            mac.update(data);
            mac.finalize().into_bytes().to_vec()
        }
        Err(_) => Vec::new(),
    }
}

/// The SigV4 `Authorization` header value (AWS General Reference, "Signature Version 4").
pub fn authorization(input: &SignInput<'_>) -> String {
    let mut headers: Vec<(String, String)> = input
        .headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.split_whitespace().collect::<Vec<_>>().join(" ")))
        .collect();
    headers.sort();
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers = headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        input.method,
        input.path,
        input.query,
        canonical_headers,
        signed_headers,
        hex(&Sha256::digest(input.body))
    );
    let date = input.amz_date.get(..8).unwrap_or_default();
    let scope = format!("{date}/{}/{}/aws4_request", input.region, input.service);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{scope}\n{}",
        input.amz_date,
        hex(&Sha256::digest(canonical_request.as_bytes()))
    );
    let k_date = hmac(format!("AWS4{}", input.secret_access_key.expose()).as_bytes(), date.as_bytes());
    let k_region = hmac(&k_date, input.region.as_bytes());
    let k_service = hmac(&k_region, input.service.as_bytes());
    let k_signing = hmac(&k_service, b"aws4_request");
    let signature = hex(&hmac(&k_signing, string_to_sign.as_bytes()));
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        input.access_key_id
    )
}

fn xml_tag<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = start + xml[start..].find(&close)?;
    Some(xml[start..end].trim())
}

/// Read `AssumeRoleWithWebIdentityResponse`'s credentials.
pub fn parse_sts(xml: &str) -> Result<AwsCredentials, KmsError> {
    let access_key_id = xml_tag(xml, "AccessKeyId").ok_or(KmsError::Malformed)?;
    let secret = xml_tag(xml, "SecretAccessKey").ok_or(KmsError::Malformed)?;
    let token = xml_tag(xml, "SessionToken").ok_or(KmsError::Malformed)?;
    let expires_at = xml_tag(xml, "Expiration")
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.timestamp())
        .ok_or(KmsError::Malformed)?;
    Ok(AwsCredentials {
        access_key_id: access_key_id.to_string(),
        secret_access_key: Secret::new(secret),
        session_token: Some(Secret::new(token)),
        expires_at: Some(expires_at),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use super::super::egress::testing::{header, ok, status, FakeKey};
    use super::super::egress::RecordingKmsAudit;
    use super::*;
    use crate::llm::transport::{HttpResponse, TransportError};

    #[test]
    fn sigv4_matches_the_aws_test_suite_get_vanilla_vector() {
        // aws-sig-v4-test-suite/get-vanilla: GET / with host + x-amz-date, empty body.
        let headers = vec![
            ("host".to_string(), "example.amazonaws.com".to_string()),
            ("x-amz-date".to_string(), "20150830T123600Z".to_string()),
        ];
        let secret = Secret::new("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY");
        let auth = authorization(&SignInput {
            method: "GET",
            path: "/",
            query: "",
            headers: &headers,
            body: b"",
            region: "us-east-1",
            service: "service",
            amz_date: "20150830T123600Z",
            access_key_id: "AKIDEXAMPLE",
            secret_access_key: &secret,
        });
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
        assert_eq!(amz_date(1_440_938_160), "20150830T123600Z");
    }

    /// A fake AWS KMS: answers the four TrentService targets the way KMS does.
    struct FakeAwsKms {
        key: FakeKey,
    }

    impl FakeAwsKms {
        fn arn(version: u32) -> String {
            format!("arn:aws:kms:eu-west-1:111122223333:key/fake-{version}")
        }

        fn answer(&self, req: &HttpRequest) -> Result<HttpResponse, TransportError> {
            self.key.enter(req)?;
            let auth = header(req, "authorization").unwrap_or_default();
            if !auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDTEST/") || !auth.contains("/eu-west-1/kms/aws4_request") {
                return status(403);
            }
            let body: Value = serde_json::from_str(req.body.as_deref().unwrap_or("{}")).unwrap();
            let context = body.pointer("/EncryptionContext/shogun:context").and_then(Value::as_str).unwrap_or("");
            match header(req, "x-amz-target").unwrap_or_default() {
                "TrentService.GenerateDataKey" => {
                    let plain = STANDARD.encode([0x42u8; 32]);
                    let (v, blob) = self.key.encrypt(&plain, context).unwrap();
                    ok(json!({ "Plaintext": plain, "CiphertextBlob": blob, "KeyId": Self::arn(v) }))
                }
                "TrentService.Encrypt" => {
                    let (v, blob) = self.key.encrypt(body["Plaintext"].as_str().unwrap(), context).unwrap();
                    ok(json!({ "CiphertextBlob": blob, "KeyId": Self::arn(v) }))
                }
                "TrentService.Decrypt" => match self.key.decrypt(body["CiphertextBlob"].as_str().unwrap(), context) {
                    Some((v, plain)) => ok(json!({ "Plaintext": plain, "KeyId": Self::arn(v) })),
                    None => status(400),
                },
                "TrentService.DescribeKey" => ok(json!({ "KeyMetadata": { "Arn": Self::arn(self.key.primary()) } })),
                _ => status(400),
            }
        }
    }

    impl HttpTransport for FakeAwsKms {
        fn send(&self, req: HttpRequest) -> impl std::future::Future<Output = Result<HttpResponse, TransportError>> + Send {
            std::future::ready(self.answer(&req))
        }
    }

    fn kms(fake: Arc<FakeAwsKms>, audit: Arc<RecordingKmsAudit>) -> AwsKms<FakeAwsKms> {
        let http = KmsHttp::new("aws-kms", fake, audit, Duration::from_secs(5));
        AwsKms::new(
            http,
            "eu-west-1",
            "alias/shogun",
            "https://kms.eu-west-1.amazonaws.com/",
            CredentialSource::Static(AwsCredentials {
                access_key_id: "AKIDTEST".into(),
                secret_access_key: Secret::new("aws-secret-never-sent"),
                session_token: Some(Secret::new("aws-session-token-1")),
                expires_at: None,
            }),
            Arc::new(|| 1_761_350_400),
        )
        .unwrap()
    }

    #[test]
    fn generate_unwrap_and_rotation_to_a_new_key_against_a_fake_kms() {
        let fake = Arc::new(FakeAwsKms { key: FakeKey::new() });
        let audit = Arc::new(RecordingKmsAudit::default());
        let aws = kms(fake.clone(), audit.clone());
        let (key, wrapped) = aws.generate("shogun/sqlcipher/v1/alice").unwrap();
        assert_eq!(wrapped.key_version, FakeAwsKms::arn(1));
        let opened = aws.unwrap(&wrapped, "shogun/sqlcipher/v1/alice").unwrap();
        assert!(!opened.stale);
        assert_eq!(opened.key.with_bytes(|b| *b), key.with_bytes(|b| *b));

        // Another principal's context does not decrypt it.
        assert_eq!(aws.unwrap(&wrapped, "shogun/sqlcipher/v1/bob").err(), Some(KmsError::Rejected(400)));

        // The alias moves to a new key: a fresh process learns the new ARN and reports stale.
        fake.key.rotate();
        let fresh_process = kms(fake.clone(), audit.clone());
        let opened = fresh_process.unwrap(&wrapped, "shogun/sqlcipher/v1/alice").unwrap();
        assert!(opened.stale, "wrapped under the old key");
        let rewrapped = fresh_process.wrap(&opened.key, "shogun/sqlcipher/v1/alice").unwrap();
        assert_eq!(rewrapped.key_version, FakeAwsKms::arn(2));
        assert!(!fresh_process.unwrap(&rewrapped, "shogun/sqlcipher/v1/alice").unwrap().stale);

        // Signed with the session token; no secret in any recorded call or request Debug.
        let seen = fake.key.seen.lock().unwrap().clone();
        assert!(seen.iter().all(|r| header(r, "x-amz-security-token") == Some("aws-session-token-1")));
        assert!(seen.iter().all(|r| !format!("{r:?}").contains("aws-session-token-1")));
        assert!(seen.iter().all(|r| !format!("{r:?}").contains("aws-secret-never-sent")));
        assert!(seen.iter().all(|r| !r.headers.iter().any(|(k, _)| k == "host")));
        let calls = audit.calls.lock().unwrap().clone();
        assert!(calls.iter().all(|(c, _)| c.destination == "kms.eu-west-1.amazonaws.com"));
    }

    #[test]
    fn the_remote_token_store_seals_credentials_under_the_same_kms_key() {
        use shogun_integrations::token_store::{KmsEnvelope, KmsSealer};
        let fake = Arc::new(FakeAwsKms { key: FakeKey::new() });
        let wrapper: Arc<dyn KeyWrapper> = Arc::new(kms(fake.clone(), Arc::new(RecordingKmsAudit::default())));
        let sealer = KmsSealer::new(wrapper);
        let row = sealer.seal("alice", "gmail-tokenset", &Secret::new("{\"refresh\":\"r-1\"}")).unwrap();
        assert_eq!(row.key_id, FakeAwsKms::arn(1));
        assert!(!String::from_utf8_lossy(&row.ciphertext).contains("r-1"));
        assert_eq!(sealer.open("alice", "gmail-tokenset", &row).unwrap(), Secret::new("{\"refresh\":\"r-1\"}"));
        assert!(sealer.open("bob", "gmail-tokenset", &row).is_err(), "bound to the principal at the KMS");
        fake.key.down.store(true, Ordering::SeqCst);
        assert_eq!(sealer.open("alice", "gmail-tokenset", &row).err(), Some(shogun_integrations::TokenStoreError::Locked));
    }

    #[test]
    fn a_down_or_refusing_kms_is_an_error() {
        let fake = Arc::new(FakeAwsKms { key: FakeKey::new() });
        let aws = kms(fake.clone(), Arc::new(RecordingKmsAudit::default()));
        let (_, wrapped) = aws.generate("c").unwrap();
        fake.key.down.store(true, Ordering::SeqCst);
        assert_eq!(aws.unwrap(&wrapped, "c").err(), Some(KmsError::Unreachable));
        fake.key.down.store(false, Ordering::SeqCst);
        let mut wrong = kms(fake, Arc::new(RecordingKmsAudit::default()));
        wrong.credentials = CredentialSource::Static(AwsCredentials {
            access_key_id: "AKIDOTHER".into(),
            secret_access_key: Secret::new("x"),
            session_token: None,
            expires_at: None,
        });
        assert_eq!(wrong.unwrap(&wrapped, "c").err(), Some(KmsError::Denied(403)));
    }

    #[test]
    fn web_identity_credentials_come_from_sts_and_are_cached() {
        let dir = std::env::temp_dir().join(format!("shogun_kms_irsa_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let token_file = dir.join("token");
        std::fs::write(&token_file, "projected-sa-jwt\n").unwrap();
        let sts_xml = "<AssumeRoleWithWebIdentityResponse><AssumeRoleWithWebIdentityResult><Credentials>\
            <AccessKeyId>AKIDTEST</AccessKeyId><SecretAccessKey>sts-secret</SecretAccessKey>\
            <SessionToken>sts-token</SessionToken><Expiration>2030-01-01T00:00:00Z</Expiration>\
            </Credentials></AssumeRoleWithWebIdentityResult></AssumeRoleWithWebIdentityResponse>";
        let transport = Arc::new(crate::llm::transport::MockTransport::new([
            HttpResponse::new(200, sts_xml),
            HttpResponse::new(200, json!({ "KeyMetadata": { "Arn": "arn:x" } }).to_string()),
            HttpResponse::new(200, json!({ "KeyMetadata": { "Arn": "arn:x" } }).to_string()),
        ]));
        let http = KmsHttp::new("aws-kms", transport.clone(), Arc::new(RecordingKmsAudit::default()), Duration::from_secs(5));
        let vars = HashMap::from([
            ("SHOGUN_AWS_KMS_KEY_ID".to_string(), "alias/shogun".to_string()),
            ("AWS_REGION".to_string(), "eu-west-1".to_string()),
            ("AWS_ROLE_ARN".to_string(), "arn:aws:iam::111122223333:role/shogun".to_string()),
            ("AWS_WEB_IDENTITY_TOKEN_FILE".to_string(), token_file.display().to_string()),
        ]);
        let aws = AwsKms::from_vars(&vars, http).unwrap();
        aws.current_arn().unwrap();
        *aws.current_arn.lock().unwrap() = None;
        aws.current_arn().unwrap();
        let sent = transport.sent();
        assert_eq!(sent.len(), 3, "one STS call, then two KMS calls on the cached credentials");
        assert_eq!(sent[0].url, "https://sts.eu-west-1.amazonaws.com/");
        assert!(sent[0].body.as_deref().unwrap().contains("WebIdentityToken=projected-sa-jwt"));
        assert!(!format!("{:?}", sent[1]).contains("sts-token"));
        assert_eq!(header(&sent[1], "x-amz-security-token"), Some("sts-token"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn configuration_names_the_missing_variable() {
        let http = || KmsHttp::new("aws-kms", Arc::new(crate::llm::transport::MockTransport::new([])), Arc::new(RecordingKmsAudit::default()), Duration::from_secs(1));
        let err = AwsKms::from_vars(&HashMap::new(), http()).err().unwrap();
        assert!(err.to_string().contains("SHOGUN_AWS_KMS_KEY_ID"));
        let vars = HashMap::from([
            ("SHOGUN_AWS_KMS_KEY_ID".to_string(), "k".to_string()),
            ("AWS_REGION".to_string(), "us-east-1".to_string()),
            ("AWS_ACCESS_KEY_ID".to_string(), "AKID".to_string()),
        ]);
        let err = AwsKms::from_vars(&vars, http()).err().unwrap();
        assert!(err.to_string().contains("AWS_SECRET_ACCESS_KEY"));
        assert!(matches!(parse_sts("<x/>"), Err(KmsError::Malformed)));
    }
}
