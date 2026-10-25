//! E50: the sidecar's connector routes (`POST /v1/internal/connectors/*`) on the real
//! `shogun-api --multi` server, with a fake vendor behind the token-exchange seam. What is proved:
//! the gateway needs the admin credential *and* the principal's own sidecar bearer; the principal is
//! the one that bearer names (a body naming another is refused); one principal never reads
//! another's grant; an agent bearer alone reaches nothing; the connect flow writes the tokenset into
//! the principal that started it and the token route then serves it; every answer is `no-store`.
#![cfg(feature = "daemon-server")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use shogun_core::daemon::Clock;
use shogun_core::llm::Secret;
use shogun_core::tenancy::{ChainedResolver, DerivedBearers, EnvKeyProvider, PrincipalId, PrincipalStore, TokenPrincipalMap};
use shogun_integrations::hosted_connect::{HostedClients, HostedConnectors, OAuthClient, Vendor};
use shogun_integrations::oauth::{TokenExchange, TokenSet};
use shogun_integrations::token::GrantStore;
use shogun_integrations::token_store::{MemorySecretStore, PrincipalGrants};
use shogun_mcp::scope::Service;
use shogun_mcp::server::{bind_exact, serve_on};
use shogun_memory::DbKey;

const SECRET: &str = "gateway-and-sidecar-derived-bearer-secret-e50x";
const ADMIN: &str = "operator-admin-credential-e50-0123";
const ACCESS: &str = "ya29.SENTINEL-alice-access-4711";

struct TempRoot(PathBuf);

impl TempRoot {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("shogun_connector_vault_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir.canonicalize().unwrap())
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn wall_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0)
    })
}

/// The vendor's token endpoint: one authorization code, never a network call.
/// The token-endpoint forms the fake vendor received.
type Forms = Arc<Mutex<Vec<Vec<(String, String)>>>>;

#[derive(Clone, Default)]
struct FakeVendor {
    forms: Forms,
}

impl TokenExchange for FakeVendor {
    fn post_form(&self, _endpoint: &str, form: &[(String, String)]) -> Result<String, String> {
        self.forms.lock().unwrap().push(form.to_vec());
        if form.iter().any(|(k, v)| k == "code" && v == "vendor-code-1") {
            return Ok(r#"{"access_token":"ya29.connected-by-flow","expires_in":3599,"refresh_token":"1//flow-refresh","scope":"https://www.googleapis.com/auth/calendar.events"}"#.into());
        }
        Err("token endpoint http 400 Bad Request".into())
    }
}

fn bearer(principal: &str, key: &str) -> String {
    DerivedBearers::new(Secret::new(SECRET)).unwrap().derive(&PrincipalId::parse(principal).unwrap(), key).unwrap()
}

fn boot(root: PathBuf, tokens: Arc<MemorySecretStore>, vendor: FakeVendor) -> u16 {
    let store = Arc::new(PrincipalStore::new(root, Arc::new(EnvKeyProvider::with_default(DbKey::new([0x5e; 32]))), wall_clock(), 8));
    let resolver = ChainedResolver {
        listed: TokenPrincipalMap::parse("carol:token-listed-carol-1").unwrap(),
        derived: Some(DerivedBearers::new(Secret::new(SECRET)).unwrap()),
    };
    let clients = HostedClients::default()
        .with_redirect_uri("https://api.example.test/v1/connectors/callback")
        .with_client(Vendor::Google, OAuthClient::new("123-abc.apps.googleusercontent.com", Some(Secret::new("web-secret"))));
    let vault = Arc::new(HostedConnectors::new(tokens, clients, move || Ok(vendor.clone())));
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let state = shogun_core::tenancy::multi::app_state(store, Arc::new(resolver), wall_clock())
                .with_admin_token(Some(ADMIN))
                .with_connector_vault(vault);
            let listener = bind_exact(0).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            tx.send(port).unwrap();
            let _ = serve_on(listener, state.with_port(port)).await;
        });
    });
    let port = rx.recv().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    port
}

struct Reply {
    status: u16,
    head: String,
    body: serde_json::Value,
}

