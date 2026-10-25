//! Licence tokens — the device-side half of Stripe billing (issue #8, FR-BIL-08 / FR-BIL-09).
//!
//! The licence API (`apps/website/src/app/api/license/verify/route.ts`) answers a verification
//! with a compact, Ed25519-signed assertion:
//!
//! ```text
//! v1.<base64url(payload JSON)>.<base64url(signature)>
//! ```
//!
//! ```json
//! { "v":1, "lic":"<uuid>", "plan":"standard|pro", "status":"active",
//!   "device":"<anonymous device id>", "iat":1754800000, "exp":1754893600,
//!   "period_end":1786336000, "cancel_at_period_end":false, "grace_days":14 }
//! ```
//!
//! Why a signed token instead of "ask the server each time":
//!
//! - **Offline (FR-BIL-09)**: a Mac with no network keeps full access for 14 days from the last
//!   successful verification, amber from day 7. That is only safe if the cached answer cannot be
//!   forged, which is what the signature buys.
//! - **Device-bound**: the device id is inside the signed bytes, and the id a token is checked
//!   against is derived from the Mac's hardware UUID ([`hardware_device_id`], issue #777), not
//!   read from the file the token sits in. A token copied to a second Mac meets a different
//!   hardware id there and is rejected. One licence, one Mac at a time. [`DeviceBinding`] also
//!   covers tokens bound to the random id saved in `billing.json` beside them. Where the
//!   hardware exposes no usable UUID (some virtual machines) the check falls back to that saved
//!   id — a weaker binding, since a copied file carries it.
//! - **No secret on the device**: verification needs only the public key, embedded in the binary.
//!   The licence *key* (the bearer used to call the API) is the only secret, and it lives in the
//!   Keychain (NFR-SEC-01).
//!
//! This crate is **pure** apart from one read: `now_ms` is always a parameter, and nothing here
//! reads the clock, the filesystem or the network. The exception is [`hardware_device_id`], which
//! asks IOKit for the platform UUID once per process (macOS only; no subprocess, no Keychain).
//! Every verification entry point also takes the binding as a value ([`DeviceBinding`]), so tests
//! inject it. The effectful side lives in the desktop app
//! (`apps/desktop/src-tauri/src/billing.rs`) and in `shogun-core::license_client`.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest as _, Sha256};
use shogun_agents::entitlement::{BillingState, PaidPlan};

pub mod account;
mod device;

pub use device::{device_id_from_platform_uuid, hardware_device_id, HARDWARE_DEVICE_ID_LEN};

/// Offline grace budget: how long a cached token keeps working with no successful verification
/// (FR-BIL-09). Measured from the token's `iat`, not from the last launch.
pub const OFFLINE_GRACE_DAYS: u64 = 14;

/// When the status indicator turns amber during that grace window (FR-BIL-09: "7日目からアンバー").
pub const OFFLINE_AMBER_DAYS: u64 = 7;

/// Hard ceiling on the `grace_days` a token may claim. The server sets the real number; this only
/// stops a future (or tampered-but-correctly-signed-by-a-leaked-key) token asking for a decade.
pub const MAX_GRACE_DAYS: u64 = 30;

const MS_PER_DAY: u64 = 24 * 60 * 60 * 1000;

/// The Ed25519 public key of the licence API, base64 (raw 32 bytes), baked into release builds.
///
/// Not a secret: it only verifies. Its private half was generated on the owner's machine by
/// `scripts/gen-license-keypair.mjs` and lives solely in the licence API's environment, so it
/// never passed through this repository or any build.
///
/// **Rotation**: the build trusts this key and, when set, the *next* key
/// ([`EMBEDDED_PUBLIC_KEY_NEXT_B64`]). To rotate, publish the new key as `ed25519_next` in
/// `packages/config/license-public-key.json`, ship a build with it here, let that build reach
/// the fleet, then switch the API to signing with it (payload `kid` names the key). Only after
/// that does it become `ed25519` / `EMBEDDED_PUBLIC_KEY_B64` and the old one is dropped. A build
/// trusting only one key would push every installed Mac into its offline-grace window at the
/// moment the API switched.
pub const EMBEDDED_PUBLIC_KEY_B64: &str = "VeGhNqy8v8KfM1/flVxJIBzgDjbQ+U8g4NQBF9nDCr8=";

/// The *next* licence-API public key during a rotation, base64 (raw 32 bytes), or `None` while
/// no rotation is in progress. Kept equal to `ed25519_next` in
/// `packages/config/license-public-key.json` (pinned by a test): the relay and the app must
/// accept the same two keys or a rotation strands one of them.
pub const EMBEDDED_PUBLIC_KEY_NEXT_B64: Option<&str> = None;

/// The longest a token may be valid for (`exp - iat`), seconds. Tokens live about a day; the
/// ceiling is what a leaked signing key is worth at most — one two-day token per request, not a
/// decade. Shared with the relay (`apps/api/src/auth.ts`).
pub const MAX_TOKEN_LIFETIME_SECS: u64 = 48 * 60 * 60;

/// How far ahead of this Mac's clock a token's `iat` may sit and still count (seconds). Beyond
/// it the token is treated as stale: either the clock is badly wrong or the token was minted
/// with a future `iat` to stretch its `iat`-anchored grace window.
pub const MAX_IAT_AHEAD_SECS: u64 = 5 * 60;

/// Backwards clock drift tolerated before `now` is clamped to the high-water mark:
/// an NTP correction or a time-zone fix moves the clock by seconds to minutes, a cheat by days.
pub const CLOCK_SKEW_TOLERANCE_MS: u64 = 10 * 60 * 1000;

/// The set of licence-API public keys this build trusts: the current key and, during a
/// rotation, the next one. Built once from the embedded constants ([`trusted_keys`]) or, in
/// tests, from a signing key of the test's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustedKeys {
    pub current: [u8; 32],
    pub next: Option<[u8; 32]>,
}

impl TrustedKeys {
    /// Trust exactly one key (tests, and the single-key wrappers kept for older call sites).
    pub const fn single(current: [u8; 32]) -> Self {
        Self { current, next: None }
    }

    /// Decode the two embedded-style constants. `None` when the current key does not decode;
    /// a `next` that does not decode is ignored rather than failing the build's only key.
    pub fn from_b64(current: &str, next: Option<&str>) -> Option<Self> {
        Some(Self { current: decode_public_key(current)?, next: next.and_then(decode_public_key) })
    }

    /// Current first, then next.
    pub fn iter(&self) -> impl Iterator<Item = &[u8; 32]> {
        std::iter::once(&self.current).chain(self.next.as_ref())
    }

    /// The trusted key whose [`key_id`] is `kid`, if any.
    pub fn for_kid(&self, kid: &str) -> Option<&[u8; 32]> {
        self.iter().find(|key| key_id(key) == kid)
    }
}

/// A key's identifier as the payload `kid` names it: the first 16 hex characters of the SHA-256
/// of the raw 32-byte public key. Shared with the signer and the relay.
pub fn key_id(key: &[u8; 32]) -> String {
    let digest = Sha256::digest(key);
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// A moment to evaluate a token at, with the monotonic high-water mark.
///
/// `max_seen_ms` is the latest wall-clock reading the desktop has recorded in `billing.json`.
/// A `now_ms` more than [`CLOCK_SKEW_TOLERANCE_MS`] *before* it is a clock that was moved back,
/// and reads as the mark instead, so turning the clock back cannot revive an expired token or
/// stretch a trial. Smaller drift is honoured as the clock says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clock {
    pub now_ms: u64,
    pub max_seen_ms: Option<u64>,
}

impl Clock {
    /// A clock with no recorded mark: `now_ms` is taken as read.
    pub const fn at(now_ms: u64) -> Self {
        Self { now_ms, max_seen_ms: None }
    }

    pub const fn with_mark(now_ms: u64, max_seen_ms: Option<u64>) -> Self {
        Self { now_ms, max_seen_ms }
    }

    /// The effective instant: [`clamp_now`] of the two.
    pub fn now(&self) -> u64 {
        clamp_now(self.now_ms, self.max_seen_ms)
    }

    /// The mark to record after this reading: the later of the two.
    pub fn next_mark(&self) -> u64 {
        self.now_ms.max(self.max_seen_ms.unwrap_or(0))
    }

    /// The mark to record right after a licence verification succeeded with a token whose `iat`
    /// (unix **ms**) is `server_iat_ms`: [`mark_after_verification`] of this reading.
    pub fn next_mark_after_verification(&self, server_iat_ms: u64) -> u64 {
        mark_after_verification(self.max_seen_ms, self.now_ms, server_iat_ms)
    }
}

/// `now_ms`, unless it sits more than [`CLOCK_SKEW_TOLERANCE_MS`] before the recorded
/// high-water mark, in which case the mark. Pure.
pub fn clamp_now(now_ms: u64, max_seen_ms: Option<u64>) -> u64 {
    match max_seen_ms {
        Some(mark) if now_ms.saturating_add(CLOCK_SKEW_TOLERANCE_MS) < mark => mark,
        _ => now_ms,
    }
}

/// The high-water mark to record right after the licence API verified this Mac and issued a
/// token whose `iat` (unix **ms**) is `server_iat_ms`.
///
/// The ordinary rule ([`Clock::next_mark`]) only ever raises the mark, so a Mac whose clock once
/// jumped a year ahead would keep that reading as its mark for good: [`clamp_now`] would pin
/// `now` there and every token would read as `Stale` — the plan bricked until the file was
/// deleted. Server time is authoritative, and a token's `iat` is server time that only a
/// server-signed token can carry, so a successful round-trip re-anchors the mark: the ordinary
/// mark, but never more than [`CLOCK_SKEW_TOLERANCE_MS`] past `server_iat_ms`. Lowering the mark
/// therefore needs a token the licence API signed just now; a clock rolled back with no
/// verification still meets the never-lower rule and [`clamp_now`]. When the mark was already at
/// or below that bound (the normal case) the result is the ordinary mark, unchanged. Pure.
pub fn mark_after_verification(existing_mark: Option<u64>, now_ms: u64, server_iat_ms: u64) -> u64 {
    let ordinary = now_ms.max(existing_mark.unwrap_or(0));
    ordinary.min(server_iat_ms.saturating_add(CLOCK_SKEW_TOLERANCE_MS))
}

/// The latest `trial_until` a token may claim, relative to its `iat`, seconds: the server
/// computes trial start + 7 days, and the trial cannot have started after the token was issued,
/// so a claim further out than 7 days plus the clock tolerance is malformed and is ignored.
pub const MAX_TRIAL_UNTIL_AFTER_IAT_SECS: u64 =
    shogun_agents::entitlement::TRIAL_DURATION_MS / 1000 + CLOCK_SKEW_TOLERANCE_MS / 1000;

