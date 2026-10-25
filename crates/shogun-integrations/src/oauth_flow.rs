//! The effectful OAuth loopback flow (feature `live`): generate entropy, open the system browser,
//! catch the redirect on a loopback socket, and exchange the code for tokens.
//!
//! Only the orchestration lives here; every decision (URL, PKCE, forms, parsing) is the pure
//! [`crate::oauth`], and the failure taxonomy is the pure [`crate::connect::ConnectError`] so the
//! desktop can map each outcome onto the FR-INT-06/07 state machine. Cannot be exercised end to
//! end on Linux CI — it opens a browser and talks to Google — but the redirect listener below IS
//! Linux-tested (a socket needs no browser). Confirm end to end on the macOS build with a real
//! Google OAuth "Desktop app" client (Developer Preview).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use crate::connect::{redirect_error_is_denial, redirect_error_is_user_denial, ConnectError};
use crate::oauth::{self, AuthConfig, Pkce, TokenExchange, TokenSet};

/// Fresh 32-byte CSPRNG entropy for a PKCE verifier.
fn entropy() -> Result<[u8; 32], String> {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).map_err(|e| format!("csprng failed: {e}"))?;
    Ok(buf)
}

/// A random-ish anti-CSRF state derived from entropy (base64url — no network, no extra dep).
fn state_from(entropy: &[u8]) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    URL_SAFE_NO_PAD.encode(entropy)
}

/// Open a URL in the default browser — the same opener the connector flows use, for other
/// browser round trips (the account sign-in, #148 P2). Fails fast when no browser takes it.
pub fn open_system_browser(url: &str) -> Result<(), String> {
    open_browser(url)
}

/// Open a URL in the system browser. The Mac app's build (`desktop`) runs `open`
/// ([`crate::browser`]); the server build has no browser to drive and reports that, so the caller
/// presents the URL itself (the hosted OAuth flow returns it on every platform, E5).
#[cfg(feature = "desktop")]
fn open_browser(url: &str) -> Result<(), String> {
    crate::browser::open(url)
}

#[cfg(not(feature = "desktop"))]
fn open_browser(_url: &str) -> Result<(), String> {
    Err("browser-open is only wired for the desktop build; present the authorization URL instead".to_string())
}

/// Run the full loopback Authorization-Code + PKCE flow and return a token set.
///
/// `listeners` must be bound to the loopback port named in `cfg.redirect_uri` (the caller binds them
/// so it can put the real, OS-assigned port into the redirect URI first). Pass both 127.0.0.1 and ::1
/// when the redirect host is `localhost`, since the browser may resolve either. `now_ms` stamps token expiry.
/// `timeout` bounds the wait for the browser redirect — an abandoned consent page ends in
/// [`ConnectError::Timeout`], never a command that hangs forever.
pub fn run_loopback_flow(
    cfg: &AuthConfig,
    scopes: &[&str],
    listeners: &[&TcpListener],
    now_ms: i64,
    exchange: &dyn TokenExchange,
    timeout: Duration,
) -> Result<TokenSet, ConnectError> {
    run_loopback_flow_cancellable(cfg, scopes, listeners, now_ms, exchange, timeout, &|| false)
}

/// Run the loopback flow with a caller-owned cancellation signal. The signal is checked before
/// opening the browser, while waiting for the redirect, and before exchanging the code.
pub fn run_loopback_flow_cancellable(
    cfg: &AuthConfig,
    scopes: &[&str],
    listeners: &[&TcpListener],
    now_ms: i64,
    exchange: &dyn TokenExchange,
    timeout: Duration,
    cancelled: &dyn Fn() -> bool,
) -> Result<TokenSet, ConnectError> {
    if cancelled() {
        return Err(ConnectError::Cancelled);
    }
    // Two INDEPENDENT draws: the state travels in the browser URL (history, referers, the
    // plaintext loopback request line), so it must never be derivable from the PKCE verifier —
    // one seed for both made state == verifier and put the verifier in the URL.
    let pkce = Pkce::from_entropy(&entropy().map_err(ConnectError::Internal)?);
    let state = state_from(&entropy().map_err(ConnectError::Internal)?);
    let url = oauth::authorize_url(cfg, scopes, &pkce, &state).map_err(ConnectError::Internal)?;
    if cancelled() {
        return Err(ConnectError::Cancelled);
    }
    open_browser(&url).map_err(ConnectError::BrowserOpen)?;

    let deadline = Instant::now() + timeout;
    let code = accept_redirect_cancellable(listeners, &state, deadline, cancelled)?;
    if cancelled() {
        return Err(ConnectError::Cancelled);
    }
    let form = oauth::token_exchange_form(cfg, &code, &pkce);
    let body = exchange
        .post_form(&cfg.token_endpoint, &form)
        .map_err(ConnectError::Exchange)?;
    if cfg.token_endpoint == oauth::GOOGLE_TOKEN_ENDPOINT {
        oauth::verify_granted_scopes(&body, scopes).map_err(ConnectError::Exchange)?;
    }
    oauth::parse_token_response_for(cfg, &body, now_ms, None).map_err(ConnectError::Exchange)
}

