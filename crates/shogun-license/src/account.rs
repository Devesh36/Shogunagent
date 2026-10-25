//! Accounts on this Mac (#148 P2, `docs/account-passkey-design.md` §6.3, §7, §8): the pure half.
//!
//! Everything here is decided without a network, a Keychain or a clock of its own, so it is
//! tested on Linux:
//!
//! - **Browser sign-in (§8)**: the PKCE verifier / S256 challenge and the app's `state` (entropy
//!   passed in), the `/app/signin` URL, the loopback redirect URI, and the classification of the
//!   request the browser sends back to the loopback listener.
//! - **Wire types**: the request bodies the app sends and the answers it reads, matching
//!   `apps/website/src/app/api/account/**`.
//! - **The session (§6.3)**: [`Session`] holds the 15-minute access token in memory only and the
//!   rotating refresh token in a [`SessionStore`] (the Keychain item `account-session` on the Mac).
//!   Refresh is single-flight: one refresh at a time, the new refresh token written to the store
//!   before anything uses it, and a lost response retried promptly so the server's 30-second
//!   "same token, same device, once" rule answers it with the same new pair.
//! - **Refusals and sign-out (§7.2 / §7.4)**: what a licence or trial answer through the account
//!   means for this Mac, and what a sign-out removes.
//! - **`account.json`**: the content-free snapshot the read-only MCP / REST / CLI faces read
//!   (FR-ACC-07). Never a token, never a device id.
//!
//! Nothing here logs. Tokens, codes, addresses and device ids never appear in a `Debug` string
//! of the types that carry them.

use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

// ─────────────────────────────────────────────────────────────────────────────────────────────
// Browser sign-in (§8)
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// Account pages and API origin, separate from the legacy billing/updater origin.
/// Owner decision 2026-10-16: move before any passkeys have been issued.
pub const SIGNIN_ORIGIN: &str = "https://shogunai.si";

/// Where the app's **Sign in** opens the system browser.
pub const SIGNIN_PATH: &str = "/app/signin";

/// Where **Continue with Google** opens it instead: straight into Google, same query.
pub const GOOGLE_SIGNIN_PATH: &str = "/api/account/google/start";

/// `"google"` goes straight to Google; anything else (or nothing) opens the sign-in page.
pub fn signin_path(provider: Option<&str>) -> &'static str {
    match provider {
        Some("google") => GOOGLE_SIGNIN_PATH,
        _ => SIGNIN_PATH,
    }
}

/// The lowest loopback port the site accepts (`isLoopbackRedirect`: a non-privileged port).
pub const MIN_LOOPBACK_PORT: u16 = 1024;

/// How long the app waits for the browser to come back before giving the sign-in up. Longer than
/// the site's own Google window (about 10 minutes, started after this clock): "Start again with
/// Google" on the site must still land on a listening loopback port.
pub const SIGNIN_WAIT_SECS: u64 = 30 * 60;

/// Bytes of entropy for the PKCE verifier and for the state, each.
pub const ENTROPY_BYTES: usize = 32;

/// Why a sign-in request could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SigninError {
    /// Less than [`ENTROPY_BYTES`] of entropy, or the same bytes for the verifier and the state.
    WeakEntropy,
    /// A privileged port: the site refuses it.
    BadPort,
}

/// One browser sign-in, as the app holds it in memory until the code comes back. The verifier
/// never leaves this process except in the exchange request body.
#[derive(Clone, PartialEq, Eq)]
pub struct SigninRequest {
    /// RFC 7636 verifier: base64url of the entropy, 43 characters.
    pub code_verifier: String,
    /// `base64url(sha256(verifier))`, sent in the browser URL.
    pub code_challenge: String,
    /// The app's own state, echoed back on the loopback redirect and checked there.
    pub state: String,
    /// `http://127.0.0.1:<port>/callback`.
    pub redirect_uri: String,
}

impl std::fmt::Debug for SigninRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SigninRequest { .. }")
    }
}

impl SigninRequest {
    /// Build a request from two independent draws of entropy and the port the listener is
    /// already bound to. Two draws, never one: the state travels in the browser URL, so it must
    /// not be derivable from the verifier.
    pub fn new(verifier_entropy: &[u8], state_entropy: &[u8], port: u16) -> Result<Self, SigninError> {
        if verifier_entropy.len() < ENTROPY_BYTES
            || state_entropy.len() < ENTROPY_BYTES
            || verifier_entropy == state_entropy
        {
            return Err(SigninError::WeakEntropy);
        }
        if port < MIN_LOOPBACK_PORT {
            return Err(SigninError::BadPort);
        }
        let code_verifier = URL_SAFE_NO_PAD.encode(&verifier_entropy[..ENTROPY_BYTES]);
        Ok(Self {
            code_challenge: pkce_challenge(&code_verifier),
            code_verifier,
            state: URL_SAFE_NO_PAD.encode(&state_entropy[..ENTROPY_BYTES]),
            redirect_uri: loopback_redirect_uri(port),
        })
    }

