//! The login Keychain items the standalone faces read on a Mac (feature `desktop`).
//!
//! Each function has a macOS body over `shogun_integrations::keychain_store` and an off-macOS
//! body that answers what the server build answers, so the module compiles everywhere the
//! feature is on. Callers in the default build keep their own `not(feature = "desktop")` arm.

use std::path::PathBuf;

use crate::db_backend::ConnectionRestrictor;
use crate::settings_seal::SealMarker;

/// The standalone faces' [`ConnectionRestrictor`]: the Keychain item the app keeps the
/// connections in, rewritten only when something changed, then the revision marker beside
/// `memory_api.json` (`settings_path`) so running sessions re-read it at their next call. Only
/// ever narrows (`memory_api_settings::withhold_in_store`). macOS only; `None` elsewhere.
#[cfg(target_os = "macos")]
pub fn connection_restrictor(settings_path: PathBuf) -> Option<ConnectionRestrictor> {
    use std::sync::Arc;
    Some(Arc::new(move |client: &str| {
        use shogun_integrations::keychain_store::{get_generic_secret_uncached, set_generic_secret};
        use shogun_mcp::memory_api_settings::{token_blob_read, withhold_in_store, TOKENS_KEYCHAIN_ACCOUNT};
        let changed = withhold_in_store(
            || token_blob_read(get_generic_secret_uncached(TOKENS_KEYCHAIN_ACCOUNT).map_err(|error| error.code())),
            |bytes| {
                set_generic_secret(TOKENS_KEYCHAIN_ACCOUNT, bytes)
                    .map_err(|_| "connections_unavailable".to_string())
            },
            client,
        )?;
        if changed {
            // A failed marker write leaves the change to the sessions' 60-second re-read.
            let _ = shogun_mcp::live_tokens::bump_revision(&shogun_mcp::live_tokens::revision_path(&settings_path));
        }
        Ok(changed)
    }))
}

#[cfg(not(target_os = "macos"))]
pub fn connection_restrictor(settings_path: PathBuf) -> Option<ConnectionRestrictor> {
    let _ = settings_path;
    None
}

/// Whether the user's own key for `provider` is in the Keychain (`agent.source.get`). `None`
/// where there is no Keychain or the provider has no BYOK account.
#[cfg(target_os = "macos")]
pub fn daemon_key_present(provider: &str) -> Option<bool> {
    let account = crate::agent_source::byok_keychain_account(provider)?;
    shogun_integrations::keychain_store::generic_secret_present(account)
}

#[cfg(not(target_os = "macos"))]
pub fn daemon_key_present(_provider: &str) -> Option<bool> {
    None
}

/// The account label stored with a connected service's tokens (`api.connections.list` with
/// `include_account_labels`), from the app's Keychain token store. `None` off macOS.
#[cfg(target_os = "macos")]
pub fn connection_account_label(service: Option<&str>) -> Option<String> {
    use shogun_integrations::token::GrantStore;
    shogun_mcp::connections_api::listed_services()
        .find(|listed| Some(listed.source_str()) == service)
        .and_then(|listed| {
            shogun_integrations::keychain::KeychainTokenStore::new("com.selectkk.shogun")
                .load(listed)
                .ok()
                .flatten()
        })
        .and_then(|tokens| tokens.account_label)
}

#[cfg(not(target_os = "macos"))]
pub fn connection_account_label(_service: Option<&str>) -> Option<String> {
    None
}

/// The memory DB key from the app's Keychain entry, for a daemon started without
/// `SHOGUN_DB_KEY` (`daemon::load_db_encryption_key`).
#[cfg(target_os = "macos")]
pub fn memory_db_key() -> Result<shogun_memory::DbKey, String> {
    use shogun_integrations::keychain_store;
    const DB_KEY_ACCOUNT: &str = "memory-db-key";
    let bytes = keychain_store::get_generic_secret(DB_KEY_ACCOUNT).map_err(|e| {
        format!(
            "could not read memory DB key from Keychain (status {}): unlock Keychain and retry",
            e.code()
        )
    })?;
    let hex = String::from_utf8(bytes)
        .map_err(|_| "memory DB key in Keychain is not valid text".to_string())?;
    shogun_memory::DbKey::from_hex(&hex)
        .ok_or_else(|| "memory DB key in Keychain is malformed".to_string())
}

#[cfg(not(target_os = "macos"))]
pub fn memory_db_key() -> Result<shogun_memory::DbKey, String> {
    Err("encrypted memory DB requires SHOGUN_DB_KEY".to_string())
}

/// The settings-seal marker in the Keychain (audit 2026-10-20 M6). `may_adopt` lets this
/// process record new kinds.
#[cfg(target_os = "macos")]
pub fn seal_marker(may_adopt: bool) -> Option<SealMarker> {
    fn read() -> Option<String> {
        use shogun_integrations::keychain_store::{
            get_generic_secret_non_interactive_uncached, SETTINGS_SEALED_ACCOUNT,
        };
        match get_generic_secret_non_interactive_uncached(SETTINGS_SEALED_ACCOUNT) {
            Ok(bytes) => String::from_utf8(bytes).ok(),
            // No item yet: nothing sealed before.
            Err(error) if error.code() == -25300 => Some(String::new()),
            Err(_) => None,
        }
    }
    fn record(kinds: &str) {
        use shogun_integrations::keychain_store::{set_generic_secret, SETTINGS_SEALED_ACCOUNT};
        if set_generic_secret(SETTINGS_SEALED_ACCOUNT, kinds.as_bytes()).is_err() {
            eprintln!("[settings-seal] could not record the seal marker");
        }
    }
    Some(SealMarker { sealed_kinds: read, record: may_adopt.then_some(record as fn(&str)) })
}

/// Off macOS there is no Keychain: a process that may adopt runs without a marker (every
/// missing seal is a first run); one that may not gets a marker that says nothing was sealed
/// and records nothing, so it never adopts.
#[cfg(not(target_os = "macos"))]
pub fn seal_marker(may_adopt: bool) -> Option<SealMarker> {
    if may_adopt {
        None
    } else {
        Some(SealMarker { sealed_kinds: || Some(String::new()), record: None })
    }
}