fn call(port: u16, method: &str, path: &str, admin: Option<&str>, principal: Option<&str>, body: &str) -> Reply {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    if let Some(admin) = admin {
        req.push_str(&format!("Authorization: Bearer {admin}\r\n"));
    }
    if let Some(principal) = principal {
        req.push_str(&format!("X-Shogun-Principal-Authorization: Bearer {principal}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    stream.write_all(req.as_bytes()).unwrap();
    let mut out = Vec::new();
    stream.read_to_end(&mut out).unwrap();
    let text = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head.lines().next().and_then(|l| l.split_whitespace().nth(1)).and_then(|c| c.parse().ok()).unwrap_or(0);
    Reply { status, head: head.to_ascii_lowercase(), body: serde_json::from_str(body).unwrap_or(serde_json::Value::Null) }
}

const TOKEN: &str = "/v1/internal/connectors/token";

#[test]
fn the_gateway_gets_the_calling_principals_token_and_no_one_elses() {
    let root = TempRoot::new("token");
    let tokens = Arc::new(MemorySecretStore::new());
    let far = wall_clock()() + 3_600_000;
    PrincipalGrants::new(tokens.as_ref(), "prn_alice")
        .save(Service::Gmail, &TokenSet { access_token: ACCESS.into(), refresh_token: Some("r".into()), expires_at_ms: far, account_label: None })
        .unwrap();
    let port = boot(root.0.clone(), tokens.clone(), FakeVendor::default());
    let alice = bearer("prn_alice", "key_01");
    let bob = bearer("prn_bob", "key_01");
    let slot = r#"{"slot":"gmail-tokenset"}"#;

    let ok = call(port, "POST", TOKEN, Some(ADMIN), Some(&alice), slot);
    assert_eq!(ok.status, 200, "{}", ok.body);
    assert_eq!(ok.body, serde_json::json!({ "access_token": ACCESS, "token_type": "Bearer", "expires_at": far }));
    assert!(ok.head.contains("cache-control: no-store"), "{}", ok.head);

    // Bob's own bearer reads Bob's (absent) grant, never Alice's.
    let theirs = call(port, "POST", TOKEN, Some(ADMIN), Some(&bob), slot);
    assert_eq!(theirs.status, 404);
    assert_eq!(theirs.body["error"], "connector_not_connected");
    assert!(!theirs.body.to_string().contains(ACCESS));

    // The body never names the principal.
    let named = call(port, "POST", TOKEN, Some(ADMIN), Some(&bob), r#"{"slot":"gmail-tokenset","principal":"prn_alice"}"#);
    assert_eq!(named.status, 400);

    // An agent bearer in place of the admin credential, no admin credential, no principal, a
    // forged principal bearer: refused, and nothing is served.
    let agent = call(port, "POST", TOKEN, Some(&alice), Some(&alice), slot);
    assert_eq!(agent.status, 403, "{}", agent.body);
    assert_eq!(agent.body["error"], "admin_required");
    assert_eq!(call(port, "POST", TOKEN, None, Some(&alice), slot).status, 401);
    assert_eq!(call(port, "POST", TOKEN, Some("not-the-admin-credential-0000"), Some(&alice), slot).status, 401);
    let missing = call(port, "POST", TOKEN, Some(ADMIN), None, slot);
    assert_eq!((missing.status, missing.body["error"].as_str()), (401, Some("principal_required")));
    let forged = format!("{}A", &alice[..alice.len() - 1]);
    let forged = if forged == alice { format!("{}B", &alice[..alice.len() - 1]) } else { forged };
    assert_eq!(call(port, "POST", TOKEN, Some(ADMIN), Some(&forged), slot).status, 401);
    assert_eq!(call(port, "GET", TOKEN, Some(ADMIN), Some(&alice), "").status, 405);
    for refused in [&agent, &missing, &named] {
        assert!(!refused.body.to_string().contains(ACCESS));
        assert!(refused.head.contains("cache-control: no-store"));
    }
}

#[test]
fn the_connect_flow_writes_into_the_principal_that_started_it() {
    let root = TempRoot::new("connect");
    let tokens = Arc::new(MemorySecretStore::new());
    let vendor = FakeVendor::default();
    let port = boot(root.0.clone(), tokens.clone(), vendor.clone());
    let alice = bearer("prn_carla", "key_07");
    let other = bearer("prn_dan", "key_07");
    let state = "gateway-sealed-state-0123456789abcdef";

    let started = call(port, "POST", "/v1/internal/connectors/connect/start", Some(ADMIN), Some(&alice), &format!(r#"{{"provider":"calendar","state":"{state}"}}"#));
    assert_eq!(started.status, 200, "{}", started.body);
    let url = started.body["authorize_url"].as_str().unwrap();
    assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"), "{url}");
    assert!(url.contains("code_challenge_method=S256") && !url.contains("web-secret"));

    // Another principal's bearer cannot finish it (and burns it).
    let finish = |who: &str| call(port, "POST", "/v1/internal/connectors/connect/finish", Some(ADMIN), Some(who), &format!(r#"{{"state":"{state}","code":"vendor-code-1"}}"#));
    assert_eq!(finish(&other).status, 404);
    assert_eq!(finish(&alice).status, 404, "single use");

    let started = call(port, "POST", "/v1/internal/connectors/connect/start", Some(ADMIN), Some(&alice), &format!(r#"{{"provider":"calendar","state":"{state}"}}"#));
    assert_eq!(started.status, 200);
    let done = finish(&alice);
    assert_eq!(done.status, 200, "{}", done.body);
    assert_eq!(done.body, serde_json::json!({ "connected": true, "provider": "calendar", "slot": "gcal-tokenset" }));
    // The code and the token never come back to the gateway; the token route serves the grant.
    assert!(!done.body.to_string().contains("ya29") && !done.body.to_string().contains("vendor-code-1"));
    let served = call(port, "POST", TOKEN, Some(ADMIN), Some(&alice), r#"{"slot":"gcal-tokenset"}"#);
    assert_eq!(served.body["access_token"], "ya29.connected-by-flow");
    assert!(PrincipalGrants::new(tokens.as_ref(), "prn_dan").load(Service::GoogleCalendar).unwrap().is_none());
    // The exchange carried the PKCE verifier and the hosted callback.
    let forms = vendor.forms.lock().unwrap();
    let last = forms.last().unwrap();
    assert!(last.iter().any(|(k, _)| k == "code_verifier"));
    assert!(last.contains(&("redirect_uri".into(), "https://api.example.test/v1/connectors/callback".into())));
}
