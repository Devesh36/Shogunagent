use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::Mutex;

use serde_json::json;

use super::*;
use crate::llm::traceability::{RecordingSink, RefusingSink};

/// A one-shot HTTP server: answers each request in turn and hands back what it received.
struct Fixture {
    origin: String,
    server: std::thread::JoinHandle<Vec<String>>,
}

fn http(status: u16, body: &Value) -> String {
    let text = body.to_string();
    format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len())
}

fn fixture(responses: Vec<String>) -> Fixture {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let origin = format!("http://{}", listener.local_addr().expect("addr"));
    let server = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for response in responses {
            let (mut stream, _): (TcpStream, _) = listener.accept().expect("accept");
            stream.set_read_timeout(Some(Duration::from_secs(2))).expect("timeout");
            let mut request = Vec::new();
            let mut buf = [0; 4096];
            loop {
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
                if let Some(end) = request.windows(4).position(|p| p == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                    let length = headers
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok()))
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            requests.push(String::from_utf8_lossy(&request).into_owned());
            let _ = stream.write_all(response.as_bytes());
        }
        requests
    });
    Fixture { origin, server }
}

fn client(origin: &str) -> (AccountClient, Arc<RecordingSink>) {
    let sink = Arc::new(RecordingSink::new());
    (AccountClient::new(origin, sink.clone()).expect("client"), sink)
}

fn tokens(n: u32) -> Value {
    json!({
        "ok": true,
        "refresh_token": format!("sr1.11111111-1111-1111-1111-111111111111.r{n}"),
        "access_token": format!("sa1.11111111-1111-1111-1111-111111111111.a{n}"),
        "access_expires_at": 1_000 + n,
        "refresh_expires_at": 9_000,
    })
}

fn request_for(port: u16) -> SigninRequest {
    SigninRequest::new(&[1; 32], &[2; 32], port).expect("request")
}

#[test]
fn the_exchange_sends_the_verifier_and_device_and_reads_the_session() {
    let mut answer = tokens(1);
    answer["account"] = json!({ "id": "a", "email": null, "email_confirmed": false });
    let f = fixture(vec![http(200, &answer)]);
    let (c, sink) = client(&f.origin);
    let req = request_for(50_000);
    let (account, t) = c.exchange(&req, &"C".repeat(43), "device-0001", Some("Studio")).expect("exchange");
    assert!(account.email.is_none());
    assert!(t.refresh_token.ends_with(".r1"));
    let sent = f.server.join().expect("server");
    assert!(sent[0].starts_with("POST /api/account/desktop/exchange "), "{}", sent[0]);
    let body: Value = serde_json::from_str(sent[0].split("\r\n\r\n").nth(1).expect("body")).expect("json");
    assert_eq!(body["code_verifier"], req.code_verifier);
    assert_eq!(body["device"], json!({ "id": "device-0001", "name": "Studio" }));
    assert_eq!(body["redirect_uri"], "http://127.0.0.1:50000/callback");
    // One content-free billing row, written before the request.
    let rows = sink.records();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].route, Route::Billing);
    assert_eq!(rows[0].purpose, "account_signin");
    assert_eq!(rows[0].chunk_bytes, 0);
}

#[test]
fn without_a_ledger_row_nothing_is_sent() {
    // A listener nobody answers: a request reaching it would hang until the timeout.
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    listener.set_nonblocking(true).expect("nonblocking");
    let origin = format!("http://{}", listener.local_addr().expect("addr"));
    let sink = Arc::new(RefusingSink::new());
    let c = AccountClient::new(&origin, sink.clone()).expect("client");
    assert_eq!(c.account("sa1.x").unwrap_err(), AccountCallError::Unrecorded);
    assert_eq!(c.refresh("sr1.x", "device-0001").unwrap_err(), TransportError::Unrecorded);
    assert_eq!(c.logout(None, Some("sr1.x")), RevokeOutcome::NotSent);
    assert_eq!(c.cancel_and_remove_card("sa1.x"), Err(AccountCallError::Unrecorded));
    assert_eq!(sink.refused(), 4);
    assert!(listener.accept().is_err(), "no connection was made");
}

