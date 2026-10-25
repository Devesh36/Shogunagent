//! The account API client (#148 P2, `docs/account-passkey-design.md` §6–§8), feature `net`.
//!
//! FR-TR-03 keeps the single raw HTTP client in shogun-core, so the desktop shell asks this
//! module rather than reaching for reqwest itself. Every decision — PKCE, the callback, the wire
//! shapes, the session rules — is the pure `shogun_license::account`; this module only moves
//! bytes, and runs the browser round trip over the loopback listener the connector flows share
//! (`shogun_integrations::oauth_flow::accept_loopback`).
//!
//! **Traceability, fail-closed (invariant 3).** Before each request one row is written on
//! `Route::Billing` with an empty chunk: the account API carries no capture or memory content —
//! only the anonymous device id, a name the user typed, and the session's own tokens — so the row
//! records that the connection happened and to whom. If the row cannot be written the request is
//! not sent ([`AccountCallError::Unrecorded`]).
//!
//! Nothing here logs. Tokens, codes, addresses and device ids never reach a message.

use std::io::Read;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use std::time::{Duration, Instant};

use reqwest::Method;
use serde_json::Value;
use shogun_integrations::connect::ConnectError;
use shogun_integrations::oauth_flow::{accept_loopback, LoopbackVerdict};
use shogun_license::account::{
    callback_profile, classify_callback, parse_account_profile, error_reason, exchange_body, parse_account_view, parse_exchange,
    parse_reauth_url, parse_session_tokens, refresh_body, AccountProfile, AccountSummary, AccountView,
    CallbackVerdict, RefreshTransport, RevokeOutcome, SessionTokens, SigninError, SigninRequest,
    TransportError, CALLBACK_PAGE, REFRESH_TIMEOUT_SECS, SIGNIN_DONE_REDIRECT,
};

use crate::llm::traceability::{RoundTrip, Route, TraceRecord, TraceabilitySink};

/// Ordinary account calls: short, so a Settings click never hangs.
const CALL_TIMEOUT: Duration = Duration::from_secs(15);
/// The largest answer read. `GET /api/account` is a few kilobytes.
const MAX_RESPONSE: u64 = 256 * 1024;

/// One bounded, memory-only provider photo cache; failures are cached briefly too.
static PROFILE_PHOTO: Mutex<Option<(String, Instant, Option<String>)>> = Mutex::new(None);

/// Drop the memory-only image when the local account changes or signs out.
pub fn clear_profile_photo_cache() {
    if let Ok(mut cache) = PROFILE_PHOTO.lock() { *cache = None; }
}

/// Why an account call produced no answer the caller can use. Content-free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountCallError {
    /// 401: no valid session (expired access token, or a session removed / revoked).
    Unauthorized,
    /// 400 / 403 / 409, with the server's reason when it is one the app acts on
    /// (`reauth_required`, `active_subscription`, `last_passkey`, …).
    Refused(Option<String>),
    /// 404: the device, passkey or link is not there (or Google is not configured).
    NotFound,
    /// 429.
    RateLimited,
    /// No answer: offline, timed out, or an answer that is not the API's.
    Unavailable,
    /// 5xx: the server answered but failed.
    ServerError,
    /// A 2xx whose body was not what was asked for.
    BadResponse,
    /// The traceability row could not be written, so nothing was sent.
    Unrecorded,
}

/// Account-derived cancellation result; no Stripe customer, subscription or card IDs cross IPC.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BillingCancellation {
    pub cancel_at_period_end: bool,
    pub card_removed: bool,
    pub current_period_end: Option<u64>,
    pub reason: Option<String>,
}

fn billing_cancellation_reason(body: &Value) -> Option<String> {
    body.get("reason").or_else(|| body.get("error"))
        .and_then(Value::as_str)
        .filter(|reason| matches!(*reason,
            "other_subscription_active" | "card_removal_failed"
            | "subscription_not_cancellable" | "billing_ownership_mismatch"))
        .map(str::to_string)
}

