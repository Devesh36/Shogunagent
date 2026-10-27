//! Entitlements at the standalone API binaries' composition boundary.
//!
//! The OSS server uses the same capabilities as `tenancy::multi`: hosted charging is owned by
//! the gateway, while local access still requires its token, store and action-level checks.
//! Only the desktop build reads the desktop's signed licence/trial source. This is selected at
//! compile time, never by an environment flag that could unlock a desktop installation.

use shogun_agents::entitlement::Entitlements;
use shogun_mcp::plan_source::PlanLock;

#[derive(Clone)]
pub struct ApiEntitlementSource {
    #[cfg(feature = "desktop")]
    desktop: shogun_mcp::plan_source::FilePlanSource,
}

impl ApiEntitlementSource {
    pub fn for_build() -> Self {
        Self {
            #[cfg(feature = "desktop")]
            desktop: shogun_mcp::plan_source::FilePlanSource::from_env(),
        }
    }

    pub fn resolve(&self, now_ms: u64) -> Entitlements {
        #[cfg(feature = "desktop")]
        {
            self.desktop.resolve(now_ms)
        }
        #[cfg(not(feature = "desktop"))]
        {
            let _ = now_ms;
            // The existing --multi server policy; this does not assert a paid licence exists.
            shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0)
        }
    }

    pub fn lock(&self, now_ms: u64) -> Option<PlanLock> {
        #[cfg(feature = "desktop")]
        {
            self.desktop.diagnose(now_ms).lock
        }
        #[cfg(not(feature = "desktop"))]
        {
            let _ = now_ms;
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(feature = "desktop"))]
    #[test]
    fn server_uses_the_multi_principal_policy_without_desktop_plan_files() {
        let source = ApiEntitlementSource::for_build();
        assert!(source.resolve(0).memory_api);
        assert!(source.resolve(u64::MAX).memory_access);
        assert_eq!(source.lock(0), None);
        assert_eq!(source.lock(u64::MAX), None);
    }

    #[cfg(feature = "desktop")]
    #[test]
    fn desktop_retains_the_file_source_and_fails_closed_without_a_signed_plan() {
        let path = std::env::temp_dir().join(format!(
            "shogun-missing-plan-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        let desktop = shogun_mcp::plan_source::FilePlanSource::at(path.join("onboarding.json"))
            .with_billing(path.join("billing.json"))
            .with_trial_relay(path.join("trial_relay.json"));
        let source = ApiEntitlementSource {
            desktop: desktop.clone(),
        };
        for now_ms in [0, 1_800_000_000_000, u64::MAX] {
            assert_eq!(source.resolve(now_ms), desktop.resolve(now_ms));
            assert_eq!(source.lock(now_ms), desktop.diagnose(now_ms).lock);
            assert!(!source.resolve(now_ms).memory_api);
            assert!(source.lock(now_ms).is_some());
        }
    }
}