    /// The URL to open in the default browser: `/app/signin`, or Google's start for
    /// `provider == Some("google")` (`signin_path`), with the same query. Every value is
    /// base64url or a fixed loopback URI, so only `:` and `/` in the redirect need escaping.
    pub fn browser_url(&self, origin: &str, provider: Option<&str>) -> String {
        format!(
            "{}{}?code_challenge={}&code_challenge_method=S256&state={}&redirect_uri={}",
            origin.trim_end_matches('/'),
            signin_path(provider),
            self.code_challenge,
            self.state,
            query_escape(&self.redirect_uri),
        )
    }
}

/// S256: `base64url(sha256(verifier))`.
pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// The loopback redirect the site accepts: exactly `http://127.0.0.1:<port>/callback`.
pub fn loopback_redirect_uri(port: u16) -> String {
    format!("http://127.0.0.1:{port}/callback")
}

fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A hand-off code as the site issues it: 256 random bits, base64url (`isDesktopCode`).
pub fn is_desktop_code(raw: &str) -> bool {
    raw.len() == 43 && raw.bytes().all(is_b64url)
}

fn is_b64url(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

/// What one request to the loopback listener means for the sign-in waiting on it.
#[derive(Clone, PartialEq, Eq)]
pub enum CallbackVerdict {
    /// The site's redirect for this sign-in: the one-time code.
    Code(String),
    /// Anything else — a favicon probe, another flow's redirect, a local process, a malformed
    /// line. Answered and ignored; the real redirect can still arrive before the deadline.
    Stray,
}

impl std::fmt::Debug for CallbackVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Code(_) => f.write_str("Code(..)"),
            Self::Stray => f.write_str("Stray"),
        }
    }
}

/// Classify the request line the browser sent to the loopback listener, e.g.
/// `GET /callback?code=…&state=… HTTP/1.1`. Only a GET of exactly `/callback` carrying a
/// well-formed code and **this** sign-in's state resolves the wait; the state is compared in
/// constant time. Values are compared raw: the site sends base64url, which needs no decoding,
/// and anything percent-encoded is not a value it sends.
pub fn classify_callback(request_line: &str, expected_state: &str) -> CallbackVerdict {
    let mut parts = request_line.split_whitespace();
    if parts.next() != Some("GET") {
        return CallbackVerdict::Stray;
    }
    let Some(target) = parts.next() else {
        return CallbackVerdict::Stray;
    };
    let Some((path, query)) = target.split_once('?') else {
        return CallbackVerdict::Stray;
    };
    if path != "/callback" {
        return CallbackVerdict::Stray;
    }
    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        match pair.split_once('=') {
            Some(("code", v)) if code.is_none() => code = Some(v),
            Some(("state", v)) if state.is_none() => state = Some(v),
            // A repeated `code` or `state` is not something the site sends.
            Some(("code", _)) | Some(("state", _)) => return CallbackVerdict::Stray,
            _ => {}
        }
    }
    match (code, state) {
        (Some(code), Some(state)) if is_desktop_code(code) && constant_time_eq(state, expected_state) => {
            CallbackVerdict::Code(code.to_string())
        }
        _ => CallbackVerdict::Stray,
    }
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The page a stray loopback request gets (a favicon probe, another flow's redirect), so no
/// browser tab hangs. Plain words, no product internals.
pub const CALLBACK_PAGE: &str = concat!(
    "HTTP/1.1 200 OK\r\n",
    "Content-Type: text/html; charset=utf-8\r\n",
    "Cache-Control: no-store\r\n",
    "Referrer-Policy: no-referrer\r\n",
    "X-Content-Type-Options: nosniff\r\n",
    "Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; img-src https://shogunai.si; base-uri 'none'; form-action 'none'; frame-ancestors 'none'\r\n",
    "Connection: close\r\n\r\n",
    include_str!("account/callback.html"),
);

/// The answer to this sign-in's own redirect, written only after the code has been read off the
/// request: send the tab to the site's "You're signed in" page, so it never rests on the
/// loopback URL. A fixed literal, so the code and state cannot leak into it; `no-referrer` keeps
/// the loopback URL (which carries them) out of the request to the site.
pub const SIGNIN_DONE_REDIRECT: &str = concat!(
    "HTTP/1.1 303 See Other\r\n",
    "Location: https://shogunai.si/app/signin/done\r\n",
    "Cache-Control: no-store\r\n",
    "Referrer-Policy: no-referrer\r\n",
    "Content-Length: 0\r\n",
    "Connection: close\r\n\r\n",
);

// ─────────────────────────────────────────────────────────────────────────────────────────────
// Wire types (apps/website/src/app/api/account/**)
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// A token pair as `/api/account/desktop/exchange` and `/api/account/session/refresh` answer it.
/// Expiry times in unix seconds. `Debug` shows neither token.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionTokens {
    pub refresh_token: String,
    pub access_token: String,
    pub access_expires_at: u64,
    pub refresh_expires_at: u64,
}

