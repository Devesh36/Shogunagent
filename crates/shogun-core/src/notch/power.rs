//! Sleep / wake policy (spec §3.9).
//!
//! The macOS adapter subscribes to `NSWorkspace` power notifications and applies these
//! effects. Decision stays here so Linux CI can pin the contract: collapse before sleep,
//! health-check 1000ms after wake. No AX, no AppKit.

use std::time::Duration;

/// Delay after `didWake` / `screensDidWake` before the panel health check (spec §3.9).
pub const WAKE_HEALTH_CHECK_DELAY: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspacePower {
    WillSleep,
    DidWake,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerEffect {
    ForceCollapse,
    HealthCheckAfter { delay: Duration },
}

pub fn on_workspace_power(event: WorkspacePower) -> PowerEffect {
    match event {
        WorkspacePower::WillSleep => PowerEffect::ForceCollapse,
        WorkspacePower::DidWake => PowerEffect::HealthCheckAfter {
            delay: WAKE_HEALTH_CHECK_DELAY,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sleep_collapses_so_the_lid_does_not_close_on_an_expanded_panel() {
        assert_eq!(
            on_workspace_power(WorkspacePower::WillSleep),
            PowerEffect::ForceCollapse
        );
    }

    #[test]
    fn wake_health_check_waits_one_second() {
        assert_eq!(
            on_workspace_power(WorkspacePower::DidWake),
            PowerEffect::HealthCheckAfter {
                delay: Duration::from_secs(1)
            }
        );
    }
}
