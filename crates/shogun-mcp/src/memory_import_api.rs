//! Memory import (issues #384 / #391) on the Memory API faces: one typed operation set every face
//! builds from its own wire shape and the daemon answers from `shogun_memory::memory_import`.
//!
//! Like the voice dictionary, this does not ride the text-oriented `write` seam: a preview is a
//! structured answer to a request *body*, and a commit returns more than a row id. The body is the
//! shared request JSON (`{"package": …}` or `{"computer_history": [{"name","markdown"}]}`, plus
//! `"select"` on commit) on every face — the faces never take a file path, so no API caller can
//! point SHOGUN at another app's data directory (#391).
//!
//! Levels: preview and list are reads (nothing is written); commit and delete are L1 — a local
//! write to the user's own memory, the same tier as `memory.append_note` / `state.update`, with no
//! egress. Imported items stay below the fact band and are undone whole by `memory.import.delete`.

use crate::memory_api::Tool;

/// The explicit-path reader and the file shape, for the faces that start from files the user named
/// (the CLI; the desktop drop zone). Re-exported so they need no direct store dependency.
pub use shogun_memory::memory_import::computer_history::{read_explicit_paths, MarkdownFile, ReadError};

/// Read the Computer History files or folder the user named and convert them into the
/// `shogun-memory-import/v1` package text the desktop paste box holds, so a dropped folder and a
/// pasted package share one preview → edit → commit path (and a commit sends exactly the package
/// the user reviewed, not a re-read of files that may have changed). Pretty-printed while that
/// still fits the request cap. Past the cap even compact, it is `too_large` here, where the screen
/// can say so about the files: short lines expand into whole items, so Markdown under the read cap
/// can convert to a package over it.
///
/// The error is a fixed code (`no_paths`, `not_found`, `not_markdown`, `no_markdown`, `too_large`,
/// `too_many`, `unreadable`) the screen words — never a path (it names the user) and never a value
/// from the files.
pub fn package_text_from_paths(paths: &[std::path::PathBuf]) -> Result<String, &'static str> {
    let files = read_explicit_paths(paths).map_err(|e| match e {
        ReadError::NoPaths => "no_paths",
        ReadError::NotFound => "not_found",
        ReadError::NotMarkdown => "not_markdown",
        ReadError::TooLarge => "too_large",
        ReadError::TooMany => "too_many",
        ReadError::Unreadable => "unreadable",
    })?;
    if files.is_empty() {
        return Err("no_markdown"); // a folder with no Markdown directly inside it
    }
    let package = shogun_memory::memory_import::computer_history::convert(&files).map_err(|_| "too_large")?;
    let cap = shogun_memory::memory_import::MAX_REQUEST_BYTES;
    match serde_json::to_string_pretty(&package) {
        Ok(pretty) if pretty.len() <= cap => Ok(pretty),
        _ => {
            let compact = package.to_string();
            if compact.len() <= cap { Ok(compact) } else { Err("too_large") }
        }
    }
}

/// The shared request body for a package the user holds as text (the desktop paste box):
/// `{"package": "<text>"}`, plus `"select": [ids…]` on commit. The text goes in as a string so the
/// package validator — not this envelope — reports JSON the user still has to fix.
pub fn package_request_body(package_text: &str, select: Option<&[String]>, people_to_state: bool) -> String {
    let mut body = serde_json::json!({ "package": package_text });
    if let Some(ids) = select {
        body["select"] = serde_json::json!(ids);
    }
    if people_to_state {
        body["people_to_state"] = serde_json::json!(true);
    }
    body.to_string()
}

/// One memory-import request, already lifted off its wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryImportOperation {
    /// Validate + propose. `body` is the shared request JSON.
    Preview { body: String },
    /// Persist the selection. `body` is the shared request JSON with `select`.
    Commit { body: String },
    /// Every import, or one (`id`) with its items; `include_low` is the FR-API-06 opt-in.
    List { id: Option<i64>, include_low: bool },
    /// Undo one import.
    Delete { id: i64 },
}

/// Whether `tool` is answered by [`crate::backend::MemoryBackend::memory_import`].
pub fn is_memory_import_tool(tool: Tool) -> bool {
    matches!(
        tool,
        Tool::MemoryImportPreview | Tool::MemoryImport | Tool::MemoryImportList | Tool::MemoryImportDelete
    )
}

