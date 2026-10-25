//! The KMS clients' path to the network: the single traced egress (`llm::transport`, invariant 9)
//! with a record per call and a blocking bridge.
//!
//! - **Traceability without bodies.** Every KMS call is recorded before it is sent
//!   ([`KmsAudit::before`]; a sink that cannot record stops the call, [`KmsError::Untraced`]) and
//!   after it answers ([`KmsAudit::after`]): provider, operation, destination host, request size,
//!   status and latency. Never a body, a key, a token or a principal id — a KMS request body holds
//!   key material (a wrapped or a plaintext data key), so not even its digest is kept.
//! - **Blocking bridge.** [`super::KeyProvider::key_for`] is synchronous and is called from inside
//!   the server's async request handlers. The KMS round trip runs on one process-wide
//!   current-thread runtime of its own (its own thread, its own connection pool), and the caller
//!   waits on a channel with a deadline, so the call works from any thread, inside a runtime or
//!   not, and never nests runtimes.

use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use shogun_integrations::token_store::KmsError;

use crate::llm::traceability::TraceUnrecorded;
use crate::llm::transport::{HttpRequest, HttpResponse, HttpTransport, ReqwestTransport};

/// One KMS call, as recorded. No body, no key, no token, no principal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KmsCall {
    pub provider: &'static str,
    /// `GenerateDataKey`, `Encrypt`, `Decrypt`, `DescribeKey`, `encrypt`, `decrypt`, `token`, …
    pub operation: &'static str,
    /// The destination host (and port), never the path or query.
    pub destination: String,
    pub request_bytes: usize,
}

/// Where KMS calls are recorded. The default ([`StderrKmsAudit`]) writes one content-free line per
/// answered call to stderr, for the operator's log pipeline.
pub trait KmsAudit: Send + Sync {
    /// Called before the request leaves. `Err` stops it.
    fn before(&self, call: &KmsCall) -> Result<(), TraceUnrecorded>;
    /// Called when the call ends: the HTTP status (`0` when nothing answered) and the latency.
    fn after(&self, call: &KmsCall, status: u16, ms: u64);
}

/// One stderr line per KMS call: `shogun-kms: <provider> <operation> <host> status=<n> bytes=<n> ms=<n>`.
pub struct StderrKmsAudit;

impl KmsAudit for StderrKmsAudit {
    fn before(&self, _call: &KmsCall) -> Result<(), TraceUnrecorded> {
        Ok(())
    }

    fn after(&self, call: &KmsCall, status: u16, ms: u64) {
        eprintln!(
            "shogun-kms: {} {} {} status={status} bytes={} ms={ms}",
            call.provider, call.operation, call.destination, call.request_bytes
        );
    }
}

/// An in-memory audit for tests.
#[derive(Default)]
pub struct RecordingKmsAudit {
    pub calls: std::sync::Mutex<Vec<(KmsCall, u16)>>,
    pub refuse: std::sync::atomic::AtomicBool,
}

impl KmsAudit for RecordingKmsAudit {
    fn before(&self, _call: &KmsCall) -> Result<(), TraceUnrecorded> {
        if self.refuse.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(TraceUnrecorded);
        }
        Ok(())
    }

    fn after(&self, call: &KmsCall, status: u16, _ms: u64) {
        if let Ok(mut calls) = self.calls.lock() {
            calls.push((call.clone(), status));
        }
    }
}

/// The host (and port) of `url`, for the record. Empty when it does not parse.
pub fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| match u.port() {
            Some(port) => format!("{h}:{port}"),
            None => h.to_string(),
        }))
        .unwrap_or_default()
}

static RUNTIME: OnceLock<Result<tokio::runtime::Handle, String>> = OnceLock::new();

fn runtime() -> Result<tokio::runtime::Handle, KmsError> {
    RUNTIME
        .get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::channel();
            let spawned = std::thread::Builder::new().name("shogun-kms".into()).spawn(move || {
                match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(rt) => {
                        let _ = tx.send(Ok(rt.handle().clone()));
                        rt.block_on(std::future::pending::<()>());
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e.to_string()));
                    }
                }
            });
            match spawned {
                Ok(_) => rx.recv().unwrap_or_else(|_| Err("the KMS runtime did not start".into())),
                Err(e) => Err(e.to_string()),
            }
        })
        .clone()
        .map_err(|_| KmsError::Config("the KMS runtime could not start"))
}

/// Run `fut` on the KMS runtime and wait for it, at most `deadline`.
pub fn block_on<F>(fut: F, deadline: Duration) -> Result<F::Output, KmsError>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let handle = runtime()?;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let task = handle.spawn(async move {
        let _ = tx.send(fut.await);
    });
    match rx.recv_timeout(deadline) {
        Ok(out) => Ok(out),
        Err(_) => {
            task.abort();
            Err(KmsError::Unreachable)
        }
    }
}

