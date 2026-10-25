//! The reporter's own list of support reports (issue #108): `support_reports.json`.
//!
//! The desktop writes it — one row per report sent from Help & Support, refreshed from the
//! server when the panel opens — and every face reads it (invariant 6): the panel, and
//! `support.reports.list` over MCP / REST / CLI. It holds the ticket id, category and the
//! server's status; **never** the report text and **never** the reporter token (that is a bearer
//! credential and lives in the Keychain, invariant 7).

use std::path::{Path, PathBuf};

/// One report as this Mac remembers it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReportRecord {
    pub ticket_id: String,
    pub category: String,
    /// Unix ms the report was sent.
    pub created_at: i64,
    /// The server's status as of `checked_at` (`open` until first checked).
    pub status: String,
    #[serde(default)]
    pub released_in: Option<String>,
    /// The operator's need_info question, when the ticket is waiting on the reporter.
    #[serde(default)]
    pub question: Option<String>,
    /// Unix ms of the last successful status check, if any.
    #[serde(default)]
    pub checked_at: Option<i64>,
}

/// At most this many reports are remembered; the oldest fall off.
pub const MAX_REPORTS: usize = 50;

pub fn path() -> Option<PathBuf> {
    crate::plan_source::support_reports_path()
}

pub fn load_at(path: &Path) -> Vec<ReportRecord> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

pub fn load() -> Vec<ReportRecord> {
    path().map(|p| load_at(&p)).unwrap_or_default()
}

/// Write the list (newest first, capped) through a temporary file and a rename, so a reader
/// never sees half a file.
pub fn save_at(path: &Path, reports: &[ReportRecord]) -> std::io::Result<()> {
    let mut sorted = reports.to_vec();
    sorted.sort_by_key(|r| std::cmp::Reverse(r.created_at));
    sorted.truncate(MAX_REPORTS);
    let json = serde_json::to_string_pretty(&sorted).map_err(std::io::Error::other)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("json.tmp{}", std::process::id()));
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)
}

/// `support.reports.list`: the reports, newest first, as the API returns them.
pub fn list_json() -> String {
    serde_json::json!({ "reports": load() }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: &str, at: i64) -> ReportRecord {
        ReportRecord {
            ticket_id: id.into(),
            category: "bug".into(),
            created_at: at,
            status: "open".into(),
            released_in: None,
            question: None,
            checked_at: None,
        }
    }

    #[test]
    fn saves_newest_first_capped_and_reads_back() {
        let dir = std::env::temp_dir().join(format!("shogun-support-reports-{}", std::process::id()));
        let path = dir.join("support_reports.json");
        let mut all: Vec<_> = (0..(MAX_REPORTS as i64 + 5)).map(|i| rec(&format!("t{i}"), i)).collect();
        all[3].status = "need_info".into();
        all[3].question = Some("Which Mac?".into());
        save_at(&path, &all).unwrap();
        let back = load_at(&path);
        assert_eq!(back.len(), MAX_REPORTS);
        assert_eq!(back[0].ticket_id, format!("t{}", MAX_REPORTS as i64 + 4));
        assert!(back.iter().all(|r| r.ticket_id != "t0"), "the oldest fell off");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("token"), "no token field is ever written");
        assert!(load_at(&dir.join("missing.json")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
