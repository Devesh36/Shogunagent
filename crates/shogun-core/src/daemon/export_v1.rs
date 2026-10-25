//! Export format v1 on the store (plan E7): `state.export {format: "export-v1"}` and the
//! export-v1 branch of `memory.import`. The format and the restore live in
//! `shogun_memory::export`; this module holds the transaction and the cursor bookkeeping.

use serde_json::{json, Value};
use shogun_mcp::export_api::{ExportOutput, ExportRequest};
use shogun_memory::export;

use super::Db;

impl Db {
    /// One export. A differential export reads from the stored cursor and advances it in the
    /// same transaction, so two concurrent differential exports cannot both claim one window.
    pub fn export_v1(&self, request: &ExportRequest) -> Result<ExportOutput, String> {
        let now = self.now_ms();
        self.with_conn_mut("state.export_v1", |conn| -> Result<ExportOutput, rusqlite::Error> {
            let own_transaction = conn.is_autocommit();
            if own_transaction {
                conn.execute_batch("BEGIN IMMEDIATE")?;
            }
            let result = (|| -> Result<ExportOutput, rusqlite::Error> {
                let since = if request.differential { export::cursor(conn)? } else { request.since_cursor };
                let built = export::export_v1(conn, since, now)?;
                if request.differential {
                    export::advance_cursor(conn, built.cursor)?;
                }
                Ok(ExportOutput { jsonl: built.jsonl(), cursor: built.cursor, since: built.since, records: built.records() })
            })();
            if own_transaction {
                match &result {
                    Ok(_) => conn.execute_batch("COMMIT")?,
                    Err(_) => {
                        let _ = conn.execute_batch("ROLLBACK");
                    }
                }
            }
            result
        })
        .map_err(|_| "export_unavailable".to_string())
    }

    /// The `memory.import.preview` answer for an export v1 package: what it holds, nothing read
    /// or written beyond parsing.
    pub(super) fn export_v1_import_preview(text: &str) -> Value {
        let mut counts = (0usize, 0usize, 0usize);
        for line in text.lines().filter(|l| !l.trim().is_empty()).skip(1) {
            match serde_json::from_str::<Value>(line).ok().as_ref().and_then(|v| v["type"].as_str().map(str::to_string)).as_deref() {
                Some("entity") => counts.0 += 1,
                Some("fact") => counts.1 += 1,
                Some("tombstone") => counts.2 += 1,
                _ => {}
            }
        }
        json!({
            "format": export::FORMAT,
            "action": "restore",
            "entities": counts.0,
            "facts": counts.1,
            "tombstones": counts.2,
            "note": "An export v1 restores every entity and fact with its quotes as new rows under this store's ids; confidence is capped below High until reviewed. Tombstones name the exporting store's ids and are not applied. Commit with memory.import (no select needed).",
        })
    }

