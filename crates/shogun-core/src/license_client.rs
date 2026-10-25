//! The licence API client (issue #8 / FR-BIL-08), feature `net`.
//!
//! FR-TR-03 keeps the single raw HTTP client in shogun-core, so the desktop shell asks this
//! module rather than reaching for reqwest itself.
//!
//! What goes out, and nothing else: the licence key, an anonymous device id and the app version
//! (FR-BIL-08 — "検証リクエストにキャプチャ内容・メモリ内容を一切含めない"). No capture text, no
//! memory content, no email. What comes back is a signed licence token that
//! `shogun_license::verify` checks offline.
//!
//! Traceability: billing traffic is out of scope for the egress ledger
//! (docs/requirements-v1.0.md §7.7 table — it carries no capture content), but it is still
//! plain HTTPS with certificate verification left on, like every other egress here.

use std::time::Duration;

use crate::llm::traceability::{Route, TraceRecord, TraceabilitySink};

/// What the licence API answers. Field names mirror the JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyResponse {
    /// Whether the subscription entitles the app right now.
    pub entitled: bool,
    /// "standard" | "pro" | none when the price is unknown to the server.
    pub plan: Option<String>,
    /// Stripe subscription status ("active" / "trialing" / "past_due" / "canceled" / …).
    pub status: String,
    /// Subscription period end, unix seconds — the "next billing date" the UI shows.
    pub current_period_end: Option<i64>,
    pub cancel_at_period_end: bool,
    /// The signed token. `None` when the subscription is not entitled — the device then simply
    /// ages out of its cached token's grace window.
    pub token: Option<String>,
    /// Why there is no token, when the server says (`seat_limit`, `device_removed`, `no_license`).
    pub reason: Option<String>,
}

/// Why a verification did not produce an answer. The caller keeps using its cached token for
/// anything that is not [`VerifyError::NotFound`] — a network outage must not lock a paying Mac
/// (FR-BIL-09).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    /// The server does not know this licence key (or it was revoked). Terminal: stop retrying.
    NotFound,
    /// Rate limited or a server-side failure. Transient: retry on the next cycle.
    Server(String),
    /// Could not reach the API at all. Transient.
    Network(String),
    /// A 2xx whose body we could not read as a verification answer.
    BadResponse(String),
    /// 401 on the account path (#148 P2): the account session was removed or revoked. Not the
    /// licence's fault; the caller signs out locally (§7.2).
    Unauthorized,
    /// The traceability row could not be written, so nothing was sent (fail-closed, invariant 3).
    Unrecorded,
}

impl VerifyError {
    /// Is this a "the licence is gone" answer rather than "we could not ask"?
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::NotFound)
    }

    pub fn message(&self) -> String {
        match self {
            Self::NotFound => "licence not found".to_string(),
            Self::Server(m) => format!("licence server error: {m}"),
            Self::Network(m) => format!("licence server unreachable: {m}"),
            Self::BadResponse(m) => format!("unexpected licence response: {m}"),
            Self::Unauthorized => "account session refused".to_string(),
            Self::Unrecorded => "licence request not recorded; not sent".to_string(),
        }
    }
}

/// Default licence API origin. Overridable with `SHOGUN_LICENSE_API` for staging and dev.
///
/// This is the deployed site — the `shogunaios.com` custom domain of the website Worker
/// (`apps/website/wrangler.jsonc`), which is what actually serves `/api/license/*` and
/// `/api/stripe/*`. Release builds ignore `SHOGUN_LICENSE_API`, so this constant is the only
/// production value — keep it in step with the Worker's routes; a host that does not resolve
/// fails verification, Checkout, the Customer Portal and the licence claim alike.
pub const DEFAULT_LICENSE_API: &str = "https://shogunaios.com";

/// The verification endpoint for a given origin.
pub fn verify_url(origin: &str) -> String {
    format!("{}/api/license/verify", origin.trim_end_matches('/'))
}

/// POST the verification. Blocking, with a short timeout: this runs at launch and on a 24h timer,
/// and it must never be able to hold a startup path open.
pub fn verify(
    origin: &str,
    license_key: &str,
    device_id: &str,
    app_version: &str,
) -> Result<VerifyResponse, VerifyError> {
    post_verify(
        origin,
        None,
        &serde_json::json!({
            "license_key": license_key,
            "device_id": device_id,
            "app_version": app_version,
        }),
    )
}

/// The same verification through the account (#148 §6.3): `Authorization: Bearer <access token>`
/// instead of a licence key, same answer, same token, same offline grace. A traceability row is
/// written first and nothing is sent without it (fail-closed); the request carries the device id
/// and the app version only. A 401 is [`VerifyError::Unauthorized`]: the session is gone.
pub fn verify_with_account(
    origin: &str,
    sink: &dyn TraceabilitySink,
    access_token: &str,
    device_id: &str,
    app_version: &str,
) -> Result<VerifyResponse, VerifyError> {
    record_account_call(origin, sink, "license_verify_account")?;
    post_verify(
        origin,
        Some(access_token),
        &serde_json::json!({ "device_id": device_id, "app_version": app_version }),
    )
}

/// One content-free `Route::Billing` row for an account-authenticated licence call, or the error
/// that keeps it from being sent.
fn record_account_call(origin: &str, sink: &dyn TraceabilitySink, purpose: &str) -> Result<(), VerifyError> {
    let host = origin
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("")
        .to_string();
    sink.try_record(TraceRecord::for_chunk(Route::Billing, purpose, host, "", false))
        .map_err(|_| VerifyError::Unrecorded)
}

