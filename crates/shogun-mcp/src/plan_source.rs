//! Plan resolution for the **standalone** Memory API faces (issue #97) — the `shogun-api` (REST)
//! and `shogun-mcp` (stdio) binaries, which run outside the desktop app and therefore cannot ask
//! the Tauri layer for the plan.
//!
//! Two inputs, both files the desktop app owns and this module re-reads on every resolution:
//!
//! - **trial stamp** — `onboarding.json` (Rust-written, versioned; see
//!   `apps/desktop/src-tauri/src/onboarding.rs`) carries `state.trial_started_at` (unix
//!   **seconds**, stamped once at first account sign-in). Re-reading means a trial expiring while
//!   a server runs locks the next request.
//! - **billing** — `billing.json` (issue #8) carries this device's signed licence token. The
//!   token is verified here, on every read, against the licence API's public key and against this
//!   Mac's hardware-derived device id (`shogun_license::hardware_device_id`, issue #777) — derived
//!   in-process, never read from the Keychain or from `billing.json`. The same file carries the
//!   desktop's monotonic **time high-water mark** (`max_seen_ms`, audit 2026-10-10 M7): the
//!   latest wall-clock reading it has recorded. Every resolution here clamps `now` to it, so a
//!   clock set back cannot revive an expired licence or stretch a trial. The desktop writes the
//!   mark (also as a mark-only `billing.json` on a Mac with no licence); the standalone faces
//!   only read it.
//! - **server trial end** — `trial_relay.json` (issue #763) records the licence API's
//!   `trial_ends_at` for this device when the desktop has asked for a trial token. It bounds the
//!   local trial (audit H2): at or past it the trial is over whatever `onboarding.json` says.
//!
//! ## Why the signed token sits in a file and the licence key does not
//!
//! NFR-SEC-01 keeps *secrets* in the Keychain, and the licence **key** (the bearer that talks to
//! the licence API) obeys that without exception — it is never written to disk. The **token** is a
//! different kind of object: a public-key-signed, device-bound, expiring assertion. Copying it
//! to another Mac grants nothing: the device id it is checked against comes from that Mac's
//! hardware, not from the copied file (issue #777: an id read from the same file would let a copy
//! carry both halves). Editing it invalidates the
//! signature, and it ages out on its own. Mirroring it into a file is what lets the CLI, the MCP
//! server and the REST face see a paid plan at all — they cannot read the app's Keychain items,
//! and a Pro subscriber whose `shogun` CLI locked itself out would be a worse outcome by far.
//! Recorded in docs/fixes/2026-08-10-stripe-billing-flow-design.md §5.
//!
//! ## The Keychain mirrors (audit 2026-10-20 M7)
//!
//! Both files above are plain JSON any program running as the user can rewrite: a later
//! `trial_started_at` stretches the trial, a lower `max_seen_ms` lets a clock set back revive an
//! expired licence. The desktop therefore keeps two values in the Keychain as well, where such a
//! program cannot rewrite them without a prompt: the trial stamp (`trial-started-at`, since audit
//! H2) and the time high-water mark (`time-high-water-mark`). A binary that can read the Keychain
//! installs a reader at start-up ([`install_keychain_mirror_reader`]); every resolution then takes
//! the **earliest** trial stamp and the **latest** mark of file and Keychain, so editing a file
//! can only ever take access away. The trial's server-side end is read from the signed trial
//! token in `trial_relay.json` ([`crate::trial_relay::trial_end_ms`]), never from an unsigned
//! field alone, and without that signed end no stamp runs a trial (issue #1031). Without a reader (Linux, tests, a Keychain that will not answer without
//! a prompt) the files decide as before.
//!
//! Path resolution: `SHOGUN_ONBOARDING_JSON` / `SHOGUN_BILLING_JSON` env overrides first in debug
//! builds only (dev / tests), else the macOS app-data location of the desktop app. When no file is found the answer
//! fails closed for paid features until the desktop persists the first-launch trial clock
//! or a signed paid licence is available.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use shogun_agents::entitlement::{
    effective_trial_start, entitlements, resolve_plan_from, BillingState, EntitlementSource,
    Entitlements, PaidPlan, PlanInputs, PlanStatus, TRIAL_DURATION_MS,
};
use shogun_license::{trusted_keys, Clock, DeviceBinding, LicenceFacts, TrustedKeys};

/// The desktop app's bundle identifier — its app-data directory holds `onboarding.json` and
/// `billing.json`. Kept in lockstep with `apps/desktop/src-tauri/tauri.conf.json`. Read only by
/// the Mac app's build (feature `desktop`, [`app_data_dir`]).
#[cfg(feature = "desktop")]
const DESKTOP_IDENTIFIER: &str = "com.syogun.shogunai";

/// File name of the billing snapshot the desktop app writes after each successful verification.
pub const BILLING_FILE: &str = "billing.json";

/// Parse `onboarding.json` text into the trial stamp in unix **ms**, tolerating anything: only a
/// versioned (v1+) file with a numeric `state.trial_started_at` (unix seconds) yields a stamp.
/// Everything else — legacy #46 files, garbage, missing fields — is `None` (trial-not-started).
pub fn parse_trial_started_at_ms(text: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    if v.get("version")?.as_u64()? < 1 {
        return None;
    }
    let secs = v.get("state")?.get("trial_started_at")?.as_i64()?;
    u64::try_from(secs).ok()?.checked_mul(1000)
}

/// The trial stamp to keep when `onboarding.json` and its Keychain mirror disagree (audit
/// 2026-10-10 H2): the **earliest**, unix seconds. `None` only when neither has one. Pure, so the
/// rule the desktop applies at load and at the first stamp is pinned on Linux.
pub fn earliest_trial_stamp_secs(file: Option<i64>, keychain: Option<i64>) -> Option<i64> {
    match (file, keychain) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// The Keychain mirror's bytes (decimal unix seconds, as the desktop writes them) as a stamp.
/// Garbage, empty or non-positive text is `None`: a mirror that cannot be read grants nothing
/// and takes nothing away.
pub fn parse_trial_stamp_mirror(bytes: &[u8]) -> Option<i64> {
    let secs: i64 = std::str::from_utf8(bytes).ok()?.trim().parse().ok()?;
    (secs > 0).then_some(secs)
}

/// The plan-clock values the desktop mirrors into the Keychain (audit 2026-10-20 M7). Not
/// secrets: copies of the trial stamp and the time high-water mark where a program rewriting
/// app-data files cannot reach them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeychainPlanMirrors {
    /// The trial stamp, unix seconds ([`parse_trial_stamp_mirror`]).
    pub trial_started_at_secs: Option<i64>,
    /// The time high-water mark, unix ms ([`parse_time_mark_mirror`]).
    pub max_seen_ms: Option<u64>,
}

impl KeychainPlanMirrors {
    /// Both mirrors from the raw Keychain bytes (`None` where an item could not be read).
    pub fn from_bytes(trial_started_at: Option<&[u8]>, time_mark: Option<&[u8]>) -> Self {
        Self {
            trial_started_at_secs: trial_started_at.and_then(parse_trial_stamp_mirror),
            max_seen_ms: time_mark.and_then(parse_time_mark_mirror),
        }
    }
}

/// The time high-water mark's Keychain bytes (decimal unix ms, as the desktop writes them) as a
/// mark. Garbage or zero is `None`: an unreadable mirror takes nothing away and grants nothing.
pub fn parse_time_mark_mirror(bytes: &[u8]) -> Option<u64> {
    let ms: u64 = std::str::from_utf8(bytes).ok()?.trim().parse().ok()?;
    (ms > 0).then_some(ms)
}

/// The bytes the desktop writes for the time high-water mark mirror.
pub fn serialize_time_mark_mirror(max_seen_ms: u64) -> String {
    max_seen_ms.to_string()
}

/// The mark to clamp `now` to: the later of `billing.json`'s and the Keychain's (audit M7).
pub fn effective_max_seen_ms(file: Option<u64>, keychain: Option<u64>) -> Option<u64> {
    match (file, keychain) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// How the process reads the Keychain mirrors; installed once by a binary that can.
pub type KeychainMirrorReader = fn() -> KeychainPlanMirrors;

static MIRROR_READER: OnceLock<KeychainMirrorReader> = OnceLock::new();
/// The last read and when: plan resolution runs on every request, the mirrors change hourly at
/// most, and a Keychain query costs milliseconds.
static MIRROR_CACHE: Mutex<Option<(Instant, KeychainPlanMirrors)>> = Mutex::new(None);
const MIRROR_CACHE_TTL: Duration = Duration::from_secs(30);

/// Install the Keychain mirror reader for this process (the standalone faces and the desktop,
/// macOS only). `false` when one was already installed (the first one stays).
pub fn install_keychain_mirror_reader(reader: KeychainMirrorReader) -> bool {
    MIRROR_READER.set(reader).is_ok()
}

/// The installed reader's answer, cached for [`MIRROR_CACHE_TTL`]; empty without a reader.
pub fn installed_keychain_mirrors() -> KeychainPlanMirrors {
    let Some(reader) = MIRROR_READER.get() else {
        return KeychainPlanMirrors::default();
    };
    let mut cache = match MIRROR_CACHE.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some((at, mirrors)) = *cache {
        if at.elapsed() < MIRROR_CACHE_TTL {
            return mirrors;
        }
    }
    let mirrors = reader();
    *cache = Some((Instant::now(), mirrors));
    mirrors
}

/// The trial stamp in force, unix ms: the earliest of `onboarding.json`'s (ms) and the Keychain
/// mirror's (seconds). Pure.
pub fn effective_trial_started_at_ms(file_ms: Option<u64>, keychain_secs: Option<i64>) -> Option<u64> {
    let keychain_ms = keychain_secs
        .and_then(|secs| u64::try_from(secs).ok())
        .and_then(|secs| secs.checked_mul(1000));
    match (file_ms, keychain_ms) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// The device-side billing snapshot (`billing.json`). Written by the desktop app after a
/// successful licence verification; read by every plan source.
///
/// Only the token matters — it is signed and carries the plan and the device it is bound to.
/// Nothing else in this file is trusted for a gating decision on a Mac with a hardware id; see
/// [`BillingSnapshot::device_id`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BillingSnapshot {
    /// The device id the desktop presented when it obtained `token` — the hardware-derived id
    /// since issue #777, a random one before. Kept for compatibility, and trusted for verification
    /// only where the Mac exposes no hardware id, or for a legacy token issued before
    /// `shogun_license::LEGACY_DEVICE_CUTOFF_SECS` (see `shogun_license::DeviceBinding`). A copy of
    /// this file brings this field along, so it can never be what proves the binding.
    pub device_id: String,
    pub token: String,
    /// Unix seconds of the last successful verification (display / diagnostics only).
    pub verified_at: Option<i64>,
    /// The time high-water mark (audit M7), unix ms: the latest clock reading the desktop has
    /// recorded. Not trusted for anything but clamping `now` upwards, which can only ever take
    /// access away. Absent in files written before it existed.
    pub max_seen_ms: Option<u64>,
}

/// Parse `billing.json`. Anything unparseable is `None` = no billing record = trial rules. A
/// mark-only file (see [`serialize_time_mark`]) is `None` here too — [`parse_max_seen_ms`] reads
/// the mark from either shape.
pub fn parse_billing_snapshot(text: &str) -> Option<BillingSnapshot> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    if v.get("version")?.as_u64()? < 1 {
        return None;
    }
    let device_id = v.get("device_id")?.as_str()?.trim().to_string();
    let token = v.get("token")?.as_str()?.trim().to_string();
    if device_id.is_empty() || token.is_empty() {
        return None;
    }
    Some(BillingSnapshot {
        device_id,
        token,
        verified_at: v.get("verified_at").and_then(serde_json::Value::as_i64),
        max_seen_ms: v.get("max_seen_ms").and_then(serde_json::Value::as_u64),
    })
}

