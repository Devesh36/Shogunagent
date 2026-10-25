//! Memory import (issues #384 / #391): claims about the user that another assistant produced, or
//! that the ChatGPT Mac app's "Computer History" wrote as Markdown, brought into SHOGUN **locally,
//! on the user's initiative, and only after review**.
//!
//! The flow is validate → preview → commit, and it is stateless between the steps:
//!
//! 1. [`package::parse_package`] checks the `shogun-memory-import/v1` shape strictly. Package-level
//!    problems (wrong format, oversize, a section of the wrong type) reject the whole package;
//!    item-level problems (a missing field, an unknown status, a value that looks like a secret or
//!    an instruction to the assistant) reject only that item, reported by id and reason — never by
//!    value, so a rejected secret is not echoed back.
//! 2. [`preview::preview`] reads the database (never writes) and gives every valid item an id, a
//!    proposed action (`new`, `merge_into` an existing record, or `duplicate_skip`), and the
//!    confidence it would be stored at — capped so an imported claim is never High.
//! 3. [`store::commit`] re-parses and re-previews the same request inside one transaction and
//!    persists only the ids the caller selected, under a new `memory_imports` row that is the
//!    provenance of every item (`imported:<platform>`).
//!
//! Nothing here calls a model or leaves the device. [`computer_history`] converts Markdown the
//! user handed over; it has no default path and never discovers files on its own.
//!
//! Design note and slice plan: `docs/specs/memory-import.md`.

pub mod computer_history;
pub mod package;
pub mod preview;
pub mod recall;
pub mod store;

use serde::Deserialize;
use unicode_normalization::UnicodeNormalization;

pub use package::{parse_package, ImportPackage, ValidationErrors};
pub use preview::{preview, ImportPreview};
pub use recall::{search_imported, ImportedMatch};
pub use store::{commit, delete_import, get_import, list_imports, CommitOutcome};

/// The one package format this module accepts.
pub const FORMAT: &str = "shogun-memory-import/v1";

/// Largest package JSON (or the Computer History Markdown together), on every face. The request
/// around it is bounded separately by [`MAX_ENVELOPE_BYTES`].
pub const MAX_REQUEST_BYTES: usize = 256 * 1024;

/// Largest request envelope: [`parse_request`] refuses past it, and the loopback REST server reads
/// the two memory-import bodies up to it (its other bodies stay at 256 KiB; the MCP stdio line cap
/// is larger). It only bounds the parse: the package inside is held to [`MAX_REQUEST_BYTES`] by
/// [`parse_package`]. The desktop and CLI send the package as a JSON string, which escaping can
/// double, and a commit adds up to [`package::MAX_TOTAL_ITEMS`] ids, so a tighter envelope would
/// refuse a package the package cap accepts (#391).
pub const MAX_ENVELOPE_BYTES: usize = 2 * MAX_REQUEST_BYTES + 64 * 1024;

/// Stored confidence for a `low` claim (Low band, < 0.5).
pub const CONFIDENCE_LOW: f64 = 0.3;
/// Stored confidence for a `medium` claim (the Medium floor).
pub const CONFIDENCE_MEDIUM: f64 = 0.5;
/// The ceiling for any imported claim — a `high` claim is stored here. Still Medium (< 0.8): a
/// statement another assistant summarised is not a fact SHOGUN observed. Same ceiling as the
/// extracted `knowledge_entities` (V37); the `memory_import_items` CHECK enforces it.
pub const CONFIDENCE_MAX: f64 = 0.6;

/// Confidence of a `people` row created from an import with `people_to_state` (owner decision
/// 2026-10-12): the user handed these people over themselves, so it is authoritative like a user
/// correction, but still below the 0.8+ "observed fact" band's top. Rows are pinned (V49).
pub const PEOPLE_CONFIDENCE: f64 = 0.8;

/// `people.extraction_key` prefix of a row an import created: lets undo find it, and lets later
/// evidence for the same name link onto it (`memory_identity::resolve_entity`).
pub const PEOPLE_KEY_PREFIX: &str = "import:";

/// Why a request was refused. Messages are fixed strings (they reach logs and API responses) —
/// never a value from the package — and start with `invalid ` for the caller's mistakes.
#[derive(Debug)]
pub enum ImportError {
    /// The package failed shape validation; the details carry paths and reason codes only.
    Package(ValidationErrors),
    /// The request envelope or the selection is wrong.
    Invalid(&'static str),
    Db(rusqlite::Error),
}

impl From<rusqlite::Error> for ImportError {
    fn from(e: rusqlite::Error) -> Self {
        ImportError::Db(e)
    }
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportError::Package(_) => f.write_str("invalid package"),
            ImportError::Invalid(why) => f.write_str(why),
            ImportError::Db(_) => f.write_str("memory import failed"),
        }
    }
}

/// Which items to commit: explicit preview ids, or every item the preview proposes by default
/// (not a duplicate, not superseded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    Ids(Vec<String>),
    Default,
}

/// Where a request came from. Recorded on the import row; API and UI imports are otherwise
/// identical (invariant 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    Ui,
    Api,
}

impl Via {
    pub fn as_str(self) -> &'static str {
        match self {
            Via::Ui => "ui",
            Via::Api => "api",
        }
    }
}

