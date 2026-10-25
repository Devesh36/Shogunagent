//! `state.export` in export format v1 (plan E7, `docs/api/export-v1.md`) and v2 (plan E27,
//! `docs/api/export-v2.md`). Additive to the tool: without `format` (or with `format: "markdown"`)
//! `state.export` answers what it always did; with `format: "export-v1"` or `"export-v2"` it
//! answers the JSON Lines export. One parser and one envelope for every face: MCP
//! `state.export {format: "export-v2", …}`, REST `POST /v1/export` (the same body), CLI
//! `shogun state export --format export-v2 [--differential] [--since-cursor <ms>]`.
//!
//! - `differential: true` exports what changed since the stored `export_cursor` (V96) and then
//!   advances it to this export's cursor — the only write, and it holds no content.
//! - `since_cursor: <ms>` exports what changed after that time and leaves the stored cursor alone.
//! - neither: the full export.
//!
//! Exports are reads: free (invariant 10).

use serde_json::{json, Value};

use crate::memory_api::Tool;

/// The `format` value that selects export v1 (the REST default).
pub const FORMAT_ARG: &str = "export-v1";
/// The `format` value that selects export v2.
pub const FORMAT_ARG_V2: &str = "export-v2";

/// Which JSON Lines export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExportFormat {
    #[default]
    V1,
    V2,
}

impl ExportFormat {
    /// The manifest's `format`.
    pub fn manifest_format(self) -> &'static str {
        match self {
            ExportFormat::V1 => "shogunagent-export/v1",
            ExportFormat::V2 => "shogunagent-export/v2",
        }
    }
}

/// A parsed export request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExportRequest {
    /// Start at the stored cursor and advance it afterwards.
    pub differential: bool,
    /// Start after this time (unix ms); the stored cursor is not touched.
    pub since_cursor: Option<i64>,
    /// v1 or v2.
    pub format: ExportFormat,
}

/// What the store produced.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExportOutput {
    /// The JSON Lines text (manifest first).
    pub jsonl: String,
    /// The cursor the manifest names; a differential export stored it.
    pub cursor: i64,
    /// Where this export started (`None` = full).
    pub since: Option<i64>,
    /// Records after the manifest.
    pub records: usize,
    /// Which format `jsonl` is.
    pub format: ExportFormat,
}

/// Whether `args` ask for export v1.
pub fn wants_v1(args: &Value) -> bool {
    args.get("format").and_then(Value::as_str) == Some(FORMAT_ARG)
}

/// Whether `args` ask for a JSON Lines export (v1 or v2) rather than the Markdown one.
pub fn wants_jsonl(args: &Value) -> bool {
    matches!(args.get("format").and_then(Value::as_str), Some(FORMAT_ARG | FORMAT_ARG_V2))
}

/// Parse the export arguments. `Err` is `<code>: <sentence>`.
pub fn parse(args: &Value) -> Result<ExportRequest, String> {
    if !args.is_object() {
        return Err("invalid_request: the body must be a JSON object.".into());
    }
    let format = match args.get("format").and_then(Value::as_str) {
        Some(FORMAT_ARG) => ExportFormat::V1,
        Some(FORMAT_ARG_V2) => ExportFormat::V2,
        _ => return Err(format!("invalid_format: format must be \"{FORMAT_ARG}\" or \"{FORMAT_ARG_V2}\" for this route.")),
    };
    let differential = match args.get("differential") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => return Err("invalid_differential: differential must be a boolean.".into()),
    };
    let since_cursor = match args.get("since_cursor") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_i64().filter(|n| *n >= 0).ok_or_else(|| "invalid_since_cursor: since_cursor is a non-negative integer (unix ms).".to_string())?),
    };
    if differential && since_cursor.is_some() {
        return Err("invalid_request: pass differential or since_cursor, not both.".into());
    }
    Ok(ExportRequest { differential, since_cursor, format })
}

/// The envelope: the export, its cursor and record count, and a free `usage`.
pub fn render(output: &ExportOutput, latency_ms: u64) -> String {
    json!({
        "tool": Tool::StateExport.wire_name(),
        "result": {
            "format": output.format.manifest_format(),
            "cursor": output.cursor,
            "since_cursor": output.since,
            "records": output.records,
            "jsonl": output.jsonl,
        },
        "usage": { "unit": "read", "quantity": 1, "cost_micro_usd": 0 },
        "latency_ms": latency_ms,
        "next_action": Value::Null,
    })
    .to_string()
}

/// Parse, run and render for any face.
pub fn serve<B: crate::backend::MemoryBackend + ?Sized>(args: &Value, backend: &B) -> Result<String, crate::ingest_api::Refusal> {
    let started = std::time::Instant::now();
    let request = parse(args).map_err(crate::ingest_api::Refusal::BadArguments)?;
    let output = backend.export_v1(&request).map_err(|_| crate::ingest_api::Refusal::Unavailable("export_unavailable"))?;
    Ok(render(&output, u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn the_v1_arguments_parse_and_conflict_where_they_should() {
        assert!(wants_v1(&json!({ "format": "export-v1" })));
        assert!(!wants_v1(&json!({ "include_low": true })));
        assert_eq!(parse(&json!({ "format": "export-v1" })).unwrap(), ExportRequest::default());
        assert!(wants_jsonl(&json!({ "format": "export-v2" })) && !wants_v1(&json!({ "format": "export-v2" })));
        assert_eq!(parse(&json!({ "format": "export-v2", "differential": true })).unwrap().format, ExportFormat::V2);
        assert!(parse(&json!({ "format": "export-v1", "differential": true })).unwrap().differential);
        assert_eq!(parse(&json!({ "format": "export-v1", "since_cursor": 5 })).unwrap().since_cursor, Some(5));
        assert!(parse(&json!({ "format": "export-v1", "differential": true, "since_cursor": 5 })).is_err());
        assert!(parse(&json!({ "format": "markdown" })).unwrap_err().starts_with("invalid_format:"));
        assert!(parse(&json!({ "format": "export-v1", "since_cursor": -1 })).is_err());
    }

    #[test]
    fn the_envelope_is_free_and_carries_the_cursor() {
        let output = ExportOutput { jsonl: "{}\n".into(), cursor: 9, since: Some(3), records: 0, format: ExportFormat::V2 };
        let value: Value = serde_json::from_str(&render(&output, 1)).unwrap();
        assert_eq!(value["tool"], "state.export");
        assert_eq!(value["result"]["cursor"], 9);
        assert_eq!(value["result"]["format"], "shogunagent-export/v2");
        assert_eq!(value["usage"]["cost_micro_usd"], 0);
    }

    #[test]
    fn a_backend_without_a_store_answers_unavailable() {
        let refused = serve(&json!({ "format": "export-v1" }), &crate::backend::StubBackend);
        assert_eq!(refused, Err(crate::ingest_api::Refusal::Unavailable("export_unavailable")));
    }
}