/// The time high-water mark in `billing.json` (audit M7), from a full snapshot or a mark-only
/// file alike. `None` for anything unreadable or a file written before the mark existed.
pub fn parse_max_seen_ms(text: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    if v.get("version")?.as_u64()? < 1 {
        return None;
    }
    v.get("max_seen_ms").and_then(serde_json::Value::as_u64)
}

/// Is this `billing.json` a mark-only file: the high-water mark with no licence in it (a Mac on
/// the trial, or one whose licence lapsed)? Such a file is "no licence", not an unverifiable one.
pub fn is_time_mark_only(text: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return false;
    };
    v.get("version").and_then(serde_json::Value::as_u64).is_some_and(|version| version >= 1)
        && v.get("token").is_none()
        && v.get("max_seen_ms").and_then(serde_json::Value::as_u64).is_some()
}

/// Serialise a snapshot for the desktop writer. Kept here so the reader and the writer can never
/// drift apart on the field names.
pub fn serialize_billing_snapshot(snap: &BillingSnapshot) -> String {
    serde_json::json!({
        "version": 1,
        "device_id": snap.device_id,
        "token": snap.token,
        "verified_at": snap.verified_at,
        "max_seen_ms": snap.max_seen_ms,
    })
    .to_string()
}

/// Serialise a mark-only `billing.json` (audit M7): what the desktop writes on a Mac with no
/// licence, and what replaces a cleared licence, so the mark survives either.
pub fn serialize_time_mark(max_seen_ms: u64) -> String {
    serde_json::json!({ "version": 1, "max_seen_ms": max_seen_ms }).to_string()
}

/// The billing state a snapshot asserts at `now_ms`: signature + device binding (this Mac's
/// hardware id, [`DeviceBinding::this_mac`]) + grace window. Any failure — no public key in this
/// build, a tampered token, another Mac's token, a copied `billing.json` — is
/// [`BillingState::Unknown`], which falls back to the trial rules rather than granting anything.
/// `now_ms` is clamped to the snapshot's own high-water mark.
pub fn billing_state_of(snap: &BillingSnapshot, now_ms: u64) -> BillingState {
    licence_facts_of(snap, Clock::with_mark(now_ms, snap.max_seen_ms)).state
}

/// [`billing_state_of`] plus the token's `trial_until` claim (audit H2-4), at a [`Clock`] the
/// caller built — the desktop's carries its in-memory mark as well as the file's.
pub fn licence_facts_of(snap: &BillingSnapshot, clock: Clock) -> LicenceFacts {
    licence_facts_with(snap, clock, trusted_keys().as_ref(), DeviceBinding::this_mac())
}

/// [`licence_facts_of`] with the verifying keys and the device binding passed in: what
/// [`FilePlanSource`] uses with its own binding, and what tests use to stand in for another Mac.
/// Crate-private so no other crate can hand it a binding other than this Mac's.
pub(crate) fn licence_facts_with(
    snap: &BillingSnapshot,
    clock: Clock,
    keys: Option<&TrustedKeys>,
    binding: DeviceBinding<'_>,
) -> LicenceFacts {
    match keys {
        Some(keys) => binding.plan_facts(&snap.token, keys, &snap.device_id, clock),
        None => LicenceFacts::default(),
    }
}

/// [`licence_facts_with`] reduced to the billing state (tests).
#[cfg(test)]
pub(crate) fn billing_state_with(
    snap: &BillingSnapshot,
    clock: Clock,
    keys: Option<&TrustedKeys>,
    binding: DeviceBinding<'_>,
) -> BillingState {
    licence_facts_with(snap, clock, keys, binding).state
}