impl std::fmt::Debug for SessionTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionTokens")
            .field("access_expires_at", &self.access_expires_at)
            .field("refresh_expires_at", &self.refresh_expires_at)
            .finish_non_exhaustive()
    }
}

/// An answer the app could not read as what it asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireError;

/// The server's token shapes (`sr1.<uuid>.<b64url>` / `sa1.…`), checked loosely: a token that
/// is not one of these is never stored.
fn is_token(raw: &str, prefix: &str) -> bool {
    raw.len() <= 128
        && raw.starts_with(prefix)
        && raw.bytes().all(|b| is_b64url(b) || b == b'.')
}

/// Read a session token pair out of an `{ ok: true, … }` body.
pub fn parse_session_tokens(body: &Value) -> Result<SessionTokens, WireError> {
    if body.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(WireError);
    }
    let text = |k: &str| body.get(k).and_then(Value::as_str).map(str::to_string);
    let refresh_token = text("refresh_token").filter(|t| is_token(t, "sr1.")).ok_or(WireError)?;
    let access_token = text("access_token").filter(|t| is_token(t, "sa1.")).ok_or(WireError)?;
    Ok(SessionTokens {
        refresh_token,
        access_token,
        access_expires_at: body.get("access_expires_at").and_then(Value::as_u64).ok_or(WireError)?,
        refresh_expires_at: body.get("refresh_expires_at").and_then(Value::as_u64).ok_or(WireError)?,
    })
}

/// The account's own address state (`accountSummary` on the server).
#[derive(Clone, PartialEq, Eq, Default)]
pub struct AccountSummary {
    /// Server account id, used only to bind the locally held provider profile.
    pub id: Option<String>,
    pub email: Option<String>,
    pub email_confirmed: bool,
    /// An address typed but not confirmed yet.
    pub pending_email: Option<String>,
    /// Another account confirmed the address this one had typed (§6.1).
    pub email_detached: bool,
}

impl std::fmt::Debug for AccountSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountSummary")
            .field("email_confirmed", &self.email_confirmed)
            .field("email_detached", &self.email_detached)
            .finish_non_exhaustive()
    }
}

fn opt_text(v: &Value, k: &str) -> Option<String> {
    v.get(k)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn parse_summary(v: &Value) -> Option<AccountSummary> {
    v.as_object()?;
    Some(AccountSummary {
        id: opt_text(v, "id"),
        email: opt_text(v, "email"),
        email_confirmed: v.get("email_confirmed").and_then(Value::as_bool).unwrap_or(false),
        pending_email: opt_text(v, "pending_email"),
        email_detached: v.get("email_detached").and_then(Value::as_bool).unwrap_or(false),
    })
}

/// Verified display metadata. Stored only in the Mac's encrypted credential store.
#[derive(Clone)]
pub struct AccountProfile {
    pub account_id: String,
    pub subject_hash: String,
    pub display_name: Option<String>,
    /// The onboarding greeting's name (Google `given_name`).
    pub first_name: Option<String>,
    pub avatar_url: Option<String>,
}

impl std::fmt::Debug for AccountProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountProfile").finish_non_exhaustive()
    }
}

/// Parse bounded local metadata; the caller additionally matches the authenticated account.
pub fn parse_account_profile(value: &Value) -> Option<AccountProfile> {
    let account_id = opt_text(value, "account_id").filter(|id| id.len() <= 128)?;
    let subject_hash = opt_text(value, "subject_hash").filter(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))?;
    let display_name = opt_text(value, "display_name").filter(|name| name.chars().count() <= 200 && !name.chars().any(char::is_control));
    let first_name = opt_text(value, "first_name").filter(|name| name.chars().count() <= 64 && !name.chars().any(char::is_control));
    let avatar_url = opt_text(value, "avatar_url").filter(|url| url.len() <= 2048);
    Some(AccountProfile { account_id, subject_hash, display_name, first_name, avatar_url })
}

/// Optional ciphertext accompanying a valid callback. Duplicates or malformed values are ignored.
pub fn callback_profile(request_line: &str, expected_state: &str) -> Option<String> {
    if !matches!(classify_callback(request_line, expected_state), CallbackVerdict::Code(_)) {
        return None;
    }
    let target = request_line.split_whitespace().nth(1)?;
    let query = target.split_once('?')?.1;
    let mut profiles = query.split('&').filter_map(|part| part.strip_prefix("profile="));
    let profile = profiles.next()?;
    if profiles.next().is_some() || profile.len() < 40 || profile.len() > 3300 || !profile.bytes().all(is_b64url) {
        return None;
    }
    Some(profile.to_string())
}

/// What the exchange answers: the account and this Mac's session.
pub fn parse_exchange(body: &Value) -> Result<(AccountSummary, SessionTokens), WireError> {
    let tokens = parse_session_tokens(body)?;
    let account = body.get("account").and_then(parse_summary).ok_or(WireError)?;
    Ok((account, tokens))
}

