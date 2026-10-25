//! The Mail (Gmail via Composio) policy file, `composio.json`, shared by the desktop and the API
//! faces (issue #82, invariant 6).
//!
//! The desktop's Composio card and Mail row in Settings → Connections write it; the MCP / REST / CLI
//! faces read it for `connections.list` and tighten it through `connections.composio.set` /
//! `connections.disconnect` ([`crate::connections_api`]). Nothing in it is a secret (invariant 7):
//! the Composio API key stays in the Keychain. It does hold the Composio user id, an account
//! identifier, so no API answer ever carries it.
//!
//! Every writer goes through [`update`] or [`update_strict`]: an `flock` on a sibling lock file, a
//! re-read under the lock, and an atomic replace. A revocation written by a daemon can therefore
//! never be lost to a desktop write that read the file a moment earlier.

use std::path::{Path, PathBuf};

use crate::composio::Disclosures;

/// The policy file, in the desktop's app-data directory.
pub const POLICY_FILE: &str = "composio.json";
/// Overrides where the API faces look for [`POLICY_FILE`].
pub const POLICY_ENV: &str = "SHOGUN_COMPOSIO_POLICY";

/// Persisted opt-in policy for Mail through Composio (FR-C2-02 / FR-C2-03). Every field defaults
/// to the safe-blocked state: nothing reaches Composio until the user enables each gate. The
/// desktop has written this exact shape since the 2026-07 decision; keep field names stable.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ComposioPolicy {
    /// When `true` (default) a confirmed send is saved as a Gmail draft instead of sent. Only the
    /// user turns it off, in Settings, and only once consent exists (FR-C2-03).
    pub draft_stop: bool,
    /// Whether the user completed the FR-C2-02 opt-in (all three disclosures). `false` by default.
    pub consent_acknowledged: bool,
    /// The three read-and-send disclosures, each acknowledged separately in Settings. Older
    /// send-only consent is deliberately not migrated: all three must be collected again.
    #[serde(default)]
    pub disclosures: Disclosures,
    /// Composio account user id for the connected Gmail account (not a secret, but an account
    /// identifier: never returned by an API face). Empty = the `SHOGUN_COMPOSIO_USER_ID` fallback.
    #[serde(default)]
    pub user_id: String,
    /// Whether Gmail is connected. Its Composio transport stores no per-service token, and the API
    /// key, user id and consent all outlive a disconnect, so this flag is what makes a disconnect
    /// durable. `serde(default)`: a file from before the flag reads as disconnected.
    #[serde(default)]
    pub gmail_connected: bool,
}

impl Default for ComposioPolicy {
    fn default() -> Self {
        Self {
            draft_stop: true,
            consent_acknowledged: false,
            disclosures: Disclosures::default(),
            user_id: String::new(),
            gmail_connected: false,
        }
    }
}

/// The only invalid combination is draft-stop OFF without consent: live sending may only be
/// enabled once the user has acknowledged the disclosures.
pub fn policy_is_valid(draft_stop: bool, consent: bool) -> bool {
    consent || draft_stop
}

/// Which gate decides a confirmed Mail send (FR-C2-02 / FR-C2-03, issue #97), so the user is told
/// the real reason it became a draft (issue #777).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendGate {
    /// Consent, Draft-only off and the plan's send unlock are all in place: the send goes out.
    Open,
    /// The three-disclosure opt-in is missing or incomplete.
    NoConsent,
    /// The user's own Draft-only mode is on.
    DraftOnly,
    /// Consent is given and Draft-only is off, but the plan does not include real sending
    /// (`Entitlements::composio_send_unlock` — Pro, or the trial).
    Plan,
}

/// The gate that decides a confirmed Mail send under `policy` and `ent`. Checked in the order the
/// user meets them in Settings: opt-in, then Draft-only, then the plan. `Open` exactly when
/// [`crate::composio::ComposioSender::send_capability`] would grant a capability for this policy.
pub fn send_gate(
    policy: &ComposioPolicy,
    ent: &shogun_agents::entitlement::Entitlements,
) -> SendGate {
    if !policy.consent_acknowledged || crate::composio::grant_consent(policy.disclosures).is_err() {
        SendGate::NoConsent
    } else if policy.draft_stop {
        SendGate::DraftOnly
    } else if !ent.composio_send_unlock {
        SendGate::Plan
    } else {
        SendGate::Open
    }
}

/// Parse `composio.json`. Unparseable text is the default. Consent without all three disclosures
/// acknowledged (a legacy send-only file, a hand edit) is no consent: draft-stop back on and Gmail
/// disconnected.
pub fn parse(text: &str) -> ComposioPolicy {
    let mut policy = serde_json::from_str::<ComposioPolicy>(text).unwrap_or_default();
    fail_closed(&mut policy);
    policy
}