/// Why a token was refused. Deliberately coarse: the UI says "couldn't verify", and a caller
/// must never branch into "grant access" on any of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LicenseError {
    /// Not three dot-separated parts, or a part that is not base64url.
    Malformed,
    /// A version prefix this build does not know. Treated as no licence, never as access.
    UnsupportedVersion,
    /// The signature does not verify against the public key — tampered, or signed by someone else.
    BadSignature,
    /// A valid token, but issued for a different device.
    DeviceMismatch,
    /// Signature verified but the payload is not a licence payload we understand.
    BadPayload,
    /// No public key is configured in this build (see [`EMBEDDED_PUBLIC_KEY_B64`]).
    NoPublicKey,
    /// The payload names a `kid` that is not one of this build's trusted keys.
    UnknownKeyId,
    /// `exp - iat` exceeds [`MAX_TOKEN_LIFETIME_SECS`] (or `exp` precedes `iat`).
    LifetimeTooLong,
}

impl LicenseError {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::UnsupportedVersion => "unsupported_version",
            Self::BadSignature => "bad_signature",
            Self::DeviceMismatch => "device_mismatch",
            Self::BadPayload => "bad_payload",
            Self::NoPublicKey => "no_public_key",
            Self::UnknownKeyId => "unknown_key_id",
            Self::LifetimeTooLong => "lifetime_too_long",
        }
    }
}

/// How current a verified token is, at a given moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Verified recently enough that the token has not expired. Normal steady state.
    Fresh,
    /// Past `exp` but inside the offline grace window. Full access, `days_offline` for the
    /// Settings offline-grace line (amber from [`OFFLINE_AMBER_DAYS`]).
    Grace { days_offline: u64 },
    /// Past the grace window. No access from this token.
    Stale,
}

impl Freshness {
    /// Does this state still entitle the app?
    pub fn is_valid(self) -> bool {
        !matches!(self, Self::Stale)
    }

    /// Should the Settings offline-grace line go amber (degraded-but-working)? Only that line —
    /// the Notch indicator is not wired to it.
    pub fn is_amber(self) -> bool {
        matches!(self, Self::Grace { days_offline } if days_offline >= OFFLINE_AMBER_DAYS)
    }
}

/// A signature-verified, device-matched licence token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LicenseToken {
    /// Licence id (not the licence key — the key never leaves the Keychain).
    pub license_id: String,
    pub plan: PaidPlan,
    /// The Stripe subscription status at issuance, verbatim ("active", "trialing", "past_due").
    pub status: String,
    /// The device this token is bound to.
    pub device_id: String,
    /// Issued-at / expires-at, unix **seconds** (as the API sends them).
    pub issued_at: u64,
    pub expires_at: u64,
    /// Subscription period end, unix seconds — the "next billing date" the UI shows.
    pub period_end: Option<i64>,
    /// The subscription stops at `period_end` (cancelled from the portal, still paid until then).
    pub cancel_at_period_end: bool,
    /// Offline grace this token grants, already clamped to [`MAX_GRACE_DAYS`].
    pub grace_days: u64,
    /// `trial_until` claim, unix seconds: the server-known end of the trial this device had,
    /// when it had one. The only thing that lets a Standard licence keep the trial's
    /// Pro-equivalent capabilities until then — the local stamp never does.
    pub trial_until: Option<u64>,
}

/// What a verified licence contributes to plan resolution
/// (`shogun_agents::entitlement::PlanInputs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LicenceFacts {
    pub state: BillingState,
    /// `trial_until` in unix **ms**, while `state` is `Active`.
    pub trial_until_ms: Option<u64>,
}

impl LicenseToken {
    /// The instant this token stops entitling anything, in unix ms.
    pub fn grace_deadline_ms(&self) -> u64 {
        if self.grace_days == 0 { return self.expires_at.saturating_mul(1000); }
        self.issued_at
            .saturating_mul(1000)
            .saturating_add(self.grace_days.saturating_mul(MS_PER_DAY))
    }

    /// Classify the token at `now_ms` with no high-water mark: [`LicenseToken::freshness_at`]
    /// of [`Clock::at`].
    pub fn freshness(&self, now_ms: u64) -> Freshness {
        self.freshness_at(Clock::at(now_ms))
    }

    /// Classify the token at `clock`.
    ///
    /// `now` is the clamped reading ([`Clock::now`]). A clock a few minutes behind issuance
    /// reads as `Fresh`: refusing there would lock out a paying user over an NTP hiccup. Beyond
    /// [`MAX_IAT_AHEAD_SECS`] the token is `Stale` — a future `iat` is how a token would stretch
    /// its `iat`-anchored grace window — and so is any token whose grace deadline the recorded
    /// mark has already passed, whatever the clock says now.
    pub fn freshness_at(&self, clock: Clock) -> Freshness {
        let now_ms = clock.now();
        let iat_ms = self.issued_at.saturating_mul(1000);
        if iat_ms > now_ms.saturating_add(MAX_IAT_AHEAD_SECS.saturating_mul(1000)) {
            return Freshness::Stale;
        }
        let exp_ms = self.expires_at.saturating_mul(1000);
        if self.grace_days == 0 {
            // A no-grace trial is usable before signed exp, never at/after it or a past-exp mark.
            return if now_ms < exp_ms && !clock.max_seen_ms.is_some_and(|mark| mark >= exp_ms) {
                Freshness::Fresh
            } else {
                Freshness::Stale
            };
        }
        if clock.max_seen_ms.is_some_and(|mark| mark >= self.grace_deadline_ms()) {
            return Freshness::Stale;
        }
        if now_ms <= exp_ms {
            return Freshness::Fresh;
        }
        if now_ms < self.grace_deadline_ms() {
            // Anchored on `iat`, like the deadline: FR-BIL-09 measures both the 14-day window
            // and the day-7 amber from the last obtained valid token. Anchoring this on `exp`
            // made amber arrive one token-lifetime late (a ~24h token: green through day 8,
            // then only a 6-day warning before the cutoff).
            let offline_ms = now_ms.saturating_sub(self.issued_at.saturating_mul(1000));
            return Freshness::Grace { days_offline: offline_ms / MS_PER_DAY };
        }
        Freshness::Stale
    }

    /// The billing state this token asserts at `now_ms` — the value
    /// `shogun_agents::entitlement::resolve_plan` consumes.
    ///
    /// The API only issues a token while the subscription is entitled, so a token that is still
    /// inside its validity window means paid access. Past the window it becomes
    /// [`BillingState::Lapsed`], which falls back to the trial rules (i.e. locked for anyone whose
    /// 7 days are up) rather than to full access.
    pub fn billing_state(&self, now_ms: u64) -> BillingState {
        self.billing_state_at(Clock::at(now_ms))
    }

    /// [`LicenseToken::billing_state`] at a [`Clock`] carrying the high-water mark.
    pub fn billing_state_at(&self, clock: Clock) -> BillingState {
        if self.freshness_at(clock).is_valid() {
            BillingState::Active(self.plan)
        } else {
            BillingState::Lapsed
        }
    }

    /// `trial_until` in unix ms, when the claim is one the server could have made: at most
    /// [`MAX_TRIAL_UNTIL_AFTER_IAT_SECS`] past `iat`. A later one is malformed and reads as no
    /// claim — it would otherwise be a signed way to a trial longer than seven days.
    pub fn trial_until_ms(&self) -> Option<u64> {
        let secs = self.trial_until?;
        (secs <= self.issued_at.saturating_add(MAX_TRIAL_UNTIL_AFTER_IAT_SECS))
            .then(|| secs.saturating_mul(1000))
    }

    /// The billing state plus the trial claim, for `PlanInputs`. The claim is only passed on
    /// while the token still entitles the Mac.
    pub fn plan_facts(&self, clock: Clock) -> LicenceFacts {
        let state = self.billing_state_at(clock);
        let trial_until_ms = match state {
            BillingState::Active(_) => self.trial_until_ms(),
            _ => None,
        };
        LicenceFacts { state, trial_until_ms }
    }
}

/// The keys this build verifies against: `SHOGUN_LICENSE_PUBKEY` (+ optional
/// `SHOGUN_LICENSE_PUBKEY_NEXT`; base64, raw 32 bytes) if set, else [`EMBEDDED_PUBLIC_KEY_B64`]
/// and [`EMBEDDED_PUBLIC_KEY_NEXT_B64`]. The env override is what dev, CI and a staging licence
/// API use; release builds carry the embedded constants.
pub fn trusted_keys() -> Option<TrustedKeys> {
    // Debug builds only: honouring the env in release would let anyone swap in their own
    // keypair and mint themselves a plan (the whole gate rests on these keys).
    #[cfg(debug_assertions)]
    if let Ok(v) = std::env::var("SHOGUN_LICENSE_PUBKEY") {
        if let Some(current) = decode_public_key(v.trim()) {
            let next = std::env::var("SHOGUN_LICENSE_PUBKEY_NEXT")
                .ok()
                .and_then(|n| decode_public_key(n.trim()));
            return Some(TrustedKeys { current, next });
        }
    }
    TrustedKeys::from_b64(EMBEDDED_PUBLIC_KEY_B64, EMBEDDED_PUBLIC_KEY_NEXT_B64)
}

/// The *current* key of [`trusted_keys`]. Kept for call sites that check one key; anything that
/// verifies a token the API may have signed during a rotation uses [`trusted_keys`].
pub fn public_key() -> Option<[u8; 32]> {
    trusted_keys().map(|keys| keys.current)
}

/// Decode a base64 (standard or url-safe, padded or not) 32-byte Ed25519 public key.
pub fn decode_public_key(b64: &str) -> Option<[u8; 32]> {
    let trimmed = b64.trim();
    if trimmed.is_empty() {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(trimmed)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(trimmed))
        .or_else(|_| URL_SAFE_NO_PAD.decode(trimmed))
        .ok()?;
    <[u8; 32]>::try_from(bytes.as_slice()).ok()
}

/// Verify a token's signature and device binding. Does **not** consider time — call
/// [`LicenseToken::freshness`] for that, so the caller decides what an expired-but-in-grace token
/// means for its own surface.
///
/// `device_id` must come from somewhere a copied `billing.json` cannot supply. A token cached on
/// this Mac is checked through [`DeviceBinding::verify`], which picks the hardware id; this raw
/// form is for a token just returned for an id this process sent itself.
///
/// Single-key form of [`verify_with`].
pub fn verify(
    token: &str,
    public_key: &[u8; 32],
    device_id: &str,
) -> Result<LicenseToken, LicenseError> {
    verify_with(token, &TrustedKeys::single(*public_key), device_id)
}