/// The shared request body every face sends to preview / commit:
///
/// * `{"package": <object or JSON string>}` — a pasted `shogun-memory-import/v1` package, or
/// * `{"computer_history": [{"name": "file.md", "markdown": "…"}]}` — Computer History files the
///   user handed over (their *contents*; the API never takes a path).
///
/// Commit adds `"select": ["entities/0", …]` or `"select": "default"`.
#[derive(Debug, Clone)]
pub struct ImportRequest {
    pub package_json: String,
    pub selection: Option<Selection>,
    /// Commit only: also create `people` state rows for new `person` entities (see
    /// [`PEOPLE_CONFIDENCE`]). Off by default — an AI-summarised package stays a weak claim.
    pub people_to_state: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRequest {
    package: Option<serde_json::Value>,
    computer_history: Option<Vec<computer_history::MarkdownFile>>,
    select: Option<serde_json::Value>,
    people_to_state: Option<bool>,
}

/// Parse the shared request body. The package inside is validated later by [`parse_package`]
/// (so its per-item errors survive); this only checks the envelope.
pub fn parse_request(body: &str) -> Result<ImportRequest, ImportError> {
    if body.len() > MAX_ENVELOPE_BYTES {
        return Err(ImportError::Invalid("invalid request: too large"));
    }
    let raw: RawRequest = serde_json::from_str(body)
        .map_err(|_| ImportError::Invalid("invalid request: expected {package} or {computer_history}"))?;
    let package_json = match (raw.package, raw.computer_history) {
        (Some(serde_json::Value::String(text)), None) => text,
        (Some(value @ serde_json::Value::Object(_)), None) => value.to_string(),
        (Some(_), None) => return Err(ImportError::Invalid("invalid package: expected a JSON object")),
        (None, Some(files)) => computer_history::convert(&files)?.to_string(),
        (Some(_), Some(_)) => {
            return Err(ImportError::Invalid("invalid request: package and computer_history are exclusive"))
        }
        (None, None) => return Err(ImportError::Invalid("invalid request: expected {package} or {computer_history}")),
    };
    let selection = match raw.select {
        None => None,
        Some(serde_json::Value::String(s)) if s == "default" => Some(Selection::Default),
        Some(serde_json::Value::Array(ids)) => {
            let ids = ids
                .into_iter()
                .map(|v| v.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()
                .ok_or(ImportError::Invalid("invalid selection: ids must be strings"))?;
            Some(Selection::Ids(ids))
        }
        Some(_) => return Err(ImportError::Invalid("invalid selection: expected an id list or \"default\"")),
    };
    Ok(ImportRequest { package_json, selection, people_to_state: raw.people_to_state.unwrap_or(false) })
}

/// The dedupe key: NFKC, lowercase, whitespace collapsed, surrounding punctuation trimmed.
/// "Acme Corp." / "ACME  corp" / "ａｃｍｅ corp" share one key.
pub fn normalize_key(text: &str) -> String {
    let folded: String = text.nfkc().flat_map(char::to_lowercase).collect();
    let collapsed = folded.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_fold_case_width_space_and_edge_punctuation() {
        assert_eq!(normalize_key("  Acme   Corp. "), "acme corp");
        assert_eq!(normalize_key("ＡＣＭＥ corp"), "acme corp");
        assert_eq!(normalize_key("**Alex**"), "alex");
        assert_eq!(normalize_key("C++"), "c");
        assert_eq!(normalize_key("..."), "");
    }

    #[test]
    fn request_envelope_accepts_object_string_or_markdown_and_nothing_else() {
        let obj = parse_request(r#"{"package":{"format":"x"}}"#).unwrap();
        assert_eq!(obj.package_json, r#"{"format":"x"}"#);
        assert!(obj.selection.is_none());
        let s = parse_request(r#"{"package":"{\"format\":\"x\"}","select":"default"}"#).unwrap();
        assert_eq!(s.package_json, r#"{"format":"x"}"#);
        assert_eq!(s.selection, Some(Selection::Default));
        let ids = parse_request(r#"{"package":{},"select":["entities/0"]}"#).unwrap();
        assert_eq!(ids.selection, Some(Selection::Ids(vec!["entities/0".into()])));
        let md = parse_request(r###"{"computer_history":[{"name":"a.md","markdown":"## Goals\n- Ship v1"}]}"###)
            .unwrap();
        assert!(md.package_json.contains("computer_history"));
        for bad in [
            "",
            "[]",
            "{}",
            r#"{"package":[]}"#,
            r#"{"package":{},"computer_history":[]}"#,
            r#"{"package":{},"select":[1]}"#,
            r#"{"package":{},"select":"all"}"#,
            r#"{"package":{},"path":"/Users/me/.codex"}"#,
        ] {
            let err = parse_request(bad).unwrap_err();
            assert!(err.to_string().starts_with("invalid "), "{bad}: {err}");
        }
    }

    /// #391: a package at the package cap, sent as a string (escaping doubles it) with every
    /// item id selected, still reaches `parse_package`; only a body past the envelope is refused.
    #[test]
    fn envelope_fits_an_escaped_package_at_the_cap_with_every_id_selected() {
        let text = format!("{{\"x\":\"{}\"}}", "\\\"".repeat((MAX_REQUEST_BYTES - 8) / 2));
        assert!(text.len() <= MAX_REQUEST_BYTES);
        let ids: Vec<String> = (0..package::MAX_TOTAL_ITEMS).map(|i| format!("relationships/{i}")).collect();
        let body = serde_json::json!({ "package": text, "select": ids }).to_string();
        assert!(body.len() > MAX_REQUEST_BYTES + 4096, "the old envelope refused this");
        let request = parse_request(&body).unwrap();
        assert_eq!(request.package_json, text);
        assert!(matches!(request.selection, Some(Selection::Ids(ref ids)) if ids.len() == package::MAX_TOTAL_ITEMS));
        let err = parse_request(&"x".repeat(MAX_ENVELOPE_BYTES + 1)).unwrap_err();
        assert_eq!(err.to_string(), "invalid request: too large");
    }
}
