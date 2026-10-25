//! macOS Keychain helpers with stable dev access (invariant 7).
//!
//! Secrets use [`SERVICE`]. A new item made from inside the app bundle trusts this binary and its
//! bundled siblings ([`BUNDLE_EXECUTABLES`]), so the Memory API sidecars an AI app launches read
//! the DB key and app grants without a Keychain prompt. Outside a bundle (cargo runs) the ACL is
//! the default one. [`ensure_bundle_acls`] repairs missing bundle entries only for accounts the
//! sidecars share ([`BUNDLE_SHARED_ACCOUNTS`]). Reads are cached in-process so one unlock covers
//! the whole session. Legacy entries under service `SHOGUN` are read once and migrated to
//! [`SERVICE`] on repair; [`crate::keychain_fallback`] decides when the legacy name is consulted.
//! A secret another process rewrites while this one runs must be read with
//! [`get_generic_secret_uncached`].

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use core_foundation::array::CFArray;
use core_foundation::base::{CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::data::CFData;
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::CFString;
use core_foundation_sys::base::CFTypeRef;
use security_framework::base::Result;
use security_framework_sys::base::errSecSuccess;
use security_framework_sys::item::{
    kSecAttrAccount, kSecAttrService, kSecClass, kSecClassGenericPassword, kSecReturnData,
    kSecReturnRef, kSecUseAuthenticationUI, kSecUseAuthenticationUISkip, kSecValueData,
};
use security_framework_sys::keychain_item::{
    SecItemAdd, SecItemCopyMatching, SecItemDelete, SecItemUpdate,
};

use crate::keychain_fallback::{self, Found};

fn cvt(status: i32) -> Result<()> {
    if status == errSecSuccess {
        Ok(())
    } else {
        Err(security_framework::base::Error::from_code(status))
    }
}

/// Keychain "service" field for every SHOGUN secret.
pub const SERVICE: &str = "com.selectkk.shogun";

/// Pre-unification service name still present on some dev machines.
const LEGACY_SERVICE: &str = "SHOGUN";

/// Keychain account for the Select KK credential (Batch lane + live meeting translate).
pub const SELECT_KK_ACCOUNT: &str = "select-kk-batch";

/// Keychain account for the user's Deepgram API key (meeting live STT).
pub const DEEPGRAM_ASR_ACCOUNT: &str = "deepgram-asr";

/// Keychain mirror of `onboarding.json`'s `trial_started_at` (unix seconds, decimal text).
/// Not a secret: it is a copy of the trial clock in a place that deleting
/// the app-data folder does not reach, so re-running onboarding cannot stamp a newer trial. The
/// desktop writes it once and reads it at load; the earliest of file and Keychain wins. Kept by
/// "Delete everything" (`KEPT_ACCOUNTS`), like the licence key: it is the plan's clock, not the
/// user's captured data.
pub const TRIAL_STARTED_AT_ACCOUNT: &str = "trial-started-at";

/// Keychain mirror of `billing.json`'s time high-water mark (`max_seen_ms`, unix ms, decimal
/// text; audit 2026-10-20 M7). Not a secret: a copy of the latest clock reading the desktop has
/// recorded, where rewriting `billing.json` cannot lower it, so a clock set back cannot revive an
/// expired licence or stretch a trial. Written by the desktop, read by every plan source; the
/// later of file and Keychain wins. Kept by "Delete everything" (`KEPT_ACCOUNTS`) like the trial
/// stamp.
pub const TIME_HIGH_WATER_MARK_ACCOUNT: &str = "time-high-water-mark";

/// Which privacy settings files this Mac has sealed at least once (audit 2026-10-20 M6,
/// `shogun_core::settings_seal`): comma-separated ids, no values. It tells the first run after
/// the upgrade (adopt the file) from a seal deleted afterwards (trust nothing the file widens).
/// Kept by "Delete everything" (`KEPT_ACCOUNTS`): the memory key, and so the seals, survive it.
pub const SETTINGS_SEALED_ACCOUNT: &str = "settings-sealed";

/// The account lists "Delete everything" works from (issue #780). They live in a module every
/// platform compiles, so the test that checks them against the tree runs on Linux too.
pub use crate::keychain_accounts::{KEPT_ACCOUNTS, USER_SECRET_ACCOUNTS};

static CACHE: OnceLock<Mutex<HashMap<String, Vec<u8>>>> = OnceLock::new();

/// Read a generic-password secret. Tries [`SERVICE`], then [`LEGACY_SERVICE`]. Migrates legacy-only
/// entries to [`SERVICE`] on first read. Cached after first successful read for this process so
/// startup does not hit the Keychain once per subsystem (DB key, Select KK, Composio, …).
pub fn get_generic_secret(account: &str) -> Result<Vec<u8>> {
    if let Ok(guard) = cache().lock() {
        if let Some(bytes) = guard.get(account) {
            return Ok(bytes.clone());
        }
    }
    let bytes = read_and_migrate(account)?;
    warm_cache(account, &bytes);
    Ok(bytes)
}

/// Read a secret straight from the Keychain, bypassing and never filling the in-process cache.
///
/// For secrets another process rewrites while this one runs: the Memory API token list is
/// rewritten by the desktop on every issue or revoke, and a standalone MCP / REST process re-reads
/// it to refuse a revoked token without a restart (`shogun_mcp::live_tokens`). The cached read
/// would hand back the list from the first read forever.
pub fn get_generic_secret_uncached(account: &str) -> Result<Vec<u8>> {
    read_and_migrate(account)
}

/// Read a secret without allowing macOS to display an authentication dialog.
///
/// Background latency-sensitive work uses this path. A locked item fails fast so callers can use
/// their normal fallback without freezing dictation behind a Keychain prompt.
pub fn get_generic_secret_non_interactive(account: &str) -> Result<Vec<u8>> {
    read_cached_without_migration(account, true)
}

/// Read a secret without an authentication dialog and without the in-process cache: for values
/// another process rewrites while this one runs, read by a process that must never prompt (the
/// plan-clock mirrors the standalone faces read, audit 2026-10-20 M7).
pub fn get_generic_secret_non_interactive_uncached(account: &str) -> Result<Vec<u8>> {
    read_either_service(account, true).map(Found::into_inner)
}

/// Interactive connector restore/retry reads must never rewrite or delete a stored grant.
pub fn get_generic_secret_read_only(account: &str) -> Result<Vec<u8>> {
    read_cached_without_migration(account, false)
}

fn read_cached_without_migration(account: &str, skip_authentication_ui: bool) -> Result<Vec<u8>> {
    if let Ok(guard) = cache().lock() {
        if let Some(bytes) = guard.get(account) {
            return Ok(bytes.clone());
        }
    }
    let bytes = read_either_service(account, skip_authentication_ui)?.into_inner();
    warm_cache(account, &bytes);
    Ok(bytes)
}

/// [`SERVICE`] first; the legacy service only when [`SERVICE`] has no such item.
fn read_either_service(account: &str, skip_authentication_ui: bool) -> Result<Found<Vec<u8>>> {
    keychain_fallback::read_with_legacy_fallback(
        || read_from_keychain(SERVICE, account, skip_authentication_ui).map_err(|e| e.code()),
        || read_from_keychain(LEGACY_SERVICE, account, skip_authentication_ui).map_err(|e| e.code()),
    )
    .map_err(security_framework::base::Error::from_code)
}

/// Read under either service, moving a legacy-only item to [`SERVICE`] (best effort: a failed
/// rewrite does not fail the read).
fn read_and_migrate(account: &str) -> Result<Vec<u8>> {
    match read_either_service(account, false)? {
        Found::Current(bytes) => Ok(bytes),
        Found::Legacy(bytes) => {
            if write_to_keychain(SERVICE, account, &bytes).is_ok() {
                let _ = delete_from_keychain(LEGACY_SERVICE, account);
            }
            Ok(bytes)
        }
    }
}

/// Store a generic-password secret under [`SERVICE`].
pub fn set_generic_secret(account: &str, password: &[u8]) -> Result<()> {
    let _ = delete_from_keychain(LEGACY_SERVICE, account);
    write_to_keychain(SERVICE, account, password)?;
    if let Ok(mut guard) = cache().lock() {
        guard.insert(account.to_string(), password.to_vec());
    }
    Ok(())
}

/// Delete a secret from both service names and drop any in-process cache entry.
pub fn delete_generic_secret(account: &str) -> Result<()> {
    let result = keychain_fallback::delete_current_and_legacy(
        || delete_from_keychain(SERVICE, account).map_err(|error| error.code()),
        || delete_from_keychain(LEGACY_SERVICE, account).map_err(|error| error.code()),
    );
    if let Ok(mut guard) = cache().lock() {
        guard.remove(account);
    }
    result.map_err(security_framework::base::Error::from_code)
}

/// Whether a secret exists for `account`, answered WITHOUT reading it (issue #110: the API faces
/// report which providers have a key, and a presence check must not pull the key into another
/// process). The query asks for no data, so the item's contents are never decrypted or returned.
/// `Some(false)` only when every service answers "not found"; any other failure is `None`
/// (unknown), never a guess.
pub fn generic_secret_present(account: &str) -> Option<bool> {
    use crate::keychain_fallback::ITEM_NOT_FOUND;
    let mut unknown = false;
    for service in [SERVICE, LEGACY_SERVICE] {
        let query = query_dict(service, account, false, false);
        let status = unsafe { SecItemCopyMatching(query.as_concrete_TypeRef(), std::ptr::null_mut()) };
        if status == errSecSuccess {
            return Some(true);
        }
        if status != ITEM_NOT_FOUND {
            unknown = true;
        }
    }
    if unknown {
        None
    } else {
        Some(false)
    }
}

/// Whether a Select KK credential is present and looks like a real Anthropic API key.
pub fn select_kk_configured() -> bool {
    get_select_kk_key().is_some()
}

/// Read the Select KK API key. Keychain first (`SELECT_KK_ACCOUNT` under [`SERVICE`] or
/// [`LEGACY_SERVICE`]); `SHOGUN_SELECT_KK` env is dev provisioning only (recap_probe parity).
pub fn get_select_kk_key() -> Option<String> {
    #[cfg(debug_assertions)]
    if let Ok(k) = std::env::var("SHOGUN_SELECT_KK") {
        let trimmed = k.trim().to_string();
        if let Some(key) = normalize_select_kk_key(&trimmed) {
            return Some(key);
        }
    }
    let bytes = get_generic_secret(SELECT_KK_ACCOUNT).ok()?;
    let raw = decode_secret(bytes)?;
    let key = normalize_select_kk_key(&raw)?;
    // Dev machines sometimes store the key as hex (same shape as the DB encryption key). Heal once.
    if raw != key {
        if let Err(e) = set_generic_secret(SELECT_KK_ACCOUNT, key.as_bytes()) {
            eprintln!("[keychain] could not rewrite normalized {SELECT_KK_ACCOUNT}: {e}");
        } else {
            eprintln!("[keychain] normalized hex-encoded {SELECT_KK_ACCOUNT} to plain text");
        }
    }
    Some(key)
}

/// Whether a Deepgram API key is present in Keychain.
pub fn deepgram_asr_configured() -> bool {
    get_deepgram_asr_key().is_some()
}

/// Read the Deepgram API key from Keychain (`DEEPGRAM_ASR_ACCOUNT` under [`SERVICE`]).
pub fn get_deepgram_asr_key() -> Option<String> {
    let bytes = get_generic_secret(DEEPGRAM_ASR_ACCOUNT).ok()?;
    decode_secret(bytes)
}

/// Store the Deepgram API key for meeting live STT. Plain text only; never logged.
pub fn set_deepgram_asr_key(key: &str) -> Result<(), String> {
    let key = key.trim();
    if key.is_empty() {
        return Err("key is empty".into());
    }
    if key.chars().count() < 8 {
        return Err("key looks too short — paste the full Deepgram API key".into());
    }
    set_generic_secret(DEEPGRAM_ASR_ACCOUNT, key.as_bytes()).map_err(|e| e.to_string())
}

/// Store the Select KK API key (Dream Cycle, recap, live translation). Plain text only.
pub fn set_select_kk_key(key: &str) -> Result<(), String> {
    let key = key.trim();
    let Some(normalized) = normalize_select_kk_key(key) else {
        return Err("key must start with sk-ant- (paste the API key, not hex)".into());
    };
    set_generic_secret(SELECT_KK_ACCOUNT, normalized.as_bytes()).map_err(|e| e.to_string())
}

/// Touch every known account once at startup so later subsystems reuse the in-process cache.
///
/// Missing accounts are ignored (`errSecItemNotFound` does not prompt). Existing items may still
/// show one Keychain dialog per launch in unsigned `tauri dev` builds — run
/// `scripts/codesign-desktop-dev.sh` after building so "Always Allow" survives rebuilds.
pub fn warm_startup_keychain(accounts: &[&str]) {
    for account in accounts {
        if let Ok(bytes) = get_generic_secret(account) {
            eprintln!("[keychain] warmed {account} ({} bytes)", bytes.len());
        }
    }
}

/// Migrate legacy `SHOGUN` service entries to [`SERVICE`] without delete-before-write.
#[deprecated(note = "use warm_startup_keychain — migration now happens inside get_generic_secret")]
pub fn repair_dev_acls(accounts: &[&str]) {
    warm_startup_keychain(accounts);
}

fn decode_secret(bytes: Vec<u8>) -> Option<String> {
    String::from_utf8(bytes)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Accept plain `sk-ant-…` keys and one common dev mistake: the key hex-encoded as ASCII.
fn normalize_select_kk_key(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.starts_with("sk-ant-") {
        return Some(trimmed.to_string());
    }
    if trimmed.len() >= 40
        && trimmed.len() % 2 == 0
        && trimmed.chars().all(|c| c.is_ascii_hexdigit())
    {
        let decoded = decode_hex_ascii(trimmed)?;
        let text = String::from_utf8(decoded).ok()?;
        let text = text.trim();
        if text.starts_with("sk-ant-") {
            return Some(text.to_string());
        }
    }
    None
}

fn decode_hex_ascii(hex: &str) -> Option<Vec<u8>> {
    let bytes = hex.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let hi = hex_nibble(pair[0])?;
        let lo = hex_nibble(pair[1])?;
        out.push((hi << 4) | lo);
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn read_from_keychain(
    service: &str,
    account: &str,
    skip_authentication_ui: bool,
) -> Result<Vec<u8>> {
    let query = query_dict(service, account, true, skip_authentication_ui);
    let mut ret: CFTypeRef = std::ptr::null();
    cvt(unsafe { SecItemCopyMatching(query.as_concrete_TypeRef(), &mut ret) })?;
    let data =
        unsafe { CFData::wrap_under_create_rule(ret as core_foundation_sys::data::CFDataRef) };
    Ok(data.bytes().to_vec())
}

fn write_to_keychain(service: &str, account: &str, password: &[u8]) -> Result<()> {
    let value = CFData::from_buffer(password);
    if read_from_keychain(service, account, false).is_ok() {
        let query = query_dict(service, account, false, false);
        let update = CFDictionary::from_CFType_pairs(&[(
            unsafe { CFString::wrap_under_get_rule(kSecValueData) },
            value.into_CFType(),
        )]);
        return cvt(unsafe {
            SecItemUpdate(query.as_concrete_TypeRef(), update.as_concrete_TypeRef())
        });
    }
    let mut attrs = base_attrs(service, account);
    attrs.push((
        unsafe { CFString::wrap_under_get_rule(kSecValueData) },
        value.into_CFType(),
    ));
    if let Some(access) = bundle_access(account) {
        let mut with_access = attrs.clone();
        with_access.push((unsafe { CFString::wrap_under_get_rule(kSecAttrAccess) }, access));
        let dict = CFDictionary::from_CFType_pairs(&with_access);
        let mut ret = std::ptr::null();
        if unsafe { SecItemAdd(dict.as_concrete_TypeRef(), &mut ret) } == errSecSuccess {
            return Ok(());
        }
        // ponytail: a refused custom ACL falls back to the default one (a prompt, not a lost secret).
    }
    let dict = CFDictionary::from_CFType_pairs(&attrs);
    let mut ret = std::ptr::null();
    cvt(unsafe { SecItemAdd(dict.as_concrete_TypeRef(), &mut ret) })
}

/// The app's executables in `Contents/MacOS`: the desktop app and its Memory API sidecars.
const BUNDLE_EXECUTABLES: &[&str] = &["shogun-desktop-spike", "shogun-mcp", "shogun-api", "shogun"];

#[link(name = "Security", kind = "framework")]
extern "C" {
    static kSecAttrAccess: core_foundation_sys::string::CFStringRef;
    static kSecACLAuthorizationDecrypt: core_foundation_sys::string::CFStringRef;
    fn SecKeychainItemCopyAccess(item: CFTypeRef, access: *mut CFTypeRef) -> i32;
    fn SecAccessCopyMatchingACLList(
        access: CFTypeRef,
        authorization: CFTypeRef,
    ) -> core_foundation_sys::array::CFArrayRef;
    fn SecACLCopyContents(
        acl: CFTypeRef,
        apps: *mut core_foundation_sys::array::CFArrayRef,
        description: *mut core_foundation_sys::string::CFStringRef,
        prompt_selector: *mut u16,
    ) -> i32;
    fn SecTrustedApplicationCopyData(
        app: CFTypeRef,
        data: *mut core_foundation_sys::data::CFDataRef,
    ) -> i32;
    fn SecTrustedApplicationCreateFromPath(
        path: *const std::os::raw::c_char,
        app: *mut CFTypeRef,
    ) -> i32;
    fn SecAccessCreate(
        descriptor: core_foundation_sys::string::CFStringRef,
        trusted_list: core_foundation_sys::array::CFArrayRef,
        access: *mut CFTypeRef,
    ) -> i32;
}

/// An ACL trusting this binary and every other [`BUNDLE_EXECUTABLES`] entry next to it. `None`
/// when this binary is not one of them (a cargo test, a dev tool) or a call fails.
fn bundle_access(account: &str) -> Option<core_foundation::base::CFType> {
    let apps = bundle_applications()?;
    let list = CFArray::from_CFTypes(&apps);
    let label = CFString::new(&format!("{SERVICE} {account}"));
    let mut access: CFTypeRef = std::ptr::null();
    let status = unsafe {
        SecAccessCreate(
            label.as_concrete_TypeRef(),
            list.as_concrete_TypeRef(),
            &mut access,
        )
    };
    (status == errSecSuccess && !access.is_null())
        .then(|| unsafe { CFType::wrap_under_create_rule(access) })
}

fn bundle_applications() -> Option<Vec<CFType>> {
    let exe = std::env::current_exe().ok()?;
    let name = exe.file_name()?.to_str()?;
    if !BUNDLE_EXECUTABLES.contains(&name) {
        return None;
    }
    let dir = exe.parent()?;
    let trusted_app = |path: Option<&std::path::Path>| -> Option<core_foundation::base::CFType> {
        let c = match path {
            Some(p) => Some(std::ffi::CString::new(p.to_str()?).ok()?),
            None => None,
        };
        let mut app: CFTypeRef = std::ptr::null();
        let status = unsafe {
            SecTrustedApplicationCreateFromPath(
                c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
                &mut app,
            )
        };
        (status == errSecSuccess && !app.is_null())
            .then(|| unsafe { core_foundation::base::CFType::wrap_under_create_rule(app) })
    };
    let mut apps = vec![trusted_app(None)?];
    for sibling in BUNDLE_EXECUTABLES.iter().filter(|s| **s != name) {
        let path = dir.join(sibling);
        if path.is_file() {
            apps.push(trusted_app(Some(&path))?);
        }
    }
    Some(apps)
}

fn delete_from_keychain(service: &str, account: &str) -> Result<()> {
    let query = query_dict(service, account, false, false);
    cvt(unsafe { SecItemDelete(query.as_concrete_TypeRef()) })
}

/// Keychain accounts the bundled Memory API sidecars (`shogun-mcp`, `shogun-api`) must read
/// without a prompt: the DB key and the grant/token registry every `tools/call` re-checks.
/// Secrets only the desktop uses stay desktop-only — widening their ACL would grow the blast
/// radius for no benefit.
const BUNDLE_SHARED_ACCOUNTS: &[&str] = &["memory-db-key", "memory-api-tokens"];

/// Repair missing bundle entries on shared items, after memory startup. An access-list update
/// can require approval even when a secret read succeeds. Never rewrite an already configured
/// item: doing so can raise another macOS password dialog on every launch.
pub fn ensure_bundle_acls() {
    for account in BUNDLE_SHARED_ACCOUNTS {
        repair_bundle_acl(account);
    }
}

fn repair_bundle_acl(account: &str) {
    let Some(apps) = bundle_applications() else {
        return;
    };
    // Failed inspection must leave the user's existing access policy alone too.
    match bundle_acl_needs_repair(account, &apps) {
        Some(true) => {}
        Some(false) => {
            eprintln!("[keychain] bundle ACL already configured for {account}; left unchanged");
            return;
        }
        None => return,
    }
    let Some(access) = bundle_access(account) else {
        return;
    };
    // Read with no UI: only a process the item already trusts (its creator) gets the bytes
    // silently; a locked-out sidecar or dev binary gets an error here, never a dialog.
    if read_from_keychain(SERVICE, account, true).is_err() {
        return;
    }
    let query = query_dict(SERVICE, account, false, false);
    let update = CFDictionary::from_CFType_pairs(&[(
        unsafe { CFString::wrap_under_get_rule(kSecAttrAccess) },
        access,
    )]);
    let status =
        unsafe { SecItemUpdate(query.as_concrete_TypeRef(), update.as_concrete_TypeRef()) };
    if status == errSecSuccess {
        eprintln!("[keychain] re-ACLed {account} for the app bundle");
    } else {
        // Never delete and re-add to force it: `memory-db-key` is the only copy of the memory
        // DB's key, and a crash or a refused add between the two would lose every memory. A
        // refused update leaves the item as it was (it still prompts, nothing is lost).
        eprintln!("[keychain] could not re-ACL {account} (status {status}); left unchanged");
    }
}

/// Metadata only: compare trusted-application descriptors without reading the secret. This
/// checks bundle membership, not signature validity; macOS still enforces each stored signing
/// requirement on access, including a genuine signer change that needs fresh user approval.
fn bundle_acl_needs_repair(account: &str, apps: &[CFType]) -> Option<bool> {
    let expected = apps
        .iter()
        .map(trusted_application_data)
        .collect::<Option<Vec<_>>>()?;
    let mut attrs = base_attrs(SERVICE, account);
    attrs.push((
        unsafe { CFString::wrap_under_get_rule(kSecReturnRef) },
        CFBoolean::true_value().into_CFType(),
    ));
    attrs.push((
        unsafe { CFString::wrap_under_get_rule(kSecUseAuthenticationUI) },
        unsafe { CFString::wrap_under_get_rule(kSecUseAuthenticationUISkip) }.into_CFType(),
    ));
    let query = CFDictionary::from_CFType_pairs(&attrs);
    let mut item = std::ptr::null();
    // SAFETY: the query requests one retained generic-password item reference, not its data.
    cvt(unsafe { SecItemCopyMatching(query.as_concrete_TypeRef(), &mut item) }).ok()?;
    if item.is_null() {
        return None;
    }
    let item = unsafe { CFType::wrap_under_create_rule(item) };
    let mut access = std::ptr::null();
    cvt(unsafe { SecKeychainItemCopyAccess(item.as_CFTypeRef(), &mut access) }).ok()?;
    if access.is_null() {
        return None;
    }
    let access = unsafe { CFType::wrap_under_create_rule(access) };
    let acls = unsafe {
        SecAccessCopyMatchingACLList(access.as_CFTypeRef(), kSecACLAuthorizationDecrypt.cast())
    };
    if acls.is_null() {
        return None;
    }
    let acls: CFArray<CFType> = unsafe { CFArray::wrap_under_create_rule(acls) };
    if acls.is_empty() {
        return None;
    }
    for acl in acls.iter() {
        let mut trusted = std::ptr::null();
        let mut description = std::ptr::null();
        let mut prompt = 0_u16;
        let status = unsafe {
            SecACLCopyContents(
                acl.as_CFTypeRef(),
                &mut trusted,
                &mut description,
                &mut prompt,
            )
        };
        // Both copy outputs belong to us even when the API returns an error.
        let trusted: Option<CFArray<CFType>> =
            (!trusted.is_null()).then(|| unsafe { CFArray::wrap_under_create_rule(trusted) });
        let _description = (!description.is_null())
            .then(|| unsafe { CFString::wrap_under_create_rule(description) });
        cvt(status).ok()?;
        // A null application list is an existing allow-all policy. Do not edit it at startup.
        let Some(trusted) = trusted else {
            return Some(false);
        };
        let present = trusted
            .iter()
            .map(|app| trusted_application_data(&app))
            .collect::<Option<Vec<_>>>()?;
        if contains_bundle_applications(&present, &expected) {
            return Some(false);
        }
    }
    Some(true)
}

fn trusted_application_data(app: &CFType) -> Option<CFData> {
    let mut data = std::ptr::null();
    cvt(unsafe { SecTrustedApplicationCopyData(app.as_CFTypeRef(), &mut data) }).ok()?;
    (!data.is_null()).then(|| unsafe { CFData::wrap_under_create_rule(data) })
}

fn contains_bundle_applications(present: &[CFData], expected: &[CFData]) -> bool {
    expected.iter().all(|app| present.contains(app))
}

fn base_attrs(service: &str, account: &str) -> Vec<(CFString, core_foundation::base::CFType)> {
    vec![
        (
            unsafe { CFString::wrap_under_get_rule(kSecClass) },
            unsafe { CFString::wrap_under_get_rule(kSecClassGenericPassword) }.into_CFType(),
        ),
        (
            unsafe { CFString::wrap_under_get_rule(kSecAttrService) },
            CFString::new(service).into_CFType(),
        ),
        (
            unsafe { CFString::wrap_under_get_rule(kSecAttrAccount) },
            CFString::new(account).into_CFType(),
        ),
    ]
}

fn warm_cache(account: &str, bytes: &[u8]) {
    if let Ok(mut guard) = cache().lock() {
        guard.insert(account.to_string(), bytes.to_vec());
    }
}

fn cache() -> &'static Mutex<HashMap<String, Vec<u8>>> {
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn query_dict(
    service: &str,
    account: &str,
    return_data: bool,
    skip_authentication_ui: bool,
) -> CFDictionary<CFString, core_foundation::base::CFType> {
    let mut attrs = base_attrs(service, account);
    if return_data {
        attrs.push((
            unsafe { CFString::wrap_under_get_rule(kSecReturnData) },
            CFBoolean::true_value().into_CFType(),
        ));
    }
    if skip_authentication_ui {
        attrs.push((
            unsafe { CFString::wrap_under_get_rule(kSecUseAuthenticationUI) },
            unsafe { CFString::wrap_under_get_rule(kSecUseAuthenticationUISkip) }.into_CFType(),
        ));
    }
    CFDictionary::from_CFType_pairs(&attrs)
}

#[cfg(test)]
mod tests {
    use super::normalize_select_kk_key;

    #[test]
    fn bundle_acl_requires_every_sibling_and_preserves_extra_grants() {
        let app = super::CFData::from_buffer(b"desktop");
        let sidecar = super::CFData::from_buffer(b"sidecar");
        let other = super::CFData::from_buffer(b"user-approved");
        let expected = vec![app.clone(), sidecar.clone()];
        assert!(!super::contains_bundle_applications(
            &[app.clone(), app.clone()],
            &expected
        ));
        assert!(super::contains_bundle_applications(
            &[sidecar, other, app],
            &expected
        ));
    }

    #[test]
    fn select_kk_accepts_plain_key() {
        assert_eq!(
            normalize_select_kk_key("sk-ant-api03-abc"),
            Some("sk-ant-api03-abc".into())
        );
    }

    #[test]
    fn select_kk_decodes_hex_mistake() {
        // "sk-ant-api03-test-key-value" hex-encoded (devs sometimes store like the DB key)
        let hex = "736b2d616e742d61706930332d746573742d6b65792d76616c7565";
        assert_eq!(
            normalize_select_kk_key(hex),
            Some("sk-ant-api03-test-key-value".into())
        );
    }

    #[test]
    fn select_kk_rejects_garbage() {
        assert!(normalize_select_kk_key("not-a-key").is_none());
    }

    #[test]
    fn bundle_shared_accounts_are_registered_accounts() {
        use super::{BUNDLE_SHARED_ACCOUNTS, KEPT_ACCOUNTS, USER_SECRET_ACCOUNTS};
        for account in BUNDLE_SHARED_ACCOUNTS {
            assert!(
                USER_SECRET_ACCOUNTS.contains(account) || KEPT_ACCOUNTS.contains(account),
                "{account} must live in keychain_accounts so Delete everything covers it"
            );
        }
    }
}
