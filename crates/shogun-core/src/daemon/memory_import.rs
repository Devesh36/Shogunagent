//! Memory import (issues #384 / #391): the one implementation every face calls — the desktop's
//! Import memory screen, and `memory.import.preview` / `memory.import` / `memory.import.list` /
//! `memory.import.delete` on MCP, REST and CLI (invariant 6).
//!
//! Every method returns the JSON a face renders. Caller mistakes come back as an `error` payload
//! with a fixed code (`invalid_package` with per-path reasons, `invalid_request`,
//! `invalid_selection`, `not_found`) — never a package value; `Err` is reserved for a store fault.
//! Local only: no model, no egress, no event_log write.

use serde_json::{json, Value};
use shogun_memory::memory_import::{self, ImportError, Via};

use super::*;

/// The JSON for a caller mistake; a store fault stays an error.
fn refusal(err: ImportError) -> Result<Value, rusqlite::Error> {
    match err {
        ImportError::Package(errors) => Ok(json!({ "error": "invalid_package", "errors": errors.0 })),
        ImportError::Invalid(why) => {
            let code = if why.starts_with("invalid selection") { "invalid_selection" } else { "invalid_request" };
            Ok(json!({ "error": code, "reason": why }))
        }
        ImportError::Db(e) => Err(e),
    }
}

impl Db {
    /// Validate a request and propose an action per item. Reads only — nothing is persisted.
    pub fn memory_import_preview(&self, body: &str) -> Result<Value, String> {
        let request = match memory_import::parse_request(body) {
            Ok(r) => r,
            Err(e) => return refusal(e).map_err(|_| "memory import failed".to_string()),
        };
        let package = match memory_import::parse_package(&request.package_json) {
            Ok(p) => p,
            Err(errors) => return Ok(json!({ "error": "invalid_package", "errors": errors.0 })),
        };
        self.with_conn("memory_import.preview", |c| memory_import::preview(c, &package))
            .map(|preview| serde_json::to_value(preview).unwrap_or_else(|_| json!({ "error": "unavailable" })))
            .map_err(|fault| format!("memory import failed: {}", fault.as_str()))
    }

    /// Persist the selected items of a request under a new import record (L1, local). Returns
    /// `{"import_id","provenance","stored","linked"}` or an `error` payload.
    pub fn memory_import_commit(&self, body: &str, via: Via) -> Result<Value, String> {
        let request = match memory_import::parse_request(body) {
            Ok(r) => r,
            Err(e) => return refusal(e).map_err(|_| "memory import failed".to_string()),
        };
        let now = self.now_ms();
        self.with_conn_mut("memory_import.commit", |c| {
            match memory_import::commit(c, &request, via, now) {
                Ok(outcome) => Ok(serde_json::to_value(outcome).unwrap_or_else(|_| json!({ "error": "unavailable" }))),
                Err(e) => refusal(e),
            }
        })
        .map_err(|fault| format!("memory import failed: {}", fault.as_str()))
    }

    /// Every import (newest first), or one import with its items. Items below the Medium band are
    /// kept only when `include_low` (FR-API-06); every item is flagged `possibly` (none is a fact).
    pub fn memory_import_list(&self, id: Option<i64>, include_low: bool) -> Result<Value, String> {
        use shogun_mcp::memory_api::{read_inclusion, ReadInclusion};
        let fault = |fault: crate::memory_health::MemoryFault| format!("memory import failed: {}", fault.as_str());
        let Some(id) = id else {
            let imports = self.with_conn("memory_import.list", memory_import::list_imports).map_err(fault)?;
            return Ok(json!({ "imports": imports }));
        };
        let Some(mut detail) = self
            .with_conn("memory_import.get", |c| memory_import::get_import(c, id))
            .map_err(fault)?
        else {
            return Ok(json!({ "error": "not_found" }));
        };
        detail.items.retain_mut(|item| match read_inclusion(item.confidence, include_low) {
            ReadInclusion::Included { .. } => {
                item.possibly = true;
                true
            }
            ReadInclusion::Excluded => false,
        });
        Ok(json!({ "import": detail }))
    }

