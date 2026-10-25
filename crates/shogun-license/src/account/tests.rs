use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::*;

const V: [u8; 32] = [7u8; 32];
const S: [u8; 32] = [9u8; 32];

// ── Browser sign-in ───────────────────────────────────────────────────────────────────────────

#[test]
fn the_app_outlives_the_sites_google_window() {
    // The site's Google sign-in lives about 10 minutes and starts after the app's clock; a retry
    // on the site needs the loopback port still open.
    assert_eq!(SIGNIN_WAIT_SECS, 30 * 60);
}

#[test]
fn pkce_matches_the_rfc_7636_vector_the_server_checks_against() {
    assert_eq!(
        pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
}

#[test]
fn a_request_has_server_shaped_values_from_two_independent_draws() {
    let r = SigninRequest::new(&V, &S, 53_682).unwrap();
    // isCodeVerifier / isCodeChallenge / isAppState on the site.
    assert_eq!(r.code_verifier.len(), 43);
    assert_eq!(r.code_challenge.len(), 43);
    assert_eq!(r.code_challenge, pkce_challenge(&r.code_verifier));
    assert!((16..=128).contains(&r.state.len()));
    assert!(r.state.bytes().all(is_b64url));
    assert_ne!(r.state, r.code_verifier, "the state must not reveal the verifier");
    assert_eq!(r.redirect_uri, "http://127.0.0.1:53682/callback");
    assert_eq!(
        r.browser_url("https://shogunai.si/", None),
        format!(
            "https://shogunai.si/app/signin?code_challenge={}&code_challenge_method=S256&state={}&redirect_uri=http%3A%2F%2F127.0.0.1%3A53682%2Fcallback",
            r.code_challenge, r.state
        )
    );
    assert!(!r.browser_url("https://shogunai.si", None).contains(&r.code_verifier));
    assert_eq!(format!("{r:?}"), "SigninRequest { .. }", "Debug shows nothing");
}

#[test]
fn google_opens_its_start_route_with_the_same_query_and_anything_else_the_signin_page() {
    let r = SigninRequest::new(&V, &S, 53_682).unwrap();
    let query = format!(
        "?code_challenge={}&code_challenge_method=S256&state={}&redirect_uri=http%3A%2F%2F127.0.0.1%3A53682%2Fcallback",
        r.code_challenge, r.state
    );
    assert_eq!(
        r.browser_url("https://shogunai.si/", Some("google")),
        format!("https://shogunai.si/api/account/google/start{query}")
    );
    for other in [None, Some(""), Some("Google"), Some("passkey"), Some("google ")] {
        assert_eq!(
            r.browser_url("https://shogunai.si", other),
            format!("https://shogunai.si/app/signin{query}"),
            "{other:?}"
        );
    }
}

#[test]
fn weak_entropy_and_privileged_ports_are_refused() {
    assert_eq!(SigninRequest::new(&V, &V, 50_000), Err(SigninError::WeakEntropy));
    assert_eq!(SigninRequest::new(&V[..16], &S, 50_000), Err(SigninError::WeakEntropy));
    assert_eq!(SigninRequest::new(&V, &S, 80), Err(SigninError::BadPort));
    assert_eq!(SigninRequest::new(&V, &S, 1023), Err(SigninError::BadPort));
    assert!(SigninRequest::new(&V, &S, 1024).is_ok());
}

fn code() -> String {
    "A".repeat(43)
}

#[test]
fn only_this_sign_ins_redirect_resolves_the_wait() {
    let state = "app-state-0123456789";
    let ok = format!("GET /callback?code={}&state={state} HTTP/1.1", code());
    assert_eq!(classify_callback(&ok, state), CallbackVerdict::Code(code()));
    // Order and extra parameters do not matter.
    let reordered = format!("GET /callback?state={state}&x=1&code={} HTTP/1.1", code());
    assert_eq!(classify_callback(&reordered, state), CallbackVerdict::Code(code()));
    for stray in [
        "GET /favicon.ico HTTP/1.1".to_string(),
        format!("GET /callback?code={}&state=another-flow-state HTTP/1.1", code()),
        format!("POST /callback?code={}&state={state} HTTP/1.1", code()),
        format!("GET /callback/?code={}&state={state} HTTP/1.1", code()),
        format!("GET /cb?code={}&state={state} HTTP/1.1", code()),
        format!("GET /callback?code=short&state={state} HTTP/1.1"),
        format!("GET /callback?code={}%41&state={state} HTTP/1.1", &code()[..40]),
        format!("GET /callback?code={}&code={}&state={state} HTTP/1.1", code(), code()),
        format!("GET /callback?state={state} HTTP/1.1"),
        format!("GET /callback?code={} HTTP/1.1", code()),
        format!("GET /callback?error=access_denied&state={state} HTTP/1.1"),
        String::new(),
        "garbage".to_string(),
    ] {
        assert_eq!(classify_callback(&stray, state), CallbackVerdict::Stray, "{stray}");
    }
    assert_eq!(format!("{:?}", classify_callback(&ok, state)), "Code(..)");
}

#[test]
fn the_callback_page_is_a_complete_http_answer_without_internals() {
    assert!(CALLBACK_PAGE.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(CALLBACK_PAGE.contains("\r\n\r\n<!doctype html>"));
    for word in ["PKCE", "OAuth", "token", "code"] {
        assert!(!CALLBACK_PAGE.contains(word), "{word}");
    }
}

#[test]
fn the_callback_page_is_private_and_does_not_claim_signin_succeeded() {
    assert!(CALLBACK_PAGE.contains("Referrer-Policy: no-referrer\r\n"));
    assert!(CALLBACK_PAGE.contains("default-src 'none'"));
    assert!(CALLBACK_PAGE.contains("referrerpolicy=\"no-referrer\""));
    assert!(CALLBACK_PAGE.contains("name=\"viewport\""));
    assert!(CALLBACK_PAGE.contains("Return to the app to finish signing in."));
    assert!(!CALLBACK_PAGE.contains("<script"));
    assert!(!CALLBACK_PAGE.contains("Signed in"));
}

#[test]
fn this_sign_ins_redirect_sends_the_tab_to_the_site_without_the_code() {
    let r = SIGNIN_DONE_REDIRECT;
    assert!(r.starts_with("HTTP/1.1 303 See Other\r\n"));
    assert!(r.contains(format!("\r\nLocation: {SIGNIN_ORIGIN}/app/signin/done\r\n").as_str()));
    assert!(r.contains("\r\nCache-Control: no-store\r\n"));
    assert!(r.contains("\r\nReferrer-Policy: no-referrer\r\n"));
    assert!(r.ends_with("\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"));
    for leak in ["code", "state", "?", "#", "127.0.0.1"] {
        assert!(!r.contains(leak), "{leak}");
    }
}

// ── Wire types ────────────────────────────────────────────────────────────────────────────────

fn tokens_body(n: u32) -> Value {
    json!({
        "ok": true,
        "refresh_token": format!("sr1.00000000-0000-0000-0000-000000000000.refresh{n}"),
        "access_token": format!("sa1.00000000-0000-0000-0000-000000000000.access{n}"),
        "access_expires_at": 1_000 + u64::from(n) * 900,
        "refresh_expires_at": 9_000_000,
    })
}

fn tokens(n: u32) -> SessionTokens {
    parse_session_tokens(&tokens_body(n)).unwrap()
}

#[test]
fn session_tokens_parse_only_from_a_whole_answer_and_never_print() {
    let t = tokens(1);
    assert!(t.refresh_token.starts_with("sr1."));
    assert_eq!(t.access_expires_at, 1_900);
    let shown = format!("{t:?}");
    assert!(!shown.contains("refresh1") && !shown.contains("access1"), "{shown}");
    let mut swapped = tokens_body(1);
    swapped["refresh_token"] = swapped["access_token"].clone();
    for bad in [
        json!({ "ok": false, "error": "unauthorized" }),
        json!({ "ok": true }),
        swapped,
        json!({ "ok": true, "refresh_token": "sr1.x", "access_token": "sa1.y" }),
        json!({ "ok": true, "refresh_token": "sr1.a b", "access_token": "sa1.y", "access_expires_at": 1, "refresh_expires_at": 2 }),
    ] {
        assert_eq!(parse_session_tokens(&bad), Err(WireError), "{bad}");
    }
}

#[test]
fn the_exchange_body_and_answer_match_the_server() {
    let r = SigninRequest::new(&V, &S, 50_000).unwrap();
    let body = exchange_body(&r, &code(), "device-0001", Some("  MacBook Pro  "));
    assert_eq!(body["code"], code());
    assert_eq!(body["code_verifier"], r.code_verifier);
    assert_eq!(body["device"], json!({ "id": "device-0001", "name": "MacBook Pro" }));
    assert_eq!(body["redirect_uri"], r.redirect_uri);
    // No name, or one the server would refuse, is simply left out.
    for name in [None, Some("   "), Some("bad\u{7}name")] {
        assert_eq!(exchange_body(&r, &code(), "device-0001", name)["device"], json!({ "id": "device-0001" }));
    }
    assert_eq!(clean_device_name(&"x".repeat(65)), None);
    assert_eq!(clean_device_name(&"x".repeat(64)).map(|n| n.len()), Some(64));

    let mut answer = tokens_body(1);
    answer["account"] = json!({ "id": "acc", "email": "a@example.test", "email_confirmed": false,
        "pending_email": "a@example.test", "email_detached": false, "created_at": 1 });
    let (account, t) = parse_exchange(&answer).unwrap();
    assert_eq!(account.pending_email.as_deref(), Some("a@example.test"));
    assert!(!account.email_confirmed);
    assert_eq!(t, tokens(1));
    assert!(!format!("{account:?}").contains("example"), "Debug never shows the address");
    assert_eq!(refresh_body("sr1.t", "dev"), json!({ "refresh_token": "sr1.t", "device_id": "dev" }));
}

#[test]
fn the_account_view_reads_the_servers_shape_and_drops_bad_rows() {
    let body = json!({
        "ok": true,
        "account": { "id": "a", "email": "me@example.test", "email_confirmed": true, "pending_email": null, "email_detached": false },
        "plan": null,
        "trial": { "trial_ends_at": 1_760_600_000 },
        "display_name": "Person Name",
        "first_name": "Person",
        "avatar_url": "https://lh3.googleusercontent.com/profile",
        "avatar_data_url": "server-must-not-inject-data",
        "identities": [{ "provider": "google", "email": "me@gmail.example", "created_at": 1 }, { "email": "x" }],
        "passkeys": [{ "id": "pk-1", "created_at": 5, "last_used_at": null, "synced": true }, { "created_at": 1 }],
        "devices": [
            { "device_id": "dev-a", "name": "MacBook Pro", "last_seen_at": 10, "holds_seat": true, "signed_in": true, "this_device": true },
            { "device_id": "dev-b", "name": null, "last_seen_at": null, "holds_seat": false, "signed_in": false, "this_device": false },
            { "name": "no id" }
        ],
        "session": { "device_id": "dev-a", "fresh_until": 1_760_000_300 },
    });
    let v = parse_account_view(&body).unwrap();
    assert_eq!(v.account.email.as_deref(), Some("me@example.test"));
    assert_eq!(v.trial_ends_at, Some(1_760_600_000));
    assert_eq!(v.display_name, None, "server views never supply locally held profile metadata");
    assert_eq!(v.first_name, None);
    assert_eq!(v.avatar_url, None);
    assert_eq!(v.avatar_data_url, None);
    assert_eq!(v.identities.len(), 1);
    assert_eq!(v.passkeys, vec![PasskeyView { id: "pk-1".into(), created_at: Some(5), last_used_at: None, synced: true }]);
    assert_eq!(v.devices.len(), 2);
    assert!(v.devices[0].this_device && v.devices[0].holds_seat);
    assert_eq!(v.devices[1].name, None);
    assert_eq!(v.fresh_until, Some(1_760_000_300));
    let shown = format!("{v:?}");
    assert!(!shown.contains("dev-a") && !shown.contains("example") && !shown.contains("Person Name") && !shown.contains("googleusercontent"), "{shown}");
    assert_eq!(parse_account_view(&json!({ "ok": false, "error": "unauthorized" })), Err(WireError));
}

#[test]
fn only_actionable_reasons_surface() {
    let body = |r: &str| json!({ "ok": false, "error": "forbidden", "reason": r });
    for r in ["reauth_required", "active_subscription", "last_passkey", "account_required"] {
        assert_eq!(error_reason(&body(r)).as_deref(), Some(r));
    }
    assert_eq!(error_reason(&body("db_unavailable")), None);
    assert_eq!(error_reason(&json!({ "ok": false })), None);
}

#[test]
fn a_reauth_link_opens_only_on_the_licence_apis_own_origin() {
    let origin = "https://shogunai.si";
    let ok = |url: &str| json!({ "ok": true, "url": url, "expires_at": 1 });
    assert_eq!(
        parse_reauth_url(&ok("https://shogunai.si/app/reauth#c=abc"), origin).as_deref(),
        Ok("https://shogunai.si/app/reauth#c=abc")
    );
    assert!(parse_reauth_url(&ok("https://shogunai.si/api/account/google/reauth?ticket=t"), origin).is_ok());
    for bad in [
        "https://shogunai.si.evil.example/app/reauth",
        "https://evil.example/app/reauth",
        "http://shogunai.si/app/reauth",
        "javascript:alert(1)",
        "https://shogunai.si/app/reauth\n",
    ] {
        assert_eq!(parse_reauth_url(&ok(bad), origin), Err(WireError), "{bad}");
    }
    assert_eq!(parse_reauth_url(&json!({ "ok": false }), origin), Err(WireError));
    // A plain-http origin is only ever a local development server.
    assert!(parse_reauth_url(&ok("http://localhost:3000/app/reauth#c=x"), "http://localhost:3000").is_ok());
    assert!(parse_reauth_url(&ok("http://staging.example/app/reauth"), "http://staging.example").is_err());
}

// ── The session ───────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct Store {
    value: Arc<Mutex<Option<String>>>,
    fail_save: Arc<AtomicBool>,
}

impl Store {
    fn with(token: &str) -> Self {
        let s = Self::default();
        *s.value.lock().unwrap() = Some(token.to_string());
        s
    }
    fn get(&self) -> Option<String> {
        self.value.lock().unwrap().clone()
    }
}

impl SessionStore for Store {
    fn load(&self) -> Result<Option<String>, StoreError> {
        Ok(self.get())
    }
    fn save(&self, refresh_token: &str) -> Result<(), StoreError> {
        if self.fail_save.load(Ordering::SeqCst) {
            return Err(StoreError);
        }
        *self.value.lock().unwrap() = Some(refresh_token.to_string());
        Ok(())
    }
    fn clear(&self) -> Result<(), StoreError> {
        *self.value.lock().unwrap() = None;
        Ok(())
    }
}

/// A server that rotates `sr1…refresh<n>` to `<n+1>`, with scripted failures first.
#[derive(Clone, Default)]
struct Server {
    calls: Arc<AtomicUsize>,
    script: Arc<Mutex<Vec<Result<(), TransportError>>>>,
    presented: Arc<Mutex<Vec<String>>>,
    pause_ms: u64,
}

impl Server {
    fn scripted(script: Vec<Result<(), TransportError>>) -> Self {
        Self { script: Arc::new(Mutex::new(script)), ..Self::default() }
    }
}

impl RefreshTransport for Server {
    fn refresh(&self, refresh_token: &str, device_id: &str) -> Result<SessionTokens, TransportError> {
        assert_eq!(device_id, "dev-1");
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.presented.lock().unwrap().push(refresh_token.to_string());
        if self.pause_ms > 0 {
            std::thread::sleep(Duration::from_millis(self.pause_ms));
        }
        let next = {
            let mut s = self.script.lock().unwrap();
            if s.is_empty() { Ok(()) } else { s.remove(0) }
        };
        next?;
        let n: u32 = refresh_token.rsplit("refresh").next().and_then(|n| n.parse().ok()).unwrap_or(0);
        Ok(tokens(n + 1))
    }
}

fn session(store: &Store, server: &Server, now: u64) -> (Session<Store, Server>, Arc<Mutex<Vec<Duration>>>) {
    let slept = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&slept);
    let s = Session::new(store.clone(), server.clone(), "dev-1")
        .with_clock(move || now, move |d| log.lock().unwrap().push(d));
    (s, slept)
}

