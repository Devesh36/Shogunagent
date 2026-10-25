//! `device.account.get` (#148 P2, FR-ACC-07): the account on this Mac, read-only, for MCP / REST /
//! CLI — the same split as #963: creating an account and signing in happen in the app only; the
//! API faces read.
//!
//! The source is `account.json`, the content-free snapshot the desktop writes after every account
//! read and clears on sign-out (`shogun_license::account::AccountSnapshot`): signed in or out, the
//! address, sign-in methods, and the devices without their ids. The plan comes from the same
//! [`FilePlanSource`] every other face resolves, so this answer can never disagree with the gate.
//! No token, no device id, no licence key ever reaches this response.

use std::path::Path;

use serde_json::{json, Value};
use shogun_agents::entitlement::PlanStatus;
use shogun_license::account::{account_status_json, parse_account_snapshot};

use crate::plan_source::FilePlanSource;

/// The plan status as the faces name it.
pub fn plan_status_name(status: PlanStatus) -> &'static str {
    match status {
        PlanStatus::TrialNotStarted => "trial_not_started",
        PlanStatus::Trial => "trial",
        PlanStatus::TrialExpired => "trial_expired",
        PlanStatus::Standard => "standard",
        PlanStatus::Pro => "pro",
    }
}

/// Read `account.json` at `path` (re-read on every call: the desktop is the only writer) and add
/// the plan in force at `now_ms`. A missing or unreadable file reads as signed out — never as a
/// fabricated account.
pub fn read(path: Option<&Path>, plan: &FilePlanSource, now_ms: u64) -> Value {
    let snapshot = path
        .and_then(|p| std::fs::read_to_string(p).ok())
        .as_deref()
        .and_then(parse_account_snapshot);
    let mut answer = account_status_json(snapshot.as_ref());
    let entitlements = plan.resolve(now_ms);
    answer["plan"] = json!({
        "status": plan_status_name(entitlements.status),
        "memory_access": entitlements.memory_access,
        // Unix seconds, while the trial runs: the same end the app shows.
        "trial_ends_at": plan.trial_ends_at_secs(now_ms),
        // Why there is no trial (`trial_used`, `trial_device_limit`, `checkout_required`,
        // `account_required`), while neither a trial nor a plan runs.
        "trial_refusal": plan.trial_refusal(entitlements.status),
    });
    answer
}

#[cfg(test)]
mod tests {
    use super::*;
    use shogun_license::account::{serialize_account_snapshot, AccountSnapshot, SnapshotDevice};

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("shogun-account-api-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn plan(d: &Path) -> FilePlanSource {
        FilePlanSource::at(d.join("onboarding.json"))
            .with_billing(d.join("billing.json"))
            .with_trial_relay(d.join("trial_relay.json"))
    }

    #[test]
    fn signed_in_shows_the_account_and_devices_without_ids() {
        let d = dir("in");
        let path = d.join("account.json");
        let snap = AccountSnapshot {
            signed_in: true,
            email: Some("me@example.test".into()),
            email_confirmed: true,
            passkey_count: 2,
            providers: vec!["google".into()],
            devices: vec![SnapshotDevice { name: Some("MacBook Pro".into()), this_device: true, holds_seat: true, signed_in: true, last_seen_at: Some(5) }],
            checked_at: Some(10),
            licence_key_linked: true,
            ..AccountSnapshot::default()
        };
        std::fs::write(&path, serialize_account_snapshot(&snap)).unwrap();
        let v = read(Some(&path), &plan(&d), 1_000);
        assert_eq!(v["signed_in"], true);
        assert_eq!(v["email"], "me@example.test");
        assert_eq!(v["sign_in_methods"]["passkeys"], 2);
        assert_eq!(v["devices"][0]["name"], "MacBook Pro");
        assert!(v["devices"][0].get("device_id").is_none());
        assert!(v.get("licence_key_linked").is_none());
        // No trial stamp on this test "Mac": the plan is locked, and the answer says so.
        assert_eq!(v["plan"]["status"], "trial_not_started");
        assert_eq!(v["plan"]["memory_access"], false);
        assert!(v["plan"]["trial_ends_at"].is_null());
        assert!(v["plan"]["trial_refusal"].is_null());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn missing_corrupt_and_signed_out_files_read_as_signed_out() {
        let d = dir("out");
        let path = d.join("account.json");
        assert_eq!(read(Some(&path), &plan(&d), 1)["signed_in"], false);
        assert_eq!(read(None, &plan(&d), 1)["signed_in"], false);
        std::fs::write(&path, "garbage").unwrap();
        assert_eq!(read(Some(&path), &plan(&d), 1)["signed_in"], false);
        std::fs::write(&path, serialize_account_snapshot(&AccountSnapshot::signed_out(Some("session_revoked")))).unwrap();
        let v = read(Some(&path), &plan(&d), 1);
        assert_eq!(v["signed_in"], false);
        assert_eq!(v["needs_sign_in"], "session_revoked");
        assert!(v.get("email").is_none());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn plan_names_cover_every_status() {
        assert_eq!(plan_status_name(PlanStatus::TrialNotStarted), "trial_not_started");
        assert_eq!(plan_status_name(PlanStatus::Trial), "trial");
        assert_eq!(plan_status_name(PlanStatus::TrialExpired), "trial_expired");
        assert_eq!(plan_status_name(PlanStatus::Standard), "standard");
        assert_eq!(plan_status_name(PlanStatus::Pro), "pro");
    }
}