    /// Undo one import: the record and every item it stored (L1, local). Linked records were never
    /// changed, so nothing else moves.
    pub fn memory_import_delete(&self, id: i64) -> Result<Value, String> {
        let deleted = self
            .with_conn_mut("memory_import.delete", |c| memory_import::delete_import(c, id))
            .map_err(|fault| format!("memory import failed: {}", fault.as_str()))?;
        Ok(if deleted { json!({ "deleted": true, "id": id }) } else { json!({ "error": "not_found" }) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn db() -> Db {
        Db::new(shogun_memory::open_in_memory().unwrap(), Arc::new(|| 7_000))
    }

    const BODY: &str = r#"{"package":{"format":"shogun-memory-import/v1","source":{"platform":"cursor"},
        "entities":[{"name":"Atlas","type":"project","confidence":"high","evidence":"e","status":"active"},
                    {"name":"Bad","type":"project","summary":"sk-ant-api03-abcdefghijklmnop","confidence":"low","evidence":"e","status":"active"}],
        "open_loops":[{"summary":"Tiny","status":"open","confidence":"low","evidence":null}]}}"#;

    #[test]
    fn preview_commit_list_delete_round_trip() {
        let db = db();
        let preview = db.memory_import_preview(BODY).unwrap();
        assert_eq!(preview["counts"]["new"], 2);
        assert_eq!(preview["rejected"][0]["reason"], "sensitive_value");
        assert!(!preview.to_string().contains("abcdefghijklmnop"), "a rejected value is never echoed");
        assert_eq!(db.memory_import_list(None, false).unwrap()["imports"], json!([]), "preview persists nothing");

        let mut commit_body: Value = serde_json::from_str(BODY).unwrap();
        commit_body["select"] = json!("default");
        let commit_body = commit_body.to_string();
        let out = db.memory_import_commit(&commit_body, Via::Api).unwrap();
        assert_eq!((out["stored"].as_u64(), out["provenance"].as_str()), (Some(2), Some("imported:cursor")));
        let id = out["import_id"].as_i64().unwrap();

        let listed = db.memory_import_list(None, false).unwrap();
        assert_eq!(listed["imports"][0]["via"], "api");
        // FR-API-06: the Low item is hidden unless asked for; the Medium one is `possibly`.
        let one = db.memory_import_list(Some(id), false).unwrap();
        assert_eq!(one["import"]["items"].as_array().map(Vec::len), Some(1));
        assert_eq!(one["import"]["items"][0]["possibly"], true);
        assert_eq!(db.memory_import_list(Some(id), true).unwrap()["import"]["items"].as_array().map(Vec::len), Some(2));

        assert_eq!(db.memory_import_delete(id).unwrap()["deleted"], true);
        assert_eq!(db.memory_import_delete(id).unwrap()["error"], "not_found");
        assert_eq!(db.memory_import_list(Some(id), true).unwrap()["error"], "not_found");
    }

    /// #391: a committed Medium claim reaches context assembly as a weak, attributed
    /// line (never a fact, never evidence); a Low one does not; undoing the import removes it.
    #[test]
    fn imported_claims_reach_context_weakly_until_the_import_is_deleted() {
        let db = db();
        let body = r#"{"package":{"format":"shogun-memory-import/v1","source":{"platform":"computer_history"},
            "profile":{"goals":[{"value":"Finish the Heron migration runbook","confidence":"high","evidence":"e","status":"active"}],
                       "preferences":[{"value":"Heron updates in the morning","confidence":"low","evidence":"e","status":"active"}]}},
            "select":"default"}"#;
        let id = db.memory_import_commit(body, Via::Ui).unwrap()["import_id"].as_i64().unwrap();

        let pack = db.assemble_context("Heron migration runbook", 8, 300);
        let line = format!(
            "possibly: imported claim (unverified; imported:computer_history, memory_import:{id}): \
             goal: Finish the Heron migration runbook [active]"
        );
        assert_eq!(pack.facts, std::slice::from_ref(&line), "one weak, attributed line; the Low claim stays out");
        assert!(pack.evidence.is_empty(), "an import is never passed off as captured evidence");

        // The compressed (Fusion) path keeps the same line.
        let (compressed, _, fell_back) = db.assemble_context_compressed_within(
            "Heron migration runbook",
            8,
            300,
            &[],
            &shogun_fusion::compress::CompressionConfig::default(),
            crate::daemon::CompressBudget::Unbounded,
        );
        assert!(!fell_back);
        assert!(compressed.facts.contains(&line), "{:?}", compressed.facts);

        assert_eq!(db.memory_import_delete(id).unwrap()["deleted"], true);
        let after = db.assemble_context("Heron migration runbook", 8, 300);
        assert!(after.facts.iter().all(|f| !f.contains("Heron")), "{:?}", after.facts);
    }

    #[test]
    fn caller_mistakes_are_error_payloads_not_faults() {
        let db = db();
        assert_eq!(db.memory_import_preview("{}").unwrap()["error"], "invalid_request");
        let bad = db.memory_import_preview(r#"{"package":{"format":"v9"}}"#).unwrap();
        assert_eq!(bad["error"], "invalid_package");
        assert!(bad["errors"].as_array().is_some_and(|e| !e.is_empty()));
        let unselected = db.memory_import_commit(BODY, Via::Ui).unwrap();
        assert_eq!(unselected["error"], "invalid_selection");
        assert_eq!(db.memory_import_list(None, false).unwrap()["imports"], json!([]));
    }
}