#[test]
fn a_fresh_access_token_is_reused_and_a_stale_one_refreshed_once() {
    let store = Store::with(&tokens(1).refresh_token);
    let server = Server::default();
    // tokens(1).access_expires_at = 1_900.
    let (s, _) = session(&store, &server, 1_000);
    s.adopt(&tokens(1)).unwrap();
    assert_eq!(s.access_token().unwrap(), tokens(1).access_token);
    assert_eq!(server.calls.load(Ordering::SeqCst), 0);
    // Inside the skew: refresh.
    let (late, _) = session(&store, &server, 1_900 - ACCESS_EXPIRY_SKEW_SECS);
    late.adopt(&tokens(1)).unwrap();
    assert_eq!(late.access_token().unwrap(), tokens(2).access_token);
    assert_eq!(store.get(), Some(tokens(2).refresh_token), "the new token is in the store");
    assert_eq!(late.access_token().unwrap(), tokens(2).access_token, "and cached");
    assert_eq!(server.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn concurrent_callers_share_one_refresh() {
    let store = Store::with(&tokens(1).refresh_token);
    let server = Server { pause_ms: 50, ..Server::default() };
    let (s, _) = session(&store, &server, 1_000);
    let s = Arc::new(s);
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let s = Arc::clone(&s);
            std::thread::spawn(move || s.access_token())
        })
        .collect();
    for h in handles {
        assert_eq!(h.join().unwrap().unwrap(), tokens(2).access_token);
    }
    assert_eq!(server.calls.load(Ordering::SeqCst), 1, "single-flight: one rotation, not eight");
}