/// [`verify`] against every trusted key.
///
/// The signature is checked first, against each trusted key in turn, and only a body one of them
/// signed is parsed. A payload `kid` then has to name the very key that signed it: a `kid` no
/// trusted key has is [`LicenseError::UnknownKeyId`]; a `kid` naming a trusted key other than the
/// signer is [`LicenseError::BadSignature`]. Without `kid` any trusted key does. `exp - iat` is
/// capped at [`MAX_TOKEN_LIFETIME_SECS`].
pub fn verify_with(
    token: &str,
    keys: &TrustedKeys,
    device_id: &str,
) -> Result<LicenseToken, LicenseError> {
    let v = verified_payload(token, keys)?;
    let device = v.get("device").and_then(serde_json::Value::as_str).unwrap_or_default();
    if device.is_empty() || device != device_id {
        return Err(LicenseError::DeviceMismatch);
    }
    licence_from_payload(&v, device)
}

/// The payload of a token one of `keys` signed, after the signature, the version and the `kid`
/// checks of [`verify_with`]. Nothing in it is interpreted before the signature is proven.
fn verified_payload(token: &str, keys: &TrustedKeys) -> Result<serde_json::Value, LicenseError> {
    let mut parts = token.trim().split('.');
    let (version, body_b64, sig_b64) = match (parts.next(), parts.next(), parts.next(), parts.next())
    {
        (Some(v), Some(b), Some(s), None) => (v, b, s),
        _ => return Err(LicenseError::Malformed),
    };
    if version != "v1" {
        return Err(LicenseError::UnsupportedVersion);
    }

    let body = URL_SAFE_NO_PAD.decode(body_b64).map_err(|_| LicenseError::Malformed)?;
    let sig_bytes = URL_SAFE_NO_PAD.decode(sig_b64).map_err(|_| LicenseError::Malformed)?;
    let sig_arr = <[u8; 64]>::try_from(sig_bytes.as_slice()).map_err(|_| LicenseError::Malformed)?;

    let signature = Signature::from_bytes(&sig_arr);
    let mut signer: Option<&[u8; 32]> = None;
    let mut usable_key = false;
    for candidate in keys.iter() {
        let Ok(key) = VerifyingKey::from_bytes(candidate) else {
            continue;
        };
        usable_key = true;
        // `verify_strict` rejects small-order / non-canonical keys and signatures — the
        // malleability class of accepted-but-not-really-valid signatures.
        if key.verify_strict(&body, &signature).is_ok() {
            signer = Some(candidate);
            break;
        }
    }
    if !usable_key {
        return Err(LicenseError::NoPublicKey);
    }
    let signer = signer.ok_or(LicenseError::BadSignature)?;

    // Only now — after the bytes are proven to be ours — is it safe to interpret them.
    let v: serde_json::Value =
        serde_json::from_slice(&body).map_err(|_| LicenseError::BadPayload)?;
    if v.get("v").and_then(serde_json::Value::as_u64) != Some(1) {
        return Err(LicenseError::UnsupportedVersion);
    }
    if let Some(kid) = v.get("kid") {
        let kid = kid.as_str().ok_or(LicenseError::BadPayload)?;
        match keys.for_kid(kid) {
            None => return Err(LicenseError::UnknownKeyId),
            Some(named) if named != signer => return Err(LicenseError::BadSignature),
            Some(_) => {}
        }
    }
    Ok(v)
}

/// A verified payload read as a paid licence (`plan` standard / pro).
fn licence_from_payload(v: &serde_json::Value, device: &str) -> Result<LicenseToken, LicenseError> {
    let plan = match v.get("plan").and_then(serde_json::Value::as_str) {
        Some("pro") => PaidPlan::Pro,
        Some("standard") => PaidPlan::Standard,
        // An unknown plan name never becomes Pro by default — a future plan must ship with the
        // build that understands it.
        _ => return Err(LicenseError::BadPayload),
    };

    let issued_at = v.get("iat").and_then(serde_json::Value::as_u64).ok_or(LicenseError::BadPayload)?;
    let expires_at = v.get("exp").and_then(serde_json::Value::as_u64).ok_or(LicenseError::BadPayload)?;
    if expires_at < issued_at || expires_at - issued_at > MAX_TOKEN_LIFETIME_SECS {
        return Err(LicenseError::LifetimeTooLong);
    }

    Ok(LicenseToken {
        license_id: v.get("lic").and_then(serde_json::Value::as_str).unwrap_or_default().to_string(),
        plan,
        status: v
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("active")
            .to_string(),
        device_id: device.to_string(),
        issued_at,
        expires_at,
        period_end: v.get("period_end").and_then(serde_json::Value::as_i64),
        cancel_at_period_end: v
            .get("cancel_at_period_end")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        grace_days: v
            .get("grace_days")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(OFFLINE_GRACE_DAYS)
            .min(MAX_GRACE_DAYS),
        trial_until: v.get("trial_until").and_then(serde_json::Value::as_u64),
    })
}

/// A signature-verified, device-matched **trial** relay token (issue #763): plan `trial`, status
/// `trialing`. It never grants a plan ([`verify_with`] refuses it); what it carries into plan
/// resolution is `period_end`, the server's end of this device's trial, which only a token the
/// licence API signed can assert (audit 2026-10-20 M7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrialToken {
    pub issued_at: u64,
    pub expires_at: u64,
    /// The trial's end, unix seconds.
    pub period_end: u64,
}

impl TrialToken {
    /// `period_end` in unix ms.
    pub fn period_end_ms(&self) -> u64 {
        self.period_end.saturating_mul(1000)
    }
}

/// Verify a trial relay token's signature and device binding, and read its trial end. Refused
/// unless the payload is plan `trial`, status `trialing`, within [`MAX_TOKEN_LIFETIME_SECS`],
/// with `exp` at or before `period_end` and `period_end` no further than
/// [`MAX_TRIAL_UNTIL_AFTER_IAT_SECS`] past `iat` (a trial cannot end later than seven days after
/// a token issued inside it).
pub fn verify_trial_with(
    token: &str,
    keys: &TrustedKeys,
    device_id: &str,
) -> Result<TrialToken, LicenseError> {
    let v = verified_payload(token, keys)?;
    let device = v.get("device").and_then(serde_json::Value::as_str).unwrap_or_default();
    if device.is_empty() || device != device_id {
        return Err(LicenseError::DeviceMismatch);
    }
    if v.get("plan").and_then(serde_json::Value::as_str) != Some("trial")
        || v.get("status").and_then(serde_json::Value::as_str) != Some("trialing")
    {
        return Err(LicenseError::BadPayload);
    }
    let issued_at = v.get("iat").and_then(serde_json::Value::as_u64).ok_or(LicenseError::BadPayload)?;
    let expires_at = v.get("exp").and_then(serde_json::Value::as_u64).ok_or(LicenseError::BadPayload)?;
    if expires_at < issued_at || expires_at - issued_at > MAX_TOKEN_LIFETIME_SECS {
        return Err(LicenseError::LifetimeTooLong);
    }
    let period_end =
        v.get("period_end").and_then(serde_json::Value::as_u64).ok_or(LicenseError::BadPayload)?;
    if period_end < expires_at || period_end - issued_at > MAX_TRIAL_UNTIL_AFTER_IAT_SECS {
        return Err(LicenseError::BadPayload);
    }
    Ok(TrialToken { issued_at, expires_at, period_end })
}

/// Verify and immediately reduce to a billing state — the one-liner the plan sources want.
/// Any failure is [`BillingState::Unknown`] (fall back to the trial rules), never access.
pub fn billing_state_from_token(
    token: &str,
    public_key: &[u8; 32],
    device_id: &str,
    now_ms: u64,
) -> BillingState {
    match verify(token, public_key, device_id) {
        Ok(t) => t.billing_state(now_ms),
        Err(_) => BillingState::Unknown,
    }
}

/// Legacy tokens — bound to the random id a pre-#777 build saved in `billing.json` rather than to
/// the hardware — are accepted on a Mac with a hardware id only when issued (`iat`) before this
/// instant, unix seconds.
///
/// Why accept them at all: an update must not lock out a paying Mac that is offline when it first
/// launches the new build. Its cached token was minted by the old build, for the old id, and keeps
/// working through its grace window. The desktop re-verifies at launch, which replaces it with a
/// hardware-bound token at the first connection.
///
/// Why only before a cutoff: a copied `billing.json` is exactly such a token. Before the cutoff it
/// is accepted for at most its 14-day grace — it cannot be refreshed on the copy (the paying Mac
/// now mints hardware-bound tokens, and the copy has no licence key) — and a token an un-updated
/// old client mints after the cutoff is refused by every build that has this check.
///
/// Baked in at compile time by `build.rs`: the build time (or `SOURCE_DATE_EPOCH`) plus 14 days,
/// clamped to [2026-10-03, 2027-01-01] UTC. The margin covers tokens the old client mints between
/// the build and the user's update, so an offline first launch of the new build still passes. The
/// ceiling stops each later release from re-admitting legacy tokens issued before *its* build, and
/// ends all legacy acceptance by mid-January 2027 (ceiling + 14-day grace). The floor stops a
/// packager's pinned epoch from rejecting all of them.
pub const LEGACY_DEVICE_CUTOFF_SECS: u64 =
    include!(concat!(env!("OUT_DIR"), "/legacy_device_cutoff.rs"));

/// The build epoch `build.rs` derived the cutoff from — for the margin test only.
#[cfg(test)]
const LEGACY_DEVICE_BUILD_EPOCH_SECS: u64 =
    include!(concat!(env!("OUT_DIR"), "/legacy_device_build_epoch.rs"));

/// What a cached licence token must be bound to on this Mac (issue #777).
///
/// [`verify`] compares a token's `device` with whatever id it is handed. The question this answers
/// is *which* id: the hardware-derived one whenever the Mac has it, never the id recorded in
/// `billing.json` next to the token — that file is exactly what a copy carries along.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceBinding<'a> {
    /// This Mac's hardware-derived device id ([`hardware_device_id`]); `None` where the platform
    /// exposes no usable UUID, in which case the recorded id is all there is to check against.
    pub hardware: Option<&'a str>,
    /// See [`LEGACY_DEVICE_CUTOFF_SECS`]. Injected so tests can sit on either side of it.
    pub legacy_cutoff_secs: u64,
}

