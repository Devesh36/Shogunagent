//! HashiCorp Vault Transit as a [`KeyWrapper`] (E26): `transit/encrypt/<key>` and
//! `transit/decrypt/<key>`, the data key drawn locally.
//!
//! - **Binding.** The context goes in `associated_data` (an `aes256-gcm96` or
//!   `chacha20-poly1305` transit key, the default type).
//! - **Rotation.** Vault ciphertext names its version (`vault:v<n>:…`). After a decrypt the
//!   provider reads the key's `latest_version` (`GET transit/keys/<key>`; a policy without `read`
//!   there only skips the check) and reports a blob from an older version stale, so it is
//!   re-wrapped under the latest one. Set `min_decryption_version` after a rotation has reached
//!   every principal to retire the old version.
//! - **Credentials.** `VAULT_TOKEN`, `VAULT_TOKEN_FILE` (a Vault Agent sink, read fresh on each
//!   call), or Kubernetes auth (`SHOGUN_VAULT_K8S_ROLE`: the pod's projected service-account token
//!   is exchanged at `auth/<mount>/login` and the client token cached for its lease). Held in
//!   [`Secret`]; read (`.expose(`) only for the `X-Vault-Token` header and the login body —
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

/// The default projected service-account token path in a pod.
pub const K8S_TOKEN_FILE: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";

/// Where the Vault token comes from.
pub enum VaultAuth {
    Static(Secret),
    File(String),
    Kubernetes { mount: String, role: String, jwt_file: String, cached: Mutex<Option<(Secret, Instant)>> },
}

/// The Vault Transit wrapper.
pub struct VaultTransit<T> {
    http: KmsHttp<T>,
    addr: String,
    mount: String,
    key: String,
    namespace: Option<String>,
    auth: VaultAuth,
}

