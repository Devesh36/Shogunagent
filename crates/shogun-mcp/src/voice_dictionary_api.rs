//! Typed voice-dictionary management contract shared by desktop, MCP, CLI, and loopback REST.
//!
//! This is intentionally separate from the text-oriented Memory API write seam. Vocabulary is
//! private user data, so adapters must deserialize this closed operation set and the DB remains
//! the single validation authority.

use shogun_memory::voice_terms::{NewVoiceTerm, VoiceTerm};
use shogun_memory::voice_terms_import::ImportReport;

/// Longest text one import accepts: 500 terms of generous rows fit many times over, and a
/// pasted spreadsheet is never megabytes. Bounds the parser's work per request.
pub const MAX_IMPORT_CHARS: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceDictionaryOperation {
    List,
    Create(NewVoiceTerm),
    Update {
        id: i64,
        term: NewVoiceTerm,
    },
    Delete {
        id: i64,
    },
    /// Bulk upsert from pasted CSV / TSV / Markdown-table text (gap C2).
    Import {
        text: String,
    },
    /// The whole dictionary as CSV, in the columns `Import` reads back.
    Export,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceDictionaryResult {
    Terms(Vec<VoiceTerm>),
    Term(VoiceTerm),
    Deleted(bool),
    Imported(ImportReport),
    Csv(String),
}

/// Typed CRUD only. Implementations must use the daemon `Db` methods and must not trace term
/// spellings or aliases.
pub trait VoiceDictionaryBackend: Send + Sync {
    fn manage_voice_dictionary(
        &self,
        operation: VoiceDictionaryOperation,
    ) -> Result<VoiceDictionaryResult, String>;
}

pub fn parse_term(body: &str) -> Result<NewVoiceTerm, String> {
    serde_json::from_str(body).map_err(|_| "invalid voice dictionary term".to_string())
}

/// The import body: `{"text": "<csv | tsv | markdown table>"}`. Empty or oversized text is the
/// caller's mistake, reported before any parsing.
pub fn parse_import(body: &str) -> Result<VoiceDictionaryOperation, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| "invalid voice dictionary import".to_string())?;
    let text = value
        .get("text")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "invalid voice dictionary import".to_string())?;
    if text.trim().is_empty() {
        return Err("voice dictionary import text is empty".to_string());
    }
    if text.chars().count() > MAX_IMPORT_CHARS {
        return Err("voice dictionary import text is too long".to_string());
    }
    Ok(VoiceDictionaryOperation::Import {
        text: text.to_string(),
    })
}

pub fn render(result: VoiceDictionaryResult) -> String {
    match result {
        VoiceDictionaryResult::Terms(terms) => serde_json::json!({ "terms": terms }).to_string(),
        VoiceDictionaryResult::Term(term) => serde_json::json!({ "term": term }).to_string(),
        VoiceDictionaryResult::Deleted(deleted) => {
            serde_json::json!({ "deleted": deleted }).to_string()
        }
        VoiceDictionaryResult::Imported(report) => serde_json::json!({
            "created": report.created,
            "updated": report.updated,
            "skipped": report
                .skipped
                .iter()
                .map(|(line, reason)| serde_json::json!({ "line": line, "reason": reason }))
                .collect::<Vec<_>>(),
        })
        .to_string(),
        VoiceDictionaryResult::Csv(csv) => {
            let count = csv.lines().count().saturating_sub(1);
            serde_json::json!({ "csv": csv, "count": count }).to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_body_needs_non_empty_bounded_text() {
        assert!(parse_import("{}").is_err());
        assert!(parse_import(r#"{"text":"   "}"#).is_err());
        assert!(parse_import("not json").is_err());
        let long = format!(r#"{{"text":"{}"}}"#, "a".repeat(MAX_IMPORT_CHARS + 1));
        assert!(parse_import(&long).is_err());
        assert_eq!(
            parse_import(r#"{"text":"canonical\nSelect KK"}"#),
            Ok(VoiceDictionaryOperation::Import {
                text: "canonical\nSelect KK".to_string()
            })
        );
    }

    #[test]
    fn import_report_and_csv_render_as_json() {
        let rendered = render(VoiceDictionaryResult::Imported(ImportReport {
            created: 2,
            updated: 1,
            skipped: vec![(4, "canonical is required".to_string())],
        }));
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value["created"], 2);
        assert_eq!(value["updated"], 1);
        assert_eq!(value["skipped"][0]["line"], 4);
        assert_eq!(value["skipped"][0]["reason"], "canonical is required");

        let rendered = render(VoiceDictionaryResult::Csv(
            "canonical,aliases,locale,scope,scope_ref,priority,enabled\nSelect KK,,,global,,0,true\n"
                .to_string(),
        ));
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value["count"], 1);
        assert!(value["csv"].as_str().unwrap().starts_with("canonical,"));
    }
}