pub(crate) fn app_data_file(env_override: &str, file: &str) -> Option<PathBuf> {
    // Debug builds only: a release binary that lets an env var re-point its billing/onboarding
    // state is a plan-gate bypass primitive (pair it with a swapped public key and the gate is
    // gone). Dev and tests keep the override.
    #[cfg(debug_assertions)]
    if let Ok(p) = std::env::var(env_override) {
        if !p.trim().is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    #[cfg(not(debug_assertions))]
    let _ = env_override;
    app_data_dir().map(|dir| dir.join(file))
}

/// The desktop app's data folder (`~/Library/Application Support/<bundle id>`); `None` off macOS,
/// without `HOME`, or in a build without the Mac app (feature `desktop`, E1-T7: the server never
/// looks beside itself for the app's files). The standalone faces also compare `SHOGUN_DB_PATH`
/// against it to explain a launcher that points elsewhere (issue #744).
pub fn app_data_dir() -> Option<PathBuf> {
    #[cfg(feature = "desktop")]
    {
        crate::desktop_paths::app_data_dir(DESKTOP_IDENTIFIER)
    }
    #[cfg(not(feature = "desktop"))]
    {
        None
    }
}

/// Shared desktop meeting settings location.
pub fn meeting_json_path() -> Option<PathBuf> {
    app_data_file("SHOGUN_MEETING_SETTINGS_JSON", "meeting.json")
}

/// One-shot Record now *offer* the desktop driver consumes (CLI / MCP / REST).
pub fn meeting_record_now_path() -> Option<PathBuf> {
    app_data_file("SHOGUN_MEETING_RECORD_NOW_JSON", "meeting_record_now.json")
}

/// Durable `meeting.start` approval outcomes (FR-API-04). Shared by MCP / REST / CLI / desktop.
pub fn meeting_start_approvals_path() -> Option<PathBuf> {
    app_data_file(
        "SHOGUN_MEETING_START_APPROVALS_JSON",
        "meeting_start_approvals.json",
    )
}

/// Shared translation preferences (issue #279 FR-TL-07: the register remembered per app).
pub fn translate_json_path() -> Option<PathBuf> {
    app_data_file("SHOGUN_TRANSLATE_JSON", "translate.json")
}

/// The app's display language (`en` default).
pub fn ui_language_json_path() -> Option<PathBuf> {
    app_data_file("SHOGUN_UI_LANGUAGE_JSON", "ui_language.json")
}

/// The desktop updater's non-secret status mirror (issue #1296), read by `device.update.get`.
pub fn update_status_json_path() -> Option<PathBuf> {
    app_data_file("SHOGUN_UPDATE_STATUS_JSON", "update_status.json")
}

/// The Automatic updates switch the desktop writes (`on`/`off`). Tauri's config dir is this same
/// app-data folder on macOS.
pub fn automatic_updates_path() -> Option<PathBuf> {
    app_data_file("SHOGUN_AUTOMATIC_UPDATES", "automatic-updates")
}

/// The Web fetch consent (issue #555 FR-WV-15): `<app-data>/web_fetch.json`, written by the
/// desktop's Connections card and read by every face before a public fetch.
pub fn web_fetch_json_path() -> Option<PathBuf> {
    app_data_file("SHOGUN_WEB_FETCH_JSON", "web_fetch.json")
}

/// Shared desktop voice (dictation) settings location.
pub fn voice_json_path() -> Option<PathBuf> {
    app_data_file("SHOGUN_VOICE_SETTINGS_JSON", "voice.json")
}

/// The reporter's list of support reports (issue #108) — ids and statuses, no text, no
/// token. Written by the desktop, read by every face.
pub fn support_reports_path() -> Option<PathBuf> {
    app_data_file("SHOGUN_SUPPORT_REPORTS_JSON", "support_reports.json")
}

/// The last nightly Dream outcome and its reason (issue #285): a cycle date, a reason code and a
/// time — never captured text. Written by the desktop's Dream driver, read by the Settings row and
/// by `dream.status` on every face.
pub fn dream_outcome_json_path() -> Option<PathBuf> {
    app_data_file("SHOGUN_DREAM_OUTCOME_JSON", "dream_outcome.json")
}

/// The "retry the Batch review now" request (`dream.retry`): a bare timestamp the MCP / REST / CLI
/// faces drop here and the desktop's Dream driver consumes. Idempotent — many requests are one.
pub fn dream_retry_request_path() -> Option<PathBuf> {
    app_data_file("SHOGUN_DREAM_RETRY_JSON", "dream_retry_request.json")
}

/// Record a retry request (see [`dream_retry_request_path`]).
pub fn request_dream_retry() -> Result<(), String> {
    let path = dream_retry_request_path().ok_or("app data folder unavailable")?;
    std::fs::write(path, "1").map_err(|_| "dream retry request could not be saved".to_string())
}

/// Consume a pending retry request; `true` when there was one.
pub fn take_dream_retry_request() -> bool {
    dream_retry_request_path().is_some_and(|path| std::fs::remove_file(path).is_ok())
}

/// The desktop's `analytics.json` (anonymous id + `opt_out`), which the stdio server reads to
/// decide whether it may count a mount (issue #400). Read only, never created here.
pub fn analytics_json_path() -> Option<PathBuf> {
    app_data_file("SHOGUN_ANALYTICS_JSON", "analytics.json")
}

/// The folders the user allowed exports into (issue #397): `{"folders": [...]}`.
pub fn export_folders_json_path() -> Option<PathBuf> {
    app_data_file("SHOGUN_EXPORT_FOLDERS_JSON", "export_folders.json")
}

/// The user's skill files (issue #251): `<app-data>/skills/*.md`. Plain files the user owns.
pub fn skills_dir() -> Option<PathBuf> {
    app_data_file("SHOGUN_SKILLS_DIR", "skills")
}

/// Where the desktop app's `onboarding.json` lives for this process: the `SHOGUN_ONBOARDING_JSON`
/// env override (debug builds only), else the macOS app-data path. `None` when neither resolves (e.g. Linux dev with
/// no override).
pub fn onboarding_json_path() -> Option<PathBuf> {
    app_data_file("SHOGUN_ONBOARDING_JSON", "onboarding.json")
}

/// Where `account.json` lives (#148 P2): the content-free account snapshot the desktop writes for
/// the read-only faces (`device.account.get`). `SHOGUN_ACCOUNT_JSON` overrides it in debug builds.
pub fn account_json_path() -> Option<PathBuf> {
    app_data_file("SHOGUN_ACCOUNT_JSON", "account.json")
}

/// Where `billing.json` lives: the `SHOGUN_BILLING_JSON` env override (debug builds only), else
/// the macOS app-data path next to `onboarding.json`.
pub fn billing_json_path() -> Option<PathBuf> {
    app_data_file("SHOGUN_BILLING_JSON", BILLING_FILE)
}

/// A plan source backed by the shared `onboarding.json` + `billing.json` files. Re-reads both on
/// every call — the stamp is written once by the desktop app, and re-reading keeps a long-running
/// server honest about trial expiry, a fresh purchase and a lapsed subscription alike.
#[derive(Debug, Clone)]
pub struct FilePlanSource {
    path: Option<PathBuf>,
    billing_path: Option<PathBuf>,
    /// `trial_relay.json`, for the licence API's `trial_ends_at` (audit H2).
    trial_relay_path: Option<PathBuf>,
    /// What the saved licence must be bound to: this Mac's hardware id (issue #777).
    binding: DeviceBinding<'static>,
    /// The Keychain mirrors (audit 2026-10-20 M7): the installed reader, or fixed values (tests).
    mirrors: Option<KeychainPlanMirrors>,
}

impl FilePlanSource {
    /// Resolve every path from the environment (see [`onboarding_json_path`]).
    pub fn from_env() -> Self {
        Self {
            path: onboarding_json_path(),
            billing_path: billing_json_path(),
            trial_relay_path: crate::trial_relay::trial_relay_json_path(),
            binding: DeviceBinding::this_mac(),
            mirrors: None,
        }
    }

    /// A source pinned to a specific onboarding file (tests / explicit config). Billing and the
    /// trial record still resolve from the environment.
    pub fn at(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            billing_path: billing_json_path(),
            trial_relay_path: crate::trial_relay::trial_relay_json_path(),
            binding: DeviceBinding::this_mac(),
            mirrors: None,
        }
    }

    /// Pin the billing snapshot path too (tests / explicit config).
    pub fn with_billing(mut self, path: PathBuf) -> Self {
        self.billing_path = Some(path);
        self
    }

    /// Pin the trial record path too (tests / explicit config).
    pub fn with_trial_relay(mut self, path: PathBuf) -> Self {
        self.trial_relay_path = Some(path);
        self
    }

    /// Stand in for another Mac's hardware (tests only — production always checks this Mac's).
    #[cfg(test)]
    fn with_binding(mut self, binding: DeviceBinding<'static>) -> Self {
        self.binding = binding;
        self
    }

    /// Use fixed Keychain mirror values instead of the installed reader (tests).
    #[must_use]
    pub fn with_keychain_mirrors(mut self, mirrors: KeychainPlanMirrors) -> Self {
        self.mirrors = Some(mirrors);
        self
    }

    fn keychain_mirrors(&self) -> KeychainPlanMirrors {
        self.mirrors.unwrap_or_else(installed_keychain_mirrors)
    }

    fn file_trial_started_at_ms(&self) -> Option<u64> {
        let path = self.path.as_ref()?;
        let text = std::fs::read_to_string(path).ok()?;
        parse_trial_started_at_ms(&text)
    }

    /// The trial stamp in force: the earliest of the file's and the Keychain mirror's (M7).
    fn trial_started_at_ms(&self, mirrors: KeychainPlanMirrors) -> Option<u64> {
        effective_trial_started_at_ms(self.file_trial_started_at_ms(), mirrors.trial_started_at_secs)
    }

    fn billing_text(&self) -> Option<String> {
        std::fs::read_to_string(self.billing_path.as_ref()?).ok()
    }

    /// The trial end this Mac can prove, unix ms: the signed trial token's end, bounded by the
    /// record (issue #1031, `trial_relay::trial_end_ms`).
    fn server_trial_ends_at_ms(&self, now_ms: u64, keys: Option<&TrustedKeys>) -> Option<u64> {
        let text = std::fs::read_to_string(self.trial_relay_path.as_ref()?).ok()?;
        let trial = crate::trial_relay::parse_trial_relay(&text);
        crate::trial_relay::trial_end_ms(trial.as_ref(), keys, self.binding, now_ms)
    }

    /// Every input of one resolution, read once, with the clock clamped to the recorded mark.
    fn inputs(&self, now_ms: u64, keys: Option<&TrustedKeys>) -> (PlanInputs, Clock) {
        let mirrors = self.keychain_mirrors();
        let billing_text = self.billing_text();
        let mark = effective_max_seen_ms(
            billing_text.as_deref().and_then(parse_max_seen_ms),
            mirrors.max_seen_ms,
        );
        let clock = Clock::with_mark(now_ms, mark);
        let facts = billing_text
            .as_deref()
            .and_then(parse_billing_snapshot)
            .map(|snap| licence_facts_with(&snap, clock, keys, self.binding))
            .unwrap_or_default();
        let inputs = PlanInputs {
            trial_started_at_ms: self.trial_started_at_ms(mirrors),
            server_trial_ends_at_ms: self.server_trial_ends_at_ms(clock.now(), keys),
            billing: facts.state,
            licence_trial_until_ms: facts.trial_until_ms,
        };
        (inputs, clock)
    }

    /// Resolve the entitlements in force at `now_ms` (inherent form of [`EntitlementSource`], so
    /// binaries don't need the trait in scope). `now_ms` is clamped to the high-water mark in
    /// `billing.json` (audit M7) before both the licence and the trial are judged.
    pub fn resolve(&self, now_ms: u64) -> Entitlements {
        self.resolve_with_keys(now_ms, trusted_keys().as_ref())
    }

    fn resolve_with_keys(&self, now_ms: u64, keys: Option<&TrustedKeys>) -> Entitlements {
        let (inputs, clock) = self.inputs(now_ms, keys);
        entitlements(resolve_plan_from(inputs), clock.now())
    }

    /// When the running trial ends, unix seconds: the start [`Self::resolve`] judges the trial
    /// from (the stamp, pulled earlier by the signed end) + 7 days. `None` unless the trial runs.
    /// Display only, the same value the desktop's `entitlement_status` shows.
    pub fn trial_ends_at_secs(&self, now_ms: u64) -> Option<u64> {
        self.trial_ends_at_secs_with_keys(now_ms, trusted_keys().as_ref())
    }

    fn trial_ends_at_secs_with_keys(&self, now_ms: u64, keys: Option<&TrustedKeys>) -> Option<u64> {
        let (inputs, clock) = self.inputs(now_ms, keys);
        if entitlements(resolve_plan_from(inputs), clock.now()).status != PlanStatus::Trial {
            return None;
        }
        let start = effective_trial_start(inputs.trial_started_at_ms, inputs.server_trial_ends_at_ms)?;
        Some(start.saturating_add(TRIAL_DURATION_MS) / 1000)
    }

    /// Why this Mac has no trial ([`crate::trial_relay::trial_refusal`]), while `status` shows
    /// none running. Display only.
    pub fn trial_refusal(&self, status: PlanStatus) -> Option<String> {
        let text = std::fs::read_to_string(self.trial_relay_path.as_ref()?).ok()?;
        crate::trial_relay::trial_refusal(crate::trial_relay::parse_trial_relay(&text).as_ref(), status)
            .map(str::to_string)
    }
}

impl EntitlementSource for FilePlanSource {
    fn entitlements(&self, now_ms: u64) -> Entitlements {
        self.resolve(now_ms)
    }
}

/// What this process found for the trial clock (`onboarding.json`). Diagnostics only (issue #744).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrialFact {
    /// No path resolves for this process (`HOME` unset, or not macOS without the debug override).
    NoPath,
    /// The file does not exist.
    Missing,
    /// The file exists but could not be read.
    Unreadable,
    /// Read, but it carries no trial stamp (first run not finished, legacy or garbage content).
    NotStamped,
    /// The trial started at this unix ms.
    Stamped(u64),
}

impl TrialFact {
    fn stamp(self) -> Option<u64> {
        match self {
            TrialFact::Stamped(ms) => Some(ms),
            _ => None,
        }
    }
}

/// What this process found for the saved licence (`billing.json`). Diagnostics only (issue #744).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BillingFact {
    /// No path resolves for this process.
    NoPath,
    /// No saved licence (never purchased, or the desktop removed it).
    Missing,
    /// The file exists but could not be read.
    Unreadable,
    /// Read, but not a billing snapshot this build understands.
    NotParsed,
    /// A snapshot whose licence did not verify (the reason is the licence crate's code).
    Unverified(&'static str),
    /// A verified licence, reduced to the state it asserts now.
    Verified(BillingState),
}

impl BillingFact {
    fn state(self) -> BillingState {
        match self {
            BillingFact::Verified(state) => state,
            _ => BillingState::Unknown,
        }
    }
}

/// Why the Memory API is locked for this process (issue #744): the reason the standalone faces
/// report next to `plan_required`, so an expired trial, a Standard plan and an unverifiable licence
/// no longer read the same. Display data only: it is derived *after* the gate decision and never
/// feeds one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanLock {
    /// A verified Standard licence: the Memory API is part of Pro.
    StandardPlan,
    /// A paid licence that has not been re-verified within its offline grace window.
    RefreshNeeded,
    /// A saved licence this build could not read or verify.
    Unverified,
    /// The 7-day trial has ended and no paid licence is present.
    TrialEnded,
    /// No trial clock on this Mac yet (setup not finished, or its file is missing or unreadable).
    TrialNotStarted,
    /// This process cannot locate the app's plan files at all.
    PlanNotFound,
}

impl PlanLock {
    /// Stable id for API consumers (JSON-RPC error `data.reason`, the REST body's `reason`).
    pub fn code(self) -> &'static str {
        match self {
            PlanLock::StandardPlan => "standard_plan",
            PlanLock::RefreshNeeded => "plan_refresh_needed",
            PlanLock::Unverified => "plan_unverified",
            PlanLock::TrialEnded => "trial_ended",
            PlanLock::TrialNotStarted => "trial_not_started",
            PlanLock::PlanNotFound => "plan_not_found",
        }
    }

    /// One sentence the user can act on. No path, no identifier: it may reach an AI client.
    pub fn fix(self) -> &'static str {
        match self {
            PlanLock::StandardPlan => {
                "The Memory API is part of Pro. Upgrade under Plan & billing in ShogunAI Settings."
            }
            PlanLock::RefreshNeeded => {
                "Your plan has not been re-verified for a while. Open ShogunAI while online so it can refresh."
            }
            PlanLock::Unverified => {
                "ShogunAI could not verify the plan saved on this Mac. Refresh it under Plan & billing in ShogunAI Settings."
            }
            // Standard does not include the Memory API, so "choose a plan" would mislead.
            PlanLock::TrialEnded => {
                "The 7-day trial has ended, and the Memory API is part of Pro. Choose Pro under Plan & billing in ShogunAI Settings."
            }
            PlanLock::TrialNotStarted => {
                "The trial has not started on this Mac. Open ShogunAI and sign in."
            }
            PlanLock::PlanNotFound => {
                "This process cannot find ShogunAI's plan on this Mac. Open ShogunAI once, then restart this app."
            }
        }
    }
}

