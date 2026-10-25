//! `memory.ingest` and `memory.recall` on the store (plan E4-T3; E5-T1 / E5-T3 finish them).
//!
//! **Ingest** appends an agent's events or turns through the event log's own append path
//! ([`event_log::insert_or_touch_with_thread`]): hidden format characters are stripped and
//! secrets masked before the row is written, an exact duplicate within the thread collapses onto
//! its existing row, the thread key derives from the caller's `thread_key`, and the local-rule
//! extraction runs over new rows exactly as it does for a synced mail or an AI session log. The
//! receipt is kept under the caller's `idempotency_key` in `memory_markers` (V86, the store's own
//! key → integer table), so a repeated key replays the first outcome and appends nothing. A
//! dedicated receipts table (the plan's V99) replaces the markers when the E2 migrations land;
//! until then the markers keep the key in the principal's store and nowhere else.
//!
//! **Recall** is the context-pack assembly ([`Db::assemble_context`]) cut to a token budget with
//! the fusion crate's estimator: High facts first, then the `possibly:` facts, then dated
//! evidence until the budget is spent.
//!
//! Actor and namespace (E2): `actor` and `namespace` are validated by the face and carried here.
//! The per-row columns are E2's (V92); until the merge the actor rides the row's app column and
//! the namespace is recorded in the receipt only ([`agent_actor_scope`] is the seam E2's
//! task-local actor context replaces).

use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use shogun_fusion::budget::{HeuristicEstimator, TokenEstimator};
use shogun_mcp::ingest_api::{IngestOutcome, IngestRequest, RecallEvidence, RecallOutcome, RecallRequest, EVENT_SOURCE};

use super::Db;
use shogun_memory::event_log::{self, NewEvent};

/// Evidence lines a recall may consider before the budget cuts them.
const RECALL_HITS: usize = 24;
/// Longest evidence excerpt a recall reads, in chars.
const RECALL_EXCERPT_CHARS: usize = 600;

/// The `memory_markers` key family of ingest receipts: `ingest:<sha256(key)[..32]>` holds the
/// item count, `ingest:<hash>:<i>` the i-th event id. Hashed so a long or odd key fits the
/// column and never lands in a key string that another reader could mistake for text.
fn receipt_key(idempotency_key: &str) -> String {
    let digest = Sha256::digest(idempotency_key.as_bytes());
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    format!("ingest:{hex}")
}

/// Run `f` as `actor` writing to `namespace`. The seam E2's task-local actor context
/// (`shogun_memory::actor`) replaces: until then it only names the write for the reader.
fn agent_actor_scope<T>(_actor: &str, _namespace: &str, f: impl FnOnce() -> T) -> T {
    f()
}