#[test]
fn a_lost_response_is_retried_with_the_same_token_inside_the_window() {
    let store = Store::with(&tokens(1).refresh_token);
    let server = Server::scripted(vec![Err(TransportError::Retryable)]);
    let (s, slept) = session(&store, &server, 5_000);
    assert_eq!(s.access_token().unwrap(), tokens(2).access_token);
    let presented = server.presented.lock().unwrap().clone();
    assert_eq!(presented, vec![tokens(1).refresh_token, tokens(1).refresh_token], "same token twice");
    assert_eq!(*slept.lock().unwrap(), vec![Duration::from_secs(1)]);
    // Every attempt failing: give up, change nothing.
    let store = Store::with(&tokens(1).refresh_token);
    let server = Server::scripted(vec![Err(TransportError::Retryable); 5]);
    let (s, slept) = session(&store, &server, 5_000);
    assert_eq!(s.access_token(), Err(AccessError::Unavailable));
    assert_eq!(server.calls.load(Ordering::SeqCst), 3);
    assert_eq!(*slept.lock().unwrap(), vec![Duration::from_secs(1), Duration::from_secs(3)]);
    assert_eq!(store.get(), Some(tokens(1).refresh_token));
    // The whole retry plan fits the server's window.
    let worst: u64 = REFRESH_RETRY_DELAYS_SECS.iter().sum::<u64>()
        + REFRESH_TIMEOUT_SECS * (REFRESH_RETRY_DELAYS_SECS.len() as u64 + 1);
    assert!(worst < LOST_RESPONSE_WINDOW_SECS, "{worst}");
}