/// `POST /api/account/desktop/exchange` body. The device is the anonymous hardware-derived id;
/// the name is the one the user gave, or none (the host name is never sent, §7.2).
pub fn exchange_body(
    req: &SigninRequest,
    code: &str,
    device_id: &str,
    device_name: Option<&str>,
) -> Value {
    let mut device = json!({ "id": device_id });
    if let Some(name) = device_name.and_then(clean_device_name) {
        device["name"] = Value::String(name);
    }
    json!({
        "code": code,
        "code_verifier": req.code_verifier,
        "device": device,
        "redirect_uri": req.redirect_uri,
    })
}

/// `POST /api/account/session/refresh` body.
pub fn refresh_body(refresh_token: &str, device_id: &str) -> Value {
    json!({ "refresh_token": refresh_token, "device_id": device_id })
}

/// The longest device name the server keeps (`DEVICE_NAME_MAX`).
pub const DEVICE_NAME_MAX: usize = 64;

/// A device name as the server accepts it: trimmed, 1–64 characters, no control characters.
/// `None` for an empty name (the server then keeps the previous one, or none).
pub fn clean_device_name(raw: &str) -> Option<String> {
    let name = raw.trim();
    (!name.is_empty() && name.chars().count() <= DEVICE_NAME_MAX && !name.chars().any(char::is_control))
        .then(|| name.to_string())
}

/// One passkey of the account, as `GET /api/account` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasskeyView {
    /// The server's row id (not the credential id): what `DELETE /api/account/passkeys/{id}` takes.
    pub id: String,
    pub created_at: Option<u64>,
    pub last_used_at: Option<u64>,
    /// A synced passkey (iCloud Keychain, a password manager).
    pub synced: bool,
}

/// One device of the account.
#[derive(Clone, PartialEq, Eq)]
pub struct DeviceView {
    /// The anonymous device id: what rename and remove take. Never written to `account.json`.
    pub device_id: String,
    pub name: Option<String>,
    pub last_seen_at: Option<u64>,
    pub holds_seat: bool,
    pub signed_in: bool,
    pub this_device: bool,
}

impl std::fmt::Debug for DeviceView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceView")
            .field("holds_seat", &self.holds_seat)
            .field("signed_in", &self.signed_in)
            .field("this_device", &self.this_device)
            .finish_non_exhaustive()
    }
}

/// A sign-in method other than passkeys (Google, #1101 D7). The provider's address is display only.
#[derive(Clone, PartialEq, Eq)]
pub struct IdentityView {
    pub provider: String,
    pub email: Option<String>,
}

impl std::fmt::Debug for IdentityView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IdentityView").field("provider", &self.provider).finish_non_exhaustive()
    }
}

/// `GET /api/account` (§5.3), as the app reads it.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct AccountView {
    pub account: AccountSummary,
    /// The linked Google identity digest, for matching the local profile. No profile data is stored server-side.
    pub google_subject_hash: Option<String>,
    /// Optional local provider profile name. Display only, never an authentication key.
    pub display_name: Option<String>,
    /// Optional local provider first name, for the onboarding greeting. Display only.
    pub first_name: Option<String>,
    /// Provider image URL, fetched only by the traced core client.
    pub avatar_url: Option<String>,
    /// Bounded image bytes encoded by the core client; never persisted.
    pub avatar_data_url: Option<String>,
    /// The server's end of the account's trial, unix seconds.
    pub trial_ends_at: Option<u64>,
    pub identities: Vec<IdentityView>,
    pub passkeys: Vec<PasskeyView>,
    pub devices: Vec<DeviceView>,
    /// Until when this session counts as freshly verified (credential changes allowed).
    pub fresh_until: Option<u64>,
}

impl std::fmt::Debug for AccountView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountView").finish_non_exhaustive()
    }
}

/// Read `GET /api/account`. Rows that do not parse are dropped rather than failing the view.
pub fn parse_account_view(body: &Value) -> Result<AccountView, WireError> {
    if body.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(WireError);
    }
    let account = body.get("account").and_then(parse_summary).ok_or(WireError)?;
    let list = |k: &str| body.get(k).and_then(Value::as_array).cloned().unwrap_or_default();
    Ok(AccountView {
        account,
        google_subject_hash: opt_text(body, "google_subject_hash"),
        display_name: None,
        first_name: None,
        avatar_url: None,
        avatar_data_url: None,
        trial_ends_at: body.pointer("/trial/trial_ends_at").and_then(Value::as_u64),
        identities: list("identities")
            .iter()
            .filter_map(|i| {
                Some(IdentityView { provider: opt_text(i, "provider")?, email: opt_text(i, "email") })
            })
            .collect(),
        passkeys: list("passkeys")
            .iter()
            .filter_map(|p| {
                Some(PasskeyView {
                    id: opt_text(p, "id")?,
                    created_at: p.get("created_at").and_then(Value::as_u64),
                    last_used_at: p.get("last_used_at").and_then(Value::as_u64),
                    synced: p.get("synced").and_then(Value::as_bool).unwrap_or(false),
                })
            })
            .collect(),
        devices: list("devices")
            .iter()
            .filter_map(|d| {
                Some(DeviceView {
                    device_id: opt_text(d, "device_id")?,
                    name: opt_text(d, "name"),
                    last_seen_at: d.get("last_seen_at").and_then(Value::as_u64),
                    holds_seat: d.get("holds_seat").and_then(Value::as_bool).unwrap_or(false),
                    signed_in: d.get("signed_in").and_then(Value::as_bool).unwrap_or(false),
                    this_device: d.get("this_device").and_then(Value::as_bool).unwrap_or(false),
                })
            })
            .collect(),
        fresh_until: body.pointer("/session/fresh_until").and_then(Value::as_u64),
    })
}

