//! The Web fetch consent (issue #555 FR-WV-15): the one-time, three-disclosure opt-in that lets
//! SHOGUN fetch a public page, post or video transcript the user names.
//!
//! Same shape as the Composio consent ([`crate::composio`]): a consent value can only be made
//! from all three acknowledgements, and every face reads the same document — the desktop's
//! `<app-data>/web_fetch.json`, written by its Connections card and read here by the MCP / REST
//! backend (invariant 6: one store, every face). No consent → no Tier-1 fetch; the screen and
//! memory (Tier 0) need none, because they add no egress.
//!
//! What the document holds is the acknowledgement, not a secret: three booleans and a timestamp.

use std::path::Path;

use crate::plan_source::web_fetch_json_path;

/// The three disclosures the consent card presents (spec §4-7, FR-WV-15).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Disclosures {
    /// (1) The link (or search term) you paste is sent to that site.
    #[serde(default)]
    pub urls_leave_device: bool,
    /// (2) The text fetched is sent to the model under your own credentials, like screen text.
    #[serde(default)]
    pub text_to_model: bool,
    /// (3) SHOGUN stores no video, audio or image — text only, and never a media file.
    #[serde(default)]
    pub no_media_stored: bool,
}

impl Disclosures {
    pub fn all_acknowledged(self) -> bool {
        self.urls_leave_device && self.text_to_model && self.no_media_stored
    }
}

/// Why consent could not be granted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentError {
    /// One or more disclosures were not acknowledged.
    IncompleteDisclosures,
}

/// Proof that the user completed the Web fetch opt-in. Private field, no `Default`, no public
/// constructor: [`grant_consent`] is the only way to hold one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebFetchConsent {
    _acknowledged: Disclosures,
}

/// Grant consent iff every disclosure is acknowledged.
pub fn grant_consent(disclosures: Disclosures) -> Result<WebFetchConsent, ConsentError> {
    if disclosures.all_acknowledged() {
        Ok(WebFetchConsent { _acknowledged: disclosures })
    } else {
        Err(ConsentError::IncompleteDisclosures)
    }
}

/// The persisted document: `{"disclosures": {…}, "acknowledged_at_ms": n}`. Absent, unreadable
/// or incomplete all read as "not consented" — the safe state is always the fallback.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WebFetchPolicy {
    #[serde(default)]
    pub disclosures: Disclosures,
    /// When the three were acknowledged (unix ms); informational.
    #[serde(default)]
    pub acknowledged_at_ms: Option<i64>,
}

impl WebFetchPolicy {
    /// The consent this document proves, if it proves one.
    pub fn consent(&self) -> Option<WebFetchConsent> {
        grant_consent(self.disclosures).ok()
    }
}

/// Parse a `web_fetch.json` body. Anything that is not a complete acknowledgement is the
/// default (unconsented) policy.
pub fn parse_policy(text: &str) -> WebFetchPolicy {
    let policy: WebFetchPolicy = serde_json::from_str(text).unwrap_or_default();
    if policy.disclosures.all_acknowledged() {
        policy
    } else {
        WebFetchPolicy::default()
    }
}

/// The policy in force before the user has ever saved one: consented (owner decision
/// 2026-10-20). Turning Web fetch off writes an unconsented document, which then wins.
fn shipped_default() -> WebFetchPolicy {
    WebFetchPolicy {
        disclosures: Disclosures { urls_leave_device: true, text_to_model: true, no_media_stored: true },
        acknowledged_at_ms: None,
    }
}

/// The seal on `web_fetch.json` (audit 2026-10-20 M6): installed by `shogun_core::settings_seal`.
/// A consent written outside the app is not followed; a revocation is.
pub static SEAL: crate::settings_seal_hook::SealSlot<WebFetchPolicy> =
    crate::settings_seal_hook::SealSlot::new();

/// Load the policy from one path, checked against its seal. No file is the shipped default
/// (on); an unreadable or incomplete one is unconsented.
pub fn load_at(path: &Path) -> WebFetchPolicy {
    let file = match std::fs::read_to_string(path) {
        Ok(text) => parse_policy(&text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => shipped_default(),
        Err(_) => WebFetchPolicy::default(),
    };
    SEAL.trusted(path, file)
}

/// Write the policy to one path atomically (temp file + rename) and seal it. Only the desktop's
/// consent card writes this file: granting consent is in-app only.
pub fn save_at(path: &Path, policy: &WebFetchPolicy) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let json = serde_json::to_string_pretty(policy).map_err(|e| e.to_string())?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, json).map_err(|e| format!("save failed: {e}"))?;
    std::fs::rename(&temporary, path).map_err(|e| {
        let _ = std::fs::remove_file(&temporary);
        format!("save failed: {e}")
    })?;
    SEAL.seal(path, policy);
    Ok(())
}

/// Load the policy from the shared app-data location. `None` path (no app-data on this
/// platform) is unconsented.
pub fn load() -> WebFetchPolicy {
    web_fetch_json_path()
        .map(|path| load_at(&path))
        .unwrap_or_default()
}

/// Whether the user has granted the Web fetch consent — the single question every face asks
/// before a Tier-1 fetch.
pub fn consented() -> bool {
    load().consent().is_some()
}

/// The one-line status every face can show (`{"consented": bool, "acknowledged_at_ms": n?}`).
pub fn status_json() -> String {
    let policy = load();
    serde_json::json!({
        "consented": policy.consent().is_some(),
        "acknowledged_at_ms": policy.acknowledged_at_ms,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consent_needs_all_three_disclosures() {
        let two = Disclosures { urls_leave_device: true, text_to_model: true, no_media_stored: false };
        assert_eq!(grant_consent(two), Err(ConsentError::IncompleteDisclosures));
        let all = Disclosures { urls_leave_device: true, text_to_model: true, no_media_stored: true };
        assert!(grant_consent(all).is_ok());
    }

    #[test]
    fn no_file_is_consented_and_a_saved_off_wins() {
        let dir = std::env::temp_dir().join(format!("shogun-web-consent-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("web_fetch.json");
        let _ = std::fs::remove_file(&path);
        assert!(load_at(&path).consent().is_some());
        std::fs::write(&path, serde_json::to_string(&WebFetchPolicy::default()).unwrap()).unwrap();
        assert!(load_at(&path).consent().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_partial_or_broken_document_reads_as_unconsented() {
        assert_eq!(parse_policy("not json"), WebFetchPolicy::default());
        assert_eq!(parse_policy("{}"), WebFetchPolicy::default());
        let partial = r#"{"disclosures":{"urls_leave_device":true,"text_to_model":true},"acknowledged_at_ms":5}"#;
        assert!(parse_policy(partial).consent().is_none());
        assert_eq!(parse_policy(partial).acknowledged_at_ms, None, "an incomplete stamp is dropped whole");
        let full = r#"{"disclosures":{"urls_leave_device":true,"text_to_model":true,"no_media_stored":true},"acknowledged_at_ms":5}"#;
        let policy = parse_policy(full);
        assert!(policy.consent().is_some());
        assert_eq!(policy.acknowledged_at_ms, Some(5));
    }
}