/// Why `entitlements` locks the Memory API, from the two facts that produced it. `None` whenever
/// `entitlements.memory_api` is true, so it can never disagree with the gate.
pub fn plan_lock(trial: TrialFact, billing: BillingFact, entitlements: &Entitlements) -> Option<PlanLock> {
    if entitlements.memory_api {
        return None;
    }
    Some(match billing {
        BillingFact::Verified(BillingState::Active(PaidPlan::Standard)) => PlanLock::StandardPlan,
        BillingFact::Verified(BillingState::Lapsed) => PlanLock::RefreshNeeded,
        BillingFact::Unreadable | BillingFact::NotParsed | BillingFact::Unverified(_) => {
            PlanLock::Unverified
        }
        // Pro always unlocks the Memory API, and `Unknown` never comes out of a verified token; if
        // either ever locks, fall through to the trial clock rather than guess.
        BillingFact::Verified(_) | BillingFact::NoPath | BillingFact::Missing => match trial {
            // Stamped, but no signed trial end yet (issue #1031): the trial is not proven here.
            TrialFact::Stamped(_) if entitlements.status == PlanStatus::TrialNotStarted => {
                PlanLock::TrialNotStarted
            }
            TrialFact::Stamped(_) => PlanLock::TrialEnded,
            TrialFact::NoPath => PlanLock::PlanNotFound,
            TrialFact::Missing | TrialFact::Unreadable | TrialFact::NotStamped => {
                PlanLock::TrialNotStarted
            }
        },
    })
}

/// The plan this process resolved and, when the Memory API is locked, why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanDiagnosis {
    pub trial: TrialFact,
    pub billing: BillingFact,
    /// The same value [`FilePlanSource::resolve`] returns at this instant.
    pub entitlements: Entitlements,
    pub lock: Option<PlanLock>,
}

fn read_fact(path: Option<&PathBuf>) -> Result<String, ReadFact> {
    let path = path.ok_or(ReadFact::NoPath)?;
    std::fs::read_to_string(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ReadFact::Missing
        } else {
            ReadFact::Unreadable
        }
    })
}

enum ReadFact {
    NoPath,
    Missing,
    Unreadable,
}

impl FilePlanSource {
    /// Where this source reads the trial clock, for diagnostics.
    pub fn onboarding_path(&self) -> Option<&PathBuf> {
        self.path.as_ref()
    }

    /// Where this source reads the saved licence, for diagnostics.
    pub fn billing_path(&self) -> Option<&PathBuf> {
        self.billing_path.as_ref()
    }

    /// Read both files once and explain the result (issue #744). The entitlements are computed
    /// with the same [`resolve_plan`] / [`entitlements`] calls as [`FilePlanSource::resolve`], from
    /// the same inputs, so the explanation cannot drift from the gate (pinned by tests).
    pub fn diagnose(&self, now_ms: u64) -> PlanDiagnosis {
        self.diagnose_with_keys(now_ms, trusted_keys())
    }