fn parse_billing_cancellation(body: &Value) -> Result<BillingCancellation, AccountCallError> {
    if body.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(AccountCallError::BadResponse);
    }
    let current_period_end = match body.get("current_period_end") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_u64().ok_or(AccountCallError::BadResponse)?),
    };
    Ok(BillingCancellation {
        cancel_at_period_end: body.get("cancel_at_period_end").and_then(Value::as_bool).ok_or(AccountCallError::BadResponse)?,
        card_removed: body.get("card_removed").and_then(Value::as_bool).ok_or(AccountCallError::BadResponse)?,
        current_period_end,
        reason: billing_cancellation_reason(body).filter(|reason| matches!(reason.as_str(), "other_subscription_active" | "card_removal_failed")),
    })
}

/// Which re-authentication the account uses for the 5-minute fresh check (§6.4 / D7b).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReauthKind {
    /// A passkey ceremony on the site (`/app/reauth`).
    Passkey,
    /// A forced fresh Google sign-in (`/api/account/google/reauth`).
    Google,
}

/// The account API at one origin, over one HTTPS client.
pub struct AccountClient {
    origin: String,
    host: String,
    http: reqwest::blocking::Client,
    sink: Arc<dyn TraceabilitySink>,
}

impl AccountClient {
    pub fn new(origin: &str, sink: Arc<dyn TraceabilitySink>) -> Result<Self, AccountCallError> {
        let http = reqwest::blocking::Client::builder()
            .user_agent("shogun/1.0")
            // No redirects and no automatic retries: a refresh's retry is ours to time (§6.3).
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(5))
            .timeout(CALL_TIMEOUT)
            .build()
            .map_err(|_| AccountCallError::Unavailable)?;
        let origin = origin.trim_end_matches('/').to_string();
        let host = origin
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or("")
            .to_string();
        Ok(Self { origin, host, http, sink })
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }

    fn call(
        &self,
        method: Method,
        path: &str,
        bearer: Option<&str>,
        body: Option<Value>,
        purpose: &'static str,
        timeout: Duration,
    ) -> Result<Value, AccountCallError> {
        // Fail-closed: no ledger row, no request (invariant 3). Empty chunk — see the module docs.
        self.sink
            .try_record(TraceRecord::for_chunk(Route::Billing, purpose, self.host.clone(), "", false))
            .map_err(|_| AccountCallError::Unrecorded)?;
        let mut request = self.http.request(method, format!("{}{path}", self.origin)).timeout(timeout);
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let started = Instant::now();
        let result = request.send();
        self.sink.round_trip(RoundTrip {
            route: Route::Billing,
            ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            status: result.as_ref().map_or(0, |r| r.status().as_u16()),
        });
        let response = result.map_err(|_| AccountCallError::Unavailable)?;
        let status = response.status().as_u16();
        let mut bytes = Vec::new();
        let read = response.take(MAX_RESPONSE + 1).read_to_end(&mut bytes);
        let parsed = if read.is_ok() && bytes.len() as u64 <= MAX_RESPONSE {
            serde_json::from_slice::<Value>(&bytes).ok()
        } else {
            None
        };
        match status {
            200..=299 => parsed.ok_or(AccountCallError::BadResponse),
            401 => Err(AccountCallError::Unauthorized),
            404 => Err(AccountCallError::NotFound),
            429 => Err(AccountCallError::RateLimited),
            400 | 403 | 409 => Err(AccountCallError::Refused(parsed.as_ref().and_then(|body| {
                error_reason(body).or_else(|| {
                    if purpose == "account_billing_cancel" { billing_cancellation_reason(body) } else { None }
                })
            }))),
            500..=599 => Err(AccountCallError::ServerError),
            _ => Err(AccountCallError::Unavailable),
        }
    }

    /// Trade the hand-off code for this Mac's session (§8 step 4).
    pub fn exchange(
        &self,
        request: &SigninRequest,
        code: &str,
        device_id: &str,
        device_name: Option<&str>,
    ) -> Result<(AccountSummary, SessionTokens), AccountCallError> {
        self.exchange_with_profile(request, code, device_id, device_name, None).map(|(account, tokens, _)| (account, tokens))
    }

    /// Optional local-only profile ciphertext is returned through the same authenticated exchange.
    pub fn exchange_with_profile(
        &self,
        request: &SigninRequest,
        code: &str,
        device_id: &str,
        device_name: Option<&str>,
        profile_envelope: Option<&str>,
    ) -> Result<(AccountSummary, SessionTokens, Option<AccountProfile>), AccountCallError> {
        let mut body = exchange_body(request, code, device_id, device_name);
        if let Some(envelope) = profile_envelope.filter(|value| value.len() <= 3300) {
            body["profile_envelope"] = Value::String(envelope.to_string());
        }
        let answer = self.call(Method::POST, "/api/account/desktop/exchange", None, Some(body), "account_signin", CALL_TIMEOUT)?;
        let (account, tokens) = parse_exchange(&answer).map_err(|_| AccountCallError::BadResponse)?;
        let profile = answer.get("profile").and_then(parse_account_profile)
            .filter(|profile| account.id.as_deref() == Some(profile.account_id.as_str()));
        Ok((account, tokens, profile))
    }

    /// `GET /api/account`: the address, sign-in methods, passkeys and devices.
    pub fn account(&self, access: &str) -> Result<AccountView, AccountCallError> {
        let answer = self.call(Method::GET, "/api/account", Some(access), None, "account_read", CALL_TIMEOUT)?;
        parse_account_view(&answer).map_err(|_| AccountCallError::BadResponse)
    }

    /// Called only after confirmation: the server derives billing ownership from the bearer.
    /// No IDs or request body are accepted; HTTP failures are never automatically retried.
    pub fn cancel_and_remove_card(&self, access: &str) -> Result<BillingCancellation, AccountCallError> {
        let answer = self.call(Method::POST, "/api/account/billing/cancel", Some(access), None,
            "account_billing_cancel", CALL_TIMEOUT)?;
        parse_billing_cancellation(&answer)
    }

    /// Fetch only Google's profile-image CDN, without credentials, redirects or disk writes.
    /// A failed photo never fails the account view. Repeated Settings reads reuse memory only.
    pub fn profile_photo(&self, raw: &str) -> Option<String> {
        let url = reqwest::Url::parse(raw).ok()?;
        if raw.len() > 2048 || url.scheme() != "https" || url.host_str() != Some("lh3.googleusercontent.com")
            || url.port().is_some() || !url.username().is_empty() || url.password().is_some()
            || url.fragment().is_some()
        {
            return None;
        }
        let mut cache = PROFILE_PHOTO.lock().ok()?;
        if let Some((key, at, data)) = cache.as_ref() {
            let ttl = if data.is_some() { 300 } else { 60 };
            if key == raw && at.elapsed() < Duration::from_secs(ttl) {
                return data.clone();
            }
        }
        let data = self.fetch_profile_photo(url);
        *cache = Some((raw.to_string(), Instant::now(), data.clone()));
        data
    }

    fn fetch_profile_photo(&self, url: reqwest::Url) -> Option<String> {
        const MAX_PHOTO_BYTES: u64 = 256 * 1024;
        self.sink.try_record(TraceRecord::for_chunk(
            Route::Billing, "account_profile_photo", "lh3.googleusercontent.com", "", false,
        )).ok()?;
        let started = Instant::now();
        let response = self.http.get(url).timeout(Duration::from_secs(5)).send();
        self.sink.round_trip(RoundTrip {
            route: Route::Billing,
            ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            status: response.as_ref().map_or(0, |r| r.status().as_u16()),
        });
        let response = response.ok()?;
        if !response.status().is_success() || response.content_length().is_some_and(|n| n > MAX_PHOTO_BYTES) {
            return None;
        }
        let mime = response.headers().get(reqwest::header::CONTENT_TYPE)?.to_str().ok()?.split(';').next()?.trim();
        if !matches!(mime, "image/jpeg" | "image/png" | "image/webp") {
            return None;
        }
        let mime = mime.to_string();
        let mut bytes = Vec::new();
        response.take(MAX_PHOTO_BYTES + 1).read_to_end(&mut bytes).ok()?;
        if bytes.is_empty() || bytes.len() as u64 > MAX_PHOTO_BYTES {
            return None;
        }
        // Reject mislabeled content before it reaches the webview decoder.
        let matches_type = match mime.as_str() {
            "image/jpeg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
            "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
            "image/webp" => bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"),
            _ => false,
        };
        matches_type.then(|| format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)))
    }

    /// Rename a device of the account (§7.2). `None` clears the name.
    pub fn rename_device(&self, access: &str, device_id: &str, name: Option<&str>) -> Result<(), AccountCallError> {
        let path = device_path(device_id)?;
        self.call(
            Method::PATCH,
            &path,
            Some(access),
            Some(serde_json::json!({ "name": name })),
            "account_device_rename",
            CALL_TIMEOUT,
        )
        .map(drop)
    }

    /// Remove a device from the account (§7.2); this Mac's own id is the same as signing out.
    pub fn remove_device(&self, access: &str, device_id: &str) -> Result<(), AccountCallError> {
        let path = device_path(device_id)?;
        self.call(Method::DELETE, &path, Some(access), None, "account_device_remove", CALL_TIMEOUT)
            .map(drop)
    }

    /// Remove one passkey. Needs a fresh check (`Refused(Some("reauth_required"))` otherwise);
    /// the last sign-in method cannot be removed (`Refused(Some("last_passkey"))`).
    pub fn remove_passkey(&self, access: &str, passkey_id: &str) -> Result<(), AccountCallError> {
        if !is_uuid(passkey_id) {
            return Err(AccountCallError::NotFound);
        }
        self.call(
            Method::DELETE,
            &format!("/api/account/passkeys/{passkey_id}"),
            Some(access),
            None,
            "account_passkey_remove",
            CALL_TIMEOUT,
        )
        .map(drop)
    }

    /// Delete the account (§7.5). Needs a fresh check; refused with `active_subscription` while a
    /// subscription runs. Nothing on any device is deleted.
    pub fn delete_account(&self, access: &str) -> Result<(), AccountCallError> {
        self.call(Method::DELETE, "/api/account", Some(access), None, "account_delete", CALL_TIMEOUT)
            .map(drop)
    }

    /// The one-time link that re-authenticates this session in the browser (§6.4 / D7b). Only a
    /// link on this client's own origin is returned.
    pub fn reauth_url(&self, access: &str, kind: ReauthKind) -> Result<String, AccountCallError> {
        let (path, purpose) = match kind {
            ReauthKind::Passkey => ("/api/account/reauth/passkey", "account_reauth_passkey"),
            ReauthKind::Google => ("/api/account/reauth/google", "account_reauth_google"),
        };
        let answer = self.call(Method::POST, path, Some(access), Some(serde_json::json!({})), purpose, CALL_TIMEOUT)?;
        parse_reauth_url(&answer, &self.origin).map_err(|_| AccountCallError::BadResponse)
    }

    /// Revoke this Mac's session on the server and free its seat (§7.4). The server tries the
    /// access token first and the refresh token in the body second, so both go when held: a
    /// sign-out queued offline is often sent after the access token expired. `None` for both is
    /// [`RevokeOutcome::AlreadyGone`] without a request — there is nothing to revoke.
    pub fn logout(&self, access: Option<&str>, refresh: Option<&str>) -> RevokeOutcome {
        if access.is_none() && refresh.is_none() {
            return RevokeOutcome::AlreadyGone;
        }
        let body = match refresh {
            Some(token) => serde_json::json!({ "refresh_token": token }),
            None => serde_json::json!({}),
        };
        match self.call(Method::POST, "/api/account/logout", access, Some(body), "account_signout", CALL_TIMEOUT) {
            Ok(_) => RevokeOutcome::Done,
            Err(AccountCallError::Unauthorized) => RevokeOutcome::AlreadyGone,
            Err(_) => RevokeOutcome::NotSent,
        }
    }
}

