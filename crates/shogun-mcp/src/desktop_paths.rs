//! The ShogunAI Mac app's data folder (feature `desktop`, E1-T7).
//!
//! `plan_source::app_data_dir` resolves the app's files (`onboarding.json`, `billing.json`,
//! `meeting.json`, …) under `~/Library/Application Support/<bundle id>` on a Mac. The server build
//! never looks there: it has no app beside it, so the plan source answers from its env overrides
//! alone. The one `cfg(target_os = "macos")` of the lookup lives here, behind the feature.

use std::path::PathBuf;

/// `~/Library/Application Support/<bundle id>` on macOS; `None` off macOS or without `HOME`.
pub fn app_data_dir(bundle_id: &str) -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        let home = std::env::var_os("HOME")?;
        return Some(PathBuf::from(home).join("Library/Application Support").join(bundle_id));
    }
    None
}