fn segment_ok(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

fn mount_ok(s: &str) -> bool {
    s.split('/').all(segment_ok)
}

impl<T: HttpTransport + 'static> VaultTransit<T> {
    /// From the environment (see `deploy/README.md`): `VAULT_ADDR`, `SHOGUN_VAULT_TRANSIT_KEY`,
    /// optional `SHOGUN_VAULT_TRANSIT_MOUNT` (default `transit`) and `VAULT_NAMESPACE`, and the auth.
    pub fn from_vars(vars: &HashMap<String, String>, http: KmsHttp<T>) -> Result<Self, TenancyError> {
        let addr = var(vars, "VAULT_ADDR").ok_or(TenancyError::KmsConfig("VAULT_ADDR"))?;
        let key = var(vars, "SHOGUN_VAULT_TRANSIT_KEY").ok_or(TenancyError::KmsConfig("SHOGUN_VAULT_TRANSIT_KEY"))?;
        let mount = var(vars, "SHOGUN_VAULT_TRANSIT_MOUNT").unwrap_or("transit");
        let auth = if let Some(token) = var(vars, "VAULT_TOKEN") {
            VaultAuth::Static(Secret::new(token))
        } else if let Some(path) = var(vars, "VAULT_TOKEN_FILE") {
            VaultAuth::File(path.to_string())
        } else if let Some(role) = var(vars, "SHOGUN_VAULT_K8S_ROLE") {
            let k8s_mount = var(vars, "SHOGUN_VAULT_K8S_MOUNT").unwrap_or("kubernetes");
            if !mount_ok(k8s_mount) {
                return Err(TenancyError::KmsConfig("SHOGUN_VAULT_K8S_MOUNT"));
            }
            VaultAuth::Kubernetes {
                mount: k8s_mount.to_string(),
                role: role.to_string(),
                jwt_file: var(vars, "SHOGUN_VAULT_K8S_TOKEN_FILE").unwrap_or(K8S_TOKEN_FILE).to_string(),
                cached: Mutex::new(None),
            }
        } else {
            return Err(TenancyError::KmsConfig("VAULT_TOKEN, VAULT_TOKEN_FILE or SHOGUN_VAULT_K8S_ROLE"));
        };
        Self::new(http, addr, mount, key, var(vars, "VAULT_NAMESPACE").map(str::to_string), auth)
    }

    pub fn new(http: KmsHttp<T>, addr: &str, mount: &str, key: &str, namespace: Option<String>, auth: VaultAuth) -> Result<Self, TenancyError> {
        if url::Url::parse(addr).is_err() {
            return Err(TenancyError::KmsConfig("VAULT_ADDR"));
        }
        if !mount_ok(mount) {
            return Err(TenancyError::KmsConfig("SHOGUN_VAULT_TRANSIT_MOUNT"));
        }
        if !segment_ok(key) {
            return Err(TenancyError::KmsConfig("SHOGUN_VAULT_TRANSIT_KEY"));
        }
        Ok(Self {
            http,
            addr: addr.trim_end_matches('/').to_string(),
            mount: mount.to_string(),
            key: key.to_string(),
            namespace,
            auth,
        })
    }

    fn headers(&self, token: Option<&Secret>) -> Vec<(String, String)> {
        let mut headers = vec![("content-type".to_string(), "application/json".to_string())];
        if let Some(token) = token {
            headers.push(("x-vault-token".to_string(), token.expose().to_string()));
        }
        if let Some(ns) = &self.namespace {
            headers.push(("x-vault-namespace".to_string(), ns.clone()));
        }
        headers
    }

    fn request(&self, method: Method, path: &str, token: Option<&Secret>, body: Option<String>) -> Result<HttpRequest, KmsError> {
        HttpRequest::new(method, format!("{}/v1/{path}", self.addr), self.headers(token), body)
            .map(HttpRequest::without_redirects)
            .map_err(|_| KmsError::Config("VAULT_ADDR must be https"))
    }

    fn token(&self) -> Result<Secret, KmsError> {
        match &self.auth {
            VaultAuth::Static(token) => Ok(token.clone()),
            VaultAuth::File(path) => read_token_file(path, "VAULT_TOKEN_FILE"),
            VaultAuth::Kubernetes { mount, role, jwt_file, cached } => {
                if let Some((token, until)) = cached.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
                    if Instant::now() < *until {
                        return Ok(token.clone());
                    }
                }
                let jwt = read_token_file(jwt_file, "SHOGUN_VAULT_K8S_TOKEN_FILE")?;
                let body = json!({ "role": role, "jwt": jwt.expose() }).to_string();
                let req = self.request(Method::Post, &format!("auth/{mount}/login"), None, Some(body))?;
                let answer = json_body(&self.http.send("login", req)?)?;
                let token = answer.pointer("/auth/client_token").and_then(Value::as_str).ok_or(KmsError::Malformed)?;
                let lease = answer.pointer("/auth/lease_duration").and_then(Value::as_u64).unwrap_or(300);
                let token = Secret::new(token);
                let until = Instant::now() + Duration::from_secs(lease.saturating_sub(lease / 10).max(1));
                *cached.lock().unwrap_or_else(PoisonError::into_inner) = Some((token.clone(), until));
                Ok(token)
            }
        }
    }

    fn post(&self, operation: &'static str, body: &Value) -> Result<Value, KmsError> {
        let token = self.token()?;
        let path = format!("{}/{operation}/{}", self.mount, self.key);
        let req = self.request(Method::Post, &path, Some(&token), Some(body.to_string()))?;
        json_body(&self.http.send(operation, req)?)
    }

    /// The transit key's latest version, or `None` when it cannot be read.
    fn latest_version(&self) -> Option<u64> {
        let token = self.token().ok()?;
        let req = self.request(Method::Get, &format!("{}/keys/{}", self.mount, self.key), Some(&token), None).ok()?;
        let answer = json_body(&self.http.send("read_key", req).ok()?).ok()?;
        answer.pointer("/data/latest_version").and_then(Value::as_u64)
    }
}

/// `vault:v<n>:…` → `n`.
pub fn ciphertext_version(ciphertext: &str) -> Option<u64> {
    ciphertext.strip_prefix("vault:v")?.split(':').next()?.parse().ok()
}