/// Server refusal reasons the app acts on. Allow-listed: anything else is a plain failure.
pub const ACTIONABLE_REASONS: [&str; 7] = [
    "reauth_required",
    "active_subscription",
    "no_google",
    "no_passkey",
    "name_invalid",
    "last_passkey",
    "account_required",
];

/// The `reason` of a `{ ok: false, error, reason }` body, when it is one the app acts on.
pub fn error_reason(body: &Value) -> Option<String> {
    body.get("reason")
        .and_then(Value::as_str)
        .filter(|r| ACTIONABLE_REASONS.contains(r))
        .map(str::to_string)
}

/// The one-time URL a re-authentication answer carries (`{ url, expires_at }`). Only an https
/// link on the account API's own origin is accepted, so a tampered answer cannot open anything
/// else in the browser.
pub fn parse_reauth_url(body: &Value, origin: &str) -> Result<String, WireError> {
    if body.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(WireError);
    }
    let url = body.get("url").and_then(Value::as_str).ok_or(WireError)?;
    let origin = origin.trim_end_matches('/');
    let rest = url.strip_prefix(origin).ok_or(WireError)?;
    if !(origin.starts_with("https://") || origin.starts_with("http://localhost"))
        || !rest.starts_with('/')
        || url.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(WireError);
    }
    Ok(url.to_string())
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// The session (§6.3)
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// A cached access token this close to expiry is refreshed instead of used.
pub const ACCESS_EXPIRY_SKEW_SECS: u64 = 60;

/// The server answers a lost response's retry for this long after a rotation (same token, same
/// device, once).
pub const LOST_RESPONSE_WINDOW_SECS: u64 = 30;

/// Waits before the retries of one refresh whose answer did not arrive. With a request timeout
/// of [`REFRESH_TIMEOUT_SECS`], the last retry still lands inside [`LOST_RESPONSE_WINDOW_SECS`].
pub const REFRESH_RETRY_DELAYS_SECS: [u64; 2] = [1, 3];

/// Per-request timeout for a refresh: three attempts plus the waits stay under 30 seconds.
pub const REFRESH_TIMEOUT_SECS: u64 = 8;

/// Where the refresh token lives: the Keychain item `account-session` on the Mac, an in-memory
/// map in tests. Nothing else may hold it (invariant 7).
pub trait SessionStore: Send + Sync {
    fn load(&self) -> Result<Option<String>, StoreError>;
    fn save(&self, refresh_token: &str) -> Result<(), StoreError>;
    fn clear(&self) -> Result<(), StoreError>;
}

/// The store could not be read or written. Content-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreError;

/// What a refresh request came to on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    /// 401: the session is gone — signed out, removed from another device, theft, recovery.
    Unauthorized,
    /// No answer, or a server failure: the request may have reached the server, so retrying the
    /// same token is safe (and is what the lost-response rule is for).
    Retryable,
    /// Rate limited, or another refusal. Not retried now; asked again later.
    Refused,
    /// The traceability row could not be written, so nothing was sent (fail-closed, invariant 3).
    Unrecorded,
}

/// The refresh call, over the shared HTTP client (`shogun_core::account_client` on the Mac).
pub trait RefreshTransport: Send + Sync {
    fn refresh(&self, refresh_token: &str, device_id: &str) -> Result<SessionTokens, TransportError>;
}

/// Why no access token is available right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessError {
    /// No session on this Mac.
    SignedOut,
    /// The server refused the session (401): it was removed or revoked. The local session has
    /// been cleared; signing in again is the only way back (§7.2).
    Revoked,
    /// Could not ask (offline, server failure, rate limited, ledger unavailable). Nothing changed.
    Unavailable,
    /// The store could not be read or the new refresh token could not be written. Nothing that
    /// was not saved is used; a retry inside the lost-response window gets the same pair.
    StoreFailed,
}

struct CachedAccess {
    token: String,
    expires_at: u64,
}

/// This Mac's account session: the refresh token in a [`SessionStore`], the access token in
/// memory only, and one refresh at a time (the cache lock is held across it, so concurrent
/// callers wait for that one answer instead of each rotating the token — which the server would
/// read as theft).
pub struct Session<S, T> {
    store: S,
    transport: T,
    device_id: String,
    cache: Mutex<Option<CachedAccess>>,
    clock: Box<dyn Fn() -> u64 + Send + Sync>,
    sleep: Box<dyn Fn(Duration) + Send + Sync>,
}