fn post_verify(
    origin: &str,
    bearer: Option<&str>,
    body: &serde_json::Value,
) -> Result<VerifyResponse, VerifyError> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("shogun/1.0")
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| VerifyError::Network(e.to_string()))?;

    let mut request = client.post(verify_url(origin)).json(body);
    if let Some(token) = bearer {
        request = request.bearer_auth(token);
    }
    let resp = request.send().map_err(|e| VerifyError::Network(e.to_string()))?;

    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(not_found_answer(&resp.text().unwrap_or_default()));
    }
    if bearer.is_some() && status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(VerifyError::Unauthorized);
    }
    if !status.is_success() {
        return Err(VerifyError::Server(status.as_u16().to_string()));
    }

    let body: serde_json::Value = resp
        .json()
        .map_err(|e| VerifyError::BadResponse(e.to_string()))?;
    parse_verify_response(&body)
}

/// Classify a 404 from the verify endpoint. Only the API's own `{ok:false,error:"not_found"}`
/// envelope means "this licence is gone"; any other 404 (a deploy that briefly dropped the route,
/// a repointed domain, a proxy page) says nothing about the licence and must not make every
/// paying Mac delete its key (issue #777).
pub fn not_found_answer(body: &str) -> VerifyError {
    let envelope = serde_json::from_str::<serde_json::Value>(body).ok();
    let is_api_not_found = envelope.as_ref().is_some_and(|v| {
        v.get("ok").and_then(serde_json::Value::as_bool) == Some(false)
            && v.get("error").and_then(serde_json::Value::as_str) == Some("not_found")
    });
    if is_api_not_found {
        VerifyError::NotFound
    } else {
        VerifyError::Server("404".to_string())
    }
}

/// What `POST /api/license/trial` answers (issue #763).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrialResponse {
    /// Whether this device's trial is still running on the server.
    pub entitled: bool,
    /// The trial relay token (plan `trial`). `None` once the trial is over.
    pub token: Option<String>,
    /// When `token` expires, unix seconds.
    pub expires_at: Option<u64>,
    /// The server's end of this device's trial, unix seconds. Carried whether
    /// or not the trial still runs: it is what makes the server's answer the local authority on
    /// when the trial is over.
    pub trial_ends_at: Option<u64>,
    /// Why there is no trial token, when the server says: `account_required` (the device-only
    /// path no longer starts trials, #148 D7c), `trial_used` (this Mac's trial went to another
    /// account, §7.3), `trial_device_limit` (the account's trial already reached its Mac cap) or
    /// `checkout_required` (FR-BIL-06: the trial starts at Checkout).
    pub reason: Option<String>,
}

pub fn trial_url(origin: &str) -> String {
    format!("{}/api/license/trial", origin.trim_end_matches('/'))
}

/// Ask for this device's trial relay token (issue #763), the Batch relay's bearer while the Mac
/// has no licence. Sends the anonymous device id and nothing else. Blocking, with the same short
/// timeout as [`verify`]: a Batch-lane caller runs its local lane when this fails.
pub fn request_trial_token(origin: &str, device_id: &str) -> Result<TrialResponse, VerifyError> {
    post_trial(origin, None, device_id)
}

/// The trial through the account (#148 §7.3): one per account and one per device, decided by the
/// server; a pre-account trial on this Mac is adopted. Fail-closed traceability first, like
/// [`verify_with_account`].
pub fn request_trial_token_with_account(
    origin: &str,
    sink: &dyn TraceabilitySink,
    access_token: &str,
    device_id: &str,
) -> Result<TrialResponse, VerifyError> {
    record_account_call(origin, sink, "license_trial_account")?;
    post_trial(origin, Some(access_token), device_id)
}

fn post_trial(origin: &str, bearer: Option<&str>, device_id: &str) -> Result<TrialResponse, VerifyError> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("shogun/1.0")
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| VerifyError::Network(e.to_string()))?;

    let mut request = client.post(trial_url(origin)).json(&serde_json::json!({ "device_id": device_id }));
    if let Some(token) = bearer {
        request = request.bearer_auth(token);
    }
    let resp = request.send().map_err(|e| VerifyError::Network(e.to_string()))?;

    let status = resp.status();
    if bearer.is_some() && status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(VerifyError::Unauthorized);
    }
    if status == reqwest::StatusCode::FORBIDDEN {
        // `account_required` is an answer, not an outage: no trial for this device without an
        // account. Anything else forbidden stays a server error.
        let body = resp.json::<serde_json::Value>().unwrap_or_default();
        return trial_refusal_answer(&body).ok_or_else(|| VerifyError::Server("403".to_string()));
    }
    if !status.is_success() {
        return Err(VerifyError::Server(status.as_u16().to_string()));
    }
    let body: serde_json::Value = resp
        .json()
        .map_err(|e| VerifyError::BadResponse(e.to_string()))?;
    parse_trial_response(&body)
}

/// A 403 from the trial endpoint that is the server's answer `account_required` (#148 D7c): no
/// token, no server end, the reason. `None` for any other 403.
pub fn trial_refusal_answer(body: &serde_json::Value) -> Option<TrialResponse> {
    (body.get("reason").and_then(serde_json::Value::as_str) == Some("account_required")).then(|| TrialResponse {
        entitled: false,
        token: None,
        expires_at: None,
        trial_ends_at: None,
        reason: Some("account_required".to_string()),
    })
}

/// The `reason` of a verification or trial answer, when it is one the app acts on.
fn answer_reason(body: &serde_json::Value) -> Option<String> {
    body.get("reason")
        .and_then(serde_json::Value::as_str)
        .filter(|r| ANSWER_REASONS.contains(r))
        .map(str::to_string)
}

/// Reasons a 200 `entitled: false` answer can carry (#148 §7.2 / §7.3; FR-BIL-06
/// `checkout_required`: the trial starts at Stripe Checkout with a card, so the app sends the
/// signed-in user there instead of waiting for a free trial token).
pub const ANSWER_REASONS: [&str; 7] = [
    "seat_limit",
    "device_removed",
    "no_license",
    "trial_used",
    "trial_device_limit",
    "account_required",
    "checkout_required",
];