/// A provider's HTTP client: the transport, the audit, the per-call deadline.
pub struct KmsHttp<T> {
    provider: &'static str,
    transport: Arc<T>,
    audit: Arc<dyn KmsAudit>,
    timeout: Duration,
}

impl KmsHttp<ReqwestTransport> {
    /// The real HTTPS transport (its own connection pool, on the KMS runtime) and the stderr audit.
    pub fn real(provider: &'static str, timeout: Duration) -> Result<Self, super::TenancyError> {
        let transport = ReqwestTransport::new()
            .map_err(|_| super::TenancyError::KmsConfig("the HTTPS client for the KMS could not be built"))?;
        Ok(Self::new(provider, Arc::new(transport), Arc::new(StderrKmsAudit), timeout))
    }
}

impl<T: HttpTransport + 'static> KmsHttp<T> {
    pub fn new(provider: &'static str, transport: Arc<T>, audit: Arc<dyn KmsAudit>, timeout: Duration) -> Self {
        Self { provider, transport, audit, timeout }
    }

    pub fn provider(&self) -> &'static str {
        self.provider
    }

    /// Send one request: recorded first, sent through the traced egress with the deadline,
    /// recorded again with its status. Non-2xx is an error (401/403 [`KmsError::Denied`]).
    pub fn send(&self, operation: &'static str, req: HttpRequest) -> Result<HttpResponse, KmsError> {
        let call = KmsCall {
            provider: self.provider,
            operation,
            destination: host_of(&req.url),
            request_bytes: req.body.as_ref().map_or(0, String::len),
        };
        self.audit.before(&call).map_err(|TraceUnrecorded| KmsError::Untraced)?;
        let started = Instant::now();
        let transport = self.transport.clone();
        let req = req.with_timeout(self.timeout);
        let result = block_on(async move { transport.send(req).await }, self.timeout + Duration::from_secs(2));
        let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let response = match result {
            Ok(Ok(response)) => response,
            Ok(Err(_)) | Err(_) => {
                self.audit.after(&call, 0, ms);
                return Err(KmsError::Unreachable);
            }
        };
        self.audit.after(&call, response.status, ms);
        match response.status {
            200..=299 => Ok(response),
            401 | 403 => Err(KmsError::Denied(response.status)),
            status => Err(KmsError::Rejected(status)),
        }
    }
}

/// Read a JSON body, or [`KmsError::Malformed`].
pub fn json(response: &HttpResponse) -> Result<serde_json::Value, KmsError> {
    serde_json::from_str(&response.body).map_err(|_| KmsError::Malformed)
}

/// A non-empty setting from `vars`.
pub fn var<'a>(vars: &'a std::collections::HashMap<String, String>, name: &str) -> Option<&'a str> {
    vars.get(name).map(|v| v.trim()).filter(|v| !v.is_empty())
}

/// A token-holding file (a projected service-account token, a Vault agent sink), read fresh on
/// each use so a rotated token is picked up. Trailing whitespace trimmed.
pub fn read_token_file(path: &str, setting: &'static str) -> Result<shogun_redact::Secret, KmsError> {
    let text = std::fs::read_to_string(path).map_err(|_| KmsError::Config(setting))?;
    let token = text.trim();
    if token.is_empty() {
        return Err(KmsError::Config(setting));
    }
    Ok(shogun_redact::Secret::new(token))
}

#[cfg(test)]
pub(crate) mod testing {
    //! A fake KMS behind the [`HttpTransport`] seam: each provider's test answers requests the way
    //! the real service does, from this shared bookkeeping.
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Mutex;

    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use sha2::{Digest, Sha256};

    use crate::llm::transport::{HttpRequest, HttpResponse, TransportError};

    /// Key versions, an outage switch, and every request seen.
    pub struct FakeKey {
        pub primary: AtomicU32,
        pub down: AtomicBool,
        pub seen: Mutex<Vec<HttpRequest>>,
    }

    impl FakeKey {
        pub fn new() -> Self {
            Self { primary: AtomicU32::new(1), down: AtomicBool::new(false), seen: Mutex::new(Vec::new()) }
        }

        pub fn rotate(&self) {
            self.primary.fetch_add(1, Ordering::SeqCst);
        }

        pub fn primary(&self) -> u32 {
            self.primary.load(Ordering::SeqCst)
        }

        /// Record the request; `Err` when the fake is down (as a refused connection).
        pub fn enter(&self, req: &HttpRequest) -> Result<(), TransportError> {
            if let Ok(mut seen) = self.seen.lock() {
                seen.push(req.clone());
            }
            if self.down.load(Ordering::SeqCst) {
                return Err(TransportError::Unreachable("fake kms down".into()));
            }
            Ok(())
        }