fn system_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl<S: SessionStore, T: RefreshTransport> Session<S, T> {
    pub fn new(store: S, transport: T, device_id: impl Into<String>) -> Self {
        Self {
            store,
            transport,
            device_id: device_id.into(),
            cache: Mutex::new(None),
            clock: Box::new(system_secs),
            sleep: Box::new(std::thread::sleep),
        }
    }

    /// Inject the clock and the sleeper (tests).
    pub fn with_clock(
        mut self,
        clock: impl Fn() -> u64 + Send + Sync + 'static,
        sleep: impl Fn(Duration) + Send + Sync + 'static,
    ) -> Self {
        self.clock = Box::new(clock);
        self.sleep = Box::new(sleep);
        self
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// Is there a session on this Mac (a refresh token in the store)?
    pub fn signed_in(&self) -> Result<bool, StoreError> {
        Ok(self.store.load()?.is_some())
    }

    /// Take a new session from a sign-in exchange: the refresh token is stored **before** the
    /// access token is cached, so nothing is used that a restart would not find.
    pub fn adopt(&self, tokens: &SessionTokens) -> Result<(), AccessError> {
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        *cache = None;
        self.store.save(&tokens.refresh_token).map_err(|_| AccessError::StoreFailed)?;
        *cache = Some(CachedAccess { token: tokens.access_token.clone(), expires_at: tokens.access_expires_at });
        Ok(())
    }

    /// A usable access token: the cached one while it has more than [`ACCESS_EXPIRY_SKEW_SECS`]
    /// left, otherwise a refresh (single-flight). On 401 the local session is cleared
    /// ([`AccessError::Revoked`]).
    pub fn access_token(&self) -> Result<String, AccessError> {
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        let now = (self.clock)();
        if let Some(c) = cache.as_ref() {
            if c.expires_at > now.saturating_add(ACCESS_EXPIRY_SKEW_SECS) {
                return Ok(c.token.clone());
            }
        }
        *cache = None;
        // The newest value in the store: after a restart, or after another rotation.
        let refresh = match self.store.load() {
            Ok(Some(t)) => t,
            Ok(None) => return Err(AccessError::SignedOut),
            Err(_) => return Err(AccessError::StoreFailed),
        };
        let started = now;
        let mut attempt = 0usize;
        loop {
            match self.transport.refresh(&refresh, &self.device_id) {
                Ok(tokens) => {
                    // Written before use (§6.3). If this fails the pair is dropped; the old
                    // token is still in the store, and asking again within the window gets the
                    // same pair back from the server.
                    self.store.save(&tokens.refresh_token).map_err(|_| AccessError::StoreFailed)?;
                    let token = tokens.access_token.clone();
                    *cache = Some(CachedAccess { token: tokens.access_token, expires_at: tokens.access_expires_at });
                    return Ok(token);
                }
                Err(TransportError::Unauthorized) => {
                    let _ = self.store.clear();
                    return Err(AccessError::Revoked);
                }
                Err(TransportError::Retryable) => {
                    let Some(delay) = REFRESH_RETRY_DELAYS_SECS.get(attempt).copied() else {
                        return Err(AccessError::Unavailable);
                    };
                    // Only inside the window the server keeps the retry open for.
                    let elapsed = (self.clock)().saturating_sub(started);
                    if elapsed + delay + REFRESH_TIMEOUT_SECS > LOST_RESPONSE_WINDOW_SECS {
                        return Err(AccessError::Unavailable);
                    }
                    attempt += 1;
                    (self.sleep)(Duration::from_secs(delay));
                }
                Err(TransportError::Refused | TransportError::Unrecorded) => {
                    return Err(AccessError::Unavailable)
                }
            }
        }
    }

    /// A call made with the cached access token got 401: drop it, so the next call refreshes
    /// (and finds out whether the session itself is gone).
    pub fn invalidate_access(&self) {
        *self.cache.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// The cached access token, if any, without refreshing — for a sign-out that must not rotate
    /// the token it is about to revoke.
    pub fn cached_access(&self) -> Option<String> {
        let cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        let now = (self.clock)();
        cache.as_ref().filter(|c| c.expires_at > now).map(|c| c.token.clone())
    }

    /// Sign out locally: clear the store and the cache, and hand back the refresh token so the
    /// caller can revoke it on the server now or at the next connection (§7.4).
    pub fn forget(&self) -> Result<Option<String>, StoreError> {
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        *cache = None;
        let token = self.store.load().ok().flatten();
        self.store.clear()?;
        Ok(token)
    }
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// Refusals and sign-out (§7.2 / §7.4)
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// What a licence verification through the account came to, as the wire reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountVerifyAnswer {
    /// Entitled, with a token (still to be verified against this Mac before it is saved).
    Entitled,
    /// Answered, not entitled, with the server's reason (`seat_limit`, `no_license`, none for a
    /// lapsed subscription).
    NotEntitled(Option<String>),
    /// 401: the session is gone.
    Unauthorized,
    /// No answer.
    Transient,
}

/// What the app does with that answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountVerifyAction {
    /// Save the new licence token.
    Save,
    /// The account has no licence, but this Mac holds a licence key: verify with the key instead
    /// (D4 — licence-key use keeps working).
    UseLicenceKey,
    /// Authoritative: no plan through the account. Drop the cached licence token now.
    Lock,
    /// Every seat is taken by other devices: drop the cached token and show the device list so
    /// the user can sign one out (§7.2).
    SeatLimit,
    /// The session was removed or revoked: sign out locally — the cached token goes, the Mac
    /// locks, memory stays (§7.2).
    SignedOutRemotely,
    /// Could not ask: keep everything; the offline grace applies (FR-BIL-09).
    KeepCached,
}

pub fn decide_account_verify(answer: &AccountVerifyAnswer, has_licence_key: bool) -> AccountVerifyAction {
    match answer {
        AccountVerifyAnswer::Entitled => AccountVerifyAction::Save,
        AccountVerifyAnswer::NotEntitled(reason) => match reason.as_deref() {
            Some("seat_limit") => AccountVerifyAction::SeatLimit,
            Some("no_license") if has_licence_key => AccountVerifyAction::UseLicenceKey,
            _ => AccountVerifyAction::Lock,
        },
        AccountVerifyAnswer::Unauthorized => AccountVerifyAction::SignedOutRemotely,
        AccountVerifyAnswer::Transient => AccountVerifyAction::KeepCached,
    }
}

/// A trial answer's reason, as the app acts on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrialRefusal {
    /// The device-only path no longer starts trials (D7c): send the user to sign in.
    AccountRequired,
    /// This Mac's trial went to another account (§7.3).
    TrialUsed,
    /// The licence-key path refused this Mac: it was removed from the licence's account; a
    /// sign-in on this Mac brings it back, never the key alone (§7.2).
    DeviceRemoved,
}