#[test]
fn cancellation_is_bodyless_account_authenticated_and_preserves_partial_card_removal() {
    let f = fixture(vec![http(200, &json!({ "ok": true, "cancel_at_period_end": true,
        "card_removed": false, "current_period_end": 2_000_000_000u64, "reason": "other_subscription_active" }))]);
    let (c, sink) = client(&f.origin);
    let answer = c.cancel_and_remove_card("sa1.t").expect("cancellation result");
    assert!(answer.cancel_at_period_end);
    assert!(!answer.card_removed);
    assert_eq!(answer.reason.as_deref(), Some("other_subscription_active"));
    let sent = f.server.join().expect("server");
    assert!(sent[0].starts_with("POST /api/account/billing/cancel "));
    assert!(sent[0].to_ascii_lowercase().contains("authorization: bearer sa1.t"));
    assert!(sent[0].split("\r\n\r\n").nth(1).expect("body separator").is_empty());
    assert_eq!(sink.records()[0].purpose, "account_billing_cancel");
    assert_eq!(sink.records()[0].chunk_bytes, 0);
}

#[test]
fn cancellation_never_invents_success_or_exposes_unknown_server_reasons() {
    assert_eq!(parse_billing_cancellation(&json!({ "ok": true })), Err(AccountCallError::BadResponse));
    assert_eq!(parse_billing_cancellation(&json!({ "ok": false, "cancel_at_period_end": true, "card_removed": true })), Err(AccountCallError::BadResponse));
    let answer = parse_billing_cancellation(&json!({ "ok": true, "cancel_at_period_end": true,
        "card_removed": false, "current_period_end": null, "reason": "private server detail" })).expect("partial result");
    assert_eq!(answer.reason, None);
    assert_eq!(answer.current_period_end, None);
}

#[test]
fn statuses_map_to_what_the_app_acts_on() {
    let f = fixture(vec![
        http(401, &json!({ "ok": false, "error": "unauthorized" })),
        http(403, &json!({ "ok": false, "error": "forbidden", "reason": "reauth_required" })),
        http(403, &json!({ "ok": false, "error": "forbidden", "reason": "csrf" })),
        http(404, &json!({ "ok": false, "error": "not_found" })),
        http(429, &json!({ "ok": false, "error": "rate_limited" })),
        http(500, &json!({ "ok": false, "error": "server_error" })),
        http(200, &json!({ "ok": true, "removed": true })),
    ]);
    let (c, _) = client(&f.origin);
    assert_eq!(c.delete_account("sa1.t").unwrap_err(), AccountCallError::Unauthorized);
    assert_eq!(c.delete_account("sa1.t").unwrap_err(), AccountCallError::Refused(Some("reauth_required".into())));
    assert_eq!(c.delete_account("sa1.t").unwrap_err(), AccountCallError::Refused(None), "unknown reasons stay out");
    assert_eq!(c.remove_device("sa1.t", "device-0001").unwrap_err(), AccountCallError::NotFound);
    assert_eq!(c.remove_device("sa1.t", "device-0001").unwrap_err(), AccountCallError::RateLimited);
    assert_eq!(c.remove_device("sa1.t", "device-0001").unwrap_err(), AccountCallError::ServerError);
    c.remove_device("sa1.t", "device-0001").expect("removed");
    let sent = f.server.join().expect("server");
    assert!(sent[6].starts_with("DELETE /api/account/devices/device-0001 "));
    assert!(sent[6].to_ascii_lowercase().contains("authorization: bearer sa1.t"));
    // Ids the server could never hold are not sent at all.
    let (c, sink) = client("http://127.0.0.1:9");
    assert_eq!(c.remove_device("sa1.t", "../../x").unwrap_err(), AccountCallError::NotFound);
    assert_eq!(c.remove_passkey("sa1.t", "not-a-uuid").unwrap_err(), AccountCallError::NotFound);
    assert!(sink.records().is_empty());
}

