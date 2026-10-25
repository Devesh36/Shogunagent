//! Bakes the legacy device-binding cutoff into the crate (issue #777).
//!
//! Before hardware-bound device ids, a licence token was bound to a random id kept in
//! `billing.json` itself. A build that binds to the hardware still accepts such a *legacy* token —
//! so an update cannot lock a paying Mac out while it is offline — but only one issued before this
//! cutoff. See `LEGACY_DEVICE_CUTOFF_SECS` in `src/lib.rs` for the reasoning.
//!
//! The value is the build time (or `SOURCE_DATE_EPOCH`, for reproducible builds) plus a 14-day
//! MARGIN, clamped to `[FLOOR, CEILING]`:
//!
//! - MARGIN: a release reaches users some time after it is built, and until they update, the old
//!   client keeps minting legacy tokens. A Mac whose first launch of the new build is offline must
//!   still pass on the token its old client minted between the build and the update. Fourteen
//!   days matches the offline grace window.
//! - FLOOR, the day this binding was introduced (2026-10-03 UTC): a build clock or a
//!   `SOURCE_DATE_EPOCH` that reads earlier (some packagers pin it to 1980) cannot make every
//!   legacy token look post-cutoff.
//! - CEILING (2027-01-01 UTC): a build made after it still uses it. Without a ceiling every future
//!   release would accept a legacy token issued before *its own* build time, so a Mac kept on an
//!   old client could feed copies to new builds forever. With it, the last legacy token any build
//!   accepts was issued before the ceiling, and its 14-day grace runs out two weeks later.
//!
//! Re-runs only when this script or `SOURCE_DATE_EPOCH` changes: a local incremental build keeps
//! the cutoff of its first build, which is only ever earlier (stricter), never later. Release
//! builds compile workspace crates from scratch (the CI cache keeps dependencies only).

use std::time::{SystemTime, UNIX_EPOCH};

/// 2026-10-03T00:00:00Z.
const FLOOR: u64 = 1_790_985_600;
/// 2027-01-01T00:00:00Z.
const CEILING: u64 = 1_798_761_600;
/// Build-to-install allowance for legacy tokens minted by the old client (14 days).
const MARGIN: u64 = 14 * 24 * 60 * 60;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(FLOOR);
    let pinned = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok());
    // A pinned epoch later than the build clock is not a real build time: ignore the excess.
    let build_epoch = pinned.unwrap_or(now).min(now);
    let cutoff = build_epoch.saturating_add(MARGIN).clamp(FLOOR, CEILING);

    let out_dir = std::env::var_os("OUT_DIR").map(std::path::PathBuf::from);
    let Some(out_dir) = out_dir else {
        // Cargo always sets OUT_DIR for build scripts; without it there is nowhere to write and
        // the include! in lib.rs fails the build loudly, which is the right outcome.
        return;
    };
    // The build epoch is written too, for the crate's own tests only (lib.rs `cfg(test)`).
    for (file, value) in [("legacy_device_cutoff.rs", cutoff), ("legacy_device_build_epoch.rs", build_epoch)] {
        if let Err(error) = std::fs::write(out_dir.join(file), format!("{value}\n")) {
            panic!("could not write {file}: {error}");
        }
    }
}