    /// [`FilePlanSource::diagnose`] with the verifying keys passed in, so tests can sign licences.
    fn diagnose_with_keys(&self, now_ms: u64, keys: Option<TrustedKeys>) -> PlanDiagnosis {
        let mirrors = self.keychain_mirrors();
        let file_trial = match read_fact(self.path.as_ref()) {
            Ok(text) => parse_trial_started_at_ms(&text).map_or(TrialFact::NotStamped, TrialFact::Stamped),
            Err(ReadFact::NoPath) => TrialFact::NoPath,
            Err(ReadFact::Missing) => TrialFact::Missing,
            Err(ReadFact::Unreadable) => TrialFact::Unreadable,
        };
        // The Keychain mirror (M7): the earliest stamp of the two is the one in force.
        let trial = effective_trial_started_at_ms(file_trial.stamp(), mirrors.trial_started_at_secs)
            .map_or(file_trial, TrialFact::Stamped);
        let billing_text = read_fact(self.billing_path.as_ref());
        let clock = Clock::with_mark(
            now_ms,
            effective_max_seen_ms(
                billing_text.as_deref().ok().and_then(parse_max_seen_ms),
                mirrors.max_seen_ms,
            ),
        );
        let mut licence_trial_until_ms = None;
        let billing = match billing_text {
            Ok(text) => match parse_billing_snapshot(&text) {
                // A mark-only file is a Mac with no licence, not an unverifiable one (M7).
                None if is_time_mark_only(&text) => BillingFact::Missing,
                None => BillingFact::NotParsed,
                Some(snap) => match keys.as_ref() {
                    None => BillingFact::Unverified(shogun_license::LicenseError::NoPublicKey.as_str()),
                    Some(keys) => match self.binding.verify(&snap.token, keys, &snap.device_id) {
                        Ok(token) => {
                            let facts = token.plan_facts(clock);
                            licence_trial_until_ms = facts.trial_until_ms;
                            BillingFact::Verified(facts.state)
                        }
                        Err(error) => BillingFact::Unverified(error.as_str()),
                    },
                },
            },
            Err(ReadFact::NoPath) => BillingFact::NoPath,
            Err(ReadFact::Missing) => BillingFact::Missing,
            Err(ReadFact::Unreadable) => BillingFact::Unreadable,
        };
        let inputs = PlanInputs {
            trial_started_at_ms: trial.stamp(),
            server_trial_ends_at_ms: self.server_trial_ends_at_ms(clock.now(), keys.as_ref()),
            billing: billing.state(),
            licence_trial_until_ms,
        };
        let entitlements = entitlements(resolve_plan_from(inputs), clock.now());
        PlanDiagnosis { trial, billing, entitlements, lock: plan_lock(trial, billing, &entitlements) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shogun_agents::entitlement::{resolve_plan, PaidPlan, TRIAL_DURATION_MS};

    #[test]
    fn parses_the_versioned_desktop_format_seconds_to_ms() {
        let json = r#"{"version":1,"state":{"completed":true,"step":"ready","trial_started_at":1753842000}}"#;
        assert_eq!(parse_trial_started_at_ms(json), Some(1_753_842_000_000));
    }

    #[test]
    fn the_earliest_trial_stamp_wins_and_a_missing_side_defers() {
        assert_eq!(earliest_trial_stamp_secs(Some(100), Some(90)), Some(90));
        assert_eq!(earliest_trial_stamp_secs(Some(90), Some(100)), Some(90));
        assert_eq!(earliest_trial_stamp_secs(Some(100), None), Some(100));
        assert_eq!(earliest_trial_stamp_secs(None, Some(100)), Some(100));
        assert_eq!(earliest_trial_stamp_secs(None, None), None);
        assert_eq!(parse_trial_stamp_mirror(b"1753842000"), Some(1_753_842_000));
        assert_eq!(parse_trial_stamp_mirror(b" 1753842000\n"), Some(1_753_842_000));
        for garbage in [&b""[..], b"abc", b"0", b"-5", b"1.5", b"\xff\xfe"] {
            assert_eq!(parse_trial_stamp_mirror(garbage), None, "{garbage:?}");
        }
    }

    #[test]
    fn missing_stamp_legacy_and_garbage_are_trial_not_started() {
        for text in [
            r#"{"version":1,"state":{"completed":false,"step":"welcome"}}"#, // no stamp yet
            r#"{"completed":true}"#,                                          // legacy #46 (version 0)
            "not json",
            "",
            r#"{"version":0,"state":{"trial_started_at":5}}"#, // pre-version files never stamped
        ] {
            assert_eq!(parse_trial_started_at_ms(text), None, "{text:?}");
        }
    }

    #[test]
    fn file_source_starts_and_expires_trial_during_setup() {
        let dir = std::env::temp_dir().join(format!("shogun-plan-src-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create plan fixture");
        let path = dir.join("onboarding.json");
        std::fs::write(&path, r#"{"version":1,"state":{"completed":false,"step":"plan"}}"#)
            .expect("write pre-trial setup");
        let relay = dir.join("trial_relay.json");
        let src = FilePlanSource::at(path.clone())
            .with_billing(dir.join("no-such-billing.json"))
            .with_trial_relay(relay.clone())
            .with_binding(DeviceBinding::RECORDED_ONLY);
        let (signing, keys) = trial_signer();
        let start_ms = 100 * 1000;
        assert_eq!(src.resolve_with_keys(start_ms, Some(&keys)).status, PlanStatus::TrialNotStarted);
        std::fs::write(&path, r#"{"version":1,"state":{"completed":false,"step":"plan","trial_started_at":100}}"#)
            .expect("write first sign-in clock before setup completes");
        // Stamped at sign-in, not yet proven by the licence API (issue #1031).
        assert_eq!(src.resolve_with_keys(start_ms + 1, Some(&keys)).status, PlanStatus::TrialNotStarted);
        std::fs::write(&relay, signed_trial_relay(&signing, "dev-trial", 100)).expect("write trial token");
        assert_eq!(src.resolve_with_keys(start_ms + 1, Some(&keys)).status, PlanStatus::Trial);
        assert_eq!(src.resolve_with_keys(start_ms + TRIAL_DURATION_MS, Some(&keys)).status, PlanStatus::TrialExpired);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn absent_file_cannot_grant_memory_api() {
        let src = FilePlanSource::at(PathBuf::from("/nonexistent/onboarding.json"))
            .with_billing(PathBuf::from("/nonexistent/billing.json"));
        let e = src.entitlements(u64::MAX);
        assert_eq!(e.status, PlanStatus::TrialNotStarted);
        assert!(!e.memory_api);
    }

    #[test]
    fn billing_snapshot_round_trips() {
        let snap = BillingSnapshot {
            device_id: "dev-1".into(),
            token: "v1.aaa.bbb".into(),
            verified_at: Some(1_754_800_000),
            max_seen_ms: None,
        };
        let text = serialize_billing_snapshot(&snap);
        assert_eq!(parse_billing_snapshot(&text), Some(snap.clone()));
        assert_eq!(parse_max_seen_ms(&text), None);
        let marked = BillingSnapshot { max_seen_ms: Some(1_754_800_000_123), ..snap };
        let text = serialize_billing_snapshot(&marked);
        assert_eq!(parse_billing_snapshot(&text), Some(marked));
        assert_eq!(parse_max_seen_ms(&text), Some(1_754_800_000_123));
        assert!(!is_time_mark_only(&text));
        // A pre-M7 file without the field still parses.
        let old = r#"{"version":1,"device_id":"dev-1","token":"v1.aaa.bbb","verified_at":5}"#;
        assert_eq!(parse_billing_snapshot(old).map(|s| s.max_seen_ms), Some(None));
    }

    /// Audit M7: a Mac with no licence keeps its high-water mark in a mark-only `billing.json`.
    #[test]
    fn a_mark_only_billing_file_is_no_licence_but_carries_the_mark() {
        let text = serialize_time_mark(1_800_000_000_000);
        assert_eq!(parse_billing_snapshot(&text), None);
        assert_eq!(parse_max_seen_ms(&text), Some(1_800_000_000_000));
        assert!(is_time_mark_only(&text));
        for not_mark in ["", "not json", r#"{"version":0,"max_seen_ms":5}"#, r#"{"version":1}"#] {
            assert!(!is_time_mark_only(not_mark), "{not_mark:?}");
            assert_eq!(parse_max_seen_ms(not_mark), None, "{not_mark:?}");
        }
        // Diagnosed as "no licence", never as an unverifiable one.
        let dir = plan_dir("mark-only");
        let onboarding = dir.join("onboarding.json");
        std::fs::write(&onboarding, onboarding_stamped(NOW_SECS - 86_400)).unwrap();
        let billing = dir.join(BILLING_FILE);
        std::fs::write(&billing, &text).unwrap();
        let (signing, keys) = trial_signer();
        let relay = dir.join("trial_relay.json");
        std::fs::write(&relay, signed_trial_relay(&signing, "dev-trial", NOW_SECS - 86_400)).unwrap();
        let got = FilePlanSource::at(onboarding)
            .with_billing(billing)
            .with_trial_relay(relay)
            .with_binding(DeviceBinding::RECORDED_ONLY)
            .diagnose_with_keys(NOW_MS, Some(keys));
        assert_eq!(got.billing, BillingFact::Missing);
        assert_eq!(got.entitlements.status, PlanStatus::Trial);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Audit M7: the standalone faces clamp `now` to the desktop's mark, so a clock set back does
    /// not reopen an ended trial.
    #[test]
    fn file_source_clamps_the_clock_to_the_recorded_mark() {
        let dir = plan_dir("mark-clamp");
        let onboarding = dir.join("onboarding.json");
        let start_secs = NOW_SECS - 10 * 86_400;
        std::fs::write(&onboarding, onboarding_stamped(start_secs)).unwrap();
        let billing = dir.join(BILLING_FILE);
        // The desktop last saw day 10 (trial over); the clock now reads day 2.
        std::fs::write(&billing, serialize_time_mark(NOW_MS)).unwrap();
        let (signing, keys) = trial_signer();
        let relay = dir.join("trial_relay.json");
        std::fs::write(&relay, signed_trial_relay(&signing, "dev-trial", start_secs)).unwrap();
        let src = FilePlanSource::at(onboarding.clone())
            .with_billing(billing.clone())
            .with_trial_relay(relay)
            .with_binding(DeviceBinding::RECORDED_ONLY);
        let day2 = (start_secs as u64 + 2 * 86_400) * 1000;
        assert_eq!(src.resolve_with_keys(day2, Some(&keys)).status, PlanStatus::TrialExpired, "rolled back: still over");
        assert_eq!(src.diagnose_with_keys(day2, Some(keys)).lock, Some(PlanLock::TrialEnded));
        // Without the mark the same reading is a running trial — the mark is what closes it.
        std::fs::remove_file(&billing).unwrap();
        assert_eq!(src.resolve_with_keys(day2, Some(&keys)).status, PlanStatus::Trial);
        // Inside the skew tolerance the clock is believed.
        std::fs::write(&billing, serialize_time_mark(day2 + 60_000)).unwrap();
        assert_eq!(src.resolve_with_keys(day2, Some(&keys)).status, PlanStatus::Trial);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The key the tests' licence API signs with, and the keys a build trusting it holds.
    fn trial_signer() -> (ed25519_dalek::SigningKey, TrustedKeys) {
        let signing = ed25519_dalek::SigningKey::from_bytes(&[21u8; 32]);
        let keys = TrustedKeys::single(signing.verifying_key().to_bytes());
        (signing, keys)
    }

    /// A trial token the way `/api/license/trial` signs it (`buildTrialTokenPayload`).
    fn mint_trial_token(signing: &ed25519_dalek::SigningKey, device: &str, iat: u64, exp: u64, end: u64) -> String {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        use ed25519_dalek::Signer;
        let body = serde_json::json!({
            "v": 1, "lic": format!("trial_{device}"), "plan": "trial", "status": "trialing",
            "device": device, "iat": iat, "exp": exp, "period_end": end,
            "cancel_at_period_end": true, "grace_days": 0,
        })
        .to_string();
        format!(
            "v1.{}.{}",
            URL_SAFE_NO_PAD.encode(body.as_bytes()),
            URL_SAFE_NO_PAD.encode(signing.sign(body.as_bytes()).to_bytes())
        )
    }

    /// `trial_relay.json` as the desktop writes it once the licence API answered for a trial that
    /// started at `start_secs`: a token bound to `device`, issued at the start, and the end.
    fn signed_trial_relay(signing: &ed25519_dalek::SigningKey, device: &str, start_secs: i64) -> String {
        use crate::trial_relay::{serialize_trial_relay, TrialRelay};
        let iat = start_secs as u64;
        let end = iat + TRIAL_DURATION_MS / 1000;
        serialize_trial_relay(&TrialRelay {
            device_id: device.into(),
            token: Some(mint_trial_token(signing, device, iat, iat + 24 * 3600, end)),
            expires_at: Some(iat + 24 * 3600),
            checked_at: iat,
            trial_ends_at: Some(end),
            signed_out_at: None,
            reason: None,
        })
    }

    /// Issue #1031: the trial runs only on the signed trial token in `trial_relay.json`, and ends
    /// at its end, whatever `onboarding.json` or the plain fields beside the token say.
    #[test]
    fn file_source_runs_the_trial_only_on_the_signed_trial_end() {
        use crate::trial_relay::{serialize_trial_relay, TrialRelay};
        let (signing, keys) = trial_signer();
        let dir = plan_dir("signed-end");
        let onboarding = dir.join("onboarding.json");
        let relay = dir.join("trial_relay.json");
        let src = FilePlanSource::at(onboarding.clone())
            .with_billing(dir.join(BILLING_FILE))
            .with_trial_relay(relay.clone())
            .with_binding(DeviceBinding::RECORDED_ONLY);
        let status = |now: u64| src.resolve_with_keys(now, Some(&keys)).status;
        let lock = |now: u64| src.diagnose_with_keys(now, Some(keys)).lock;

        // The server's trial started six days ago; its token sits in the record.
        let server_start = NOW_SECS - 6 * 86_400;
        let server_end_ms = (server_start as u64 + TRIAL_DURATION_MS / 1000) * 1000;
        std::fs::write(&relay, signed_trial_relay(&signing, "dev-relay", server_start)).unwrap();
        // `onboarding.json` deleted and re-stamped an hour ago: still the server's end.
        std::fs::write(&onboarding, onboarding_stamped(NOW_SECS - 3600)).unwrap();
        assert_eq!(status(NOW_MS), PlanStatus::Trial);
        // The end the faces show is the server's, not the newer stamp + 7 days.
        assert_eq!(src.trial_ends_at_secs_with_keys(NOW_MS, Some(&keys)), Some(server_end_ms / 1000));
        assert_eq!(status(server_end_ms - 1), PlanStatus::Trial);
        assert_eq!(status(server_end_ms), PlanStatus::TrialExpired);
        assert_eq!(lock(server_end_ms), Some(PlanLock::TrialEnded));
        // `onboarding.json` deleted: not started, whatever the token says.
        std::fs::remove_file(&onboarding).unwrap();
        assert_eq!(status(NOW_MS), PlanStatus::TrialNotStarted);

        // `trial_relay.json` deleted, or its token removed and the plain end pushed out: a stamp
        // alone runs nothing (sign in), with every gate closed.
        std::fs::write(&onboarding, onboarding_stamped(NOW_SECS - 3600)).unwrap();
        std::fs::remove_file(&relay).unwrap();
        let alone = src.resolve_with_keys(NOW_MS, Some(&keys));
        assert_eq!(alone.status, PlanStatus::TrialNotStarted);
        assert!(!alone.memory_api && !alone.agent_execution && !alone.local_capture && !alone.memory_access);
        assert_eq!(lock(NOW_MS), Some(PlanLock::TrialNotStarted));
        let edited = TrialRelay {
            device_id: "dev-relay".into(),
            token: None,
            expires_at: None,
            checked_at: NOW_SECS as u64,
            trial_ends_at: Some(NOW_SECS as u64 + 30 * 86_400),
            signed_out_at: None,
            reason: None,
        };
        std::fs::write(&relay, serialize_trial_relay(&edited)).unwrap();
        assert_eq!(status(NOW_MS), PlanStatus::TrialNotStarted);
        // The licence API said the trial is over (no token, its end passed): over.
        let over = TrialRelay { trial_ends_at: Some(NOW_SECS as u64 - 3600), ..edited.clone() };
        std::fs::write(&relay, serialize_trial_relay(&over)).unwrap();
        assert_eq!(status(NOW_MS), PlanStatus::TrialExpired);
        assert_eq!(lock(NOW_MS), Some(PlanLock::TrialEnded));
        assert_eq!(src.trial_ends_at_secs_with_keys(NOW_MS, Some(&keys)), None, "no end once over");
        // Another account's trial on this Mac (PR #1314 audit): the refusal is what the faces show.
        let used = TrialRelay { trial_ends_at: None, reason: Some("trial_used".into()), ..edited.clone() };
        std::fs::write(&relay, serialize_trial_relay(&used)).unwrap();
        assert_eq!(status(NOW_MS), PlanStatus::TrialNotStarted);
        assert_eq!(src.trial_refusal(PlanStatus::TrialNotStarted).as_deref(), Some("trial_used"));

        // A token for another device, or signed by another key, proves nothing.
        let stranger = ed25519_dalek::SigningKey::from_bytes(&[22u8; 32]);
        for (device, signer) in [("dev-other", &signing), ("dev-relay", &stranger)] {
            let iat = server_start as u64;
            let token = mint_trial_token(signer, device, iat, iat + 3600, iat + TRIAL_DURATION_MS / 1000);
            let forged = TrialRelay { token: Some(token), trial_ends_at: None, ..edited.clone() };
            std::fs::write(&relay, serialize_trial_relay(&forged)).unwrap();
            assert_eq!(status(NOW_MS), PlanStatus::TrialNotStarted, "{device}");
        }
        // The shipped key never trusts the tests' token.
        std::fs::write(&relay, signed_trial_relay(&signing, "dev-relay", server_start)).unwrap();
        assert_eq!(src.resolve(NOW_MS).status, PlanStatus::TrialNotStarted);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_source_grants_standard_the_trial_bonus_only_from_the_token() {
        let signing = ed25519_dalek::SigningKey::from_bytes(&[17u8; 32]);
        let keys = TrustedKeys::single(signing.verifying_key().to_bytes());
        let device = "dev-bonus";
        let running = onboarding_stamped(NOW_SECS - 86_400);
        let plain = mint_token(&signing, device, "standard", "active", NOW_SECS - 60, NOW_SECS + 3600);
        let with_claim = {
            use base64::engine::general_purpose::URL_SAFE_NO_PAD;
            use base64::Engine as _;
            use ed25519_dalek::Signer;
            let body = serde_json::json!({
                "v": 1, "lic": "lic-bonus", "plan": "standard", "status": "active", "device": device,
                "iat": NOW_SECS - 60, "exp": NOW_SECS + 3600, "period_end": 2_000_000_000i64,
                "cancel_at_period_end": false, "grace_days": 14, "trial_until": NOW_SECS + 86_400,
            })
            .to_string();
            format!(
                "v1.{}.{}",
                URL_SAFE_NO_PAD.encode(body.as_bytes()),
                URL_SAFE_NO_PAD.encode(signing.sign(body.as_bytes()).to_bytes())
            )
        };
        for (tag, token, want_api) in [("plain", plain, false), ("claim", with_claim, true)] {
            let dir = plan_dir(&format!("bonus-{tag}"));
            let onboarding = dir.join("onboarding.json");
            std::fs::write(&onboarding, &running).unwrap();
            let billing = dir.join(BILLING_FILE);
            std::fs::write(
                &billing,
                serialize_billing_snapshot(&BillingSnapshot {
                    device_id: device.into(),
                    token,
                    verified_at: None,
                    max_seen_ms: None,
                }),
            )
            .unwrap();
            let src = FilePlanSource::at(onboarding)
                .with_billing(billing)
                .with_binding(DeviceBinding::RECORDED_ONLY);
            let got = src.resolve_with_keys(NOW_MS, Some(&keys));
            assert_eq!(got.status, PlanStatus::Standard, "{tag}");
            assert_eq!(got.memory_api, want_api, "{tag}: a running local trial alone grants nothing");
            assert_eq!(src.diagnose_with_keys(NOW_MS, Some(keys)).entitlements, got, "{tag}");
            if want_api {
                // …and the claim ends when the token says, not when the stamp would.
                assert!(!src.resolve_with_keys(NOW_MS + 86_400 * 1000, Some(&keys)).memory_api);
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn garbage_billing_files_are_no_billing_record() {
        for text in [
            "",
            "not json",
            r#"{"version":0,"device_id":"d","token":"t"}"#,
            r#"{"version":1,"device_id":"","token":"t"}"#,
            r#"{"version":1,"device_id":"d"}"#,
        ] {
            assert_eq!(parse_billing_snapshot(text), None, "{text:?}");
        }
    }

    #[test]
    fn an_unverifiable_token_never_grants_a_plan() {
        // No public key configured in a test build (and a garbage token anyway) → Unknown, which
        // falls back to the trial rules rather than to access.
        let snap = BillingSnapshot {
            device_id: "dev-1".into(),
            token: "v1.not-a-real-token.sig".into(),
            verified_at: None,
            max_seen_ms: None,
        };
        assert_eq!(billing_state_of(&snap, 0), BillingState::Unknown);
    }

    /// Billing wins over the trial clock (`resolve_plan`), and the token is what carries it (#8).
    #[test]
    fn a_valid_token_beats_an_expired_trial() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        use ed25519_dalek::{Signer, SigningKey};

        let signing = SigningKey::from_bytes(&[3u8; 32]);
        let body = serde_json::json!({
            "v": 1, "lic": "lic-1", "plan": "pro", "status": "active", "device": "dev-1",
            "iat": 0, "exp": 3600, "period_end": 1_800_000_000i64,
            "cancel_at_period_end": false, "grace_days": 14,
        })
        .to_string();
        let token = format!(
            "v1.{}.{}",
            URL_SAFE_NO_PAD.encode(body.as_bytes()),
            URL_SAFE_NO_PAD.encode(signing.sign(body.as_bytes()).to_bytes())
        );
        let snap = BillingSnapshot { device_id: "dev-1".into(), token, verified_at: None, max_seen_ms: None };

        let key = signing.verifying_key().to_bytes();
        let state = shogun_license::billing_state_from_token(&snap.token, &key, &snap.device_id, 0);
        assert_eq!(state, BillingState::Active(PaidPlan::Pro));

        // …and that state overrides a trial stamped 30 days ago.
        let stamp = Some(0);
        let now = 30 * 24 * 3600 * 1000;
        let plan = resolve_plan(stamp, state);
        assert_eq!(entitlements(plan, now).status, PlanStatus::Pro);
    }

    const DAY_MS: u64 = 24 * 3600 * 1000;

    /// Mint a token the way the licence API does, so the journey below exercises the real
    /// verifier rather than a hand-built `BillingState`.
    fn mint_token(
        signing: &ed25519_dalek::SigningKey,
        device: &str,
        plan: &str,
        status: &str,
        iat_secs: i64,
        exp_secs: i64,
    ) -> String {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        use ed25519_dalek::Signer;

        let body = serde_json::json!({
            "v": 1, "lic": "lic-journey", "plan": plan, "status": status, "device": device,
            "iat": iat_secs, "exp": exp_secs, "period_end": 2_000_000_000i64,
            "cancel_at_period_end": false, "grace_days": 14,
        })
        .to_string();
        format!(
            "v1.{}.{}",
            URL_SAFE_NO_PAD.encode(body.as_bytes()),
            URL_SAFE_NO_PAD.encode(signing.sign(body.as_bytes()).to_bytes())
        )
    }

    /// 要件 §6.12 受け入れ基準: トライアル満了 → 課金 → 復元, walked as one timeline on a mocked
    /// clock, through the real seam — signed token → `verify` → `billing_state` → `resolve_plan`
    /// → `entitlements`.
    ///
    /// Pins the composition across crates, which is where a plan gate actually fails, and two
    /// things the separate tests cannot see together: buying
    /// **Standard** restores Standard and not the Pro-equivalent trial the user just lost, and
    /// local capture and search survive every single step (least-destructive posture — never
    /// punch holes in year-scale memory).
    #[test]
    fn trial_expiry_then_purchase_then_restore_across_the_whole_seam() {
        let signing = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let key = signing.verifying_key().to_bytes();
        let device = "dev-journey";
        let trial_start_ms = 0u64;
        let stamp = Some(trial_start_ms);

        // Day 3 — trialing, everything unlocked.
        let day3 = entitlements(resolve_plan(stamp, BillingState::Unknown), 3 * DAY_MS);
        assert_eq!(day3.status, PlanStatus::Trial);
        assert!(day3.agent_execution && day3.memory_api);

        // Day 8 — the trial is over and the paid surfaces are shut.
        let expired_at = trial_start_ms + TRIAL_DURATION_MS + DAY_MS;
        let expired = entitlements(resolve_plan(stamp, BillingState::Unknown), expired_at);
        assert_eq!(expired.status, PlanStatus::TrialExpired);
        assert!(!expired.first_layer_reads && !expired.agent_execution && !expired.memory_api);

        // Day 8, minutes later — they buy Standard. The signed token is the whole restore path.
        let bought_at_secs = (expired_at / 1000) as i64;
        let token = mint_token(
            &signing,
            device,
            "standard",
            "active",
            bought_at_secs,
            bought_at_secs + 26 * 3600,
        );
        let paid = shogun_license::billing_state_from_token(&token, &key, device, expired_at);
        assert_eq!(paid, BillingState::Active(PaidPlan::Standard));

        let restored = entitlements(resolve_plan(stamp, paid), expired_at);
        assert_eq!(restored.status, PlanStatus::Standard, "restored within the same instant");
        assert!(restored.first_layer_reads && restored.background_intelligence);
        // #940: the purchase brings capture and memory access back with nothing deleted.
        assert!(restored.local_capture && restored.memory_access);
        assert!(
            restored.agent_execution && !restored.memory_api && !restored.composio_send_unlock,
            "buying Standard restores Standard — agents yes (2026-09-24), API and Composio send no"
        );

        // Offline on the paid plan (FR-BIL-09): full access through the grace window, amber from
        // day 7, and only then back to the expired posture. The trial stamp is long gone, so this
        // also proves the fallback is the locked state and not a second trial.
        for (days_offline, want_paid) in [(1u64, true), (6, true), (7, true), (13, true), (14, false)] {
            let now = expired_at + days_offline * DAY_MS;
            let state = shogun_license::billing_state_from_token(&token, &key, device, now);
            let e = entitlements(resolve_plan(stamp, state), now);
            if want_paid {
                assert_eq!(e.status, PlanStatus::Standard, "day {days_offline} offline still paid");
                assert!(e.local_capture && e.memory_access, "day {days_offline}: grace keeps memory");
            } else {
                assert_eq!(e.status, PlanStatus::TrialExpired, "day {days_offline} past the window");
                assert!(!e.first_layer_reads, "past the window the paid surfaces close");
                // #940: past the grace window capture stops and memory locks too
                assert!(!e.local_capture && !e.memory_access, "day {days_offline}: memory locks");
            }
        }

        // Cancelling is the same shape: the API stops issuing, so the last token simply ages out
        // — which the day-14 row above already walks. What matters is where it lands, and that a
        // wiped onboarding stamp cannot turn a lapsed subscription back into a fresh trial.
        let after = expired_at + 20 * DAY_MS;
        let aged = shogun_license::billing_state_from_token(&token, &key, device, after);
        assert_eq!(aged, BillingState::Lapsed, "an aged-out token is lapsed, never Unknown");
        let no_stamp = entitlements(resolve_plan(None, aged), after);
        assert_eq!(
            no_stamp.status,
            PlanStatus::TrialExpired,
            "deleting onboarding.json must not re-grant a trial to a lapsed account"
        );
        assert!(!no_stamp.local_capture && !no_stamp.memory_access);
    }

    // Lock reasons (issue #744)

    const NOW_SECS: i64 = 1_800_000_000;
    const NOW_MS: u64 = NOW_SECS as u64 * 1000;

    /// A fresh folder standing in for the app-data directory of one test case.
    fn plan_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("shogun-plan-diag-{tag}-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn onboarding_stamped(start_secs: i64) -> String {
        format!(r#"{{"version":1,"state":{{"completed":true,"step":"ready","trial_started_at":{start_secs}}}}}"#)
    }

    /// The trial clock and saved licence one case writes, `None` meaning "no such file".
    struct Case {
        name: &'static str,
        onboarding: Option<String>,
        billing: Option<String>,
    }

    fn cases(signing: &ed25519_dalek::SigningKey) -> Vec<(Case, Option<PlanLock>)> {
        let other = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let device = "dev-diag";
        let fresh = |key: &ed25519_dalek::SigningKey, plan: &str| {
            mint_token(key, device, plan, "active", NOW_SECS - 60, NOW_SECS + 3600)
        };
        // Issued 20 days ago, valid a day, 14 days of grace: past the window now.
        let lapsed = mint_token(signing, device, "pro", "active", NOW_SECS - 20 * 86_400, NOW_SECS - 19 * 86_400);
        let billing = |token: String| {
            Some(serialize_billing_snapshot(&BillingSnapshot {
                device_id: device.into(),
                token,
                verified_at: None,
                max_seen_ms: None,
            }))
        };
        let expired = Some(onboarding_stamped(NOW_SECS - 30 * 86_400));
        let running = Some(onboarding_stamped(NOW_SECS - 86_400));
        let unstamped = Some(r#"{"version":1,"state":{"completed":false,"step":"welcome"}}"#.to_string());
        vec![
            (Case { name: "trial running", onboarding: running.clone(), billing: None }, None),
            (Case { name: "trial ended", onboarding: expired.clone(), billing: None }, Some(PlanLock::TrialEnded)),
            (Case { name: "no onboarding file", onboarding: None, billing: None }, Some(PlanLock::TrialNotStarted)),
            (Case { name: "setup not finished", onboarding: unstamped, billing: None }, Some(PlanLock::TrialNotStarted)),
            (
                Case { name: "standard licence", onboarding: expired.clone(), billing: billing(fresh(signing, "standard")) },
                Some(PlanLock::StandardPlan),
            ),
            (Case { name: "pro licence", onboarding: expired.clone(), billing: billing(fresh(signing, "pro")) }, None),
            (
                Case { name: "pro licence past its grace window", onboarding: expired.clone(), billing: billing(lapsed.clone()) },
                Some(PlanLock::RefreshNeeded),
            ),
            (
                // Lapsed falls back to the trial clock, and a running trial still unlocks.
                Case { name: "lapsed licence while the trial runs", onboarding: running, billing: billing(lapsed) },
                None,
            ),
            (
                Case { name: "licence signed by another key", onboarding: expired.clone(), billing: billing(fresh(&other, "pro")) },
                Some(PlanLock::Unverified),
            ),
            (
                Case {
                    name: "licence for another device id",
                    onboarding: expired.clone(),
                    billing: Some(serialize_billing_snapshot(&BillingSnapshot {
                        device_id: "some-other-device".into(),
                        token: fresh(signing, "pro"),
                        verified_at: None,
                        max_seen_ms: None,
                    })),
                },
                Some(PlanLock::Unverified),
            ),
            (Case { name: "unparseable billing file", onboarding: expired, billing: Some("not json".into()) }, Some(PlanLock::Unverified)),
        ]
    }

    fn source_for(case: &Case, dir: &std::path::Path, signing: &ed25519_dalek::SigningKey) -> FilePlanSource {
        let onboarding = dir.join("onboarding.json");
        let billing = dir.join(BILLING_FILE);
        let relay = dir.join("trial_relay.json");
        if let Some(text) = &case.onboarding {
            std::fs::write(&onboarding, text).unwrap();
            // The licence API's trial token for that stamp, as the desktop records it.
            if let Some(stamp_ms) = parse_trial_started_at_ms(text) {
                std::fs::write(&relay, signed_trial_relay(signing, "dev-trial", (stamp_ms / 1000) as i64)).unwrap();
            }
        }
        if let Some(text) = &case.billing {
            std::fs::write(&billing, text).unwrap();
        }
        // The pre-#777 recorded-id check, so these cases read the same on a Mac (which has a
        // hardware id) as on Linux CI. The hardware binding has its own cases below.
        FilePlanSource::at(onboarding)
            .with_billing(billing)
            .with_trial_relay(relay)
            .with_binding(DeviceBinding::RECORDED_ONLY)
    }

    #[test]
    fn every_lock_names_its_cause_and_an_unlocked_plan_names_none() {
        let signing = ed25519_dalek::SigningKey::from_bytes(&[11u8; 32]);
        let key = signing.verifying_key().to_bytes();
        for (i, (case, want)) in cases(&signing).into_iter().enumerate() {
            let dir = plan_dir(&format!("reason-{i}"));
            let got = source_for(&case, &dir, &signing).diagnose_with_keys(NOW_MS, Some(TrustedKeys::single(key)));
            assert_eq!(got.lock, want, "{}", case.name);
            assert_eq!(got.lock.is_none(), got.entitlements.memory_api, "{}: lock iff gate closed", case.name);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn the_diagnosis_reaches_the_gate_decision_resolve_reaches() {
        // With the shipped key none of the test licences verify, so this compares against the
        // real `resolve` path; with the test key it compares against the same pure functions fed
        // the verifier's own answer.
        let signing = ed25519_dalek::SigningKey::from_bytes(&[11u8; 32]);
        let key = signing.verifying_key().to_bytes();
        for (i, (case, _)) in cases(&signing).into_iter().enumerate() {
            let dir = plan_dir(&format!("same-{i}"));
            let src = source_for(&case, &dir, &signing);
            assert_eq!(src.diagnose(NOW_MS).entitlements, src.resolve(NOW_MS), "{}: shipped key", case.name);

            let stamp = case.onboarding.as_deref().and_then(parse_trial_started_at_ms);
            let state = case
                .billing
                .as_deref()
                .and_then(parse_billing_snapshot)
                .map_or(BillingState::Unknown, |snap| {
                    shogun_license::billing_state_from_token(&snap.token, &key, &snap.device_id, NOW_MS)
                });
            assert_eq!(
                src.diagnose_with_keys(NOW_MS, Some(TrustedKeys::single(key))).entitlements,
                entitlements(resolve_plan(stamp, state), NOW_MS),
                "{}: test key",
                case.name
            );
            assert_eq!(
                src.resolve_with_keys(NOW_MS, Some(&TrustedKeys::single(key))),
                entitlements(resolve_plan(stamp, state), NOW_MS),
                "{}: resolve, test key",
                case.name
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn unresolvable_paths_are_plan_not_found_not_an_expired_trial() {
        let src = FilePlanSource {
            path: None,
            billing_path: None,
            trial_relay_path: None,
            binding: DeviceBinding::RECORDED_ONLY,
            mirrors: Some(KeychainPlanMirrors::default()),
        };
        let got = src.diagnose(NOW_MS);
        assert_eq!((got.trial, got.billing), (TrialFact::NoPath, BillingFact::NoPath));
        assert_eq!(got.lock, Some(PlanLock::PlanNotFound));
        assert_eq!(got.entitlements, src.resolve(NOW_MS));
    }

    #[test]
    fn an_unreadable_file_is_told_apart_from_a_missing_one() {
        // A directory where the file should be: exists, cannot be read as text.
        let dir = plan_dir("unreadable");
        let onboarding = dir.join("onboarding.json");
        std::fs::create_dir_all(&onboarding).unwrap();
        let billing = dir.join(BILLING_FILE);
        std::fs::create_dir_all(&billing).unwrap();
        let got = FilePlanSource::at(onboarding).with_billing(billing).diagnose(NOW_MS);
        assert_eq!((got.trial, got.billing), (TrialFact::Unreadable, BillingFact::Unreadable));
        assert_eq!(got.lock, Some(PlanLock::Unverified), "an unreadable saved licence is the actionable part");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Hardware device binding (issue #777)

    /// Mac A's and Mac B's hardware-derived ids, and the random id a pre-#777 build saved in Mac
    /// A's `billing.json`.
    const HW_A: &str = "4H9iyD3A6qHY2K_QmvBt6BY9XVGThDWcDpTCyvV_iHM";
    const HW_B: &str = "YLBItJt5pOm21XXUG5u3Yp_iuTj6PjOY-uvskMDp9Ig";
    const RECORDED_A: &str = "0b7f3c1e-5a2d-4c6b-9e8f-1a2b3c4d5e6f";
    /// The cutoff these cases are built around: ten days before "now".
    const CUTOFF_SECS: u64 = NOW_SECS as u64 - 10 * 86_400;

    fn mac(hardware: &'static str) -> DeviceBinding<'static> {
        DeviceBinding { hardware: Some(hardware), legacy_cutoff_secs: CUTOFF_SECS }
    }

    /// Mac A's `billing.json`: a Pro token bound to `token_device`, beside `file_device`.
    fn billing_file(signing: &ed25519_dalek::SigningKey, token_device: &str, file_device: &str, iat: i64) -> String {
        serialize_billing_snapshot(&BillingSnapshot {
            device_id: file_device.into(),
            token: mint_token(signing, token_device, "pro", "active", iat, iat + 26 * 3600),
            verified_at: Some(iat),
            max_seen_ms: None,
        })
    }

    /// Resolve `billing` (with an expired trial) as the Mac `binding` describes.
    fn resolve_on(binding: DeviceBinding<'static>, billing: &str, signing: &ed25519_dalek::SigningKey, tag: &str) -> PlanDiagnosis {
        let dir = plan_dir(tag);
        let onboarding = dir.join("onboarding.json");
        std::fs::write(&onboarding, onboarding_stamped(NOW_SECS - 30 * 86_400)).unwrap();
        let path = dir.join(BILLING_FILE);
        std::fs::write(&path, billing).unwrap();
        // This Mac's own (ended) trial, so a refused licence reads as "trial ended".
        let relay = dir.join("trial_relay.json");
        let device = binding.hardware.unwrap_or("dev-trial");
        std::fs::write(&relay, signed_trial_relay(signing, device, NOW_SECS - 30 * 86_400)).unwrap();
        let got = FilePlanSource::at(onboarding)
            .with_billing(path)
            .with_trial_relay(relay)
            .with_binding(binding)
            .diagnose_with_keys(NOW_MS, Some(TrustedKeys::single(signing.verifying_key().to_bytes())));
        let _ = std::fs::remove_dir_all(&dir);
        got
    }

    /// A copy of Mac A's billing.json on Mac B must not make Mac B Pro. The file names A's device
    /// id and the token says A, but Mac B's hardware says B.
    #[test]
    fn a_copied_billing_file_does_not_carry_the_plan_to_another_mac() {
        let signing = ed25519_dalek::SigningKey::from_bytes(&[13u8; 32]);
        let key = signing.verifying_key().to_bytes();
        let copied = billing_file(&signing, HW_A, HW_A, NOW_SECS - 60);

        let on_b = resolve_on(mac(HW_B), &copied, &signing, "copy-b");
        assert_eq!(on_b.billing, BillingFact::Unverified("device_mismatch"));
        assert_eq!(on_b.entitlements.status, PlanStatus::TrialExpired);
        assert!(!on_b.entitlements.memory_api && !on_b.entitlements.agent_execution);
        assert_eq!(on_b.lock, Some(PlanLock::Unverified));

        let snap = parse_billing_snapshot(&copied).unwrap();
        assert_eq!(
            billing_state_with(&snap, Clock::at(NOW_MS), Some(&TrustedKeys::single(key)), mac(HW_B)),
            BillingState::Unknown
        );

        // Even a copy of a hardware-bound file from before the cutoff: never treated as legacy.
        let early = billing_file(&signing, HW_A, HW_A, CUTOFF_SECS as i64 - 86_400);
        assert_eq!(resolve_on(mac(HW_B), &early, &signing, "copy-early-b").entitlements.status, PlanStatus::TrialExpired);

        // The same copy as a legacy file (random recorded id, minted by an old client after the
        // cutoff) fares no better.
        let legacy_copy = billing_file(&signing, RECORDED_A, RECORDED_A, NOW_SECS - 60);
        let on_b = resolve_on(mac(HW_B), &legacy_copy, &signing, "copy-legacy-b");
        assert_eq!(on_b.entitlements.status, PlanStatus::TrialExpired);
        assert_eq!(on_b.lock, Some(PlanLock::Unverified));
    }

    #[test]
    fn a_hardware_bound_licence_unlocks_its_own_mac() {
        let signing = ed25519_dalek::SigningKey::from_bytes(&[13u8; 32]);
        let own = billing_file(&signing, HW_A, HW_A, NOW_SECS - 60);
        let got = resolve_on(mac(HW_A), &own, &signing, "own-a");
        assert_eq!(got.billing, BillingFact::Verified(BillingState::Active(PaidPlan::Pro)));
        assert_eq!(got.entitlements.status, PlanStatus::Pro);
        assert_eq!(got.lock, None);
        // A stale recorded id beside a hardware-bound token does not matter.
        let mixed = billing_file(&signing, HW_A, RECORDED_A, NOW_SECS - 60);
        assert_eq!(resolve_on(mac(HW_A), &mixed, &signing, "own-mixed").entitlements.status, PlanStatus::Pro);
    }

    /// Updating while offline: Mac A's cached token is the old client's, bound to the random id.
    #[test]
    fn a_legacy_licence_issued_before_the_cutoff_keeps_working() {
        let signing = ed25519_dalek::SigningKey::from_bytes(&[13u8; 32]);
        // Issued 11 days ago, a day before the cutoff: past exp, inside the 14-day grace.
        let legacy = billing_file(&signing, RECORDED_A, RECORDED_A, CUTOFF_SECS as i64 - 86_400);
        let got = resolve_on(mac(HW_A), &legacy, &signing, "legacy-before");
        assert_eq!(got.billing, BillingFact::Verified(BillingState::Active(PaidPlan::Pro)));
        assert_eq!(got.entitlements.status, PlanStatus::Pro);
    }

    #[test]
    fn a_legacy_licence_issued_after_the_cutoff_is_refused() {
        let signing = ed25519_dalek::SigningKey::from_bytes(&[13u8; 32]);
        let legacy = billing_file(&signing, RECORDED_A, RECORDED_A, CUTOFF_SECS as i64 + 60);
        let got = resolve_on(mac(HW_A), &legacy, &signing, "legacy-after");
        assert_eq!(got.billing, BillingFact::Unverified("device_mismatch"));
        assert_eq!(got.entitlements.status, PlanStatus::TrialExpired);
    }

    /// A Mac with no usable hardware UUID (some VMs): the saved id decides.
    #[test]
    fn without_a_hardware_id_the_saved_device_id_decides() {
        let signing = ed25519_dalek::SigningKey::from_bytes(&[13u8; 32]);
        let own = billing_file(&signing, RECORDED_A, RECORDED_A, NOW_SECS - 60);
        let got = resolve_on(DeviceBinding::RECORDED_ONLY, &own, &signing, "fallback");
        assert_eq!(got.entitlements.status, PlanStatus::Pro);
        let foreign = billing_file(&signing, RECORDED_A, "some-other-device", NOW_SECS - 60);
        let got = resolve_on(DeviceBinding::RECORDED_ONLY, &foreign, &signing, "fallback-foreign");
        assert_eq!(got.entitlements.status, PlanStatus::TrialExpired);
    }

    #[test]
    fn reasons_carry_stable_codes_and_no_paths_or_identifiers() {
        let all = [
            PlanLock::StandardPlan,
            PlanLock::RefreshNeeded,
            PlanLock::Unverified,
            PlanLock::TrialEnded,
            PlanLock::TrialNotStarted,
            PlanLock::PlanNotFound,
        ];
        let codes: std::collections::BTreeSet<_> = all.iter().map(|l| l.code()).collect();
        assert_eq!(codes.len(), all.len(), "codes are unique");
        for lock in all {
            assert!(lock.code().chars().all(|c| c.is_ascii_lowercase() || c == '_'), "{}", lock.code());
            let fix = lock.fix();
            assert!(fix.ends_with('.'), "{fix}");
            assert!(!fix.contains('/') && !fix.contains('~') && !fix.contains("Library"), "no path: {fix}");
        }
    }

    // Audit 2026-10-20 M7: the Keychain mirrors bound what an edited file can claim.

    #[test]
    fn mirrors_take_the_earliest_stamp_and_the_latest_mark() {
        assert_eq!(effective_trial_started_at_ms(Some(5_000), Some(3)), Some(3_000));
        assert_eq!(effective_trial_started_at_ms(Some(2_000), Some(3)), Some(2_000));
        assert_eq!(effective_trial_started_at_ms(None, Some(3)), Some(3_000));
        assert_eq!(effective_trial_started_at_ms(Some(2_000), None), Some(2_000));
        assert_eq!(effective_trial_started_at_ms(None, Some(-1)), None);
        assert_eq!(effective_max_seen_ms(Some(5), Some(9)), Some(9));
        assert_eq!(effective_max_seen_ms(Some(9), None), Some(9));
        assert_eq!(effective_max_seen_ms(None, Some(9)), Some(9));
        assert_eq!(parse_time_mark_mirror(b" 1800000000000\n"), Some(1_800_000_000_000));
        assert_eq!(parse_time_mark_mirror(b"0"), None);
        assert_eq!(parse_time_mark_mirror(b"-5"), None);
        assert_eq!(parse_time_mark_mirror(b"junk"), None);
        assert_eq!(
            parse_time_mark_mirror(serialize_time_mark_mirror(42).as_bytes()),
            Some(42)
        );
        let both = KeychainPlanMirrors::from_bytes(Some(b"100"), Some(b"200"));
        assert_eq!(both, KeychainPlanMirrors { trial_started_at_secs: Some(100), max_seen_ms: Some(200) });
    }

    #[test]
    fn a_later_stamp_written_into_onboarding_json_does_not_restart_the_trial() {
        let dir = plan_dir("m7-stamp");
        let started = NOW_SECS - 10 * 24 * 3600;
        // The file was rewritten to claim the trial started an hour ago.
        std::fs::write(dir.join("onboarding.json"), onboarding_stamped(NOW_SECS - 3600)).unwrap();
        // A signed trial token whose end is still ahead (#1031: no trial runs without one), so
        // the files alone would grant a trial and the mirror is what closes it.
        let (signing, keys) = trial_signer();
        std::fs::write(dir.join("trial_relay.json"), signed_trial_relay(&signing, "dev-trial", NOW_SECS - 4 * 24 * 3600))
            .unwrap();
        let src = FilePlanSource::at(dir.join("onboarding.json"))
            .with_billing(dir.join("billing.json"))
            .with_trial_relay(dir.join("trial_relay.json"))
            .with_binding(DeviceBinding::RECORDED_ONLY);
        let file_only = src.clone().with_keychain_mirrors(KeychainPlanMirrors::default());
        assert!(file_only.resolve_with_keys(NOW_MS, Some(&keys)).memory_api, "the file alone would grant a trial");
        let mirrored = src.with_keychain_mirrors(KeychainPlanMirrors {
            trial_started_at_secs: Some(started),
            max_seen_ms: None,
        });
        let got = mirrored.resolve_with_keys(NOW_MS, Some(&keys));
        assert_eq!(got.status, PlanStatus::TrialExpired);
        assert!(!got.memory_api);
        assert_eq!(mirrored.diagnose_with_keys(NOW_MS, Some(keys)).trial, TrialFact::Stamped(started as u64 * 1000));
        // Deleting the file does not help either.
        std::fs::remove_file(dir.join("onboarding.json")).unwrap();
        assert_eq!(mirrored.resolve_with_keys(NOW_MS, Some(&keys)).status, PlanStatus::TrialExpired);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rolling_back_the_mark_in_billing_json_does_not_revive_an_expired_trial() {
        let dir = plan_dir("m7-mark");
        let started = NOW_SECS;
        std::fs::write(dir.join("onboarding.json"), onboarding_stamped(started)).unwrap();
        // The real time is eight days on; the clock was set back to day one and the file's mark
        // rolled back with it.
        let real_ms = NOW_MS + 8 * DAY_MS;
        let rolled_back_ms = NOW_MS + DAY_MS;
        std::fs::write(dir.join("billing.json"), serialize_time_mark(rolled_back_ms)).unwrap();
        // The licence API's signed token for that trial (#1031), ending on day seven.
        let (signing, keys) = trial_signer();
        std::fs::write(dir.join("trial_relay.json"), signed_trial_relay(&signing, "dev-trial", started)).unwrap();
        let src = FilePlanSource::at(dir.join("onboarding.json"))
            .with_billing(dir.join("billing.json"))
            .with_trial_relay(dir.join("trial_relay.json"))
            .with_binding(DeviceBinding::RECORDED_ONLY);
        let file_only = src.clone().with_keychain_mirrors(KeychainPlanMirrors::default());
        assert!(file_only.resolve_with_keys(rolled_back_ms, Some(&keys)).memory_api, "the file alone is fooled");
        let mirrored = src.with_keychain_mirrors(KeychainPlanMirrors {
            trial_started_at_secs: Some(started),
            max_seen_ms: Some(real_ms),
        });
        assert_eq!(mirrored.resolve_with_keys(rolled_back_ms, Some(&keys)).status, PlanStatus::TrialExpired);
        assert!(!mirrored.diagnose_with_keys(rolled_back_ms, Some(keys)).entitlements.memory_api);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