#[test]
fn refresh_maps_a_lost_answer_to_a_retry_and_a_401_to_revoked() {
    let f = fixture(vec![
        http(200, &tokens(2)),
        http(401, &json!({ "ok": false, "error": "unauthorized" })),
        http(500, &json!({ "ok": false })),
        http(200, &json!({ "ok": true })),
        http(429, &json!({ "ok": false })),
    ]);
    let (c, _) = client(&f.origin);
    let t = c.refresh("sr1.11111111-1111-1111-1111-111111111111.r1", "device-0001").expect("rotated");
    assert!(t.access_token.ends_with(".a2"));
    assert_eq!(c.refresh("sr1.x", "device-0001").unwrap_err(), TransportError::Unauthorized);
    assert_eq!(c.refresh("sr1.x", "device-0001").unwrap_err(), TransportError::Retryable);
    assert_eq!(c.refresh("sr1.x", "device-0001").unwrap_err(), TransportError::Retryable, "an unreadable 2xx may have rotated");
    assert_eq!(c.refresh("sr1.x", "device-0001").unwrap_err(), TransportError::Refused);
    let sent = f.server.join().expect("server");
    let body: Value = serde_json::from_str(sent[0].split("\r\n\r\n").nth(1).expect("body")).expect("json");
    assert_eq!(body, json!({ "refresh_token": "sr1.11111111-1111-1111-1111-111111111111.r1", "device_id": "device-0001" }));
}

#[test]
fn logout_by_either_credential_and_its_outcomes() {
    let f = fixture(vec![
        http(200, &json!({ "ok": true, "signed_out": true })),
        http(401, &json!({ "ok": false, "error": "unauthorized" })),
        http(503, &json!({ "ok": false })),
    ]);
    let (c, _) = client(&f.origin);
    assert_eq!(c.logout(Some("sa1.t"), Some("sr1.t")), RevokeOutcome::Done);
    assert_eq!(c.logout(None, Some("sr1.t")), RevokeOutcome::AlreadyGone);
    assert_eq!(c.logout(None, Some("sr1.t")), RevokeOutcome::NotSent);
    let sent = f.server.join().expect("server");
    assert!(sent[0].to_ascii_lowercase().contains("authorization: bearer sa1.t"));
    assert!(sent[0].contains("\"refresh_token\":\"sr1.t\""), "both credentials go");
    assert!(sent[1].contains("\"refresh_token\":\"sr1.t\""));
    assert!(!sent[1].to_ascii_lowercase().contains("authorization:"));
    // Nothing to revoke: no request at all.
    let (c, sink) = client("http://127.0.0.1:9");
    assert_eq!(c.logout(None, None), RevokeOutcome::AlreadyGone);
    assert!(sink.records().is_empty());
}

#[test]
fn reauth_links_are_accepted_only_on_the_same_origin() {
    // The same-origin rule itself is the pure parser's (shogun_license::account tests); here, the
    // routing and the refusal.
    let g = fixture(vec![
        http(200, &json!({ "ok": true, "url": "https://evil.example/app/reauth", "expires_at": 1 })),
        http(400, &json!({ "ok": false, "error": "bad_request", "reason": "no_passkey" })),
    ]);
    let (c, _) = client(&g.origin);
    assert_eq!(c.reauth_url("sa1.t", ReauthKind::Passkey).unwrap_err(), AccountCallError::BadResponse);
    assert_eq!(c.reauth_url("sa1.t", ReauthKind::Google).unwrap_err(), AccountCallError::Refused(Some("no_passkey".into())));
    let sent = g.server.join().expect("server");
    assert!(sent[0].starts_with("POST /api/account/reauth/passkey "));
    assert!(sent[1].starts_with("POST /api/account/reauth/google "));
}

#[test]
fn the_browser_round_trip_returns_this_sign_ins_code() {
    let opened = Arc::new(Mutex::new(String::new()));
    let seen = Arc::clone(&opened);
    let mut draws = [[3u8; 32], [4u8; 32]].into_iter();
    let draws = Mutex::new(&mut draws);
    let result = run_browser_signin(
        "https://shogunai.si",
        None,
        &|| draws.lock().ok().and_then(|mut d| d.next()),
        &|url| {
            *seen.lock().map_err(|_| "lock")? = url.to_string();
            // The "browser": a stray probe, another flow's redirect, then this one's.
            let parsed = url::Url::parse(url).map_err(|_| "url")?;
            let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
            let redirect = url::Url::parse(&pairs["redirect_uri"]).map_err(|_| "redirect")?;
            let port = redirect.port().ok_or("port")?;
            let state = pairs["state"].clone();
            std::thread::spawn(move || {
                for line in [
                    "GET /favicon.ico HTTP/1.1".to_string(),
                    format!("GET /callback?code={}&state=other-flow-state HTTP/1.1", "X".repeat(43)),
                    format!("GET /callback?code={}&state={state} HTTP/1.1", "Y".repeat(43)),
                ] {
                    if let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) {
                        let _ = s.write_all(format!("{line}\r\nHost: x\r\n\r\n").as_bytes());
                        let mut buf = [0u8; 512];
                        let _ = s.read(&mut buf);
                    }
                }
            });
            Ok(())
        },
        Duration::from_secs(5),
        &|| false,
    )
    .expect("signed in");
    assert_eq!(result.code, "Y".repeat(43));
    let url = opened.lock().expect("lock").clone();
    assert!(url.starts_with("https://shogunai.si/app/signin?code_challenge="));
    assert!(!url.contains(&result.request.code_verifier), "the verifier never enters the browser");
    assert!(result.request.redirect_uri.starts_with("http://127.0.0.1:"));
    assert_eq!(format!("{result:?}"), "BrowserSignin { .. }");
}