#[test]
fn no_retry_once_the_window_would_be_past() {
    let store = Store::with(&tokens(1).refresh_token);
    let server = Server::scripted(vec![Err(TransportError::Retryable); 3]);
    let ticks = Arc::new(AtomicU64::new(5_000));
    let clock = Arc::clone(&ticks);
    let s = Session::new(store.clone(), server.clone(), "dev-1").with_clock(
        // Each reading is 25 s later: a slow network.
        move || clock.fetch_add(25, Ordering::SeqCst),
        |_| {},
    );
    assert_eq!(s.access_token(), Err(AccessError::Unavailable));
    assert_eq!(server.calls.load(Ordering::SeqCst), 1, "a retry after the window would read as theft");
}

#[test]
fn a_restart_after_rotation_uses_the_newest_stored_token() {
    let store = Store::with(&tokens(1).refresh_token);
    let server = Server::default();
    let (first, _) = session(&store, &server, 5_000);
    first.access_token().unwrap();
    drop(first);
    // The app restarts: a new session object, the same Keychain item.
    let (second, _) = session(&store, &server, 5_000);
    assert_eq!(second.access_token().unwrap(), tokens(3).access_token);
    let presented = server.presented.lock().unwrap().clone();
    assert_eq!(presented, vec![tokens(1).refresh_token, tokens(2).refresh_token], "never the old one again");
}