pub fn trial_refusal(reason: Option<&str>) -> Option<TrialRefusal> {
    match reason? {
        "account_required" => Some(TrialRefusal::AccountRequired),
        "trial_used" => Some(TrialRefusal::TrialUsed),
        "device_removed" => Some(TrialRefusal::DeviceRemoved),
        _ => None,
    }
}

/// How a server revocation at sign-out went (§7.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevokeOutcome {
    /// Revoked; the seat is free.
    Done,
    /// 401: the session was already gone (removed elsewhere, or revoked earlier).
    AlreadyGone,
    /// Offline, server failure, rate limited or ledger unavailable: not revoked yet.
    NotSent,
}

/// Keep the queued revocation for the next connection? Only when it has not been answered.
pub fn keep_pending_revoke(outcome: RevokeOutcome) -> bool {
    outcome == RevokeOutcome::NotSent
}

/// What a local sign-out removes (§7.4). Memory and settings are never in this list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignOutSteps {
    /// The Keychain item `account-session`. Always.
    pub delete_session: bool,
    /// `billing.json`'s licence token (the time mark stays). Always.
    pub delete_licence_token: bool,
    /// The trial relay token, and the trial marked over on this Mac until the next sign-in.
    pub end_trial_relay: bool,
    /// The licence key, when it belongs to the account (claimed while signed in). A key bought
    /// without an account stays: it is the licence's, not the account's (D4).
    pub delete_licence_key: bool,
}

pub fn sign_out_steps(licence_key_linked: bool) -> SignOutSteps {
    SignOutSteps {
        delete_session: true,
        delete_licence_token: true,
        end_trial_relay: true,
        delete_licence_key: licence_key_linked,
    }
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// account.json — what the read-only faces see (FR-ACC-07)
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// Why this Mac needs a sign-in, for the faces and the panel.
pub const NEEDS_SIGN_IN_REASONS: [&str; 4] =
    ["account_required", "device_removed", "session_revoked", "signed_out"];

/// The account as this Mac last saw it. Written by the desktop app only; read by MCP / REST /
/// CLI. Holds no token and no device id. The address is the user's own and is shown to the
/// user's own tools, like the plan.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct AccountSnapshot {
    pub signed_in: bool,
    pub email: Option<String>,
    pub email_confirmed: bool,
    pub pending_email: Option<String>,
    /// Sign-in methods besides passkeys (`google`).
    pub providers: Vec<String>,
    pub passkey_count: usize,
    pub devices: Vec<SnapshotDevice>,
    /// When the account was last read from the server, unix seconds.
    pub checked_at: Option<u64>,
    /// Why a sign-in is needed, when it is (one of [`NEEDS_SIGN_IN_REASONS`]).
    pub needs_sign_in: Option<String>,
    /// The licence key on this Mac was claimed while signed in to this account (§7.4: signing out
    /// removes it). Never shown on the faces.
    pub licence_key_linked: bool,
}

impl std::fmt::Debug for AccountSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountSnapshot")
            .field("signed_in", &self.signed_in)
            .field("devices", &self.devices.len())
            .field("needs_sign_in", &self.needs_sign_in)
            .finish_non_exhaustive()
    }
}