/// Ask the backend for a Stripe Checkout URL for `plan` × `interval`.
///
/// The app never holds a Stripe Price ID — it names the plan and the server picks the price
/// (issue #8 セキュリティ). The returned URL is Stripe-hosted and opens in the system browser, so
/// no card data ever touches the app (FR-BIL-07).
///
/// `promo_code` is a launch code as typed (issue #580); the server validates it and answers
/// `Server("promo_invalid" | "promo_exhausted")` when it will not honour it.
pub fn checkout_url(
    origin: &str,
    plan: &str,
    interval: &str,
    claim_nonce: Option<&str>,
    promo_code: Option<&str>,
    redeem_only: bool,
) -> Result<CheckoutOpened, VerifyError> {
    let body = checkout_body(plan, interval, claim_nonce, promo_code, redeem_only);
    let answer = post_for_body(
        &format!("{}/api/stripe/checkout", origin.trim_end_matches('/')),
        &body,
        None,
    )?;
    parse_checkout_response(&answer)
}

/// [`checkout_url`] while signed in (#148 §7.1): the access token rides as the bearer, so the
/// server puts the account id in the Checkout metadata and the webhook links the purchase to the
/// account. Fail-closed traceability first. A 401 is [`VerifyError::Unauthorized`] (refresh, then
/// ask again) — never a purchase silently made without the account.
#[allow(clippy::too_many_arguments)] // the checkout body's fields plus the account bearer; a params struct adds nothing
pub fn checkout_url_with_account(
    origin: &str,
    sink: &dyn TraceabilitySink,
    access_token: &str,
    plan: &str,
    interval: &str,
    claim_nonce: Option<&str>,
    promo_code: Option<&str>,
    redeem_only: bool,
) -> Result<CheckoutOpened, VerifyError> {
    record_account_call(origin, sink, "stripe_checkout_account")?;
    let body = checkout_body(plan, interval, claim_nonce, promo_code, redeem_only);
    let answer = post_for_body(
        &format!("{}/api/stripe/checkout", origin.trim_end_matches('/')),
        &body,
        Some(access_token),
    )?;
    parse_checkout_response(&answer)
}

/// The paid renewal selected before the full Pro trial begins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TrialRenewalPlan { Standard, Pro }

impl TrialRenewalPlan {
    fn as_str(self) -> &'static str { match self { Self::Standard => "standard", Self::Pro => "pro" } }
}

/// The selected renewal period; the UI starts with monthly selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TrialRenewalInterval { Monthly, Annual }

impl TrialRenewalInterval {
    fn as_str(self) -> &'static str { match self { Self::Monthly => "monthly", Self::Annual => "annual" } }
}

/// Open only the authenticated account's seven-day, card-backed Pro trial and chosen renewal.
/// The response must attest the trial terms so an older server cannot silently sell a plan.
pub fn checkout_trial_with_account(
    origin: &str,
    sink: &dyn TraceabilitySink,
    access_token: &str,
    plan: TrialRenewalPlan,
    interval: TrialRenewalInterval,
) -> Result<String, VerifyError> {
    record_account_call(origin, sink, "stripe_trial_checkout_account")?;
    let answer = post_for_body(
        &format!("{}/api/stripe/checkout", origin.trim_end_matches('/')),
        &trial_checkout_body(plan, interval),
        Some(access_token),
    )?;
    parse_trial_checkout_response(&answer, plan, interval)
}

fn trial_checkout_body(plan: TrialRenewalPlan, interval: TrialRenewalInterval) -> serde_json::Value {
    // Account ownership supplies activation and stable server idempotency; no claim capability.
    serde_json::json!({ "plan": plan, "interval": interval, "trial_only": true })
}

fn parse_trial_checkout_response(body: &serde_json::Value, plan: TrialRenewalPlan, interval: TrialRenewalInterval) -> Result<String, VerifyError> {
    let raw = parse_url_response(body)?;
    let url = reqwest::Url::parse(&raw).map_err(|_| VerifyError::BadResponse("trial checkout url".into()))?;
    if body.get("trial_only").and_then(serde_json::Value::as_bool) != Some(true)
        || body.get("trial_days").and_then(serde_json::Value::as_u64) != Some(7)
        || body.get("amount_due_now").and_then(serde_json::Value::as_u64) != Some(0)
        || body.get("trial_plan").and_then(serde_json::Value::as_str) != Some("pro")
        || body.get("plan").and_then(serde_json::Value::as_str) != Some(plan.as_str())
        || body.get("interval").and_then(serde_json::Value::as_str) != Some(interval.as_str())
        || body.get("grant").is_some_and(|grant| !grant.is_null())
    {
        return Err(VerifyError::BadResponse("trial checkout terms".into()));
    }
    if url.scheme() != "https" || url.host_str() != Some("checkout.stripe.com")
        || !url.username().is_empty() || url.password().is_some() || url.port().is_some()
        || raw.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(VerifyError::BadResponse("trial checkout url".into()));
    }
    Ok(raw)
}