fn fail_closed(policy: &mut ComposioPolicy) {
    if !policy.disclosures.all_acknowledged() {
        policy.consent_acknowledged = false;
        policy.draft_stop = true;
        policy.gmail_connected = false;
    }
    if !policy.consent_acknowledged {
        policy.draft_stop = true;
    }
}

/// `composio.json` for a daemon whose DB is at `db_path`: [`POLICY_ENV`] when set, otherwise beside
/// the DB (the desktop keeps both in its app-data directory).
pub fn resolve_path(db_path: &str) -> PathBuf {
    std::env::var(POLICY_ENV)
        .ok()
        .filter(|p| !p.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(db_path)
                .parent()
                .map_or_else(|| PathBuf::from(POLICY_FILE), |dir| dir.join(POLICY_FILE))
        })
}

/// What the desktop runs on: the saved policy, or the default (everything blocked) when the file
/// is missing or unreadable.
pub fn load_effective(path: &Path) -> ComposioPolicy {
    std::fs::read_to_string(path)
        .map(|text| parse(&text))
        .unwrap_or_default()
}

/// The saved policy for an API writer. A missing file is the default. An unreadable or corrupt
/// file is an error, never silently replaced (it already reads as everything blocked).
pub fn load_strict(path: &Path) -> Result<ComposioPolicy, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str::<ComposioPolicy>(&text)
            .map(|mut policy| {
                fail_closed(&mut policy);
                policy
            })
            .map_err(|_| "composio settings are unreadable".to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ComposioPolicy::default()),
        Err(_) => Err("composio settings are unreadable".to_string()),
    }
}

/// Read, patch and replace the policy under the cross-process lock (the desktop's writers). A
/// missing or corrupt file starts from the default, as the desktop always has: the user at
/// Settings is the actor. `patch` may refuse; then nothing is written. A patch that changes
/// nothing writes nothing.
pub fn update(
    path: &Path,
    patch: impl FnOnce(&mut ComposioPolicy) -> Result<(), String>,
) -> Result<ComposioPolicy, String> {
    locked_update(path, false, patch)
}

/// [`update`] for an API writer: a corrupt file is refused instead of replaced.
pub fn update_strict(
    path: &Path,
    patch: impl FnOnce(&mut ComposioPolicy) -> Result<(), String>,
) -> Result<ComposioPolicy, String> {
    locked_update(path, true, patch)
}