/// A device as the faces list it: no id.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SnapshotDevice {
    pub name: Option<String>,
    pub last_seen_at: Option<u64>,
    pub holds_seat: bool,
    pub signed_in: bool,
    pub this_device: bool,
}

impl AccountSnapshot {
    /// The snapshot of a signed-in account, from `GET /api/account`.
    pub fn from_view(view: &AccountView, checked_at: u64, licence_key_linked: bool) -> Self {
        Self {
            signed_in: true,
            email: view.account.email.clone(),
            email_confirmed: view.account.email_confirmed,
            pending_email: view.account.pending_email.clone(),
            providers: view.identities.iter().map(|i| i.provider.clone()).collect(),
            passkey_count: view.passkeys.len(),
            devices: view
                .devices
                .iter()
                .map(|d| SnapshotDevice {
                    name: d.name.clone(),
                    last_seen_at: d.last_seen_at,
                    holds_seat: d.holds_seat,
                    signed_in: d.signed_in,
                    this_device: d.this_device,
                })
                .collect(),
            checked_at: Some(checked_at),
            needs_sign_in: None,
            licence_key_linked,
        }
    }

    /// Signed out, with the reason a sign-in is needed (or none for a Mac that never signed in).
    pub fn signed_out(reason: Option<&str>) -> Self {
        Self {
            needs_sign_in: reason.filter(|r| NEEDS_SIGN_IN_REASONS.contains(r)).map(str::to_string),
            ..Self::default()
        }
    }
}

pub fn serialize_account_snapshot(s: &AccountSnapshot) -> String {
    json!({
        "version": 1,
        "signed_in": s.signed_in,
        "email": s.email,
        "email_confirmed": s.email_confirmed,
        "pending_email": s.pending_email,
        "providers": s.providers,
        "passkey_count": s.passkey_count,
        "devices": s.devices.iter().map(|d| json!({
            "name": d.name,
            "last_seen_at": d.last_seen_at,
            "holds_seat": d.holds_seat,
            "signed_in": d.signed_in,
            "this_device": d.this_device,
        })).collect::<Vec<_>>(),
        "checked_at": s.checked_at,
        "needs_sign_in": s.needs_sign_in,
        "licence_key_linked": s.licence_key_linked,
    })
    .to_string()
}

/// Parse `account.json`. Anything unreadable is `None` (the same as no file: never signed in).
pub fn parse_account_snapshot(text: &str) -> Option<AccountSnapshot> {
    let v: Value = serde_json::from_str(text).ok()?;
    if v.get("version")?.as_u64()? != 1 {
        return None;
    }
    let signed_in = v.get("signed_in")?.as_bool()?;
    Some(AccountSnapshot {
        signed_in,
        email: opt_text(&v, "email"),
        email_confirmed: v.get("email_confirmed").and_then(Value::as_bool).unwrap_or(false),
        pending_email: opt_text(&v, "pending_email"),
        providers: v
            .get("providers")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default(),
        passkey_count: v
            .get("passkey_count")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(0),
        devices: v
            .get("devices")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|d| SnapshotDevice {
                        name: opt_text(d, "name"),
                        last_seen_at: d.get("last_seen_at").and_then(Value::as_u64),
                        holds_seat: d.get("holds_seat").and_then(Value::as_bool).unwrap_or(false),
                        signed_in: d.get("signed_in").and_then(Value::as_bool).unwrap_or(false),
                        this_device: d.get("this_device").and_then(Value::as_bool).unwrap_or(false),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        checked_at: v.get("checked_at").and_then(Value::as_u64),
        needs_sign_in: opt_text(&v, "needs_sign_in").filter(|r| NEEDS_SIGN_IN_REASONS.contains(&r.as_str())),
        licence_key_linked: signed_in && v.get("licence_key_linked").and_then(Value::as_bool).unwrap_or(false),
    })
}

/// The read-only projection the MCP / REST / CLI faces return (`device.account.get`): signed in
/// or out, the address, sign-in methods, and devices without ids. The plan is added by the caller
/// from `plan_source`, the same resolution every other face uses.
pub fn account_status_json(snapshot: Option<&AccountSnapshot>) -> Value {
    let Some(s) = snapshot.filter(|s| s.signed_in) else {
        return json!({
            "signed_in": false,
            "needs_sign_in": snapshot.and_then(|s| s.needs_sign_in.clone()),
        });
    };
    json!({
        "signed_in": true,
        "email": s.email,
        "email_confirmed": s.email_confirmed,
        "pending_email": s.pending_email,
        "sign_in_methods": {
            "passkeys": s.passkey_count,
            "providers": s.providers,
        },
        "devices": s.devices.iter().map(|d| json!({
            "name": d.name,
            "last_seen_at": d.last_seen_at,
            "holds_seat": d.holds_seat,
            "signed_in": d.signed_in,
            "this_device": d.this_device,
        })).collect::<Vec<_>>(),
        "checked_at": s.checked_at,
    })
}

#[cfg(test)]
mod tests;