impl RefreshTransport for AccountClient {
    fn refresh(&self, refresh_token: &str, device_id: &str) -> Result<SessionTokens, TransportError> {
        let answer = self
            .call(
                Method::POST,
                "/api/account/session/refresh",
                None,
                Some(refresh_body(refresh_token, device_id)),
                "account_refresh",
                Duration::from_secs(REFRESH_TIMEOUT_SECS),
            )
            .map_err(|e| match e {
                AccountCallError::Unauthorized => TransportError::Unauthorized,
                AccountCallError::Unavailable | AccountCallError::ServerError => TransportError::Retryable,
                AccountCallError::Unrecorded => TransportError::Unrecorded,
                // A 2xx we could not read may well have rotated the token: retry it like a
                // lost response, which the server answers with the same pair.
                AccountCallError::BadResponse => TransportError::Retryable,
                _ => TransportError::Refused,
            })?;
        parse_session_tokens(&answer).map_err(|_| TransportError::Retryable)
    }
}

/// `/api/account/devices/{id}` for an id the server could hold (`isValidDeviceId`).
fn device_path(device_id: &str) -> Result<String, AccountCallError> {
    let ok = (8..=64).contains(&device_id.len())
        && device_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok {
        Ok(format!("/api/account/devices/{device_id}"))
    } else {
        Err(AccountCallError::NotFound)
    }
}