fn locked_update(
    path: &Path,
    strict: bool,
    patch: impl FnOnce(&mut ComposioPolicy) -> Result<(), String>,
) -> Result<ComposioPolicy, String> {
    let _lock = crate::settings_file::lock(path, "composio settings are busy")?;
    let before = if strict {
        load_strict(path)?
    } else {
        load_effective(path)
    };
    let mut policy = before.clone();
    patch(&mut policy)?;
    if policy != before {
        let json = serde_json::to_string_pretty(&policy).map_err(|e| e.to_string())?;
        crate::settings_file::write_atomic(path, json.as_bytes(), true)?;
    }
    Ok(policy)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Barrier};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    pub(crate) fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "shogun-composio-{tag}-{}-{}.json",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    pub(crate) const ALL: Disclosures =
        Disclosures { via_third_party: true, data_types: true, revocable: true };

    pub(crate) fn consented(draft_stop: bool, connected: bool) -> ComposioPolicy {
        ComposioPolicy {
            draft_stop,
            consent_acknowledged: true,
            disclosures: ALL,
            user_id: "user-123".into(),
            gmail_connected: connected,
        }
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("json.lock"));
    }

    #[test]
    fn the_file_shape_is_the_desktop_one() {
        // The desktop has written this shape since the 2026-07 decision. A file without the later
        // fields reads with safe defaults.
        let p: ComposioPolicy =
            serde_json::from_str(r#"{"draft_stop":true,"consent_acknowledged":false}"#).unwrap();
        assert_eq!(p, ComposioPolicy::default());
        let v = serde_json::to_value(consented(false, true)).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "draft_stop": false,
                "consent_acknowledged": true,
                "disclosures": { "via_third_party": true, "data_types": true, "revocable": true },
                "user_id": "user-123",
                "gmail_connected": true,
            })
        );
    }

    /// Issue #777: a Standard user who opted in and turned Draft-only off is told the plan kept the
    /// send a draft — not that an opt-in is missing.
    #[test]
    fn the_send_gate_names_the_plan_only_when_the_plan_closed_it() {
        use shogun_agents::entitlement::{entitlements, Plan, TRIAL_DURATION_MS};
        let standard = entitlements(Plan::Standard, 0);
        let pro = entitlements(Plan::Pro, 0);
        let trial = entitlements(Plan::Trial { started_at_ms: Some(0) }, 1);
        let expired = entitlements(Plan::Trial { started_at_ms: Some(0) }, TRIAL_DURATION_MS);

        assert_eq!(send_gate(&consented(false, true), &standard), SendGate::Plan);
        assert_eq!(send_gate(&consented(false, true), &expired), SendGate::Plan);
        assert_eq!(send_gate(&consented(false, true), &pro), SendGate::Open);
        assert_eq!(send_gate(&consented(false, true), &trial), SendGate::Open);
        // The user's own Draft-only choice is named before the plan.
        assert_eq!(send_gate(&consented(true, true), &standard), SendGate::DraftOnly);
        assert_eq!(send_gate(&consented(true, true), &pro), SendGate::DraftOnly);
        // No (or incomplete) opt-in comes first of all.
        assert_eq!(send_gate(&ComposioPolicy::default(), &pro), SendGate::NoConsent);
        let partial = ComposioPolicy {
            disclosures: Disclosures { revocable: false, ..ALL },
            ..consented(false, true)
        };
        assert_eq!(send_gate(&partial, &pro), SendGate::NoConsent);

        // `Open` is exactly when the sender would hand out a send capability.
        for policy in [consented(false, true), consented(true, true), ComposioPolicy::default(), partial] {
            for ent in [&standard, &pro, &trial, &expired] {
                let capability = crate::composio::grant_consent(policy.disclosures)
                    .ok()
                    .filter(|_| policy.consent_acknowledged)
                    .is_some_and(|consent| {
                        let mut sender = crate::composio::ComposioSender::new(consent);
                        sender.set_draft_stop(policy.draft_stop);
                        sender.send_capability(ent).is_some()
                    });
                assert_eq!(send_gate(&policy, ent) == SendGate::Open, capability, "{policy:?}");
            }
        }
    }

    #[test]
    fn consent_without_every_disclosure_is_no_consent() {
        let legacy = parse(r#"{"draft_stop":false,"consent_acknowledged":true,"gmail_connected":true}"#);
        assert!(!legacy.consent_acknowledged);
        assert!(!legacy.gmail_connected);
        assert!(legacy.draft_stop);
        // Draft-stop OFF without consent is not a state the file can put us in.
        let odd = parse(
            r#"{"draft_stop":false,"consent_acknowledged":false,"disclosures":{"via_third_party":true,"data_types":true,"revocable":true}}"#,
        );
        assert!(odd.draft_stop);
        assert_eq!(parse("not json"), ComposioPolicy::default());
        assert!(policy_is_valid(true, false));
        assert!(policy_is_valid(false, true));
        assert!(!policy_is_valid(false, false));
    }

    #[test]
    fn updates_are_atomic_and_a_patch_can_refuse() {
        let path = temp_path("update");
        assert_eq!(load_strict(&path), Ok(ComposioPolicy::default()), "missing = default");
        let saved = update(&path, |p| {
            *p = consented(true, true);
            Ok(())
        })
        .unwrap();
        assert_eq!(load_effective(&path), saved);
        let before = std::fs::read_to_string(&path).unwrap();
        assert!(update(&path, |_| Err("no".into())).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before, "a refused patch writes nothing");
        cleanup(&path);
    }

    #[test]
    fn a_corrupt_file_is_never_replaced_by_an_api_write() {
        let path = temp_path("corrupt");
        std::fs::write(&path, "{not json").unwrap();
        assert!(load_strict(&path).is_err());
        assert_eq!(load_effective(&path), ComposioPolicy::default(), "reads as everything blocked");
        assert!(update_strict(&path, |p| {
            p.draft_stop = true;
            Ok(())
        })
        .is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{not json");
        // The desktop (the user at Settings) may start over from the default, as it always has.
        assert!(update(&path, |p| {
            p.user_id = "u".into();
            Ok(())
        })
        .is_ok());
        cleanup(&path);
    }

    #[test]
    fn concurrent_writers_never_lose_a_revocation() {
        // A daemon revoking consent while the desktop saves the user id: both changes land.
        let path = temp_path("race");
        update(&path, |p| {
            *p = consented(true, true);
            Ok(())
        })
        .unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let revoke = {
            let (path, barrier) = (path.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                update_strict(&path, |p| {
                    p.consent_acknowledged = false;
                    p.disclosures = Disclosures::default();
                    p.draft_stop = true;
                    p.gmail_connected = false;
                    Ok(())
                })
            })
        };
        let user_id = {
            let (path, barrier) = (path.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                update(&path, |p| {
                    p.user_id = "renamed".into();
                    Ok(())
                })
            })
        };
        barrier.wait();
        revoke.join().unwrap().unwrap();
        user_id.join().unwrap().unwrap();
        let stored = load_effective(&path);
        assert!(!stored.consent_acknowledged);
        assert!(!stored.gmail_connected);
        assert_eq!(stored.user_id, "renamed");
        cleanup(&path);
    }

    #[test]
    fn the_api_faces_find_the_file_beside_the_db() {
        if std::env::var(POLICY_ENV).is_err() {
            assert_eq!(
                resolve_path("/data/shogun/memory.db"),
                PathBuf::from("/data/shogun/composio.json")
            );
        }
    }
}