impl<T: HttpTransport + 'static> KeyWrapper for VaultTransit<T> {
    fn provider(&self) -> &'static str {
        "vault"
    }

    fn wrap(&self, key: &DataKey, context: &str) -> Result<WrappedDataKey, KmsError> {
        let plaintext = key.with_bytes(|b| STANDARD.encode(b));
        let answer = self.post("encrypt", &json!({ "plaintext": plaintext, "associated_data": STANDARD.encode(context) }))?;
        let ciphertext = answer.pointer("/data/ciphertext").and_then(Value::as_str).ok_or(KmsError::Malformed)?;
        let version = ciphertext_version(ciphertext).ok_or(KmsError::Malformed)?;
        Ok(WrappedDataKey { key_version: format!("v{version}"), blob: ciphertext.as_bytes().to_vec() })
    }

    fn unwrap(&self, wrapped: &WrappedDataKey, context: &str) -> Result<Unwrapped, KmsError> {
        let ciphertext = std::str::from_utf8(&wrapped.blob).map_err(|_| KmsError::Malformed)?;
        let answer = self.post("decrypt", &json!({ "ciphertext": ciphertext, "associated_data": STANDARD.encode(context) }))?;
        let plain = answer.pointer("/data/plaintext").and_then(Value::as_str).ok_or(KmsError::Malformed)?;
        let mut bytes = STANDARD.decode(plain).map_err(|_| KmsError::Malformed)?;
        let key = DataKey::from_slice(&mut bytes)?;
        let stale = match (ciphertext_version(ciphertext), self.latest_version()) {
            (Some(used), Some(latest)) => used < latest,
            _ => false,
        };
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

    struct FakeVault {
        key: FakeKey,
    }

    impl FakeVault {
        fn answer(&self, req: &HttpRequest) -> Result<HttpResponse, TransportError> {
            self.key.enter(req)?;
            let path = req.url.strip_prefix("https://vault.internal:8200/v1/").unwrap_or("");
            if path == "auth/kubernetes/login" {
                let body: Value = serde_json::from_str(req.body.as_deref().unwrap()).unwrap();
                if body["role"] != "shogun" || body["jwt"] != "pod-sa-jwt" {
                    return status(403);
                }
                return ok(json!({ "auth": { "client_token": "hvs.k8s-token", "lease_duration": 3600 } }));
            }
            if header(req, "x-vault-token") != Some("hvs.k8s-token") || header(req, "x-vault-namespace") != Some("acme") {
                return status(403);
            }
            let body: Value = req.body.as_deref().map(|b| serde_json::from_str(b).unwrap()).unwrap_or(Value::Null);
            let aad = || String::from_utf8(STANDARD.decode(body["associated_data"].as_str().unwrap()).unwrap()).unwrap();
            match path {
                "transit/encrypt/principals" => {
                    let (v, blob) = self.key.encrypt(body["plaintext"].as_str().unwrap(), &aad()).unwrap();
                    ok(json!({ "data": { "ciphertext": format!("vault:v{v}:{blob}"), "key_version": v } }))
                }
                "transit/decrypt/principals" => {
                    let text = body["ciphertext"].as_str().unwrap();
                    let blob = text.splitn(3, ':').nth(2).unwrap();
                    match self.key.decrypt(blob, &aad()) {
                        Some((_, plain)) => ok(json!({ "data": { "plaintext": plain } })),
                        None => status(400),
                    }
                }
                "transit/keys/principals" => ok(json!({ "data": { "latest_version": self.key.primary() } })),
                _ => status(404),
            }
        }
    }

    impl HttpTransport for FakeVault {
        fn send(&self, req: HttpRequest) -> impl std::future::Future<Output = Result<HttpResponse, TransportError>> + Send {
            std::future::ready(self.answer(&req))
        }
    }

    fn vault(fake: Arc<FakeVault>, jwt: &std::path::Path) -> VaultTransit<FakeVault> {
        let http = KmsHttp::new("vault", fake, Arc::new(RecordingKmsAudit::default()), Duration::from_secs(5));
        let vars = HashMap::from([
            ("VAULT_ADDR".to_string(), "https://vault.internal:8200".to_string()),
            ("VAULT_NAMESPACE".to_string(), "acme".to_string()),
            ("SHOGUN_VAULT_TRANSIT_KEY".to_string(), "principals".to_string()),
            ("SHOGUN_VAULT_K8S_ROLE".to_string(), "shogun".to_string()),
            ("SHOGUN_VAULT_K8S_TOKEN_FILE".to_string(), jwt.display().to_string()),
        ]);
        VaultTransit::from_vars(&vars, http).unwrap()
    }

    #[test]
    fn kubernetes_login_wrap_unwrap_and_rotation() {
        let dir = std::env::temp_dir().join(format!("shogun_kms_vault_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let jwt = dir.join("token");
        std::fs::write(&jwt, "pod-sa-jwt").unwrap();
        let fake = Arc::new(FakeVault { key: FakeKey::new() });
        let kms = vault(fake.clone(), &jwt);

        let (key, wrapped) = kms.generate("shogun/sqlcipher/v1/alice").unwrap();
        assert_eq!(wrapped.key_version, "v1");
        assert!(wrapped.blob.starts_with(b"vault:v1:"));
        let opened = kms.unwrap(&wrapped, "shogun/sqlcipher/v1/alice").unwrap();
        assert!(!opened.stale);
        assert_eq!(opened.key.with_bytes(|b| *b), key.with_bytes(|b| *b));
        assert_eq!(kms.unwrap(&wrapped, "shogun/sqlcipher/v1/bob").err(), Some(KmsError::Rejected(400)));

        fake.key.rotate();
        let opened = kms.unwrap(&wrapped, "shogun/sqlcipher/v1/alice").unwrap();
        assert!(opened.stale, "v1 blob, latest v2");
        assert_eq!(kms.wrap(&opened.key, "shogun/sqlcipher/v1/alice").unwrap().key_version, "v2");

        let seen = fake.key.seen.lock().unwrap().clone();
        assert_eq!(seen.iter().filter(|r| r.url.ends_with("/login")).count(), 1, "client token cached");
        assert!(seen.iter().all(|r| !format!("{r:?}").contains("hvs.k8s-token")));

        fake.key.down.store(true, Ordering::SeqCst);
        assert_eq!(kms.unwrap(&wrapped, "shogun/sqlcipher/v1/alice").err(), Some(KmsError::Unreachable));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plain_http_and_unsafe_names_are_refused() {
        let http = || KmsHttp::new("vault", Arc::new(crate::llm::transport::MockTransport::new([])), Arc::new(RecordingKmsAudit::default()), Duration::from_secs(1));
        let base = |extra: (&str, &str)| {
            let mut vars = HashMap::from([
                ("VAULT_ADDR".to_string(), "https://vault.internal:8200".to_string()),
                ("SHOGUN_VAULT_TRANSIT_KEY".to_string(), "principals".to_string()),
                ("VAULT_TOKEN".to_string(), "hvs.x".to_string()),
            ]);
            vars.insert(extra.0.to_string(), extra.1.to_string());
            vars
        };
        assert!(VaultTransit::from_vars(&base(("X", "")), http()).is_ok());
        assert!(VaultTransit::from_vars(&base(("SHOGUN_VAULT_TRANSIT_KEY", "../sys")), http()).is_err());
        assert!(VaultTransit::from_vars(&base(("SHOGUN_VAULT_TRANSIT_MOUNT", "a//b")), http()).is_err());
        let plain = VaultTransit::from_vars(&base(("VAULT_ADDR", "http://vault.internal:8200")), http()).unwrap();
        assert_eq!(plain.wrap(&DataKey::from_bytes([1; 32]), "c").err(), Some(KmsError::Config("VAULT_ADDR must be https")));
        let mut none = base(("X", ""));
        none.remove("VAULT_TOKEN");
        assert!(VaultTransit::from_vars(&none, http()).is_err());
        assert_eq!(ciphertext_version("vault:v12:abc"), Some(12));
        assert_eq!(ciphertext_version("abc"), None);
    }
}