/// Build the operation for `tool` from a face's pieces: the request body (MCP arguments / REST
/// body) and the id (MCP `id` argument / REST path). `None` when a required id is missing.
pub fn operation(tool: Tool, body: &str, id: Option<i64>, include_low: bool) -> Option<MemoryImportOperation> {
    let id = id.filter(|id| *id > 0);
    match tool {
        Tool::MemoryImportPreview => Some(MemoryImportOperation::Preview { body: body.to_string() }),
        Tool::MemoryImport => Some(MemoryImportOperation::Commit { body: body.to_string() }),
        Tool::MemoryImportList => Some(MemoryImportOperation::List { id, include_low }),
        Tool::MemoryImportDelete => id.map(|id| MemoryImportOperation::Delete { id }),
        _ => None,
    }
}

/// The HTTP status for a memory-import answer: caller mistakes are 400, a missing import 404,
/// a completed write 202 (the Memory API's write status), a read 200.
pub fn http_status(tool: Tool, json: &str) -> u16 {
    let error = serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string));
    match error.as_deref() {
        Some("not_found") => 404,
        Some(code) if code.starts_with("invalid") => 400,
        Some(_) => 500,
        None if matches!(tool, Tool::MemoryImport | Tool::MemoryImportDelete) => 202,
        None => 200,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operations_and_statuses() {
        assert_eq!(
            operation(Tool::MemoryImportPreview, "{}", None, false),
            Some(MemoryImportOperation::Preview { body: "{}".into() })
        );
        assert_eq!(operation(Tool::MemoryImportDelete, "", None, false), None);
        assert_eq!(operation(Tool::MemoryImportDelete, "", Some(0), false), None);
        assert_eq!(
            operation(Tool::MemoryImportList, "", Some(3), true),
            Some(MemoryImportOperation::List { id: Some(3), include_low: true })
        );
        assert_eq!(operation(Tool::StateUpdate, "{}", None, false), None);
        assert!(!is_memory_import_tool(Tool::StateUpdate));

        assert_eq!(http_status(Tool::MemoryImportPreview, r#"{"items":[]}"#), 200);
        assert_eq!(http_status(Tool::MemoryImport, r#"{"import_id":1}"#), 202);
        assert_eq!(http_status(Tool::MemoryImport, r#"{"error":"invalid_selection"}"#), 400);
        assert_eq!(http_status(Tool::MemoryImportPreview, r#"{"error":"invalid_package"}"#), 400);
        assert_eq!(http_status(Tool::MemoryImportList, r#"{"error":"not_found"}"#), 404);
        assert_eq!(http_status(Tool::MemoryImportList, r#"{"error":"unavailable"}"#), 500);
    }

    #[test]
    fn package_body_carries_text_and_selection() {
        let body: serde_json::Value = serde_json::from_str(&package_request_body("{\"a\":1}", None, false)).unwrap();
        assert_eq!(body, serde_json::json!({ "package": "{\"a\":1}" }));
        let ids = vec!["entities/0".to_string()];
        let body: serde_json::Value =
            serde_json::from_str(&package_request_body("x", Some(ids.as_slice()), false)).unwrap();
        assert_eq!(body["select"], serde_json::json!(["entities/0"]));
        let parsed = shogun_memory::memory_import::parse_request(&package_request_body("{}", Some(&[]), false)).unwrap();
        assert_eq!(parsed.package_json, "{}");
    }

    #[test]
    fn named_files_become_package_text_and_errors_never_name_the_path() {
        let dir = std::env::temp_dir().join(format!("shogun-import-ui-{}", std::process::id()));
        let folder = dir.join("history");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("memory.md"), "## Goals\n- Ship v1\n").unwrap();
        std::fs::write(folder.join("notes.txt"), "ignored").unwrap();

        let text = package_text_from_paths(std::slice::from_ref(&folder)).unwrap();
        let package = shogun_memory::memory_import::parse_package(&text).unwrap();
        assert_eq!(package.platform.as_str(), "computer_history");
        assert_eq!(package.items.len(), 1);
        assert_eq!(package.items[0].name, "Ship v1");

        assert_eq!(package_text_from_paths(&[dir.join("private-name.md")]), Err("not_found"));
        assert_eq!(package_text_from_paths(&[folder.join("notes.txt")]), Err("not_markdown"));
        assert_eq!(package_text_from_paths(&[]), Err("no_paths"));
        let empty = dir.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(package_text_from_paths(&[empty]), Err("no_markdown"));
        // ~100 KiB of one-word bullets reads fine but converts to megabytes of items.
        let bullets: String = (0..12_000).map(|i| format!("- p{i}\n")).collect();
        std::fs::write(dir.join("many.md"), format!("## Projects\n{bullets}")).unwrap();
        assert_eq!(package_text_from_paths(&[dir.join("many.md")]), Err("too_large"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