/// Fixed diagnostic class and IPC code. Never returns an unfiltered server reason or body.
pub fn trial_checkout_failure(error: &VerifyError) -> (&'static str, &'static str) {
    match error {
        VerifyError::Server(reason) => match reason.as_str() {
            "trial_used" => ("trial_used", "trial_used"),
            "trial_device_limit" => ("trial_device_limit", "trial_device_limit"),
            "account_required" => ("account_required", "account_required"),
            "trial_selection_locked" => ("trial_selection_locked", "trial_selection_locked"),
            "trial_checkout_unavailable" => ("trial_checkout_unavailable", "trial_checkout_unavailable"),
            "400" => ("http_bad_request", "trial_unavailable"),
            "403" => ("http_forbidden", "trial_unavailable"),
            "409" => ("http_conflict", "trial_unavailable"),
            "429" => ("http_rate_limited", "trial_unavailable"),
            "500" | "502" | "503" | "504" => ("http_server_error", "trial_unavailable"),
            _ => ("server_refused", "trial_unavailable"),
        },
        VerifyError::BadResponse(reason) if reason == "trial checkout terms" => ("trial_terms_unconfirmed", "trial_terms_unconfirmed"),
        VerifyError::BadResponse(reason) if reason == "trial checkout url" || reason == "no url" => ("checkout_url_invalid", "trial_unavailable"),
        VerifyError::BadResponse(_) => ("bad_response", "trial_unavailable"),
        VerifyError::Unrecorded => ("audit_unavailable", "trial_audit_unavailable"),
        VerifyError::Unauthorized => ("account_required", "account_required"),
        VerifyError::Network(_) => ("network_unavailable", "trial_unavailable"),
        VerifyError::NotFound => ("http_not_found", "trial_unavailable"),
    }
}

/// What opening Checkout came back with: the hosted URL, and what a typed code grants when the
/// server says (`grant`), so the app can name it instead of a generic "Code applied".
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CheckoutOpened {
    pub url: String,
    pub grant: Option<CheckoutGrant>,
}

/// A code's grant as the checkout route reports it: a licence for `days` (operations code, no
/// purchase) or `free_months` before the first charge (launch code at Checkout).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CheckoutGrant {
    pub plan: String,
    pub days: Option<u32>,
    pub free_months: Option<u32>,
}

/// Pure parser for the checkout answer. A missing or malformed `grant` is no grant: the URL is
/// what matters, and the app then says only "Code applied".
pub fn parse_checkout_response(body: &serde_json::Value) -> Result<CheckoutOpened, VerifyError> {
    let url = parse_url_response(body)?;
    let grant = body.get("grant").and_then(|g| {
        let plan = g.get("plan").and_then(serde_json::Value::as_str).filter(|p| matches!(*p, "standard" | "pro"))?;
        let count = |key: &str, max: u64| {
            g.get(key).and_then(serde_json::Value::as_u64).filter(|n| (1..=max).contains(n)).and_then(|n| u32::try_from(n).ok())
        };
        let (days, free_months) = (count("days", 3650), count("free_months", 24));
        (days.is_some() || free_months.is_some()).then(|| CheckoutGrant { plan: plan.to_string(), days, free_months })
    });
    Ok(CheckoutOpened { url, grant })
}

/// Why Checkout did not open: a short class for the log (never user data) and the line the buyer
/// reads. Every refusal the route can send has its own line; anything else is the generic retry.
pub fn checkout_failure(e: &VerifyError) -> (&'static str, &'static str) {
    match e {
        VerifyError::Server(r) => match r.as_str() {
            "promo_invalid" => ("promo_invalid", "That code isn't valid. Check it and try again, or clear it to continue without one."),
            "promo_exhausted" => ("promo_exhausted", "That code has been fully redeemed. Clear it to continue without one."),
            "promo_new_plans_only" => ("promo_new_plans_only", "That code is for new plans. Your current plan stays as it is."),
            "promo_monthly_only" => ("promo_monthly_only", "That code covers monthly plans only. Pick a monthly plan, or clear it to continue."),
            "429" | "rate_limited" => ("rate_limited", "Too many tries. Wait a minute, then try again."),
            "401" | "unauthorized" => ("unauthorized", "Your sign-in has expired. Sign in again, then try again."),
            "billing_not_configured" | "price_not_configured" | "price_mode_mismatch" | "no_checkout_url" => {
                ("checkout_unavailable", "Checkout isn't available right now. Try again later, or buy in Settings › Plan & billing.")
            }
            "400" | "bad_request" => ("bad_request", "Couldn't open checkout. Try again."),
            _ => ("server_error", "Couldn't open checkout. Try again."),
        },
        VerifyError::Unauthorized => ("unauthorized", "Your sign-in has expired. Sign in again, then try again."),
        VerifyError::Network(_) => ("network", "Couldn't reach checkout. Check your connection and try again."),
        VerifyError::Unrecorded => ("unrecorded", "Couldn't open checkout. Try again."),
        VerifyError::NotFound => ("not_found", "Couldn't open checkout. Try again."),
        VerifyError::BadResponse(_) => ("bad_response", "Couldn't open checkout. Try again."),
    }
}

/// The checkout request body. Pure, so the promo field is testable without a server.
///
/// `redeem_only` is the code field of a Mac that already has a plan: the server then redeems only
/// a code that grants a licence without a purchase and refuses everything else, so it can never
/// start a second subscription.
pub fn checkout_body(
    plan: &str,
    interval: &str,
    claim_nonce: Option<&str>,
    promo_code: Option<&str>,
    redeem_only: bool,
) -> serde_json::Value {
    let mut body = serde_json::json!({ "plan": plan, "interval": interval, "source": "app" });
    if let Some(nonce) = claim_nonce {
        body["claim_nonce"] = serde_json::Value::String(nonce.to_string());
    }
    if let Some(code) = promo_code.map(str::trim).filter(|c| !c.is_empty()) {
        body["promo_code"] = serde_json::Value::String(code.to_string());
    }
    if redeem_only {
        body["redeem_only"] = serde_json::Value::Bool(true);
    }
    body
}

