//! Identity of the SHOGUN process itself, as seen through NSWorkspace / AX.
//!
//! Capture, the notch Idle label, live-screen chat, and the meeting watchdog all have to answer
//! the same question: "is this frontmost app us?" Walking our own tree stores "reading SHOGUN",
//! overwrites the user's working context, and (when the name is not passed) can start a Meet
//! leave-grace the moment the user opens the notch.
//!
//! The product identifier is `com.syogun.shogunai` (`tauri.conf.json`). The spike id stays so a
//! leftover local build is still recognised. Matching is bundle-first so the meeting path, which
//! calls this with an empty name, still works.

/// Shipping / current desktop identifier. Lockstep with `apps/desktop/src-tauri/tauri.conf.json`.
pub const DESKTOP_BUNDLE_ID: &str = "com.syogun.shogunai";

/// Bundle ids this process reports as itself.
const OWN_BUNDLES: &[&str] = &[
    DESKTOP_BUNDLE_ID,
    "dev.shogun.spike", // leftover local spike builds
];

/// Localized / product / cargo package names that mean "us", not a user focus target.
const OWN_NAMES: &[&str] = &["ShogunAI", "SHOGUN", "shogun-desktop-spike"];

/// True when frontmost is SHOGUN itself (or the empty-bundle NSPanel quirk). Must not drive
/// Idle "reading …" or the context-cache walk — the panel would report reading itself.
pub fn is_own_app(bundle_id: &str, name: &str) -> bool {
    if OWN_BUNDLES.iter().any(|b| {
        bundle_id.eq_ignore_ascii_case(b)
            || bundle_id
                .get(..b.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b))
                && bundle_id
                    .get(b.len()..)
                    .is_some_and(|suffix| suffix.starts_with('.'))
    }) {
        return true;
    }
    if OWN_NAMES
        .iter()
        .any(|n| name.eq_ignore_ascii_case(n) || bundle_id.eq_ignore_ascii_case(n))
    {
        return true;
    }
    // Overlay / nonactivating panel often reports an empty bundle id while we are frontmost
    // (see meeting lane). Empty + empty/Shogun name ⇒ self, not an unknown third-party app.
    bundle_id.is_empty()
        && (name.is_empty() || OWN_NAMES.iter().any(|n| name.eq_ignore_ascii_case(n)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipping_bundle_is_us_even_when_the_caller_passes_no_name() {
        // Meeting watchdog: `is_own_app(bundle_id, "")`. A name-only match would miss this.
        assert!(is_own_app(DESKTOP_BUNDLE_ID, ""));
        assert!(is_own_app("COM.SYOGUN.SHOGUNAI", "Settings"));
    }

    #[test]
    fn isolated_build_bundle_suffixes_are_us_but_lookalikes_are_not() {
        assert!(is_own_app("com.syogun.shogunai.test", ""));
        assert!(is_own_app("COM.SYOGUN.SHOGUNAI.qa.local", ""));
        assert!(is_own_app("dev.shogun.spike.test", ""));
        assert!(!is_own_app("com.syogun.shogunaihelper", "Helper"));
        assert!(!is_own_app("com.syogun.shogunai-test", "Helper"));
    }

    #[test]
    fn leftover_spike_builds_and_product_names_still_count() {
        assert!(is_own_app("dev.shogun.spike", ""));
        assert!(is_own_app("com.example.unknown", "ShogunAI"));
        assert!(is_own_app("", "SHOGUN"));
        assert!(is_own_app("", ""));
    }

    #[test]
    fn chrome_vscode_and_discord_are_not_us() {
        assert!(!is_own_app("com.google.Chrome", "Google Chrome"));
        assert!(!is_own_app("com.microsoft.VSCode", "Code"));
        assert!(!is_own_app("com.hnc.Discord", "Discord"));
        assert!(!is_own_app("com.apple.Safari", ""));
        assert!(!is_own_app("", "Google Chrome"));
    }
}