#[test]
fn the_browser_round_trip_times_out_cancels_and_fails_fast() {
    let entropy = |n: u8| move || Some([n; 32]);
    let mut n = 0u8;
    let counter = Mutex::new(&mut n);
    let fresh = || {
        counter.lock().ok().map(|mut c| {
            **c = c.wrapping_add(1);
            [**c; 32]
        })
    };
    assert_eq!(
        run_browser_signin("https://x", None, &fresh, &|_| Ok(()), Duration::from_millis(150), &|| false).unwrap_err(),
        SigninFlowError::Timeout
    );
    assert_eq!(
        run_browser_signin("https://x", None, &fresh, &|_| Ok(()), Duration::from_secs(5), &|| true).unwrap_err(),
        SigninFlowError::Cancelled
    );
    assert_eq!(
        run_browser_signin("https://x", None, &fresh, &|_| Err("no browser".into()), Duration::from_secs(5), &|| false)
            .unwrap_err(),
        SigninFlowError::BrowserOpen
    );
    // The same draw twice would put the verifier in the URL as the state.
    assert_eq!(
        run_browser_signin("https://x", None, &entropy(5), &|_| Ok(()), Duration::from_secs(5), &|| false).unwrap_err(),
        SigninFlowError::Entropy
    );
    assert_eq!(
        run_browser_signin("https://x", None, &|| None, &|_| Ok(()), Duration::from_secs(5), &|| false).unwrap_err(),
        SigninFlowError::Entropy
    );
}

#[test]
fn profile_photo_rejects_other_destinations_before_any_egress() {
    let (c, sink) = client("https://account.example.test");
    for url in [
        "http://lh3.googleusercontent.com/a",
        "https://127.0.0.1/a",
        "https://lh3.googleusercontent.com.evil.test/a",
        "https://lh3.googleusercontent.com:444/a",
        "https://user:pass@lh3.googleusercontent.com/a",
        "https://lh3.googleusercontent.com/a#fragment",
        "data:image/png;base64,AAAA",
    ] {
        assert!(c.profile_photo(url).is_none());
    }
    assert!(sink.records().is_empty());
}

#[test]
fn optional_profile_exchange_binds_metadata_to_the_authenticated_account() {
    let mut answer = tokens(1);
    answer["account"] = json!({ "id": "account-a", "email": null, "email_confirmed": false });
    answer["profile"] = json!({ "account_id": "account-a", "subject_hash": "a".repeat(64),
        "display_name": "Person Name", "first_name": "Person", "avatar_url": "https://lh3.googleusercontent.com/a/photo" });
    let mut mismatched = answer.clone();
    mismatched["profile"]["account_id"] = json!("another-account");
    let f = fixture(vec![http(200, &answer), http(200, &mismatched)]);
    let (c, _) = client(&f.origin);
    let req = request_for(50_000);
    let envelope = "E".repeat(100);
    let (_, _, profile) = c.exchange_with_profile(&req, &"C".repeat(43), "device-0001", None, Some(&envelope)).expect("exchange");
    let profile = profile.expect("bound profile");
    assert_eq!(profile.account_id, "account-a");
    assert_eq!(profile.first_name.as_deref(), Some("Person"));
    let (_, _, profile) = c.exchange_with_profile(&req, &"D".repeat(43), "device-0001", None, None).expect("login still succeeds");
    assert!(profile.is_none());
    let sent = f.server.join().expect("server");
    let body: Value = serde_json::from_str(sent[0].split("\r\n\r\n").nth(1).expect("body")).expect("json");
    assert_eq!(body["profile_envelope"], envelope);
}