fn read_receipt(conn: &Connection, key: &str) -> Result<Option<Vec<i64>>, rusqlite::Error> {
    let count: Option<i64> = conn
        .query_row("SELECT value FROM memory_markers WHERE key = ?1", [key], |r| r.get(0))
        .optional()?;
    let Some(count) = count else {
        return Ok(None);
    };
    let mut ids = Vec::with_capacity(usize::try_from(count).unwrap_or(0));
    for index in 0..count {
        let id: Option<i64> = conn
            .query_row(
                "SELECT value FROM memory_markers WHERE key = ?1",
                [format!("{key}:{index}")],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = id {
            ids.push(id);
        }
    }
    Ok(Some(ids))
}

fn write_receipt(conn: &Connection, key: &str, ids: &[i64]) -> Result<(), rusqlite::Error> {
    for (index, id) in ids.iter().enumerate() {
        conn.execute(
            "INSERT OR REPLACE INTO memory_markers (key, value) VALUES (?1, ?2)",
            params![format!("{key}:{index}"), id],
        )?;
    }
    conn.execute(
        "INSERT OR REPLACE INTO memory_markers (key, value) VALUES (?1, ?2)",
        params![key, i64::try_from(ids.len()).unwrap_or(i64::MAX)],
    )?;
    Ok(())
}

impl Db {
    /// `memory.ingest`: see the module docs. One transaction holds the rows and the receipt, so a
    /// crash between them leaves neither; the caller retries with the same key.
    pub fn agent_ingest(&self, request: &IngestRequest) -> Result<IngestOutcome, String> {
        let key = receipt_key(&request.idempotency_key);
        let now = self.now_ms();
        let thread_key = request.thread_key.as_deref();
        let actor = request.actor.as_str();
        let namespace = request.namespace.as_str();
        agent_actor_scope(actor, namespace, || {
            self.with_conn_mut("agent_ingest", |conn| -> Result<IngestOutcome, rusqlite::Error> {
                // The dream-effect fence may already hold a transaction; nest only when free.
                let own_transaction = conn.is_autocommit();
                if own_transaction {
                    conn.execute_batch("BEGIN IMMEDIATE")?;
                }
                let outcome = (|| -> Result<IngestOutcome, rusqlite::Error> {
                    if let Some(ids) = read_receipt(conn, &key)? {
                        return Ok(IngestOutcome {
                            event_ids: ids,
                            inserted: 0,
                            deduplicated: 0,
                            replayed: true,
                            thread_key: thread_key
                                .and_then(|id| shogun_memory::thread::thread_key(EVENT_SOURCE, Some(id), None, None)),
                        });
                    }
                    let mut outcome = IngestOutcome::default();
                    for item in &request.items {
                        let ts = item.ts_ms.unwrap_or(now);
                        // The thread scopes the dedup; the hash is the body's, as for every writer.
                        let hash = event_log::content_hash(&item.content);
                        let title = item.title.as_deref().map(str::to_string).unwrap_or_else(|| request.source.clone());
                        let ev = NewEvent {
                            ts,
                            source: EVENT_SOURCE,
                            kind: &item.kind,
                            // E2 (V92) gives the row an `actor` column; until then the writer is
                            // the row's app, read back as `agent:<actor>`.
                            app_bundle_id: Some(actor),
                            window_title: Some(&title),
                            content: &item.content,
                            content_hash: &hash,
                            dwell_ms: 0,
                            display_id: None,
                            window_bounds: None,
                        };
                        let (id, touched) = event_log::insert_or_touch_with_thread(conn, &ev, thread_key)?;
                        self.reconcile_hot_locked(conn, id);
                        outcome.event_ids.push(id);
                        if touched {
                            outcome.deduplicated += 1;
                            continue;
                        }
                        outcome.inserted += 1;
                        // The first-stage local-rule extraction (low-confidence candidates linked
                        // to this event), as for a synced mail: the hourly review confirms them.
                        let candidates = shogun_memory::extract::extract_untrusted(&item.content);
                        if !candidates.is_empty() {
                            let _ = shogun_memory::extract::persist_candidates_deduped(conn, id, &candidates, ts, now);
                        }
                    }
                    outcome.thread_key = thread_key
                        .and_then(|id| shogun_memory::thread::thread_key(EVENT_SOURCE, Some(id), None, None));
                    write_receipt(conn, &key, &outcome.event_ids)?;
                    Ok(outcome)
                })();
                if own_transaction {
                    match &outcome {
                        Ok(_) => conn.execute_batch("COMMIT")?,
                        Err(_) => {
                            let _ = conn.execute_batch("ROLLBACK");
                        }
                    }
                }
                outcome
            })
            .map_err(|_| "ingest_unavailable".to_string())
        })
        .inspect(|outcome| {
            // New rows may have seeded low-confidence candidates the panel lists (issue #357).
            if outcome.inserted > 0 {
                super::maintenance::notify_state_changed();
            }
        })
    }

    /// `memory.recall`: the context-pack assembly within `budget_tokens`. `include_scratch` is
    /// accepted; the scratch namespace has nothing to add until E2-T5 lands.
    pub fn agent_recall(&self, request: &RecallRequest) -> Result<RecallOutcome, String> {
        let pack = self.assemble_context(&request.query, RECALL_HITS, RECALL_EXCERPT_CHARS);
        let estimator = HeuristicEstimator::default();
        let budget = request.budget_tokens;
        let mut outcome = RecallOutcome::default();
        let mut used = 0usize;
        let mut take = |text: &str| -> bool {
            let cost = estimator.count(text).max(1);
            if used + cost > budget {
                return false;
            }
            used += cost;
            true
        };
        // High facts first, then the hedged ones, then evidence: what an agent should know
        // before what it could verify. Anything left out marks the answer truncated.
        let (possibly, facts): (Vec<String>, Vec<String>) = pack
            .facts
            .into_iter()
            .partition(|fact| fact.starts_with(shogun_fusion::confidence::POSSIBLY_PREFIX));
        for fact in facts {
            if take(&fact) {
                outcome.facts.push(fact);
            } else {
                outcome.truncated = true;
            }
        }
        for fact in possibly {
            if take(&fact) {
                outcome.possibly.push(fact);
            } else {
                outcome.truncated = true;
            }
        }
        for evidence in pack.evidence {
            // A withheld connection's recall has no meeting or screen-text evidence (2026-10-20).
            if !shogun_mcp::source_policy::current_allows_source(&evidence.source) {
                continue;
            }
            let line = format!(
                "{} {} {}",
                evidence.source,
                evidence.title.as_deref().unwrap_or_default(),
                evidence.excerpt
            );
            if take(&line) {
                outcome.evidence.push(RecallEvidence {
                    event_id: evidence.event_id,
                    ts_ms: evidence.ts,
                    source: evidence.source,
                    title: evidence.title,
                    excerpt: evidence.excerpt,
                });
            } else {
                outcome.truncated = true;
            }
        }
        outcome.used_tokens = used;
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use shogun_mcp::ingest_api::{ingest_request, recall_request, Namespace};

    use super::*;

    fn db() -> Db {
        Db::open_in_memory(Arc::new(|| 1_000_000)).expect("in-memory store")
    }

    fn request(key: &str, contents: &[&str]) -> IngestRequest {
        ingest_request(&json!({
            "events": contents.iter().map(|c| json!({ "content": c, "kind": "message", "title": "standup" })).collect::<Vec<_>>(),
            "source": "harness",
            "actor": "agent:test",
            "thread_key": "task-7",
            "idempotency_key": key,
        }))
        .expect("valid request")
    }

    #[test]
    fn ingest_appends_sanitized_rows_once_per_idempotency_key() {
        let db = db();
        let first = db
            .agent_ingest(&request("k1", &["Alice agreed to send the deck by Friday", "token sk-live-ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 leaked"]))
            .expect("ingest");
        assert_eq!(first.event_ids.len(), 2);
        assert_eq!((first.inserted, first.deduplicated, first.replayed), (2, 0, false));
        assert_eq!(first.thread_key.as_deref(), Some("agent:task-7"));

        // The same key replays the first receipt and appends nothing, whatever the body says.
        let replay = db.agent_ingest(&request("k1", &["something else entirely"])).expect("replay");
        assert!(replay.replayed);
        assert_eq!(replay.event_ids, first.event_ids);
        let rows: i64 = db
            .with_conn("count", |c| c.query_row("SELECT COUNT(*) FROM event_log WHERE source = 'agent'", [], |r| r.get(0)))
            .expect("count");
        assert_eq!(rows, 2);

        // A new key with the same text collapses onto the existing rows (exact dedup per thread).
        let again = db.agent_ingest(&request("k2", &["Alice agreed to send the deck by Friday"])).expect("dedup");
        assert_eq!((again.inserted, again.deduplicated), (0, 1));
        assert_eq!(again.event_ids, vec![first.event_ids[0]]);

        // The secret never reached the store: the append path masked it.
        let bodies: Vec<String> = db
            .with_conn("bodies", |c| {
                let mut stmt = c.prepare("SELECT content FROM event_log WHERE source = 'agent'")?;
                let rows = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>();
                rows
            })
            .expect("bodies");
        assert!(bodies.iter().all(|b| !b.contains("ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789")), "{bodies:?}");
        assert!(bodies.iter().any(|b| b.contains("Alice agreed")));
    }

    #[test]
    fn turns_are_one_event_each_with_the_role_as_title_and_a_scratch_namespace_is_accepted() {
        let db = db();
        let request = ingest_request(&json!({
            "turns": [{ "role": "user", "content": "please book the room" }, { "role": "assistant", "content": "booked for 3pm", "ts_ms": 500 }],
            "source": "harness", "actor": "agent:test", "thread_key": "chat-1", "idempotency_key": "t1", "namespace": "agent_scratch"
        }))
        .expect("valid");
        assert_eq!(request.namespace, Namespace::AgentScratch);
        let outcome = db.agent_ingest(&request).expect("ingest");
        assert_eq!(outcome.inserted, 2);
        let rows: Vec<(String, String, i64)> = db
            .with_conn("rows", |c| {
                let mut stmt = c.prepare("SELECT kind, window_title, ts FROM event_log WHERE source = 'agent' ORDER BY id")?;
                let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>();
                rows
            })
            .expect("rows");
        assert_eq!(rows[0], ("turn".into(), "user".into(), 1_000_000));
        assert_eq!(rows[1], ("turn".into(), "assistant".into(), 500));
    }

    #[test]
    fn recall_returns_within_the_budget_with_possibly_facts_apart() {
        let db = db();
        db.agent_ingest(&request("r1", &["We decided the launch is on March 3 and Bob owns the deck for the launch review."]))
            .expect("ingest");
        let wide = db.agent_recall(&recall_request(&json!({ "query": "launch deck", "budget_tokens": 4000 })).expect("req")).expect("recall");
        assert!(!wide.truncated, "{wide:?}");
        assert!(wide.used_tokens <= 4000);
        assert!(wide.facts.iter().all(|f| !f.starts_with("possibly:")));
        assert!(wide.possibly.iter().all(|f| f.starts_with("possibly:")));
        let narrow = db.agent_recall(&recall_request(&json!({ "query": "launch deck", "budget_tokens": 100 })).expect("req")).expect("recall");
        assert!(narrow.used_tokens <= 100, "{narrow:?}");
        if wide.evidence.len() + wide.facts.len() + wide.possibly.len() > narrow.evidence.len() + narrow.facts.len() + narrow.possibly.len() {
            assert!(narrow.truncated);
        }
    }
}