/// The small "you can close this" page every loopback request gets, so no browser tab ever hangs.
const REPLY_PAGE: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<html><body>Sign-in received. Return to ShogunAI to check connection status.</body></html>";

/// What one loopback request means for the flow waiting on `expected_state`.
#[derive(Debug, PartialEq, Eq)]
enum RedirectOutcome {
    /// The provider's redirect for this flow: the authorization code.
    Code(String),
    /// The provider's error redirect for this flow (or one that names no state).
    Failed(ConnectError),
    /// Not this flow's redirect: answer it and keep waiting.
    Stray,
}

/// The anti-CSRF state carried by a loopback request, if any.
fn request_state(request_line: &str) -> Option<String> {
    let target = request_line.split_whitespace().nth(1)?;
    let parsed = url::Url::parse("http://127.0.0.1").and_then(|base| base.join(target)).ok()?;
    let state = parsed.query_pairs().find(|(key, _)| key == "state")?;
    Some(state.1.into_owned())
}

/// Constant-time comparison of a redirect's state with this flow's, so timing does not reveal how
/// much of a guessed state matched. An empty expected state never matches.
fn state_matches(got: &str, expected: &str) -> bool {
    let (a, b) = (got.as_bytes(), expected.as_bytes());
    if b.is_empty() || a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Classify one loopback request. Only this flow's redirect resolves the wait. A request with
/// `code`+`state` whose state is not this flow's (a stale browser retry of an earlier flow's
/// redirect, or any local process) is stray rather than a reason to abort the real sign-in: its
/// code is never exchanged, and the matching redirect can still arrive before the deadline. An
/// error redirect ends the flow only when it carries this flow's state (audit 2026-10-20 F6):
/// any local process can reach the loopback port, so a bare `?error=access_denied` with no state,
/// or another flow's state, is stray and cannot cancel the user's sign-in. A provider that does
/// not echo the state on an error leaves the flow to its deadline ([`ConnectError::Timeout`]).
/// With the state: `access_denied` is the user declining ([`ConnectError::Denied`]); any other
/// provider error is the provider refusing the request. States are compared in constant time.
fn classify_redirect(request_line: &str, expected_state: &str) -> RedirectOutcome {
    match oauth::parse_redirect(request_line) {
        Ok((code, got)) if state_matches(&got, expected_state) => RedirectOutcome::Code(code),
        Ok(_) => RedirectOutcome::Stray,
        Err(e) if redirect_error_is_denial(&e) => {
            if !request_state(request_line).is_some_and(|got| state_matches(&got, expected_state)) {
                RedirectOutcome::Stray
            } else if redirect_error_is_user_denial(&e) {
                RedirectOutcome::Failed(ConnectError::Denied)
            } else {
                RedirectOutcome::Failed(ConnectError::BadRedirect(
                    "provider refused the sign-in request".to_string(),
                ))
            }
        }
        // A favicon probe, a health check, a malformed request.
        Err(_) => RedirectOutcome::Stray,
    }
}

/// Accept loopback connections until this flow's OAuth redirect arrives or `deadline` passes.
///
/// Every request is answered so no browser tab hangs; [`classify_redirect`] decides which one
/// resolves the wait. No redirect by `deadline` → [`ConnectError::Timeout`].
#[cfg(test)]
fn accept_redirect(
    listeners: &[&TcpListener],
    expected_state: &str,
    deadline: Instant,
) -> Result<String, ConnectError> {
    accept_redirect_cancellable(listeners, expected_state, deadline, &|| false)
}

fn accept_redirect_cancellable(
    listeners: &[&TcpListener],
    expected_state: &str,
    deadline: Instant,
    cancelled: &dyn Fn() -> bool,
) -> Result<String, ConnectError> {
    accept_loopback(listeners, deadline, cancelled, REPLY_PAGE, REPLY_PAGE, &|first_line| {
        match classify_redirect(first_line, expected_state) {
            RedirectOutcome::Code(code) => LoopbackVerdict::Done(code),
            RedirectOutcome::Failed(error) => LoopbackVerdict::Fail(error),
            RedirectOutcome::Stray => LoopbackVerdict::Stray,
        }
    })
}

/// What one loopback request means to a flow using [`accept_loopback`].
#[derive(Debug, PartialEq, Eq)]
pub enum LoopbackVerdict<T> {
    /// The redirect this flow was waiting for.
    Done(T),
    /// The redirect ends the flow with this error.
    Fail(ConnectError),
    /// Not this flow's: answered, and the wait goes on.
    Stray,
}

/// Accept loopback connections until `classify` recognises this flow's redirect (from the HTTP
/// request line) or `deadline` passes. Every request is answered so no browser tab hangs: the
/// redirect that ends the wait with `done_reply`, every other one with `reply` (each a complete
/// HTTP response); stray requests never end the wait. Shared by the connector OAuth flows and the
/// account sign-in (#148 P2), whose redirect rules differ.
pub fn accept_loopback<T>(
    listeners: &[&TcpListener],
    deadline: Instant,
    cancelled: &dyn Fn() -> bool,
    reply: &str,
    done_reply: &str,
    classify: &dyn Fn(&str) -> LoopbackVerdict<T>,
) -> Result<T, ConnectError> {
    for listener in listeners {
        listener.set_nonblocking(true).map_err(|e| {
            ConnectError::ListenerBind(format!("nonblocking accept unavailable: {e}"))
        })?;
    }
    loop {
        if cancelled() {
            return Err(ConnectError::Cancelled);
        }
        let mut accepted = false;
        for (i, listener) in listeners.iter().enumerate() {
        match listener.accept() {
            Ok((mut stream, _)) => {
                accepted = true;
                // Read the request line with a short blocking timeout so one stalled client
                // cannot eat the whole flow window.
                let _ = stream.set_nonblocking(false);
                // Account sign-in may carry an opaque local-profile envelope. Read complete
                // lines across packets with a fixed deadline; a slow peer cannot reset it.
                let connection_deadline = (Instant::now() + Duration::from_secs(5)).min(deadline);
                let mut bytes = Vec::new();
                let first_line = loop {
                    if cancelled() || bytes.len() >= 4096 {
                        break None;
                    }
                    let Some(remaining) = connection_deadline.checked_duration_since(Instant::now()) else {
                        break None;
                    };
                    if remaining.is_zero() || stream.set_read_timeout(Some(remaining)).is_err() {
                        break None;
                    }
                    let mut chunk = [0_u8; 512];
                    let limit = chunk.len().min(4096 - bytes.len());
                    let n = match stream.read(&mut chunk[..limit]) {
                        Ok(0) | Err(_) => break None,
                        Ok(n) => n,
                    };
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(end) = bytes.iter().position(|b| *b == b'\n') {
                        break std::str::from_utf8(&bytes[..end]).ok().map(|line| line.trim_end_matches('\r').to_string());
                    }
                };
                let outcome = first_line.as_deref().map_or(LoopbackVerdict::Stray, classify);
                // Always answer the browser so the tab doesn't hang, regardless of parse result.
                let answer = if matches!(outcome, LoopbackVerdict::Done(_)) { done_reply } else { reply };
                let _ = stream.write_all(answer.as_bytes());
                if cancelled() {
                    return Err(ConnectError::Cancelled);
                }
                match outcome {
                    LoopbackVerdict::Done(value) => return Ok(value),
                    LoopbackVerdict::Fail(error) => return Err(error),
                    // A stray request — answered above; keep waiting for the real redirect.
                    LoopbackVerdict::Stray => {}
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) if matches!(e.kind(), std::io::ErrorKind::ConnectionAborted | std::io::ErrorKind::Interrupted) => {}
            Err(e) if i == 0 => return Err(ConnectError::ListenerBind(format!("loopback accept failed: {e}"))),
            Err(_) => {}
        }
        }
        if cancelled() {
            return Err(ConnectError::Cancelled);
        }
        if !accepted {
            std::thread::sleep(Duration::from_millis(100));
        }
        if Instant::now() >= deadline {
            return Err(ConnectError::Timeout);
        }
    }
}

// The concrete `reqwest` token exchange (HttpTokenExchange) lives in shogun-core — the single
// allowlisted HTTP-egress crate (FR-TR-03). This module stays HTTP-client-free: it only orchestrates
// the loopback flow over the injected `TokenExchange` seam.

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier};

    #[test]
    fn state_is_derived_deterministically_from_entropy() {
        assert_eq!(state_from(&[9u8; 32]), state_from(&[9u8; 32]));
        assert_ne!(state_from(&[1u8; 32]), state_from(&[2u8; 32]));
    }

    struct FakeExchange(String);
    impl TokenExchange for FakeExchange {
        fn post_form(&self, _e: &str, _f: &[(String, String)]) -> Result<String, String> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn fake_exchange_parses_into_token_set() {
        // Exercises the pure exchange→parse seam without touching the network or a browser.
        let ex = FakeExchange(r#"{"access_token":"at","expires_in":3600,"refresh_token":"rt"}"#.to_string());
        let cfg = AuthConfig::google("cid", None, "http://127.0.0.1:0/callback");
        let pkce = Pkce::from_entropy(&[3u8; 32]);
        let form = oauth::token_exchange_form(&cfg, "code", &pkce);
        let body = ex.post_form(&cfg.token_endpoint, &form).unwrap();
        let ts = oauth::parse_token_response(&body, 0, None).unwrap();
        assert_eq!(ts.access_token, "at");
        assert_eq!(ts.refresh_token.as_deref(), Some("rt"));
    }

    fn bound_listener() -> (TcpListener, u16) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, port)
    }

    fn send_request(port: u16, request_line: &str) {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(format!("{request_line}\r\nHost: x\r\n\r\n").as_bytes()).unwrap();
        // Read the reply so the listener's write succeeds before we drop the socket.
        let mut buf = [0u8; 256];
        let _ = s.read(&mut buf);
    }

    #[test]
    fn accept_redirect_times_out_without_a_redirect() {
        let (listener, _) = bound_listener();
        let err = accept_redirect(&[&listener], "xyz", Instant::now() + Duration::from_millis(150))
            .unwrap_err();
        assert_eq!(err, ConnectError::Timeout);
    }

    #[test]
    fn cancelling_idle_loopback_releases_listener_promptly() {
        let (listener, port) = bound_listener();
        let cancelled = Arc::new(AtomicBool::new(false));
        let ready = Arc::new(Barrier::new(2));
        let worker_cancelled = Arc::clone(&cancelled);
        let worker_ready = Arc::clone(&ready);
        let worker = std::thread::spawn(move || {
            worker_ready.wait();
            accept_redirect_cancellable(
                &[&listener],
                "xyz",
                Instant::now() + Duration::from_secs(5),
                &|| worker_cancelled.load(Ordering::Acquire),
            )
        });
        ready.wait();
        std::thread::sleep(Duration::from_millis(150));
        assert!(!worker.is_finished(), "idle listener must still be waiting");
        let start = Instant::now();
        cancelled.store(true, Ordering::Release);
        assert_eq!(worker.join().unwrap(), Err(ConnectError::Cancelled));
        assert!(start.elapsed() < Duration::from_secs(2));
        // The worker owns the socket; joining it must release the fixed callback port.
        TcpListener::bind(("127.0.0.1", port)).unwrap();
    }

    #[test]
    fn accept_redirect_skips_stray_requests_then_returns_the_code() {
        let (listener, port) = bound_listener();
        let sender = std::thread::spawn(move || {
            // A browser favicon probe first, then the real redirect.
            send_request(port, "GET /favicon.ico HTTP/1.1");
            send_request(port, "GET /callback?code=abc123&state=xyz HTTP/1.1");
        });
        let code =
            accept_redirect(&[&listener], "xyz", Instant::now() + Duration::from_secs(5)).unwrap();
        sender.join().unwrap();
        assert_eq!(code, "abc123");
    }

    #[test]
    fn accept_redirect_ignores_another_flows_redirect_and_keeps_waiting() {
        let (listener, port) = bound_listener();
        let sender = std::thread::spawn(move || {
            // A stale retry of an earlier flow's redirect (or any local process), then the
            // provider's redirect for this flow.
            send_request(port, "GET /callback?code=stale&state=earlier HTTP/1.1");
            send_request(port, "GET /callback?error=access_denied&state=earlier HTTP/1.1");
            send_request(port, "GET /callback?code=real&state=xyz HTTP/1.1");
        });
        let code =
            accept_redirect(&[&listener], "xyz", Instant::now() + Duration::from_secs(5)).unwrap();
        sender.join().unwrap();
        assert_eq!(code, "real");
    }

    #[test]
    fn mismatched_state_alone_never_ends_the_flow() {
        let (listener, port) = bound_listener();
        let sender = std::thread::spawn(move || {
            send_request(port, "GET /callback?code=stale&state=earlier HTTP/1.1");
        });
        let err = accept_redirect(&[&listener], "xyz", Instant::now() + Duration::from_millis(600))
            .unwrap_err();
        sender.join().unwrap();
        assert_eq!(err, ConnectError::Timeout);
    }

    #[test]
    fn accept_redirect_takes_the_redirect_from_any_listener() {
        let (idle, _) = bound_listener();
        let (listener, port) = bound_listener();
        let sender = std::thread::spawn(move || {
            send_request(port, "GET /callback?code=second&state=s HTTP/1.1");
        });
        let code = accept_redirect(&[&idle, &listener], "s", Instant::now() + Duration::from_secs(5))
            .unwrap();
        sender.join().unwrap();
        assert_eq!(code, "second");
    }

    #[test]
    fn accept_redirect_surfaces_user_denial() {
        let (listener, port) = bound_listener();
        let sender = std::thread::spawn(move || {
            send_request(port, "GET /callback?error=access_denied&state=xyz HTTP/1.1");
        });
        let err = accept_redirect(&[&listener], "xyz", Instant::now() + Duration::from_secs(5))
            .unwrap_err();
        sender.join().unwrap();
        assert_eq!(err, ConnectError::Denied);
    }

    /// Audit 2026-10-20 F6: any local process can reach the loopback port. An error redirect
    /// without this flow's state must not cancel the sign-in; the real code still arrives.
    #[test]
    fn a_stateless_error_redirect_cannot_cancel_the_flow() {
        let (listener, port) = bound_listener();
        let sender = std::thread::spawn(move || {
            send_request(port, "GET /callback?error=access_denied HTTP/1.1");
            send_request(port, "GET /callback?error=invalid_client HTTP/1.1");
            send_request(port, "GET /callback?error=access_denied&state= HTTP/1.1");
            send_request(port, "GET /callback?error=access_denied&state=xy HTTP/1.1");
            send_request(port, "GET /callback?code=real&state=xyz HTTP/1.1");
        });
        let code =
            accept_redirect(&[&listener], "xyz", Instant::now() + Duration::from_secs(5)).unwrap();
        sender.join().unwrap();
        assert_eq!(code, "real");
    }

    #[test]
    fn a_stateless_error_redirect_alone_ends_in_timeout_not_denial() {
        let (listener, port) = bound_listener();
        let sender = std::thread::spawn(move || {
            send_request(port, "GET /callback?error=access_denied HTTP/1.1");
        });
        let err = accept_redirect(&[&listener], "xyz", Instant::now() + Duration::from_millis(600))
            .unwrap_err();
        sender.join().unwrap();
        assert_eq!(err, ConnectError::Timeout);
    }

    #[test]
    fn state_comparison_is_exact() {
        assert!(state_matches("xyz", "xyz"));
        assert!(!state_matches("xy", "xyz"));
        assert!(!state_matches("xyzz", "xyz"));
        assert!(!state_matches("", ""));
        assert!(!state_matches("XYZ", "xyz"));
    }

    #[test]
    fn provider_errors_other_than_declining_are_not_user_denial() {
        assert_eq!(
            classify_redirect("GET /callback?error=access_denied&state=xyz HTTP/1.1", "xyz"),
            RedirectOutcome::Failed(ConnectError::Denied)
        );
        assert!(matches!(
            classify_redirect("GET /callback?error=invalid_client&state=xyz HTTP/1.1", "xyz"),
            RedirectOutcome::Failed(ConnectError::BadRedirect(_))
        ));
        assert!(matches!(
            classify_redirect("GET /callback?error=unauthorized_client&state=xyz HTTP/1.1", "xyz"),
            RedirectOutcome::Failed(ConnectError::BadRedirect(_))
        ));
        // No state: stray (F6), whatever the error.
        assert_eq!(
            classify_redirect("GET /callback?error=unauthorized_client HTTP/1.1", "xyz"),
            RedirectOutcome::Stray
        );
        assert_eq!(
            classify_redirect("GET /callback?error=access_denied HTTP/1.1", "xyz"),
            RedirectOutcome::Stray
        );
        assert_eq!(
            classify_redirect("GET /callback?error=invalid_client&state=other HTTP/1.1", "xyz"),
            RedirectOutcome::Stray
        );
        assert_eq!(
            classify_redirect("GET /callback?code=c&state=other HTTP/1.1", "xyz"),
            RedirectOutcome::Stray
        );
    }

    #[cfg(not(feature = "desktop"))]
    #[test]
    fn the_server_build_reports_that_it_opens_no_browser() {
        assert!(open_system_browser("https://example.test/auth").is_err());
    }
    #[test]
    fn loopback_reads_a_fragmented_bounded_profile_request_line() {
        let (listener, port) = bound_listener();
        let request_line = format!("GET /callback?code=code&state=state&profile={} HTTP/1.1", "p".repeat(3300));
        let expected = request_line.clone();
        let worker = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            stream.write_all(&request_line.as_bytes()[..1000]).unwrap();
            std::thread::sleep(Duration::from_millis(30));
            stream.write_all(&request_line.as_bytes()[1000..]).unwrap();
            stream.write_all(b"\r\nHost: localhost\r\n\r\n").unwrap();
            let mut response = [0; 256];
            let _ = stream.read(&mut response);
        });
        let line = accept_loopback(&[&listener], Instant::now() + Duration::from_secs(3), &|| false,
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n", "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
            &|line| LoopbackVerdict::Done(line.to_string())).unwrap();
        worker.join().unwrap();
        assert_eq!(line, expected);
    }

    #[test]
    fn only_the_redirect_that_ends_the_wait_gets_the_done_reply() {
        let (listener, port) = bound_listener();
        let answers = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::clone(&answers);
        let sender = std::thread::spawn(move || {
            for line in ["GET /favicon.ico HTTP/1.1", "GET /callback?code=stale&state=other HTTP/1.1", "GET /callback?code=real&state=xyz HTTP/1.1"] {
                let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
                s.write_all(format!("{line}\r\nHost: x\r\n\r\n").as_bytes()).unwrap();
                let mut answer = String::new();
                let _ = s.read_to_string(&mut answer);
                seen.lock().unwrap().push(answer);
            }
        });
        let code = accept_loopback(&[&listener], Instant::now() + Duration::from_secs(5), &|| false,
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nstray", "HTTP/1.1 303 See Other\r\nLocation: /done\r\nContent-Length: 0\r\n\r\n",
            &|line| if line.contains("state=xyz") { LoopbackVerdict::Done(line.to_string()) } else { LoopbackVerdict::Stray }).unwrap();
        sender.join().unwrap();
        assert!(code.contains("code=real"));
        let answers = answers.lock().unwrap();
        assert_eq!(answers.len(), 3);
        assert!(answers[0].ends_with("stray") && answers[1].ends_with("stray"));
        assert!(answers[2].starts_with("HTTP/1.1 303 See Other\r\nLocation: /done\r\n"));
    }

    #[test]
    fn incomplete_slow_loopback_request_cannot_extend_flow_deadline() {
        let (listener, port) = bound_listener();
        let peer = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            for _ in 0..50 {
                if stream.write_all(b"x").is_err() { break; }
                std::thread::sleep(Duration::from_millis(30));
            }
        });
        let started = Instant::now();
        let result = accept_loopback::<()>(&[&listener], started + Duration::from_millis(180), &|| false,
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n", "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
            &|_| panic!("incomplete line must not be classified"));
        assert!(matches!(result, Err(ConnectError::Timeout)));
        assert!(started.elapsed() < Duration::from_secs(2));
        peer.join().unwrap();
    }

}