/// Collect the licence this Mac just paid for, using the claim nonce it sent to Checkout.
///
/// `Ok(None)` means "not yet" — the webhook may not have landed, and the server deliberately
/// answers the same way for a nonce it does not recognise, so there is nothing more to learn from
/// a miss than "keep waiting". The caller polls until its own deadline and then falls back to
/// manual entry.
///
/// An `Err` is worth polling again even when the server had already redeemed the nonce: it
/// answers the same nonce with the same key for a short window after the first redemption, until
/// this Mac's first verification is recorded (issue #777). A lost response does not lose the key.
///
/// This is what keeps a Keychain-grade secret off the buyer's screen: without it the key has to
/// be printed on the success page and retyped into Settings.
pub fn claim_license(origin: &str, nonce: &str) -> Result<Option<String>, VerifyError> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("shogun/1.0")
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| VerifyError::Network(e.to_string()))?;
    let resp = client
        .post(format!("{}/api/license/claim", origin.trim_end_matches('/')))
        .json(&serde_json::json!({ "nonce": nonce }))
        .send()
        .map_err(|e| VerifyError::Network(e.to_string()))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(VerifyError::Server(status.as_u16().to_string()));
    }
    let body: serde_json::Value = resp
        .json()
        .map_err(|e| VerifyError::BadResponse(e.to_string()))?;
    parse_claim_response(&body)
}

/// Ask the backend for a Stripe Customer Portal URL for this licence.
pub fn portal_url(origin: &str, license_key: &str) -> Result<String, VerifyError> {
    post_for_url(
        &format!("{}/api/stripe/portal", origin.trim_end_matches('/')),
        &serde_json::json!({ "license_key": license_key }),
        None,
    )
}

/// Shared shape of the two "give me a hosted Stripe URL" calls.
fn post_for_url(url: &str, body: &serde_json::Value, bearer: Option<&str>) -> Result<String, VerifyError> {
    parse_url_response(&post_for_body(url, body, bearer)?)
}

/// POST and read a 2xx JSON body; a refusal is the allow-listed reason or the bare status.
fn post_for_body(url: &str, body: &serde_json::Value, bearer: Option<&str>) -> Result<serde_json::Value, VerifyError> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("shogun/1.0")
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| VerifyError::Network(e.to_string()))?;
    let mut request = client.post(url).json(body);
    if let Some(token) = bearer {
        request = request.bearer_auth(token);
    }
    let resp = request.send().map_err(|e| VerifyError::Network(e.to_string()))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(VerifyError::NotFound);
    }
    if bearer.is_some() && status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(VerifyError::Unauthorized);
    }
    if !status.is_success() {
        // A 400 can carry a reason the buyer can act on (a mistyped launch code); anything else
        // is the bare status.
        let reason = resp.json::<serde_json::Value>().ok().and_then(|b| error_reason(&b));
        return Err(VerifyError::Server(reason.unwrap_or_else(|| status.as_u16().to_string())));
    }
    resp.json().map_err(|e| VerifyError::BadResponse(e.to_string()))
}

/// The `reason` of a `{ ok:false, error, reason }` body, when it is one the app acts on.
///
/// Allow-listed rather than passed through: the string ends up in a user-facing message, and a
/// server reason like `price_mode_mismatch` means nothing to the buyer.
pub fn error_reason(body: &serde_json::Value) -> Option<String> {
    body.get("reason")
        .and_then(serde_json::Value::as_str)
        .filter(|r| PROMO_REASONS.contains(r))
        .map(str::to_string)
}

/// Checkout refusals the buyer gets a line for: the code they typed (issue #580), and the
/// route's own "Checkout is not set up" reasons, which get one honest line instead of a retry.
pub const PROMO_REASONS: [&str; 14] = [
    "promo_invalid",
    "promo_exhausted",
    "promo_monthly_only",
    "promo_new_plans_only",
    "billing_not_configured",
    "price_not_configured",
    "price_mode_mismatch",
    "no_checkout_url",
    "trial_used",
    "trial_device_limit",
    "account_required",
    "trial_unavailable",
    "trial_selection_locked",
    "trial_checkout_unavailable",
];

/// Pure parser for the `{ ok, url }` bodies.
pub fn parse_url_response(body: &serde_json::Value) -> Result<String, VerifyError> {
    if body.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        let err = body.get("error").and_then(serde_json::Value::as_str).unwrap_or("unknown");
        return Err(VerifyError::Server(err.to_string()));
    }
    body.get("url")
        .and_then(serde_json::Value::as_str)
        .filter(|u| !u.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| VerifyError::BadResponse("no url".to_string()))
}

/// Pure parser for the `{ ok, license_key }` / `{ ok, pending }` claim bodies.
///
/// A body carrying neither is treated as pending rather than as an error: the polling caller has
/// a deadline of its own, and turning an unexpected shape into a hard failure would drop a buyer
/// into manual entry over a field we simply did not recognise.
pub fn parse_claim_response(body: &serde_json::Value) -> Result<Option<String>, VerifyError> {
    if body.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        let err = body
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        return Err(VerifyError::Server(err.to_string()));
    }
    Ok(body
        .get("license_key")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_string))
}

/// Pure parser for the verification body — separated so it is testable without a server.
pub fn parse_verify_response(body: &serde_json::Value) -> Result<VerifyResponse, VerifyError> {
    if body.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        let err = body
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        return Err(VerifyError::Server(err.to_string()));
    }
    Ok(VerifyResponse {
        entitled: body
            .get("entitled")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        plan: body
            .get("plan")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        status: body
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        current_period_end: body.get("current_period_end").and_then(serde_json::Value::as_i64),
        cancel_at_period_end: body
            .get("cancel_at_period_end")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        token: body
            .get("token")
            .and_then(serde_json::Value::as_str)
            .filter(|t| !t.trim().is_empty())
            .map(str::to_string),
        reason: answer_reason(body),
    })
}