fn is_uuid(raw: &str) -> bool {
    raw.len() == 36
        && raw.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// The browser round trip (§8 steps 1–3)
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// Why the browser part of a sign-in did not produce a code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SigninFlowError {
    /// The loopback listener could not be bound, or got a privileged port.
    Listener,
    /// No entropy for the verifier or the state.
    Entropy,
    /// The default browser could not be opened.
    BrowserOpen,
    /// The browser did not come back in time.
    Timeout,
    /// The user cancelled from the app.
    Cancelled,
}

/// The browser part of one sign-in: the request (with its verifier, still in memory only) and the
/// one-time code the site handed back. The exchange is the next step.
pub struct BrowserSignin {
    pub request: SigninRequest,
    pub code: String,
    pub profile_envelope: Option<String>,
}

impl std::fmt::Debug for BrowserSignin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BrowserSignin { .. }")
    }
}

/// Bind an ephemeral loopback port, open `/app/signin` (or Google's start for
/// `provider == Some("google")`) in the default browser, and wait for the
/// site's redirect back with a code and this sign-in's state (§8). `entropy` is called twice, for
/// two independent draws (verifier and state). `open` is the browser opener
/// (`shogun_integrations::oauth_flow::open_system_browser` on the Mac).
pub fn run_browser_signin(
    origin: &str,
    provider: Option<&str>,
    entropy: &dyn Fn() -> Option<[u8; 32]>,
    open: &dyn Fn(&str) -> Result<(), String>,
    wait: Duration,
    cancelled: &dyn Fn() -> bool,
) -> Result<BrowserSignin, SigninFlowError> {
    if cancelled() {
        return Err(SigninFlowError::Cancelled);
    }
    // Bound first, so the real port goes into the redirect URI.
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|_| SigninFlowError::Listener)?;
    let port = listener.local_addr().map_err(|_| SigninFlowError::Listener)?.port();
    let verifier = entropy().ok_or(SigninFlowError::Entropy)?;
    let state = entropy().ok_or(SigninFlowError::Entropy)?;
    let request = SigninRequest::new(&verifier, &state, port).map_err(|e| match e {
        SigninError::WeakEntropy => SigninFlowError::Entropy,
        SigninError::BadPort => SigninFlowError::Listener,
    })?;
    if cancelled() {
        return Err(SigninFlowError::Cancelled);
    }
    open(&request.browser_url(origin, provider)).map_err(|_| SigninFlowError::BrowserOpen)?;
    let expected = request.state.clone();
    let (code, profile_envelope) = accept_loopback(&[&listener], Instant::now() + wait, cancelled, CALLBACK_PAGE, SIGNIN_DONE_REDIRECT, &|line| {
        match classify_callback(line, &expected) {
            CallbackVerdict::Code(code) => LoopbackVerdict::Done((code, callback_profile(line, &expected))),
            CallbackVerdict::Stray => LoopbackVerdict::Stray,
        }
    })
    .map_err(|e| match e {
        ConnectError::Cancelled => SigninFlowError::Cancelled,
        ConnectError::Timeout => SigninFlowError::Timeout,
        _ => SigninFlowError::Listener,
    })?;
    Ok(BrowserSignin { request, code, profile_envelope })
}

#[cfg(test)]
mod tests;