#[test]
fn a_refused_session_is_cleared_and_stays_signed_out() {
    let store = Store::with(&tokens(1).refresh_token);
    let server = Server::scripted(vec![Err(TransportError::Unauthorized)]);
    let (s, _) = session(&store, &server, 5_000);
    assert_eq!(s.access_token(), Err(AccessError::Revoked));
    assert_eq!(store.get(), None);
    assert_eq!(s.access_token(), Err(AccessError::SignedOut));
    assert!(!s.signed_in().unwrap());
}

#[test]
fn rate_limits_and_an_unrecorded_request_change_nothing() {
    for err in [TransportError::Refused, TransportError::Unrecorded] {
        let store = Store::with(&tokens(1).refresh_token);
        let server = Server::scripted(vec![Err(err.clone())]);
        let (s, slept) = session(&store, &server, 5_000);
        assert_eq!(s.access_token(), Err(AccessError::Unavailable), "{err:?}");
        assert_eq!(store.get(), Some(tokens(1).refresh_token));
        assert!(slept.lock().unwrap().is_empty());
    }
}

#[test]
fn a_new_token_that_cannot_be_stored_is_never_used() {
    let store = Store::with(&tokens(1).refresh_token);
    store.fail_save.store(true, Ordering::SeqCst);
    let server = Server::default();
    let (s, _) = session(&store, &server, 5_000);
    assert_eq!(s.access_token(), Err(AccessError::StoreFailed));
    assert_eq!(store.get(), Some(tokens(1).refresh_token), "the old token stays for the retry");
    assert_eq!(s.cached_access(), None);
    // The Keychain is back: the retry presents the same old token (the server's lost-response
    // rule hands back the same pair).
    store.fail_save.store(false, Ordering::SeqCst);
    s.access_token().unwrap();
    let presented = server.presented.lock().unwrap().clone();
    assert_eq!(presented, vec![tokens(1).refresh_token, tokens(1).refresh_token]);
}

