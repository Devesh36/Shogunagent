//! The Keychain reader behind the plan-clock mirrors (audit 2026-10-20 M7).
//!
//! `shogun_mcp::plan_source` takes the earliest trial stamp and the latest time high-water mark
//! of file and Keychain, but cannot reach the Keychain itself. Every process that resolves a
//! plan from the shared files (the standalone `shogun-mcp` / `shogun-api` faces, and the desktop
//! for its in-process faces) calls [`install`] once at start-up. Reads never show a Keychain
//! dialog: an item that would need one reads as absent and the files decide, as before.

/// Install the reader for this process. `false` off macOS, or when one is already installed.
pub fn install() -> bool {
    #[cfg(target_os = "macos")]
    {
        shogun_mcp::plan_source::install_keychain_mirror_reader(read)
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

#[cfg(target_os = "macos")]
fn read() -> shogun_mcp::plan_source::KeychainPlanMirrors {
    use shogun_integrations::keychain_store::{
        get_generic_secret_non_interactive_uncached, TIME_HIGH_WATER_MARK_ACCOUNT,
        TRIAL_STARTED_AT_ACCOUNT,
    };
    let trial = get_generic_secret_non_interactive_uncached(TRIAL_STARTED_AT_ACCOUNT).ok();
    let mark = get_generic_secret_non_interactive_uncached(TIME_HIGH_WATER_MARK_ACCOUNT).ok();
    shogun_mcp::plan_source::KeychainPlanMirrors::from_bytes(trial.as_deref(), mark.as_deref())
}