impl DeviceBinding<'static> {
    /// The binding in force in this process: the hardware id, the compiled-in cutoff.
    pub fn this_mac() -> Self {
        Self { hardware: hardware_device_id(), legacy_cutoff_secs: LEGACY_DEVICE_CUTOFF_SECS }
    }

    /// No hardware id: check against the recorded id. What
    /// [`DeviceBinding::this_mac`] amounts to off macOS or on a VM without a UUID; tests use it to
    /// stay platform-independent.
    pub const RECORDED_ONLY: Self = Self { hardware: None, legacy_cutoff_secs: 0 };
}

impl<'a> DeviceBinding<'a> {
    /// The device id this Mac presents to the licence API: the hardware id when there is one.
    /// `None` means "fall back to the id saved in `billing.json`, or mint one".
    pub fn request_id(&self) -> Option<&'a str> {
        self.hardware
    }

    /// Verify a cached token against this binding. `recorded_device_id` is the `device_id` saved
    /// beside the token in `billing.json`; it is trusted only where there is no hardware id, or
    /// for a legacy token — bound to a UUID-shaped (pre-#777 random) id — issued before
    /// [`DeviceBinding::legacy_cutoff_secs`].
    pub fn verify(
        &self,
        token: &str,
        keys: &TrustedKeys,
        recorded_device_id: &str,
    ) -> Result<LicenseToken, LicenseError> {
        let Some(hardware) = self.hardware else {
            return verify_with(token, keys, recorded_device_id);
        };
        match verify_with(token, keys, hardware) {
            // A legacy token is one bound to a pre-#777 random id (UUID-shaped). A token bound to
            // a hardware-derived id that is not this Mac's is another Mac's, whenever it was
            // issued — never a legacy case.
            Err(LicenseError::DeviceMismatch)
                if recorded_device_id != hardware && device::is_uuid_shaped(recorded_device_id) =>
            {
                let legacy = verify_with(token, keys, recorded_device_id)?;
                if legacy.issued_at < self.legacy_cutoff_secs {
                    Ok(legacy)
                } else {
                    Err(LicenseError::DeviceMismatch)
                }
            }
            other => other,
        }
    }

    /// Verify a trial relay token against this binding: the hardware id when there is one, else
    /// the id recorded beside it in `trial_relay.json`. Trial tokens have been requested with the
    /// hardware id since they existed (issue #763), so there is no legacy fallback.
    pub fn verify_trial(
        &self,
        token: &str,
        keys: &TrustedKeys,
        recorded_device_id: &str,
    ) -> Result<TrialToken, LicenseError> {
        verify_trial_with(token, keys, self.hardware.unwrap_or(recorded_device_id))
    }

    /// [`DeviceBinding::verify`] reduced to a billing state, like [`billing_state_from_token`]:
    /// any failure is [`BillingState::Unknown`], never access.
    pub fn billing_state(
        &self,
        token: &str,
        keys: &TrustedKeys,
        recorded_device_id: &str,
        clock: Clock,
    ) -> BillingState {
        self.plan_facts(token, keys, recorded_device_id, clock).state
    }

    /// [`DeviceBinding::verify`] reduced to [`LicenceFacts`]: any failure is
    /// [`BillingState::Unknown`] with no trial claim, never access.
    pub fn plan_facts(
        &self,
        token: &str,
        keys: &TrustedKeys,
        recorded_device_id: &str,
        clock: Clock,
    ) -> LicenceFacts {
        match self.verify(token, keys, recorded_device_id) {
            Ok(t) => t.plan_facts(clock),
            Err(_) => LicenceFacts::default(),
        }
    }
}

/// Start trying to re-verify this long before the cached token expires (issue #29 §4-1). Tokens
/// live about 24 hours from issuance, so a fixed 24-hour timer lands every device on the licence
/// API at the moment its token lapses; starting six hours early leaves room for a failed attempt
/// or two before expiry, and the offline grace (14 days) is untouched.
pub const REVERIFY_LEAD_SECS: u64 = 6 * 60 * 60;

/// Between attempts inside that lead window — three tries before the token expires.
pub const REVERIFY_RETRY_SECS: u64 = 2 * 60 * 60;

/// Between attempts once the token has expired and the device is running on its grace window.
/// Slower than inside the lead window: a device past `exp` has full access for days, and during a
/// licence-API outage a whole fleet sits here, so this is the rate that outage sees.
pub const REVERIFY_GRACE_RETRY_SECS: u64 = 6 * 60 * 60;

/// Never wait longer than this between attempts (FR-BIL-08: 起動時 + 24時間ごと stays the ceiling).
pub const REVERIFY_MAX_SECS: u64 = 24 * 60 * 60;

/// Seconds until the next re-verification should be attempted, given the cached token's expiry
/// (`None`: no token, or one this build cannot read — the plain 24-hour cadence).
///
/// Pure (`now_secs` is a parameter). The desktop loop calls this after every attempt, so a
/// successful one — a fresh token — pushes the next try out to ~18 hours, and a failed one keeps
/// the retry cadence until a new token arrives.
pub fn next_reverify_in_secs(now_secs: u64, expires_at: Option<u64>) -> u64 {
    let Some(exp) = expires_at else {
        return REVERIFY_MAX_SECS;
    };
    let start = exp.saturating_sub(REVERIFY_LEAD_SECS);
    if now_secs < start {
        (start - now_secs).min(REVERIFY_MAX_SECS)
    } else if now_secs < exp {
        REVERIFY_RETRY_SECS
    } else {
        REVERIFY_GRACE_RETRY_SECS
    }
}

/// [`next_reverify_in_secs`] for a cached token string: verified (signature + device) first, so
/// a tampered or foreign token cannot schedule anything but the plain 24-hour cadence.
pub fn next_reverify_for_token(
    token: &str,
    public_key: &[u8; 32],
    device_id: &str,
    now_secs: u64,
) -> u64 {
    let expires_at = verify(token, public_key, device_id).ok().map(|t| t.expires_at);
    next_reverify_in_secs(now_secs, expires_at)
}

/// How long to wait after the 1st, 2nd, 3rd … consecutive verification that could not reach an
/// answer (offline, licence-API 5xx, rate limited, timed out) — issue #777. Without it a failed
/// check waits for the next scheduled attempt, which with no token (a purchase whose first
/// verification failed) is a full day.
pub const REVERIFY_BACKOFF_SECS: [u64; 4] = [5 * 60, 15 * 60, 60 * 60, 2 * 60 * 60];

/// [`next_reverify_in_secs`] after `consecutive_failures` attempts in a row that got no
/// authoritative answer. Zero failures is the plain schedule. After a failure the wait is the
/// next step of [`REVERIFY_BACKOFF_SECS`] (never later than the schedule would try anyway), and
/// once the steps run out it is the schedule capped at [`REVERIFY_GRACE_RETRY_SECS`] — so a Mac
/// that keeps failing asks again every few hours, not once a day, and a fleet sitting through a
/// licence-API outage still converges on the grace-window rate.
pub fn next_reverify_after_failures(
    now_secs: u64,
    expires_at: Option<u64>,
    consecutive_failures: u32,
) -> u64 {
    let scheduled = next_reverify_in_secs(now_secs, expires_at);
    if consecutive_failures == 0 {
        return scheduled;
    }
    let ceiling = usize::try_from(consecutive_failures - 1)
        .ok()
        .and_then(|step| REVERIFY_BACKOFF_SECS.get(step).copied())
        .unwrap_or(REVERIFY_GRACE_RETRY_SECS);
    scheduled.min(ceiling)
}

/// [`next_reverify_after_failures`] for a cached token string, verified first like
/// [`next_reverify_for_token`].
pub fn next_reverify_for_token_after_failures(
    token: &str,
    public_key: &[u8; 32],
    device_id: &str,
    now_secs: u64,
    consecutive_failures: u32,
) -> u64 {
    let expires_at = verify(token, public_key, device_id).ok().map(|t| t.expires_at);
    next_reverify_after_failures(now_secs, expires_at, consecutive_failures)
}

/// Quick retries for the first verification of a licence this Mac has just claimed after a
/// purchase (issue #777), in seconds after the previous attempt. The buyer is usually looking at
/// Plan & billing, so a passing blip should not cost them the [`REVERIFY_BACKOFF_SECS`] minutes.
/// Past these the key stays in the Keychain and the verification loop keeps trying on its backoff.
pub const ACTIVATION_RETRY_SECS: [u64; 3] = [10, 30, 90];