#[test]
fn adopt_stores_before_caching_and_forget_hands_back_the_token() {
    let store = Store::default();
    store.fail_save.store(true, Ordering::SeqCst);
    let server = Server::default();
    let (s, _) = session(&store, &server, 1_000);
    assert_eq!(s.adopt(&tokens(1)), Err(AccessError::StoreFailed));
    assert_eq!(s.cached_access(), None, "nothing in memory that a restart would not find");
    store.fail_save.store(false, Ordering::SeqCst);
    s.adopt(&tokens(1)).unwrap();
    assert_eq!(s.cached_access(), Some(tokens(1).access_token));
    assert_eq!(s.forget().unwrap(), Some(tokens(1).refresh_token));
    assert_eq!(store.get(), None);
    assert_eq!(s.cached_access(), None);
    assert_eq!(s.access_token(), Err(AccessError::SignedOut));
    assert_eq!(server.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn invalidating_the_access_token_makes_the_next_call_refresh() {
    let store = Store::with(&tokens(1).refresh_token);
    let server = Server::scripted(vec![Err(TransportError::Unauthorized)]);
    let (s, _) = session(&store, &server, 1_000);
    s.adopt(&tokens(1)).unwrap();
    assert!(s.access_token().is_ok());
    // A resource call answered 401: the device was removed elsewhere (§7.2).
    s.invalidate_access();
    assert_eq!(s.access_token(), Err(AccessError::Revoked));
    assert_eq!(store.get(), None);
}

// ── Refusals and sign-out ─────────────────────────────────────────────────────────────────────

#[test]
fn account_verification_answers_map_to_one_action_each() {
    use AccountVerifyAction as A;
    use AccountVerifyAnswer as Q;
    let nr = |r: &str| Q::NotEntitled(Some(r.to_string()));
    for (answer, key, want) in [
        (Q::Entitled, false, A::Save),
        (Q::Entitled, true, A::Save),
        (nr("seat_limit"), true, A::SeatLimit),
        (nr("no_license"), true, A::UseLicenceKey),
        (nr("no_license"), false, A::Lock),
        (Q::NotEntitled(None), true, A::Lock),
        (Q::Unauthorized, true, A::SignedOutRemotely),
        (Q::Transient, false, A::KeepCached),
    ] {
        assert_eq!(decide_account_verify(&answer, key), want, "{answer:?} key={key}");
    }
}

#[test]
fn trial_refusals_and_revocations() {
    assert_eq!(trial_refusal(Some("account_required")), Some(TrialRefusal::AccountRequired));
    assert_eq!(trial_refusal(Some("trial_used")), Some(TrialRefusal::TrialUsed));
    assert_eq!(trial_refusal(Some("device_removed")), Some(TrialRefusal::DeviceRemoved));
    assert_eq!(trial_refusal(Some("rate_limited")), None);
    assert_eq!(trial_refusal(None), None);
    assert!(keep_pending_revoke(RevokeOutcome::NotSent));
    assert!(!keep_pending_revoke(RevokeOutcome::Done));
    assert!(!keep_pending_revoke(RevokeOutcome::AlreadyGone));
}

#[test]
fn sign_out_removes_the_session_and_tokens_and_only_a_linked_key() {
    let linked = sign_out_steps(true);
    assert!(linked.delete_session && linked.delete_licence_token && linked.end_trial_relay);
    assert!(linked.delete_licence_key);
    assert!(!sign_out_steps(false).delete_licence_key, "a key bought without an account stays (D4)");
}

// ── account.json ──────────────────────────────────────────────────────────────────────────────

fn view() -> AccountView {
    AccountView {
        account: AccountSummary { email: Some("me@example.test".into()), email_confirmed: true, ..AccountSummary::default() },
        google_subject_hash: None,
        display_name: None,
        avatar_url: None,
        first_name: None,
        avatar_data_url: None,
        trial_ends_at: None,
        identities: vec![IdentityView { provider: "google".into(), email: Some("me@gmail.example".into()) }],
        passkeys: vec![PasskeyView { id: "pk".into(), created_at: None, last_used_at: None, synced: true }],
        devices: vec![DeviceView {
            device_id: "secret-device-id".into(),
            name: Some("MacBook Pro".into()),
            last_seen_at: Some(9),
            holds_seat: true,
            signed_in: true,
            this_device: true,
        }],
        fresh_until: None,
    }
}

#[test]
fn the_snapshot_round_trips_without_ids_or_tokens() {
    let snap = AccountSnapshot::from_view(&view(), 100, true);
    let text = serialize_account_snapshot(&snap);
    assert!(!text.contains("secret-device-id"), "{text}");
    assert!(!text.contains("pk\""), "no passkey ids");
    assert_eq!(parse_account_snapshot(&text), Some(snap.clone()));
    let status = account_status_json(Some(&snap));
    assert_eq!(status["signed_in"], true);
    assert_eq!(status["email"], "me@example.test");
    assert_eq!(status["sign_in_methods"], json!({ "passkeys": 1, "providers": ["google"] }));
    assert_eq!(status["devices"][0]["this_device"], true);
    assert!(status.get("licence_key_linked").is_none(), "internal flag stays internal");
    assert!(!status.to_string().contains("secret-device-id"));
}

#[test]
fn signed_out_and_unreadable_snapshots_read_as_signed_out() {
    let out = AccountSnapshot::signed_out(Some("device_removed"));
    let parsed = parse_account_snapshot(&serialize_account_snapshot(&out)).unwrap();
    assert!(!parsed.signed_in);
    assert_eq!(account_status_json(Some(&parsed)), json!({ "signed_in": false, "needs_sign_in": "device_removed" }));
    assert_eq!(AccountSnapshot::signed_out(Some("made_up")).needs_sign_in, None);
    assert_eq!(account_status_json(None), json!({ "signed_in": false, "needs_sign_in": null }));
    for text in ["", "{}", "garbage", r#"{"version":2,"signed_in":true}"#, r#"{"version":1}"#] {
        assert_eq!(parse_account_snapshot(text), None, "{text}");
    }
    // A signed-out file never claims a linked key.
    let sneaky = r#"{"version":1,"signed_in":false,"licence_key_linked":true}"#;
    assert!(!parse_account_snapshot(sneaky).unwrap().licence_key_linked);
}

#[test]
fn production_account_origin_is_separate_from_legacy_billing() {
    assert_eq!(SIGNIN_ORIGIN, "https://shogunai.si");
    let answer = json!({"ok": true, "url": "https://shogunai.si/app/reauth#c=challenge"});
    assert!(parse_reauth_url(&answer, SIGNIN_ORIGIN).is_ok());
    let legacy = json!({"ok": true, "url": "https://shogunaios.com/app/reauth#c=challenge"});
    assert!(parse_reauth_url(&legacy, SIGNIN_ORIGIN).is_err());
}

#[test]
fn local_profile_callback_is_optional_bounded_and_does_not_change_code_validation() {
    let state = "S".repeat(43);
    let envelope = "P".repeat(3300);
    let line = format!("GET /callback?code={}&state={state}&profile={envelope} HTTP/1.1", code());
    assert_eq!(callback_profile(&line, &state), Some(envelope));
    assert!(line.len() < 4096);
    for bad in ["short".to_string(), "P".repeat(3301), "percent%20encoded".into(), "P".repeat(40) + "&profile=duplicate"] {
        let line = format!("GET /callback?code={}&state={state}&profile={bad} HTTP/1.1", code());
        assert_eq!(callback_profile(&line, &state), None);
        assert!(matches!(classify_callback(&line, &state), CallbackVerdict::Code(_)));
    }
    assert_eq!(callback_profile(&line, "other-state"), None);
}

#[test]
fn local_profile_metadata_is_bounded_and_redacted() {
    let value = json!({ "account_id": "account-a", "subject_hash": "a".repeat(64),
        "display_name": "Person Name", "first_name": "Person", "avatar_url": "https://lh3.googleusercontent.com/a/photo" });
    let profile = parse_account_profile(&value).unwrap();
    assert_eq!(profile.account_id, "account-a");
    assert_eq!(profile.first_name.as_deref(), Some("Person"));
    assert!(!format!("{profile:?}").contains("Person"));
    for bad in [json!("x".repeat(65)), json!("bad\nname"), json!(7)] {
        let mut v = value.clone();
        v["first_name"] = bad;
        assert_eq!(parse_account_profile(&v).unwrap().first_name, None, "a bad name drops, the profile stays");
    }
    let mut old = value.clone();
    old.as_object_mut().unwrap().remove("first_name");
    assert_eq!(parse_account_profile(&old).unwrap().first_name, None, "profiles saved before the field still read");
    let mut invalid = value;
    invalid["subject_hash"] = json!("not-a-digest");
    assert!(parse_account_profile(&invalid).is_none());
}