/// Pure parser for the trial body. A token only counts with `entitled: true` and an expiry, so
/// a half-formed answer never becomes a bearer.
pub fn parse_trial_response(body: &serde_json::Value) -> Result<TrialResponse, VerifyError> {
    if body.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        let err = body
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        return Err(VerifyError::Server(err.to_string()));
    }
    let entitled = body
        .get("entitled")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let expires_at = body.get("expires_at").and_then(serde_json::Value::as_u64);
    let token = body
        .get("token")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|t| entitled && expires_at.is_some() && !t.is_empty())
        .map(str::to_string);
    Ok(TrialResponse {
        entitled,
        expires_at: token.as_ref().and(expires_at),
        token,
        trial_ends_at: body.get("trial_ends_at").and_then(serde_json::Value::as_u64),
        reason: answer_reason(body),
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_the_api_not_found_envelope_is_terminal() {
        let gone = not_found_answer(r#"{"ok":false,"error":"not_found"}"#);
        assert_eq!(gone, VerifyError::NotFound);
        assert!(gone.is_terminal());
        for body in [
            "",
            "<!DOCTYPE html><title>404: This page could not be found.</title>",
            r#"{"ok":false,"error":"server_error"}"#,
            r#"{"error":"not_found"}"#,
            r#"{"ok":true,"error":"not_found"}"#,
        ] {
            let answer = not_found_answer(body);
            assert_eq!(answer, VerifyError::Server("404".to_string()), "{body}");
            assert!(!answer.is_terminal(), "{body}");
        }
    }

    use super::*;

    #[test]
    fn parses_a_trial_answer_and_ignores_a_token_it_should_not_use() {
        assert_eq!(
            trial_url("https://shogunaios.com/"),
            "https://shogunaios.com/api/license/trial"
        );
        let running = serde_json::json!({
            "ok": true, "entitled": true, "trial_ends_at": 1_760_600_000_i64,
            "token": "v1.body.sig", "expires_at": 1_760_100_000_u64,
        });
        assert_eq!(
            parse_trial_response(&running).unwrap(),
            TrialResponse {
                entitled: true,
                token: Some("v1.body.sig".to_string()),
                expires_at: Some(1_760_100_000),
                trial_ends_at: Some(1_760_600_000),
                reason: None,
            }
        );
        let ended = serde_json::json!({
            "ok": true, "entitled": false, "trial_ends_at": 1_760_600_000_i64,
            "token": null, "expires_at": null,
        });
        assert_eq!(
            parse_trial_response(&ended).unwrap(),
            TrialResponse {
                entitled: false,
                token: None,
                expires_at: None,
                trial_ends_at: Some(1_760_600_000),
                reason: None,
            }
        );
        // An answer without the server's end leaves it unknown rather than inventing one.
        let bare = serde_json::json!({ "ok": true, "entitled": true, "token": "v1.body.sig", "expires_at": 1 });
        assert_eq!(parse_trial_response(&bare).unwrap().trial_ends_at, None);
        // A token without an expiry, or with `entitled: false`, is not used.
        for body in [
            serde_json::json!({ "ok": true, "entitled": true, "token": "v1.body.sig" }),
            serde_json::json!({ "ok": true, "entitled": false, "token": "v1.body.sig", "expires_at": 1 }),
        ] {
            assert_eq!(parse_trial_response(&body).unwrap().token, None, "{body}");
        }
        assert_eq!(
            parse_trial_response(&serde_json::json!({ "ok": false, "error": "rate_limited" })),
            Err(VerifyError::Server("rate_limited".to_string()))
        );
    }

    #[test]
    fn builds_the_endpoint_without_doubling_slashes() {
        assert_eq!(verify_url("https://shogunaios.com/"), "https://shogunaios.com/api/license/verify");
        assert_eq!(verify_url("http://localhost:3000"), "http://localhost:3000/api/license/verify");
    }

    #[test]
    fn parses_an_entitled_answer() {
        let body = serde_json::json!({
            "ok": true, "entitled": true, "plan": "pro", "status": "active",
            "current_period_end": 1_800_000_000i64, "cancel_at_period_end": false,
            "token": "v1.aaa.bbb", "grace_days": 14,
        });
        let r = parse_verify_response(&body).expect("parse");
        assert!(r.entitled);
        assert_eq!(r.plan.as_deref(), Some("pro"));
        assert_eq!(r.token.as_deref(), Some("v1.aaa.bbb"));
    }

    #[test]
    fn a_lapsed_answer_carries_status_but_no_token() {
        let body = serde_json::json!({
            "ok": true, "entitled": false, "plan": "pro", "status": "canceled",
            "current_period_end": 1_700_000_000i64, "cancel_at_period_end": true, "token": null,
        });
        let r = parse_verify_response(&body).expect("parse");
        assert!(!r.entitled);
        assert_eq!(r.status, "canceled");
        assert_eq!(r.token, None);
    }

    #[test]
    fn an_error_body_is_an_error_not_a_default_answer() {
        let body = serde_json::json!({ "ok": false, "error": "rate_limited" });
        assert_eq!(
            parse_verify_response(&body),
            Err(VerifyError::Server("rate_limited".to_string()))
        );
    }

    #[test]
    fn parses_a_hosted_url_answer_and_rejects_an_empty_one() {
        assert_eq!(
            parse_url_response(&serde_json::json!({ "ok": true, "url": "https://checkout.stripe.com/x" })),
            Ok("https://checkout.stripe.com/x".to_string())
        );
        assert_eq!(
            parse_url_response(&serde_json::json!({ "ok": true, "url": "" })),
            Err(VerifyError::BadResponse("no url".to_string()))
        );
        assert_eq!(
            parse_url_response(&serde_json::json!({ "ok": false, "error": "billing_not_configured" })),
            Err(VerifyError::Server("billing_not_configured".to_string()))
        );
    }

    #[test]
    fn the_launch_code_rides_in_the_body_only_when_typed() {
        let with = checkout_body("pro", "annual", Some("n"), Some("  ShogunAI202609 "), false);
        assert_eq!(with["promo_code"], "ShogunAI202609");
        assert_eq!(with["claim_nonce"], "n");
        assert!(with.get("redeem_only").is_none());
        for none in [None, Some(""), Some("   ")] {
            assert!(checkout_body("pro", "annual", None, none, false).get("promo_code").is_none());
        }
        // The code field of a Mac that already has a plan says so, and only then.
        assert_eq!(checkout_body("pro", "monthly", Some("n"), Some("x"), true)["redeem_only"], true);
    }

    #[test]
    fn trial_checkout_requires_selected_renewal_and_full_pro_trial_proof() {
        for plan in [TrialRenewalPlan::Standard, TrialRenewalPlan::Pro] {
            for interval in [TrialRenewalInterval::Monthly, TrialRenewalInterval::Annual] {
                assert_eq!(trial_checkout_body(plan, interval), serde_json::json!({
                    "plan": plan.as_str(), "interval": interval.as_str(), "trial_only": true,
                }));
                let answer = serde_json::json!({ "ok": true, "url": "https://checkout.stripe.com/c/pay/session",
                    "trial_only": true, "trial_plan": "pro", "trial_days": 7, "amount_due_now": 0,
                    "plan": plan.as_str(), "interval": interval.as_str() });
                assert!(parse_trial_checkout_response(&answer, plan, interval).is_ok());
                for key in ["trial_only", "trial_plan", "trial_days", "amount_due_now", "plan", "interval"] {
                    let mut missing = answer.clone();
                    missing.as_object_mut().expect("object").remove(key);
                    let error = parse_trial_checkout_response(&missing, plan, interval).expect_err("missing trial proof");
                    assert_eq!(trial_checkout_failure(&error), ("trial_terms_unconfirmed", "trial_terms_unconfirmed"));
                }
                let other_plan = if plan == TrialRenewalPlan::Pro { "standard" } else { "pro" };
                let other_interval = if interval == TrialRenewalInterval::Annual { "monthly" } else { "annual" };
                for (key, value) in [
                    ("trial_only", serde_json::json!(false)),
                    ("trial_plan", serde_json::json!("standard")),
                    ("trial_days", serde_json::json!(0)),
                    ("amount_due_now", serde_json::json!(1188)),
                    ("plan", serde_json::json!(other_plan)),
                    ("interval", serde_json::json!(other_interval)),
                    ("url", serde_json::json!("https://checkout.stripe.com.attacker.example/pay")),
                    ("grant", serde_json::json!({ "plan": "pro", "days": 365 })),
                ] {
                    let mut changed = answer.clone();
                    changed[key] = value;
                    assert!(parse_trial_checkout_response(&changed, plan, interval).is_err(), "invalid {key}");
                }
            }
        }
    }

    #[test]
    fn trial_checkout_diagnostics_are_fixed_and_selection_lock_is_actionable() {
        assert_eq!(trial_checkout_failure(&VerifyError::Server("trial_selection_locked".into())),
            ("trial_selection_locked", "trial_selection_locked"));
        assert_eq!(trial_checkout_failure(&VerifyError::BadResponse("trial checkout url".into())),
            ("checkout_url_invalid", "trial_unavailable"));
        assert_eq!(trial_checkout_failure(&VerifyError::Network("private url and token".into())),
            ("network_unavailable", "trial_unavailable"));
        assert_eq!(trial_checkout_failure(&VerifyError::Server("private server reason".into())),
            ("server_refused", "trial_unavailable"));
        assert!(serde_json::from_str::<TrialRenewalPlan>("\"unsupported\"").is_err());
        assert!(serde_json::from_str::<TrialRenewalInterval>("\"weekly\"").is_err());
    }

    #[test]
    fn only_promo_refusals_surface_as_a_reason() {
        let body = |r: &str| serde_json::json!({ "ok": false, "error": "bad_request", "reason": r });
        assert_eq!(error_reason(&body("promo_invalid")), Some("promo_invalid".to_string()));
        assert_eq!(error_reason(&body("promo_exhausted")), Some("promo_exhausted".to_string()));
        assert_eq!(error_reason(&body("promo_new_plans_only")), Some("promo_new_plans_only".to_string()));
        assert_eq!(error_reason(&body("price_mode_mismatch")), Some("price_mode_mismatch".to_string()));
        assert_eq!(error_reason(&body("db_unavailable")), None);
        assert_eq!(error_reason(&serde_json::json!({ "ok": false })), None);
    }

    #[test]
    fn a_checkout_answer_carries_a_code_grant_only_when_well_formed() {
        let url = "https://checkout.stripe.com/x";
        let ops = parse_checkout_response(&serde_json::json!({ "ok": true, "url": url, "grant": { "plan": "pro", "days": 365 } }));
        assert_eq!(ops.map(|o| o.grant), Ok(Some(CheckoutGrant { plan: "pro".into(), days: Some(365), free_months: None })));
        let promo = parse_checkout_response(&serde_json::json!({ "ok": true, "url": url, "grant": { "plan": "standard", "free_months": 1 } }));
        assert_eq!(promo.map(|o| o.grant.and_then(|g| g.free_months)), Ok(Some(1)));
        for grant in [
            serde_json::json!(null),
            serde_json::json!({ "plan": "pro" }),
            serde_json::json!({ "plan": "gold", "days": 30 }),
            serde_json::json!({ "plan": "pro", "days": 0 }),
            serde_json::json!({ "plan": "pro", "free_months": 99 }),
        ] {
            let opened = parse_checkout_response(&serde_json::json!({ "ok": true, "url": url, "grant": grant.clone() }));
            assert_eq!(opened, Ok(CheckoutOpened { url: url.into(), grant: None }), "{grant}");
        }
        assert!(parse_checkout_response(&serde_json::json!({ "ok": false, "error": "server_error" })).is_err());
    }

    #[test]
    fn every_checkout_refusal_has_its_own_line_and_a_content_free_class() {
        let generic = "Couldn't open checkout. Try again.";
        let cases = [
            (VerifyError::Server("promo_invalid".into()), "promo_invalid"),
            (VerifyError::Server("promo_monthly_only".into()), "promo_monthly_only"),
            (VerifyError::Server("429".into()), "rate_limited"),
            (VerifyError::Server("price_mode_mismatch".into()), "checkout_unavailable"),
            (VerifyError::Unauthorized, "unauthorized"),
            (VerifyError::Network("dns: secret.example".into()), "network"),
        ];
        for (e, class) in cases {
            let (got, line) = checkout_failure(&e);
            assert_eq!(got, class);
            assert_ne!(line, generic, "{class} has its own line");
            assert!(!line.contains("secret.example"));
        }
        assert_eq!(checkout_failure(&VerifyError::Server("500".into())), ("server_error", generic));
        assert_eq!(checkout_failure(&VerifyError::Unrecorded).1, generic);
    }

    #[test]
    fn a_claim_answer_is_the_key_or_keep_waiting() {
        assert_eq!(
            parse_claim_response(&serde_json::json!({ "ok": true, "license_key": "shogun-AAAA" })),
            Ok(Some("shogun-AAAA".to_string()))
        );
        // Pending, unknown nonce, already redeemed and expired are one answer by design.
        assert_eq!(
            parse_claim_response(&serde_json::json!({ "ok": true, "pending": true })),
            Ok(None)
        );
        assert_eq!(
            parse_claim_response(&serde_json::json!({ "ok": true, "license_key": "   " })),
            Ok(None),
            "a blank key is not a key"
        );
        assert_eq!(
            parse_claim_response(&serde_json::json!({ "ok": true })),
            Ok(None),
            "an unrecognised shape keeps the caller waiting rather than failing the purchase"
        );
        assert_eq!(
            parse_claim_response(&serde_json::json!({ "ok": false, "error": "rate_limited" })),
            Err(VerifyError::Server("rate_limited".to_string()))
        );
    }

    #[test]
    fn account_reasons_ride_along_and_unknown_ones_do_not() {
        let seat = serde_json::json!({ "ok": true, "entitled": false, "reason": "seat_limit", "token": null, "status": "active" });
        assert_eq!(parse_verify_response(&seat).unwrap().reason.as_deref(), Some("seat_limit"));
        let removed = serde_json::json!({ "ok": true, "entitled": false, "reason": "device_removed", "status": "active" });
        assert_eq!(parse_verify_response(&removed).unwrap().reason.as_deref(), Some("device_removed"));
        let other = serde_json::json!({ "ok": true, "entitled": false, "reason": "db_unavailable", "status": "x" });
        assert_eq!(parse_verify_response(&other).unwrap().reason, None);
        let used = serde_json::json!({ "ok": true, "entitled": false, "reason": "trial_used", "token": null });
        assert_eq!(parse_trial_response(&used).unwrap().reason.as_deref(), Some("trial_used"));
        let capped = serde_json::json!({ "ok": true, "entitled": false, "reason": "trial_device_limit", "token": null });
        assert_eq!(parse_trial_response(&capped).unwrap().reason.as_deref(), Some("trial_device_limit"));
        // FR-BIL-06: the card-required funnel answers `checkout_required` instead of starting a
        // free trial; it is an answer the app acts on, not an outage.
        let checkout = serde_json::json!({
            "ok": true, "entitled": false, "reason": "checkout_required",
            "trial_ends_at": null, "token": null, "expires_at": null
        });
        let answer = parse_trial_response(&checkout).unwrap();
        assert_eq!(answer.reason.as_deref(), Some("checkout_required"));
        assert!(!answer.entitled);
        assert_eq!(answer.token, None);
        // The closed device-only path (#148 D7c) is an answer with a reason, not an outage.
        let closed = serde_json::json!({ "ok": false, "error": "forbidden", "reason": "account_required" });
        let answer = trial_refusal_answer(&closed).expect("an answer");
        assert!(!answer.entitled && answer.token.is_none() && answer.trial_ends_at.is_none());
        assert_eq!(answer.reason.as_deref(), Some("account_required"));
        assert_eq!(trial_refusal_answer(&serde_json::json!({ "ok": false, "error": "forbidden" })), None);
    }

    #[test]
    fn account_calls_are_not_sent_without_a_ledger_row() {
        let sink = crate::llm::traceability::RefusingSink::new();
        // Port 9 (discard) on loopback: a request that went out would fail differently.
        assert_eq!(
            verify_with_account("http://127.0.0.1:9", &sink, "sa1.t", "device-0001", "1.0"),
            Err(VerifyError::Unrecorded)
        );
        assert_eq!(
            request_trial_token_with_account("http://127.0.0.1:9", &sink, "sa1.t", "device-0001"),
            Err(VerifyError::Unrecorded)
        );
        assert_eq!(
            checkout_url_with_account("http://127.0.0.1:9", &sink, "sa1.t", "pro", "annual", None, None, false),
            Err(VerifyError::Unrecorded)
        );
        assert_eq!(
            checkout_trial_with_account("http://127.0.0.1:9", &sink, "sa1.t", TrialRenewalPlan::Pro, TrialRenewalInterval::Monthly),
            Err(VerifyError::Unrecorded)
        );
        assert_eq!(sink.refused(), 4);
        assert!(!VerifyError::Unrecorded.is_terminal());
        assert!(!VerifyError::Unauthorized.is_terminal(), "a refused session is not a gone licence");
        assert!(!VerifyError::Unauthorized.message().contains("sa1"));
    }

    #[test]
    fn only_not_found_is_terminal() {
        assert!(VerifyError::NotFound.is_terminal());
        assert!(!VerifyError::Network("offline".into()).is_terminal());
        assert!(!VerifyError::Server("500".into()).is_terminal());
    }
}