        fn pad(version: u32, context: &str) -> Vec<u8> {
            let mut h = Sha256::new();
            h.update(b"fake-kms");
            h.update(version.to_be_bytes());
            h.update(context.as_bytes());
            h.finalize().to_vec()
        }

        fn check(version: u32, context: &str) -> [u8; 4] {
            let mut h = Sha256::new();
            h.update(b"fake-kms-check");
            h.update(version.to_be_bytes());
            h.update(context.as_bytes());
            let d = h.finalize();
            [d[0], d[1], d[2], d[3]]
        }

        /// "Encrypt" `plain` under the primary version, bound to `context`: base64 of
        /// `version ‖ check(context) ‖ plain ⊕ pad`.
        pub fn encrypt(&self, plain_b64: &str, context: &str) -> Option<(u32, String)> {
            let plain = STANDARD.decode(plain_b64).ok()?;
            let version = self.primary();
            let pad = Self::pad(version, context);
            let mut out = version.to_be_bytes().to_vec();
            out.extend_from_slice(&Self::check(version, context));
            out.extend(plain.iter().zip(pad.iter().cycle()).map(|(a, b)| a ^ b));
            Some((version, STANDARD.encode(out)))
        }

        /// Reverse [`Self::encrypt`]; `None` when the context does not match (a real KMS answers
        /// 400 then).
        pub fn decrypt(&self, cipher_b64: &str, context: &str) -> Option<(u32, String)> {
            let bytes = STANDARD.decode(cipher_b64).ok()?;
            if bytes.len() < 8 {
                return None;
            }
            let version = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            if bytes[4..8] != Self::check(version, context) {
                return None;
            }
            let pad = Self::pad(version, context);
            let plain: Vec<u8> = bytes[8..].iter().zip(pad.iter().cycle()).map(|(a, b)| a ^ b).collect();
            Some((version, STANDARD.encode(plain)))
        }
    }

    pub fn ok(body: serde_json::Value) -> Result<HttpResponse, TransportError> {
        Ok(HttpResponse::new(200, body.to_string()))
    }

    pub fn status(code: u16) -> Result<HttpResponse, TransportError> {
        Ok(HttpResponse::new(code, "{\"error\":\"fake\"}"))
    }

    pub fn header<'a>(req: &'a HttpRequest, name: &str) -> Option<&'a str> {
        req.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::llm::transport::{Method, MockTransport};

    #[test]
    fn a_call_is_recorded_without_its_body_and_an_unrecordable_call_is_not_sent() {
        let audit = Arc::new(RecordingKmsAudit::default());
        let transport = Arc::new(MockTransport::new([HttpResponse::new(200, "{}"), HttpResponse::new(403, "{}")]));
        let http = KmsHttp::new("vault", transport.clone(), audit.clone(), Duration::from_secs(5));
        let req = HttpRequest::new(Method::Post, "https://vault.example:8200/v1/transit/decrypt/k", vec![], Some("{\"ciphertext\":\"secret-bytes\"}".into())).unwrap();
        http.send("decrypt", req.clone()).unwrap();
        assert_eq!(http.send("decrypt", req.clone()).err(), Some(KmsError::Denied(403)));
        let calls = audit.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0.destination, "vault.example:8200");
        assert_eq!(calls[0].1, 200);
        assert!(!format!("{calls:?}").contains("secret-bytes"));

        audit.refuse.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(http.send("decrypt", req).err(), Some(KmsError::Untraced));
        assert_eq!(transport.sent().len(), 2, "an unrecorded call never reaches the transport");
    }

    #[tokio::test]
    async fn the_bridge_works_from_inside_a_runtime_and_reports_a_closed_port_as_unreachable() {
        // A real socket: a loopback port nothing listens on (http to loopback is admitted in tests).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let audit = Arc::new(RecordingKmsAudit::default());
        let http = KmsHttp::new("vault", Arc::new(ReqwestTransport::new().unwrap()), audit.clone(), Duration::from_secs(3));
        let req = HttpRequest::new(Method::Get, format!("http://127.0.0.1:{port}/v1/sys/health"), vec![], None).unwrap();
        assert_eq!(http.send("health", req).err(), Some(KmsError::Unreachable));
        assert_eq!(audit.calls.lock().unwrap()[0].1, 0);
    }

    #[test]
    fn hosts_never_carry_paths() {
        assert_eq!(host_of("https://kms.eu-west-1.amazonaws.com/"), "kms.eu-west-1.amazonaws.com");
        assert_eq!(host_of("https://cloudkms.googleapis.com/v1/projects/p/x:decrypt"), "cloudkms.googleapis.com");
        assert_eq!(host_of("not a url"), "");
    }
}
