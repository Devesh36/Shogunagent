//! Non-secret direct Gmail sending policy shared by desktop and API faces.
//! OAuth credentials stay in Keychain. A missing or unreadable policy keeps draft-only on.

use std::path::{Path, PathBuf};

pub const POLICY_FILE: &str = "gmail-policy.json";
pub const POLICY_ENV: &str = "SHOGUN_GMAIL_POLICY";

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GmailPolicy {
    pub draft_stop: bool,
}

impl Default for GmailPolicy {
    fn default() -> Self {
        Self { draft_stop: true }
    }
}

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

/// The seal on `gmail-policy.json` (audit 2026-10-20 M6): installed by
/// `shogun_core::settings_seal`. Draft-only turned off outside the app is not followed; turned on,
/// it is.
pub static SEAL: crate::settings_seal_hook::SealSlot<GmailPolicy> =
    crate::settings_seal_hook::SealSlot::new();

pub fn load_effective(path: &Path) -> GmailPolicy {
    let file = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    SEAL.trusted(path, file)
}

fn load_strict(path: &Path) -> Result<GmailPolicy, String> {
    let file = match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|_| "Gmail settings are unreadable")?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => GmailPolicy::default(),
        Err(_) => return Err("Gmail settings are unreadable".into()),
    };
    Ok(SEAL.trusted(path, file))
}

pub fn update(
    path: &Path,
    patch: impl FnOnce(&mut GmailPolicy) -> Result<(), String>,
) -> Result<GmailPolicy, String> {
    locked_update(path, false, patch)
}

pub fn update_strict(
    path: &Path,
    patch: impl FnOnce(&mut GmailPolicy) -> Result<(), String>,
) -> Result<GmailPolicy, String> {
    locked_update(path, true, patch)
}

fn locked_update(
    path: &Path,
    strict: bool,
    patch: impl FnOnce(&mut GmailPolicy) -> Result<(), String>,
) -> Result<GmailPolicy, String> {
    let _lock = crate::settings_file::lock(path, "Gmail settings are busy")?;
    let before = if strict {
        load_strict(path)?
    } else {
        load_effective(path)
    };
    let mut policy = before;
    patch(&mut policy)?;
    if policy != before {
        let json = serde_json::to_vec_pretty(&policy)
            .map_err(|_| "Gmail settings could not be saved".to_string())?;
        crate::settings_file::write_atomic(path, &json, true)?;
        // The app's change, or draft-only turned on from an API face: seal it (M6).
        SEAL.seal(path, &policy);
    }
    Ok(policy)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrupt_policy_cannot_enable_sending_or_be_replaced_by_api() {
        let path = crate::composio_policy::tests::temp_path("gmail-corrupt");
        assert!(load_effective(&path).draft_stop);
        std::fs::write(&path, r#"{"draft_stop":false,"token":"bad"}"#).unwrap();
        assert!(load_effective(&path).draft_stop);
        assert!(update_strict(&path, |p| {
            p.draft_stop = true;
            Ok(())
        })
        .is_err());
        assert!(std::fs::read_to_string(&path).unwrap().contains("token"));
        std::fs::remove_file(&path).unwrap();
        let _ = std::fs::remove_file(path.with_extension("json.lock"));
    }

    #[test]
    fn direct_policy_is_independent_of_legacy_consent() {
        let path = crate::composio_policy::tests::temp_path("gmail-policy");
        update(&path, |p| {
            p.draft_stop = false;
            Ok(())
        })
        .unwrap();
        assert!(!load_effective(&path).draft_stop);
        update_strict(&path, |p| {
            p.draft_stop = true;
            Ok(())
        })
        .unwrap();
        assert!(load_effective(&path).draft_stop);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\n  \"draft_stop\": true\n}"
        );
        std::fs::remove_file(&path).unwrap();
        let _ = std::fs::remove_file(path.with_extension("json.lock"));
    }
}