    /// The `memory.import` commit for an export v1 package, as `ctx`.
    pub(super) fn export_v1_import_commit(&self, text: &str, ctx: &shogun_memory::actor::WriteContext) -> Result<Value, String> {
        let now = self.now_ms();
        let outcome = self
            .with_conn_mut("memory_import.export_v1", |conn| match export::import_v1(conn, text, ctx, now) {
                Ok(report) => Ok(json!({ "format": export::FORMAT, "restored": report })),
                Err(export::ImportError::NotExportV1) => Ok(json!({ "error": "invalid_package", "reason": "not an export v1" })),
                Err(export::ImportError::TooLarge) => Ok(json!({ "error": "invalid_request", "reason": "invalid request: too large" })),
                Err(export::ImportError::Db(error)) => Err(error),
            })
            .map_err(|fault| format!("memory import failed: {}", fault.as_str()))?;
        if outcome.get("restored").is_some() {
            super::maintenance::notify_state_changed();
        }
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use std::sync::Arc;

    use super::*;
    use shogun_memory::actor::{Actor, WriteContext};

    fn db_at(now: i64) -> Db {
        Db::open_in_memory(Arc::new(move || now)).expect("in-memory store")
    }

    /// Ingest one promise and confirm it as a commitment with its quote, the way the review does.
    fn seed_commitment(db: &Db, text: &str) -> i64 {
        let request = shogun_mcp::ingest_api::ingest_request(&json!({
            "events": [{ "content": text }], "source": "t", "actor": "agent:t", "idempotency_key": text,
        }))
        .unwrap();
        let event_id = db.agent_ingest(&request).unwrap().event_ids[0];
        db.with_conn_mut("seed", |c| -> Result<i64, shogun_memory::MemoryError> {
            let id = shogun_memory::state::insert_commitment(
                c,
                &shogun_memory::state::NewCommitment {
                    direction: shogun_memory::state::CommitmentDirection::Mine,
                    counterparty_id: None,
                    description: text,
                    due_at: None,
                    status: shogun_memory::state::CommitmentStatus::Open,
                    project_id: None,
                    confidence: 0.9,
                    now: 1_000,
                },
                &[shogun_memory::state::Provenance::new(event_id)],
            )?;
            c.execute(
                "UPDATE state_provenance SET quote = ?1 WHERE state_table = 'commitments' AND state_id = ?2",
                rusqlite::params![text, id],
            )?;
            Ok(id)
        })
        .unwrap()
    }

    #[test]
    fn state_export_emits_v1_through_the_face_and_advances_only_a_differential_cursor() {
        let db = db_at(10_000);
        seed_commitment(&db, "I'll send the rollout plan to Oren by Friday.");
        let backend = crate::db_backend::DbBackend::new(db.clone());
        let text = shogun_mcp::export_api::serve(&json!({ "format": "export-v1" }), &backend).unwrap();
        let envelope: Value = serde_json::from_str(&text).unwrap();
        let jsonl = envelope["result"]["jsonl"].as_str().unwrap();
        assert!(export::is_export_v1(jsonl));
        assert!(jsonl.contains("\"quote\":\"I'll send the rollout plan to Oren by Friday.\""), "{jsonl}");
        assert_eq!(envelope["usage"]["cost_micro_usd"], 0, "exports are free");
        // A full export leaves the cursor alone; a differential one advances it.
        assert_eq!(db.with_conn("cursor", export::cursor).unwrap(), None);
        let first = db.export_v1(&ExportRequest { differential: true, since_cursor: None }).unwrap();
        assert_eq!(first.since, None);
        assert!(first.cursor >= 10_000, "the cursor is at least the export's time");
        assert_eq!(db.with_conn("cursor", export::cursor).unwrap(), Some(first.cursor));
        // Nothing changed since: the next differential export carries nothing, not even a row
        // whose provenance edit put its `updated_at` a millisecond past the clock.
        let second = db.export_v1(&ExportRequest { differential: true, since_cursor: None }).unwrap();
        assert_eq!((second.since, second.records), (Some(first.cursor), 0));
    }

    #[test]
    fn memory_import_accepts_an_export_v1_and_restores_rows_and_quotes() {
        let source = db_at(10_000);
        seed_commitment(&source, "I'll send the rollout plan to Oren by Friday.");
        seed_commitment(&source, "I'll draft the cost estimate for Atlas Ledger.");
        let jsonl = source.export_v1(&ExportRequest::default()).unwrap().jsonl;

        // Every commitment row of the source, the low-confidence local candidates included: an
        // export is the whole world model.
        let count = |db: &Db| -> i64 {
            db.with_conn("count", |c| c.query_row("SELECT COUNT(*) FROM commitments", [], |r| r.get(0))).unwrap()
        };
        let source_rows = count(&source);
        assert!(source_rows >= 2);

        let target = db_at(20_000);
        let body = json!({ "package": jsonl }).to_string();
        let preview = target.memory_import_preview(&body).unwrap();
        assert_eq!(preview["action"], "restore", "{preview}");
        assert_eq!(preview["facts"].as_i64(), Some(source_rows), "{preview}");
        let importer = WriteContext::world(Actor::agent("migrator").unwrap());
        let committed = target.memory_import_commit_as(&body, shogun_memory::memory_import::Via::Api, &importer).unwrap();
        assert_eq!(committed["restored"]["facts"].as_i64(), Some(source_rows), "{committed}");
        let quotes = |db: &Db| -> Vec<String> {
            db.with_conn("quotes", |c| {
                let mut stmt = c.prepare("SELECT quote FROM state_provenance WHERE quote IS NOT NULL ORDER BY quote")?;
                let rows = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>();
                rows
            })
            .unwrap()
        };
        assert_eq!(quotes(&target), quotes(&source));
        assert_eq!(count(&target), source_rows);
    }
}