/// Seconds to wait before activation retry number `attempt` (1-based: retry 1 follows the
/// immediate first try), or `None` once the quick retries are spent.
pub fn activation_retry_in_secs(attempt: u32) -> Option<u64> {
    let step = usize::try_from(attempt.checked_sub(1)?).ok()?;
    ACTIVATION_RETRY_SECS.get(step).copied()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    const DEVICE: &str = "device-abc123";

    /// A release build with no usable embedded key cannot verify anything, and the env override
    /// is compiled out of release — so every installed Mac would fall straight into its
    /// offline-grace window and then lock out, with nothing in the build to say why.
    #[test]
    fn the_embedded_public_key_is_a_real_ed25519_key() {
        let server_trust: serde_json::Value = serde_json::from_str(include_str!(
            "../../../packages/config/license-public-key.json"
        ))
        .expect("server trust root is valid JSON");
        assert_eq!(server_trust["ed25519"], EMBEDDED_PUBLIC_KEY_B64);
        let decoded = decode_public_key(EMBEDDED_PUBLIC_KEY_B64)
            .expect("EMBEDDED_PUBLIC_KEY_B64 must decode to 32 bytes");
        assert_eq!(decoded.len(), 32);
        // Decoding checks the length; this checks it is a point the verifier will accept, which
        // 32 arbitrary bytes are not guaranteed to be.
        ed25519_dalek::VerifyingKey::from_bytes(&decoded)
            .expect("EMBEDDED_PUBLIC_KEY_B64 must be a valid Ed25519 public key");
    }

    /// The pair to the above: a build that verifies against a key we do not hold the private half
    /// of grants nothing. The signature here is over the wrong key deliberately.
    #[test]
    fn the_embedded_key_rejects_a_token_signed_by_someone_else() {
        let embedded = decode_public_key(EMBEDDED_PUBLIC_KEY_B64).expect("decodes");
        let verifying = ed25519_dalek::VerifyingKey::from_bytes(&embedded).expect("valid key");
        let body = b"not a real payload";
        let sig = key().sign(body);
        assert!(
            verifying.verify_strict(body, &sig).is_err(),
            "a token signed by the test key must not verify against the production key"
        );
    }

    fn key() -> SigningKey {
        // A fixed seed: the tests must be deterministic, and this key signs nothing real.
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn mint(payload: serde_json::Value) -> String {
        let body = serde_json::to_vec(&payload).expect("serialize");
        let sig = key().sign(&body);
        format!(
            "v1.{}.{}",
            URL_SAFE_NO_PAD.encode(&body),
            URL_SAFE_NO_PAD.encode(sig.to_bytes())
        )
    }

    fn payload(plan: &str, iat: u64, exp: u64) -> serde_json::Value {
        serde_json::json!({
            "v": 1, "lic": "lic-1", "plan": plan, "status": "active", "device": DEVICE,
            "iat": iat, "exp": exp, "period_end": 1_800_000_000i64,
            "cancel_at_period_end": false, "grace_days": OFFLINE_GRACE_DAYS,
        })
    }

    fn pubkey() -> [u8; 32] {
        key().verifying_key().to_bytes()
    }

    fn keys() -> TrustedKeys {
        TrustedKeys::single(pubkey())
    }

    const HOUR: u64 = 60 * 60;

    #[test]
    fn reverification_starts_six_hours_before_expiry() {
        let iat = 1_000_000;
        let exp = iat + 24 * HOUR;
        // Just verified: wait until six hours before expiry (18h), not a flat 24h.
        assert_eq!(next_reverify_in_secs(iat, Some(exp)), 18 * HOUR);
        assert_eq!(next_reverify_in_secs(exp - 7 * HOUR, Some(exp)), HOUR);
        // Inside the lead window, a failed attempt retries every two hours: three tries before
        // the token lapses (at -6h, -4h, -2h).
        assert_eq!(next_reverify_in_secs(exp - 6 * HOUR, Some(exp)), REVERIFY_RETRY_SECS);
        assert_eq!(next_reverify_in_secs(exp - 1, Some(exp)), REVERIFY_RETRY_SECS);
        // Past expiry the grace window carries access; retry slower.
        assert_eq!(next_reverify_in_secs(exp, Some(exp)), REVERIFY_GRACE_RETRY_SECS);
        assert_eq!(next_reverify_in_secs(exp + 9 * 24 * HOUR, Some(exp)), REVERIFY_GRACE_RETRY_SECS);
        // No token (or an unreadable one): the plain 24-hour cadence.
        assert_eq!(next_reverify_in_secs(iat, None), REVERIFY_MAX_SECS);
        // A token claiming a far-off expiry never stretches the wait past 24h.
        assert_eq!(next_reverify_in_secs(iat, Some(iat + 30 * 24 * HOUR)), REVERIFY_MAX_SECS);
    }

    #[test]
    fn a_failed_check_retries_on_a_backoff_instead_of_waiting_for_the_schedule() {
        let iat = 1_000_000;
        let exp = iat + 24 * HOUR;
        // No failures: exactly the plain schedule.
        assert_eq!(next_reverify_after_failures(iat, Some(exp), 0), 18 * HOUR);
        assert_eq!(next_reverify_after_failures(iat, None, 0), REVERIFY_MAX_SECS);
        // A just-verified token whose next check keeps failing: 5m, 15m, 1h, 2h, then 6h.
        let waits: Vec<u64> =
            (1..=6).map(|n| next_reverify_after_failures(iat, Some(exp), n)).collect();
        assert_eq!(
            waits,
            vec![5 * 60, 15 * 60, HOUR, 2 * HOUR, REVERIFY_GRACE_RETRY_SECS, REVERIFY_GRACE_RETRY_SECS]
        );
        // No token at all (a purchase whose first verification failed): never a whole day.
        assert_eq!(next_reverify_after_failures(iat, None, 1), 5 * 60);
        assert_eq!(next_reverify_after_failures(iat, None, 40), REVERIFY_GRACE_RETRY_SECS);
        // Never later than the schedule: inside the lead window its 2h retry wins over the
        // exhausted-backoff ceiling.
        assert_eq!(next_reverify_after_failures(exp - HOUR, Some(exp), 9), REVERIFY_RETRY_SECS);
        // A failure count past the table stays at the ceiling.
        assert_eq!(
            next_reverify_after_failures(exp + HOUR, Some(exp), u32::MAX),
            REVERIFY_GRACE_RETRY_SECS
        );
    }

    #[test]
    fn the_after_failure_schedule_reads_expiry_only_from_a_verified_token() {
        let iat = 1_000_000;
        let token = mint(payload("standard", iat, iat + 24 * HOUR));
        assert_eq!(
            next_reverify_for_token_after_failures(&token, &pubkey(), DEVICE, iat, 0),
            18 * HOUR
        );
        assert_eq!(
            next_reverify_for_token_after_failures(&token, &pubkey(), DEVICE, iat, 2),
            15 * 60
        );
        assert_eq!(
            next_reverify_for_token_after_failures("v1.x.y", &pubkey(), DEVICE, iat, 0),
            REVERIFY_MAX_SECS
        );
    }

    #[test]
    fn a_claimed_licence_gets_three_quick_activation_retries() {
        assert_eq!(activation_retry_in_secs(0), None);
        assert_eq!(activation_retry_in_secs(1), Some(10));
        assert_eq!(activation_retry_in_secs(2), Some(30));
        assert_eq!(activation_retry_in_secs(3), Some(90));
        assert_eq!(activation_retry_in_secs(4), None);
        assert_eq!(activation_retry_in_secs(u32::MAX), None);
    }

    #[test]
    fn reverification_reads_expiry_only_from_a_verified_token() {
        let iat = 1_000_000;
        let token = mint(payload("standard", iat, iat + 24 * HOUR));
        assert_eq!(next_reverify_for_token(&token, &pubkey(), DEVICE, iat), 18 * HOUR);
        // Someone else's device, or a forged token: no schedule is derived from it.
        assert_eq!(
            next_reverify_for_token(&token, &pubkey(), "other-device", iat),
            REVERIFY_MAX_SECS
        );
        assert_eq!(next_reverify_for_token("v1.x.y", &pubkey(), DEVICE, iat), REVERIFY_MAX_SECS);
    }

    #[test]
    fn verifies_a_well_formed_token() {
        let t = verify(&mint(payload("pro", 1000, 2000)), &pubkey(), DEVICE).expect("verify");
        assert_eq!(t.plan, PaidPlan::Pro);
        assert_eq!(t.license_id, "lic-1");
        assert_eq!(t.period_end, Some(1_800_000_000));
        assert_eq!(t.grace_days, OFFLINE_GRACE_DAYS);
    }

    #[test]
    fn rejects_a_tampered_payload() {
        let token = mint(payload("standard", 1000, 2000));
        // Re-encode the payload as Pro, keep the original signature.
        let mut parts = token.split('.');
        let (_v, _b, sig) = (
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
        );
        let forged_body = serde_json::to_vec(&payload("pro", 1000, 2000)).expect("serialize");
        let forged = format!("v1.{}.{}", URL_SAFE_NO_PAD.encode(&forged_body), sig);
        assert_eq!(verify(&forged, &pubkey(), DEVICE), Err(LicenseError::BadSignature));
    }

    #[test]
    fn rejects_another_devices_token() {
        let token = mint(payload("pro", 1000, 2000));
        assert_eq!(verify(&token, &pubkey(), "someone-elses-mac"), Err(LicenseError::DeviceMismatch));
    }

    #[test]
    fn rejects_a_token_signed_by_a_different_key() {
        let token = mint(payload("pro", 1000, 2000));
        let other = SigningKey::from_bytes(&[9u8; 32]).verifying_key().to_bytes();
        assert_eq!(verify(&token, &other, DEVICE), Err(LicenseError::BadSignature));
    }

    #[test]
    fn rejects_malformed_and_unknown_versions() {
        let pk = pubkey();
        assert_eq!(verify("not-a-token", &pk, DEVICE), Err(LicenseError::Malformed));
        assert_eq!(verify("v1.a.b.c", &pk, DEVICE), Err(LicenseError::Malformed));
        let body = URL_SAFE_NO_PAD.encode(b"{}");
        assert_eq!(
            verify(&format!("v2.{body}.{body}"), &pk, DEVICE),
            Err(LicenseError::UnsupportedVersion)
        );
    }

    #[test]
    fn unknown_plan_is_never_silently_pro() {
        let token = mint(payload("enterprise", 1000, 2000));
        assert_eq!(verify(&token, &pubkey(), DEVICE), Err(LicenseError::BadPayload));
    }

    #[test]
    fn grace_window_boundaries() {
        // issued at t=0, expires after 1h, 14 days of grace from issuance
        let iat = 0;
        let exp = 3600;
        let t = verify(&mint(payload("pro", iat, exp)), &pubkey(), DEVICE).expect("verify");

        let exp_ms = exp * 1000;
        assert_eq!(t.freshness(exp_ms), Freshness::Fresh, "last ms before expiry is fresh");
        assert_eq!(t.freshness(exp_ms + 1), Freshness::Grace { days_offline: 0 });

        // day 7 offline → amber, still valid
        let day7 = exp_ms + 7 * MS_PER_DAY;
        assert!(t.freshness(day7).is_valid());
        assert!(t.freshness(day7).is_amber());

        // the deadline is measured from issuance: iat + 14d
        let deadline = t.grace_deadline_ms();
        assert_eq!(deadline, 14 * MS_PER_DAY);
        assert!(t.freshness(deadline - 1).is_valid());
        assert_eq!(t.freshness(deadline), Freshness::Stale);
        assert!(!t.freshness(deadline).is_valid());
    }

    #[test]
    fn amber_arrives_on_day_7_from_issuance_even_with_a_day_long_token() {
        // FR-BIL-09: amber from day 7, cutoff at day 14 — both measured from the token. With a
        // ~24h token, anchoring days_offline on exp instead of iat would push amber to day 8 and
        // shrink the warning window to 6 days.
        let iat = 0;
        let exp = 24 * 3600; // a one-day token
        let t = verify(&mint(payload("pro", iat, exp)), &pubkey(), DEVICE).expect("verify");
        let day7 = 7 * MS_PER_DAY;
        assert!(t.freshness(day7).is_amber(), "amber exactly at iat + 7d");
        assert!(!t.freshness(day7 - 1).is_amber(), "still green just before day 7");
        assert!(t.freshness(day7 - 1).is_valid() && t.freshness(day7).is_valid());
    }

    #[test]
    fn a_slightly_backwards_clock_does_not_lock_a_paying_user_out() {
        let t = verify(&mint(payload("pro", 10_000, 20_000)), &pubkey(), DEVICE).expect("verify");
        // Up to five minutes behind issuance: an NTP hiccup, still Fresh.
        assert_eq!(t.freshness((10_000 - MAX_IAT_AHEAD_SECS) * 1000), Freshness::Fresh);
        // Further back than that the token was minted in this Mac's future: not trusted.
        assert_eq!(t.freshness((10_000 - MAX_IAT_AHEAD_SECS) * 1000 - 1), Freshness::Stale);
        assert_eq!(t.freshness(0), Freshness::Stale);
        assert_eq!(t.billing_state(0), BillingState::Lapsed);
    }

    // Key rotation: two trusted keys, `kid`, lifetime ceiling

    fn next_key() -> SigningKey {
        SigningKey::from_bytes(&[8u8; 32])
    }

    fn both() -> TrustedKeys {
        TrustedKeys { current: pubkey(), next: Some(next_key().verifying_key().to_bytes()) }
    }

    fn mint_with(signing: &SigningKey, payload: serde_json::Value) -> String {
        let body = serde_json::to_vec(&payload).expect("serialize");
        let sig = signing.sign(&body);
        format!("v1.{}.{}", URL_SAFE_NO_PAD.encode(&body), URL_SAFE_NO_PAD.encode(sig.to_bytes()))
    }

    #[test]
    fn the_embedded_next_key_matches_the_server_trust_root() {
        let server_trust: serde_json::Value = serde_json::from_str(include_str!(
            "../../../packages/config/license-public-key.json"
        ))
        .expect("server trust root is valid JSON");
        match server_trust.get("ed25519_next").and_then(serde_json::Value::as_str) {
            Some(next) => {
                assert_eq!(EMBEDDED_PUBLIC_KEY_NEXT_B64, Some(next), "next key out of step with the JSON");
                let decoded = decode_public_key(next).expect("ed25519_next decodes to 32 bytes");
                ed25519_dalek::VerifyingKey::from_bytes(&decoded).expect("ed25519_next is a valid key");
                assert_ne!(decoded, decode_public_key(EMBEDDED_PUBLIC_KEY_B64).expect("current"));
            }
            None => assert_eq!(EMBEDDED_PUBLIC_KEY_NEXT_B64, None, "a next key the JSON does not publish"),
        }
        let keys = trusted_keys().expect("the build has keys");
        assert_eq!(Some(keys.current), decode_public_key(EMBEDDED_PUBLIC_KEY_B64));
        assert_eq!(keys.next, EMBEDDED_PUBLIC_KEY_NEXT_B64.and_then(decode_public_key));
        assert_eq!(public_key(), Some(keys.current));
    }

    #[test]
    fn key_ids_are_sixteen_hex_characters_of_the_sha256() {
        let kid = key_id(&pubkey());
        assert_eq!(kid.len(), 16);
        assert!(kid.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(kid, key_id(&next_key().verifying_key().to_bytes()));
        // Pinned so the signer and the relay can check the same derivation.
        let digest = Sha256::digest(pubkey());
        assert_eq!(kid, digest[..8].iter().map(|b| format!("{b:02x}")).collect::<String>());
        assert_eq!(both().for_kid(&kid), Some(&pubkey()));
        assert_eq!(both().for_kid("0000000000000000"), None);
    }

    #[test]
    fn a_token_signed_by_the_next_key_verifies_during_a_rotation() {
        let token = mint_with(&next_key(), payload("pro", 1000, 2000));
        // Without `kid`: either trusted key will do.
        assert!(verify_with(&token, &both(), DEVICE).is_ok());
        // A build that does not yet trust the next key refuses it, as it must.
        assert_eq!(verify_with(&token, &keys(), DEVICE), Err(LicenseError::BadSignature));
        // The current key keeps working alongside.
        assert!(verify_with(&mint(payload("pro", 1000, 2000)), &both(), DEVICE).is_ok());
        // The hardware binding goes through the same two keys.
        let mut p = payload("pro", CUTOFF + 10 * HOUR, CUTOFF + 36 * HOUR);
        p["device"] = serde_json::json!(HW_A);
        assert!(on(HW_A).verify(&mint_with(&next_key(), p), &both(), HW_A).is_ok());
    }

    #[test]
    fn kid_routes_to_exactly_the_named_key() {
        let current_kid = key_id(&pubkey());
        let next_kid = key_id(&next_key().verifying_key().to_bytes());
        let with_kid = |kid: &str| {
            let mut p = payload("pro", 1000, 2000);
            p["kid"] = serde_json::json!(kid);
            p
        };
        // Signed by the key the kid names: fine, for either key.
        assert!(verify_with(&mint(with_kid(&current_kid)), &both(), DEVICE).is_ok());
        assert!(verify_with(&mint_with(&next_key(), with_kid(&next_kid)), &both(), DEVICE).is_ok());
        // Signed by a trusted key but naming the other trusted key: refused.
        assert_eq!(
            verify_with(&mint(with_kid(&next_kid)), &both(), DEVICE),
            Err(LicenseError::BadSignature)
        );
        // Naming a key this build does not trust: refused, even though the signature is good.
        assert_eq!(
            verify_with(&mint(with_kid("ffffffffffffffff")), &both(), DEVICE),
            Err(LicenseError::UnknownKeyId)
        );
        // A build with only the current key refuses a kid for the (unknown to it) next key.
        assert_eq!(
            verify_with(&mint_with(&next_key(), with_kid(&next_kid)), &keys(), DEVICE),
            Err(LicenseError::BadSignature)
        );
        // A non-string kid is not a licence payload.
        let mut p = payload("pro", 1000, 2000);
        p["kid"] = serde_json::json!(7);
        assert_eq!(verify_with(&mint(p), &both(), DEVICE), Err(LicenseError::BadPayload));
    }

    #[test]
    fn a_token_cannot_claim_more_than_two_days_of_validity() {
        let iat = 1_000_000;
        assert!(verify(&mint(payload("pro", iat, iat + MAX_TOKEN_LIFETIME_SECS)), &pubkey(), DEVICE).is_ok());
        assert_eq!(
            verify(&mint(payload("pro", iat, iat + MAX_TOKEN_LIFETIME_SECS + 1)), &pubkey(), DEVICE),
            Err(LicenseError::LifetimeTooLong)
        );
        assert_eq!(
            verify(&mint(payload("pro", iat, iat + 10 * 365 * 24 * HOUR)), &pubkey(), DEVICE),
            Err(LicenseError::LifetimeTooLong)
        );
        assert_eq!(
            verify(&mint(payload("pro", iat, iat - 1)), &pubkey(), DEVICE),
            Err(LicenseError::LifetimeTooLong)
        );
        // …and the reduced forms stay fail-closed on it.
        let long = mint(payload("pro", iat, iat + 10 * 365 * 24 * HOUR));
        assert_eq!(billing_state_from_token(&long, &pubkey(), DEVICE, iat * 1000), BillingState::Unknown);
        assert_eq!(next_reverify_for_token(&long, &pubkey(), DEVICE, iat), REVERIFY_MAX_SECS);
    }

    #[test]
    fn the_trial_until_claim_is_carried_only_while_the_token_entitles() {
        let iat = 1_000_000;
        let plain = verify(&mint(payload("standard", iat, iat + HOUR)), &pubkey(), DEVICE).expect("verify");
        assert_eq!(plain.trial_until, None);
        assert_eq!(plain.plan_facts(Clock::at(iat * 1000)).trial_until_ms, None);

        let mut p = payload("standard", iat, iat + HOUR);
        p["trial_until"] = serde_json::json!(iat + 3 * 24 * HOUR);
        let t = verify(&mint(p), &pubkey(), DEVICE).expect("verify");
        assert_eq!(t.trial_until, Some(iat + 3 * 24 * HOUR));
        assert_eq!(
            t.plan_facts(Clock::at(iat * 1000)),
            LicenceFacts {
                state: BillingState::Active(PaidPlan::Standard),
                trial_until_ms: Some((iat + 3 * 24 * HOUR) * 1000),
            }
        );
        // Past the grace window the token is Lapsed and the claim goes with it.
        assert_eq!(
            t.plan_facts(Clock::at(t.grace_deadline_ms())),
            LicenceFacts { state: BillingState::Lapsed, trial_until_ms: None }
        );
        // The hardware-binding form carries it too, and a failed verification carries nothing.
        let mut p = payload("standard", CUTOFF + HOUR, CUTOFF + 2 * HOUR);
        p["device"] = serde_json::json!(HW_A);
        p["trial_until"] = serde_json::json!(CUTOFF + 48 * HOUR);
        let token = mint(p);
        let facts = on(HW_A).plan_facts(&token, &keys(), HW_A, Clock::at((CUTOFF + HOUR) * 1000));
        assert_eq!(facts.trial_until_ms, Some((CUTOFF + 48 * HOUR) * 1000));
        assert_eq!(on(HW_B).plan_facts(&token, &keys(), HW_A, Clock::at((CUTOFF + HOUR) * 1000)), LicenceFacts::default());
    }

    #[test]
    fn a_trial_until_further_out_than_the_trial_can_reach_is_ignored() {
        let iat = 1_000_000;
        let mut p = payload("standard", iat, iat + HOUR);
        // At the bound: honoured (a trial that began the instant the token was issued, plus skew).
        p["trial_until"] = serde_json::json!(iat + MAX_TRIAL_UNTIL_AFTER_IAT_SECS);
        let t = verify(&mint(p.clone()), &pubkey(), DEVICE).expect("verify");
        assert_eq!(t.trial_until_ms(), Some((iat + MAX_TRIAL_UNTIL_AFTER_IAT_SECS) * 1000));
        assert_eq!(
            t.plan_facts(Clock::at(iat * 1000)).trial_until_ms,
            Some((iat + MAX_TRIAL_UNTIL_AFTER_IAT_SECS) * 1000)
        );
        // One second past it: the signature still verifies, the claim is dropped, the plan is plain.
        p["trial_until"] = serde_json::json!(iat + MAX_TRIAL_UNTIL_AFTER_IAT_SECS + 1);
        let t = verify(&mint(p.clone()), &pubkey(), DEVICE).expect("verify");
        assert_eq!(t.trial_until, Some(iat + MAX_TRIAL_UNTIL_AFTER_IAT_SECS + 1), "the raw claim is kept");
        assert_eq!(t.trial_until_ms(), None);
        assert_eq!(
            t.plan_facts(Clock::at(iat * 1000)),
            LicenceFacts { state: BillingState::Active(PaidPlan::Standard), trial_until_ms: None }
        );
        p["trial_until"] = serde_json::json!(u64::MAX);
        let t = verify(&mint(p), &pubkey(), DEVICE).expect("verify");
        assert_eq!(t.trial_until_ms(), None);
        // The bound is 7 days plus the clock tolerance.
        assert_eq!(MAX_TRIAL_UNTIL_AFTER_IAT_SECS, 7 * 24 * HOUR + CLOCK_SKEW_TOLERANCE_MS / 1000);
    }

    // Monotonic time high-water mark

    #[test]
    fn a_verification_re_anchors_the_mark_to_server_time_and_nothing_else_lowers_it() {
        let iat_ms = 1_800_000_000_000;
        let skew = CLOCK_SKEW_TOLERANCE_MS;
        // Normal case: the mark is at or below the server bound → the ordinary rule, unchanged.
        assert_eq!(mark_after_verification(None, iat_ms + 5_000, iat_ms), iat_ms + 5_000);
        assert_eq!(mark_after_verification(Some(iat_ms - HOUR * 1000), iat_ms + 5_000, iat_ms), iat_ms + 5_000);
        assert_eq!(mark_after_verification(Some(iat_ms + 60_000), iat_ms + 5_000, iat_ms), iat_ms + 60_000);
        // A Mac a minute slow keeps its own reading (the bound is a ceiling, not a floor).
        assert_eq!(mark_after_verification(Some(iat_ms - 120_000), iat_ms - 60_000, iat_ms), iat_ms - 60_000);
        // Glitch: the clock once read a year ahead and the mark stuck there. Before verification
        // every token is Stale; the round-trip pulls the mark back to the server bound.
        let year = 365 * 24 * HOUR * 1000;
        let glitch = iat_ms + year;
        let t = verify(&mint(payload("pro", iat_ms / 1000, iat_ms / 1000 + 24 * HOUR)), &pubkey(), DEVICE)
            .expect("verify");
        assert_eq!(t.freshness_at(Clock::with_mark(iat_ms + 5_000, Some(glitch))), Freshness::Stale);
        let mark = Clock::with_mark(iat_ms + 5_000, Some(glitch)).next_mark_after_verification(iat_ms);
        assert_eq!(mark, iat_ms + skew);
        assert!(mark <= iat_ms + skew, "(a) never past the server's iat plus the tolerance");
        assert_eq!(t.freshness_at(Clock::with_mark(iat_ms + 5_000, Some(mark))), Freshness::Fresh);
        assert_eq!(Clock::with_mark(iat_ms + 5_000, Some(mark)).now(), iat_ms + 5_000);
        // A rollback with no verification still clamps: the ordinary mark only grows, and a clock
        // set back a day reads as the mark.
        let day_back = mark - 24 * HOUR * 1000;
        assert_eq!(Clock::with_mark(day_back, Some(mark)).next_mark(), mark);
        assert_eq!(clamp_now(day_back, Some(mark)), mark);
        // (b) the lowering needs the server's iat: an iat the attacker wishes for is not signed,
        // and with the real one a rolled-back clock cannot pull the mark below where it stands.
        assert_eq!(mark_after_verification(Some(mark), day_back, iat_ms), mark);
    }

    #[test]
    fn clamp_now_honours_small_drift_and_pins_large_rollbacks() {
        let mark = 1_800_000_000_000;
        // No mark, or a clock at or past the mark: as read.
        assert_eq!(clamp_now(mark - 5000, None), mark - 5000);
        assert_eq!(clamp_now(mark, Some(mark)), mark);
        assert_eq!(clamp_now(mark + 1, Some(mark)), mark + 1);
        // Inside the skew tolerance: as read.
        assert_eq!(clamp_now(mark - CLOCK_SKEW_TOLERANCE_MS, Some(mark)), mark - CLOCK_SKEW_TOLERANCE_MS);
        // Beyond it: the mark.
        assert_eq!(clamp_now(mark - CLOCK_SKEW_TOLERANCE_MS - 1, Some(mark)), mark);
        assert_eq!(clamp_now(0, Some(mark)), mark);
        let clock = Clock::with_mark(mark - 24 * HOUR * 1000, Some(mark));
        assert_eq!(clock.now(), mark);
        assert_eq!(clock.next_mark(), mark);
        assert_eq!(Clock::with_mark(mark + 7, Some(mark)).next_mark(), mark + 7);
        assert_eq!(Clock::at(42).next_mark(), 42);
    }

    #[test]
    fn a_clock_rolled_back_does_not_revive_an_expired_token() {
        let iat = 1_000_000;
        let t = verify(&mint(payload("pro", iat, iat + 24 * HOUR)), &pubkey(), DEVICE).expect("verify");
        let deadline = t.grace_deadline_ms();
        // The Mac has seen day 15 (past the 14-day grace), then the clock is set back to day 2.
        let day2 = (iat + 2 * 24 * HOUR) * 1000;
        let day15 = (iat + 15 * 24 * HOUR) * 1000;
        assert_eq!(t.freshness(day2), Freshness::Grace { days_offline: 2 }, "honest clock: in grace");
        assert_eq!(t.freshness_at(Clock::with_mark(day2, Some(day15))), Freshness::Stale);
        assert_eq!(t.billing_state_at(Clock::with_mark(day2, Some(day15))), BillingState::Lapsed);
        // Even a mark inside the skew tolerance of `now` counts once it has passed the deadline.
        let just_past = deadline + 1;
        assert_eq!(
            t.billing_state_at(Clock::with_mark(just_past - 60_000, Some(just_past))),
            BillingState::Lapsed
        );
        // The pure grace rule is untouched: a mark before the deadline changes nothing.
        assert_eq!(
            t.freshness_at(Clock::with_mark(day2, Some(day2 + HOUR * 1000))),
            Freshness::Grace { days_offline: 2 }
        );
    }

    #[test]
    fn a_forward_clock_is_unaffected_by_the_mark() {
        let iat = 1_000_000;
        let t = verify(&mint(payload("pro", iat, iat + 24 * HOUR)), &pubkey(), DEVICE).expect("verify");
        let mark = (iat + HOUR) * 1000;
        for now in [mark, mark + 1, (iat + 23 * HOUR) * 1000] {
            assert_eq!(t.freshness_at(Clock::with_mark(now, Some(mark))), Freshness::Fresh, "{now}");
            assert_eq!(t.freshness_at(Clock::with_mark(now, Some(mark))), t.freshness(now));
        }
        let in_grace = (iat + 3 * 24 * HOUR) * 1000;
        assert_eq!(
            t.freshness_at(Clock::with_mark(in_grace, Some(mark))),
            Freshness::Grace { days_offline: 3 }
        );
    }

    #[test]
    fn small_backwards_drift_is_tolerated_against_the_mark() {
        let iat = 1_000_000;
        let t = verify(&mint(payload("pro", iat, iat + 24 * HOUR)), &pubkey(), DEVICE).expect("verify");
        let mark = (iat + 23 * HOUR) * 1000 + CLOCK_SKEW_TOLERANCE_MS;
        // Ten minutes behind the mark: the clock is believed, and it says Fresh.
        let now = mark - CLOCK_SKEW_TOLERANCE_MS;
        assert_eq!(Clock::with_mark(now, Some(mark)).now(), now);
        assert_eq!(t.freshness_at(Clock::with_mark(now, Some(mark))), Freshness::Fresh);
        // A day behind a mark that is past `exp`: the mark wins and the token is in grace.
        let past_exp = (iat + 26 * HOUR) * 1000;
        assert_eq!(
            t.freshness_at(Clock::with_mark(past_exp - 24 * HOUR * 1000, Some(past_exp))),
            Freshness::Grace { days_offline: 1 }
        );
    }

    #[test]
    fn billing_state_tracks_the_window() {
        let t = verify(&mint(payload("standard", 0, 3600)), &pubkey(), DEVICE).expect("verify");
        assert_eq!(t.billing_state(0), BillingState::Active(PaidPlan::Standard));
        assert_eq!(t.billing_state(t.grace_deadline_ms()), BillingState::Lapsed);
    }

    #[test]
    fn zero_grace_trial_is_fresh_before_expiry_and_never_extends_past_it() {
        let iat = 1_800_000_000;
        let exp = iat + HOUR;
        let mut body = payload("pro", iat, exp);
        body["status"] = serde_json::json!("trialing");
        body["grace_days"] = serde_json::json!(0);
        body["period_end"] = serde_json::json!(exp);
        let token = verify(&mint(body), &pubkey(), DEVICE).expect("signed Pro trial");
        let exp_ms = exp * 1000;
        assert_eq!(token.grace_deadline_ms(), exp_ms);
        assert_eq!(token.freshness_at(Clock::with_mark(iat * 1000, Some(iat * 1000))), Freshness::Fresh);
        assert_eq!(token.freshness_at(Clock::with_mark(exp_ms - 1, Some(exp_ms - 1))), Freshness::Fresh);
        assert_eq!(token.billing_state(exp_ms - 1), BillingState::Active(PaidPlan::Pro));
        assert_eq!(token.freshness(exp_ms), Freshness::Stale);
        assert_eq!(token.freshness(exp_ms + 1), Freshness::Stale);
        // Even a rollback within the normal skew tolerance cannot revive the expired trial.
        assert_eq!(token.freshness_at(Clock::with_mark(exp_ms - 1, Some(exp_ms))), Freshness::Stale);
        assert_eq!(token.billing_state_at(Clock::with_mark(iat * 1000, Some(exp_ms + 1))), BillingState::Lapsed);
    }

    #[test]
    fn grace_days_are_clamped() {
        let mut p = payload("pro", 0, 3600);
        p["grace_days"] = serde_json::json!(3650);
        let t = verify(&mint(p), &pubkey(), DEVICE).expect("verify");
        assert_eq!(t.grace_days, MAX_GRACE_DAYS);
    }

    #[test]
    fn a_bad_token_is_unknown_not_access() {
        assert_eq!(
            billing_state_from_token("garbage", &pubkey(), DEVICE, 0),
            BillingState::Unknown
        );
    }

    // Hardware device binding (issue #777)

    /// Two Macs' hardware ids, and the random id a pre-#777 build saved in Mac A's billing.json.
    const HW_A: &str = "4H9iyD3A6qHY2K_QmvBt6BY9XVGThDWcDpTCyvV_iHM";
    const HW_B: &str = "YLBItJt5pOm21XXUG5u3Yp_iuTj6PjOY-uvskMDp9Ig";
    const RECORDED_A: &str = "0b7f3c1e-5a2d-4c6b-9e8f-1a2b3c4d5e6f";
    const CUTOFF: u64 = 1_790_985_600;

    fn token_for(device: &str, iat: u64) -> String {
        let mut p = payload("pro", iat, iat + 26 * HOUR);
        p["device"] = serde_json::json!(device);
        mint(p)
    }

    fn on(hardware: &str) -> DeviceBinding<'_> {
        DeviceBinding { hardware: Some(hardware), legacy_cutoff_secs: CUTOFF }
    }

    #[test]
    fn a_hardware_bound_token_verifies_whatever_the_file_records() {
        let token = token_for(HW_A, CUTOFF + 10 * HOUR);
        assert!(on(HW_A).verify(&token, &keys(), HW_A).is_ok());
        // The recorded id is not consulted for a token bound to this Mac's hardware.
        assert!(on(HW_A).verify(&token, &keys(), RECORDED_A).is_ok());
        assert!(on(HW_A).verify(&token, &keys(), "anything-else").is_ok());
    }

    #[test]
    fn a_copied_hardware_bound_token_is_refused_on_another_mac() {
        let token = token_for(HW_A, CUTOFF + 10 * HOUR);
        // The copy brings its file's device id along; Mac B's hardware says otherwise.
        assert_eq!(on(HW_B).verify(&token, &keys(), HW_A), Err(LicenseError::DeviceMismatch));
        assert_eq!(
            on(HW_B).billing_state(&token, &keys(), HW_A, Clock::at((CUTOFF + 11 * HOUR) * 1000)),
            BillingState::Unknown
        );
    }

    #[test]
    fn a_legacy_token_counts_only_if_issued_before_the_cutoff() {
        let before = token_for(RECORDED_A, CUTOFF - 1);
        let at = token_for(RECORDED_A, CUTOFF);
        let after = token_for(RECORDED_A, CUTOFF + HOUR);
        // On the Mac it was minted for (hardware A) and on a copy (hardware B) alike: the device
        // cannot tell the two apart, which is why the window is closed by time instead.
        for hw in [HW_A, HW_B] {
            let t = on(hw).verify(&before, &keys(), RECORDED_A).expect("pre-cutoff legacy token");
            assert_eq!(t.device_id, RECORDED_A);
            assert_eq!(on(hw).verify(&at, &keys(), RECORDED_A), Err(LicenseError::DeviceMismatch));
            assert_eq!(on(hw).verify(&after, &keys(), RECORDED_A), Err(LicenseError::DeviceMismatch));
        }
        // A legacy token only ever counts for the id recorded beside it.
        assert_eq!(
            on(HW_A).verify(&before, &keys(), "1b7f3c1e-5a2d-4c6b-9e8f-1a2b3c4d5e6f"),
            Err(LicenseError::DeviceMismatch)
        );
        // …and only for a UUID-shaped (pre-#777 random) one.
        let odd = token_for("device-abc123", CUTOFF - 1);
        assert_eq!(on(HW_A).verify(&odd, &keys(), "device-abc123"), Err(LicenseError::DeviceMismatch));
    }

    /// A hardware-bound token is never "legacy", however early it was issued: a copy of Mac A's
    /// file brings A's hardware id as its recorded id, and Mac B must not fall back to trusting it.
    #[test]
    fn a_copied_hardware_bound_token_is_refused_even_before_the_cutoff() {
        let token = token_for(HW_A, CUTOFF - 10 * HOUR);
        assert_eq!(on(HW_B).verify(&token, &keys(), HW_A), Err(LicenseError::DeviceMismatch));
    }

    #[test]
    fn a_pre_cutoff_legacy_token_dies_with_its_grace_window() {
        let iat = CUTOFF - HOUR;
        let token = token_for(RECORDED_A, iat);
        let at_ms = |secs: u64| secs * 1000;
        assert_eq!(
            on(HW_B).billing_state(&token, &keys(), RECORDED_A, Clock::at(at_ms(iat + 13 * 24 * HOUR))),
            BillingState::Active(PaidPlan::Pro)
        );
        assert_eq!(
            on(HW_B).billing_state(&token, &keys(), RECORDED_A, Clock::at(at_ms(iat + 14 * 24 * HOUR))),
            BillingState::Lapsed
        );
    }

    #[test]
    fn the_legacy_path_never_relaxes_the_other_checks() {
        let other = SigningKey::from_bytes(&[9u8; 32]).verifying_key().to_bytes();
        let token = token_for(RECORDED_A, CUTOFF - HOUR);
        assert_eq!(on(HW_A).verify(&token, &TrustedKeys::single(other), RECORDED_A), Err(LicenseError::BadSignature));
        assert_eq!(on(HW_A).verify("garbage", &keys(), RECORDED_A), Err(LicenseError::Malformed));
    }

    #[test]
    fn without_a_hardware_id_the_recorded_id_decides_as_before() {
        let token = token_for(RECORDED_A, CUTOFF + 100 * HOUR);
        let none = DeviceBinding::RECORDED_ONLY;
        assert!(none.verify(&token, &keys(), RECORDED_A).is_ok(), "no cutoff applies here");
        assert_eq!(none.verify(&token, &keys(), "other"), Err(LicenseError::DeviceMismatch));
        assert_eq!(none.request_id(), None);
        assert_eq!(on(HW_A).request_id(), Some(HW_A));
    }

    const MARGIN_SECS: u64 = 14 * 24 * HOUR;

    #[test]
    fn the_compiled_cutoff_sits_inside_its_clamp() {
        // build.rs: build time (or SOURCE_DATE_EPOCH) + 14 days, clamped to
        // [2026-10-03, 2027-01-01] UTC.
        assert!((1_790_985_600..=1_798_761_600).contains(&LEGACY_DEVICE_CUTOFF_SECS));
        assert_eq!(
            LEGACY_DEVICE_CUTOFF_SECS,
            (LEGACY_DEVICE_BUILD_EPOCH_SECS + MARGIN_SECS).clamp(1_790_985_600, 1_798_761_600)
        );
        assert_eq!(DeviceBinding::this_mac().legacy_cutoff_secs, LEGACY_DEVICE_CUTOFF_SECS);
        assert_eq!(DeviceBinding::this_mac().hardware, hardware_device_id());
    }

    /// The margin: the old client keeps minting legacy tokens after this build is made, until the
    /// user installs it. One minted a day after the build must still pass on an offline first
    /// launch of the new build — on the compiled-in cutoff, not a test value.
    #[test]
    fn a_legacy_token_minted_after_the_build_but_inside_the_margin_is_accepted() {
        let compiled = DeviceBinding { hardware: Some(HW_A), legacy_cutoff_secs: LEGACY_DEVICE_CUTOFF_SECS };
        // Inside the margin whenever the clamp did not bite (always, for a build before
        // 2026-12-18); otherwise the last second before the ceiling.
        let a_day_after_build = (LEGACY_DEVICE_BUILD_EPOCH_SECS + 24 * HOUR).min(LEGACY_DEVICE_CUTOFF_SECS - 1);
        let token = token_for(RECORDED_A, a_day_after_build);
        assert!(compiled.verify(&token, &keys(), RECORDED_A).is_ok());
        let last = token_for(RECORDED_A, LEGACY_DEVICE_CUTOFF_SECS - 1);
        assert!(compiled.verify(&last, &keys(), RECORDED_A).is_ok());
        let past = token_for(RECORDED_A, LEGACY_DEVICE_CUTOFF_SECS);
        assert_eq!(compiled.verify(&past, &keys(), RECORDED_A), Err(LicenseError::DeviceMismatch));
    }

    #[test]
    fn public_key_decoding_accepts_both_alphabets() {
        let raw = pubkey();
        let std_b64 = base64::engine::general_purpose::STANDARD.encode(raw);
        let url_b64 = URL_SAFE_NO_PAD.encode(raw);
        assert_eq!(decode_public_key(&std_b64), Some(raw));
        assert_eq!(decode_public_key(&url_b64), Some(raw));
        assert_eq!(decode_public_key(""), None);
        assert_eq!(decode_public_key("short"), None);
    }

    fn trial_payload(iat: u64, exp: u64, end: u64) -> serde_json::Value {
        serde_json::json!({
            "v": 1, "lic": format!("trial_{DEVICE}"), "plan": "trial", "status": "trialing",
            "device": DEVICE, "iat": iat, "exp": exp, "period_end": end,
            "cancel_at_period_end": true, "grace_days": 0,
        })
    }

    /// Audit 2026-10-20 M7: the trial's end is read only from a token the licence API signed.
    #[test]
    fn a_signed_trial_token_carries_the_trial_end() {
        let iat = 1_800_000_000;
        let end = iat + 3 * 24 * HOUR;
        let token = mint(trial_payload(iat, iat + 24 * HOUR, end));
        let t = verify_trial_with(&token, &keys(), DEVICE).unwrap();
        assert_eq!(t.period_end, end);
        assert_eq!(t.period_end_ms(), end * 1000);
        // It is never a licence.
        assert_eq!(verify_with(&token, &keys(), DEVICE), Err(LicenseError::BadPayload));
        // Bound to the device: the hardware id when there is one.
        let binding = DeviceBinding { hardware: Some(DEVICE), legacy_cutoff_secs: 0 };
        assert!(binding.verify_trial(&token, &keys(), "recorded-other").is_ok());
        let elsewhere = DeviceBinding { hardware: Some("another-mac"), legacy_cutoff_secs: 0 };
        assert_eq!(elsewhere.verify_trial(&token, &keys(), DEVICE), Err(LicenseError::DeviceMismatch));
        assert!(DeviceBinding::RECORDED_ONLY.verify_trial(&token, &keys(), DEVICE).is_ok());
    }

    #[test]
    fn a_trial_token_that_is_tampered_or_not_a_trial_is_refused() {
        let iat = 1_800_000_000;
        let token = mint(trial_payload(iat, iat + HOUR, iat + 2 * HOUR));
        // Edit the payload: the signature no longer matches.
        let mut parts: Vec<String> = token.split('.').map(str::to_string).collect();
        let mut body: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(&parts[1]).unwrap()).unwrap();
        body["period_end"] = serde_json::json!(iat + 300 * 24 * HOUR);
        parts[1] = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&body).unwrap());
        assert_eq!(verify_trial_with(&parts.join("."), &keys(), DEVICE), Err(LicenseError::BadSignature));
        // A paid licence is not a trial token.
        let pro = mint(payload("pro", iat, iat + HOUR));
        assert_eq!(verify_trial_with(&pro, &keys(), DEVICE), Err(LicenseError::BadPayload));
        // A signed end further than seven days (plus skew) past issuance is malformed.
        let far = mint(trial_payload(iat, iat + HOUR, iat + MAX_TRIAL_UNTIL_AFTER_IAT_SECS + 1));
        assert_eq!(verify_trial_with(&far, &keys(), DEVICE), Err(LicenseError::BadPayload));
        // `exp` past the trial's end is malformed too.
        let late = mint(trial_payload(iat, iat + 2 * HOUR, iat + HOUR));
        assert_eq!(verify_trial_with(&late, &keys(), DEVICE), Err(LicenseError::BadPayload));
        // No end at all.
        let mut no_end = trial_payload(iat, iat + HOUR, iat + 2 * HOUR);
        no_end.as_object_mut().unwrap().remove("period_end");
        assert_eq!(verify_trial_with(&mint(no_end), &keys(), DEVICE), Err(LicenseError::BadPayload));
    }
}
