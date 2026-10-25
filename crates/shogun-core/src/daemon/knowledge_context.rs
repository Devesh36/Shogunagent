//! Query-scoped extracted knowledge, grounded in a still-retained literal source quote.
use super::*;
use shogun_memory::search::{plan_memory_query, MemoryQueryFocus};
use std::collections::HashSet;

const KNOWLEDGE_CONTEXT_SQL: &str =
    "WITH terms AS (SELECT value FROM json_each(?1)), candidates AS (
       SELECT k.*, (SELECT count(*) FROM terms WHERE value=k.kind
         OR (k.kind='education' AND value IN ('school','college','university','study','studies'))
         OR (k.kind='employment' AND value IN ('work','job','career','role'))
         OR (k.kind='activity' AND value IN ('task','tasks','activity'))) AS kind_hits,
         (SELECT count(*) FROM terms WHERE instr(lower(k.name),value)>0) AS name_hits,
         (SELECT count(*) FROM terms WHERE instr(lower(k.context),value)>0) AS context_hits
       FROM knowledge_entities_fts
       JOIN knowledge_entities k ON k.id=knowledge_entities_fts.rowid
       WHERE knowledge_entities_fts MATCH ?4 AND k.confidence >= 0.5
         AND length(CAST(k.name AS BLOB)) <= 160 AND length(CAST(k.context AS BLOB)) <= 240
     ), matched AS (
       SELECT * FROM candidates WHERE name_hits > 0 OR context_hits >= 2
       ORDER BY kind_hits DESC, name_hits DESC, context_hits DESC, updated_at DESC, id LIMIT 32
     ), grounded AS (
       SELECT k.kind,k.name,k.context,k.kind_hits,k.name_hits,k.context_hits,e.id,e.ts,substr(e.source,1,64) AS source,p.quote,
         row_number() OVER (PARTITION BY k.id ORDER BY e.ts DESC,e.id DESC) AS position
       FROM matched k JOIN knowledge_provenance p ON p.entity_id=k.id JOIN event_log e ON e.id=p.event_id
       WHERE (?2 IS NULL OR e.ts >= ?2) AND (?3 IS NULL OR e.ts < ?3)
         AND p.quote IS NOT NULL AND length(CAST(p.quote AS BLOB)) BETWEEN 1 AND 800
         AND instr(e.content,p.quote)>0 AND instr(p.quote,k.name)>0 AND instr(p.quote,k.context)>0
     ) SELECT kind,name,context,id,ts,source,quote FROM grounded WHERE position=1
       ORDER BY kind_hits DESC, name_hits DESC, context_hits DESC,ts DESC,id DESC LIMIT 32";

#[cfg(test)]
const LEGACY_KNOWLEDGE_CONTEXT_SQL: &str =
    "WITH terms AS (SELECT value FROM json_each(?1)), candidates AS (
       SELECT k.*, (SELECT count(*) FROM terms WHERE value=k.kind
         OR (k.kind='education' AND value IN ('school','college','university','study','studies'))
         OR (k.kind='employment' AND value IN ('work','job','career','role'))
         OR (k.kind='activity' AND value IN ('task','tasks','activity'))) AS kind_hits,
         (SELECT count(*) FROM terms WHERE instr(lower(k.name),value)>0) AS name_hits,
         (SELECT count(*) FROM terms WHERE instr(lower(k.context),value)>0) AS context_hits
       FROM knowledge_entities k WHERE ?4 IS NOT NULL AND confidence >= 0.5
         AND length(CAST(name AS BLOB)) <= 160 AND length(CAST(context AS BLOB)) <= 240
     ), matched AS (
       SELECT * FROM candidates WHERE name_hits > 0 OR context_hits >= 2
       ORDER BY kind_hits DESC, name_hits DESC, context_hits DESC, updated_at DESC, id LIMIT 32
     ), grounded AS (
       SELECT k.kind,k.name,k.context,k.kind_hits,k.name_hits,k.context_hits,e.id,e.ts,substr(e.source,1,64) AS source,p.quote,
         row_number() OVER (PARTITION BY k.id ORDER BY e.ts DESC,e.id DESC) AS position
       FROM matched k JOIN knowledge_provenance p ON p.entity_id=k.id JOIN event_log e ON e.id=p.event_id
       WHERE (?2 IS NULL OR e.ts >= ?2) AND (?3 IS NULL OR e.ts < ?3)
         AND p.quote IS NOT NULL AND length(CAST(p.quote AS BLOB)) BETWEEN 1 AND 800
         AND instr(e.content,p.quote)>0 AND instr(p.quote,k.name)>0 AND instr(p.quote,k.context)>0
     ) SELECT kind,name,context,id,ts,source,quote FROM grounded WHERE position=1
       ORDER BY kind_hits DESC, name_hits DESC, context_hits DESC,ts DESC,id DESC LIMIT 32";

impl Db {
    /// Return evidence, never owner-profile assertions. Generic context-only matches require two
    /// topical terms; a named subject match takes priority. Unknown/old quotes fail closed.
    pub(super) fn knowledge_context_evidence(
        &self,
        query: &str,
        max_hits: usize,
        excerpt_chars: usize,
    ) -> Vec<Evidence> {
        if max_hits == 0 || excerpt_chars == 0 {
            return Vec::new();
        }
        let now = self.now_ms();
        let plan = plan_memory_query(query, now, local_day_bounds(now));
        if plan.topical_terms.is_empty()
            || matches!(
                plan.focus,
                MemoryQueryFocus::Meeting
                    | MemoryQueryFocus::Brief
                    | MemoryQueryFocus::Period
                    | MemoryQueryFocus::Thread
            )
        {
            return Vec::new();
        }
        let mut query_terms = plan.topical_terms;
        query_terms.sort();
        query_terms.dedup();
        let Some(fts_query) = shogun_memory::search::fts_query(query) else {
            return Vec::new();
        };
        self.query_knowledge_context_evidence(
            &query_terms,
            plan.from_ms,
            plan.to_ms,
            &fts_query,
            (max_hits, excerpt_chars),
            KNOWLEDGE_CONTEXT_SQL,
        )
    }

    fn query_knowledge_context_evidence(
        &self,
        query_terms: &[String],
        from_ms: Option<i64>,
        to_ms: Option<i64>,
        fts_query: &str,
        limits: (usize, usize),
        sql: &str,
    ) -> Vec<Evidence> {
        let (max_hits, excerpt_chars) = limits;
        let Ok(terms) = serde_json::to_string(&query_terms) else {
            return Vec::new();
        };
        self.with_conn("knowledge.context", |conn| {
            let mut stmt = conn.prepare(sql)?;
            let rows = stmt.query_map(rusqlite::params![terms, from_ms, to_ms, fts_query], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?, r.get::<_, i64>(4)?, r.get::<_, String>(5)?, r.get::<_, String>(6)?))
            })?;
            let mut evidence = Vec::new();
            let mut seen = HashSet::new();
            for row in rows {
                let (kind, name, context, event_id, ts, source, quote) = row?;
                let matches = |text: &str| shogun_memory::search::lexical_terms(text)
                    .into_iter().map(|term| term.to_lowercase())
                    .filter(|term| query_terms.contains(term)).collect::<HashSet<_>>().len();
                if matches(&name) == 0 && matches(&context) < 2 { continue; }
                // Apply the same sanitization on read: changed sources or legacy/corrupt rows
                // must not turn stored text into generated instructions or secret material.
                if [&name, &context, &quote].iter().any(|text| {
                    !shogun_memory::sanitize::persist_generated(text)
                        .is_some_and(|clean| clean.text == text.as_str())
                }) || !seen.insert(event_id) {
                    continue;
                }
                evidence.push(Evidence {
                    event_id, ts, source,
                    // Every extracted entity is stored at medium confidence (V37 caps it at
                    // 0.6), so the kind/name pairing is a classification, not a fact: it gets
                    // the same "possibly" hedge summaries in that band carry. The quote is
                    // verbatim source and needs no hedge.
                    title: Some(format!("Possibly {kind} about {name} (medium-confidence extraction, unverified); timestamp is capture time, not event date; not proof of owner identity or current status")),
                    excerpt: quote.chars().take(excerpt_chars.min(800)).collect(),
                    frame_id: None,
                });
                if evidence.len() == max_hits.min(4) { break; }
            }
            Ok::<_, rusqlite::Error>(evidence)
        }).unwrap_or_default()
    }

    /// Specific matched knowledge gets a small share of the existing evidence budget. A miss
    /// leaves the ordering untouched; no additional provider request or context route exists.
    pub(super) fn prioritize_knowledge_evidence(
        &self,
        query: &str,
        max_hits: usize,
        excerpt_chars: usize,
        evidence: &mut Vec<Evidence>,
    ) -> bool {
        let mut relevant = self.knowledge_context_evidence(query, max_hits, excerpt_chars);
        if relevant.is_empty() {
            return false;
        }
        let mut seen: HashSet<i64> = relevant.iter().map(|item| item.event_id).collect();
        relevant.extend(
            std::mem::take(evidence)
                .into_iter()
                .filter(|item| seen.insert(item.event_id)),
        );
        relevant.truncate(max_hits);
        *evidence = relevant;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_knowledge_context_evidence(
        db: &Db,
        query: &str,
        max_hits: usize,
        excerpt_chars: usize,
    ) -> Vec<Evidence> {
        if max_hits == 0 || excerpt_chars == 0 {
            return Vec::new();
        }
        let now = db.now_ms();
        let plan = plan_memory_query(query, now, local_day_bounds(now));
        if plan.topical_terms.is_empty()
            || matches!(
                plan.focus,
                MemoryQueryFocus::Meeting
                    | MemoryQueryFocus::Brief
                    | MemoryQueryFocus::Period
                    | MemoryQueryFocus::Thread
            )
        {
            return Vec::new();
        }
        let mut query_terms = plan.topical_terms;
        query_terms.sort();
        query_terms.dedup();
        let Some(fts_query) = shogun_memory::search::fts_query(query) else {
            return Vec::new();
        };
        db.query_knowledge_context_evidence(
            &query_terms,
            plan.from_ms,
            plan.to_ms,
            &fts_query,
            (max_hits, excerpt_chars),
            LEGACY_KNOWLEDGE_CONTEXT_SQL,
        )
    }

    fn record(
        db: &Db,
        name: &str,
        context: &str,
        ts: i64,
    ) -> Result<i64, Box<dyn std::error::Error>> {
        record_kind(
            db,
            name,
            context,
            ts,
            shogun_memory::knowledge::Kind::Employment,
        )
    }

    fn record_kind(
        db: &Db,
        name: &str,
        context: &str,
        ts: i64,
        kind: shogun_memory::knowledge::Kind,
    ) -> Result<i64, Box<dyn std::error::Error>> {
        let quote = format!("{name} {context}");
        let (id, _) = db
            .capture(&NewEvent {
                ts,
                source: "capture",
                kind: "text",
                app_bundle_id: Some("test"),
                window_title: None,
                content: &quote,
                content_hash: &quote,
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            })
            .ok_or("capture fixture failed")?;
        db.with_conn_mut("test.knowledge", |conn| {
            shogun_memory::knowledge::persist(
                conn,
                id,
                &shogun_memory::knowledge::Candidate {
                    kind,
                    name: name.into(),
                    context: context.into(),
                    quote, ..Default::default() },
                ts,
            )
        })
        .map_err(|_| "knowledge fixture failed")?;
        Ok(id)
    }

    #[test]
    fn indexed_lookup_matches_legacy_scan_for_existing_semantics(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(Arc::new(|| 1_000))?;
        record(&db, "Alice Ng", "works at Acme on payments.", 100)?;
        record(&db, "Bob", "uses Zellij for distributed tracing.", 200)?;
        record_kind(
            &db,
            "東京大学",
            "量子物理を研究している。",
            300,
            shogun_memory::knowledge::Kind::Education,
        )?;
        for query in [
            "Alice Ng",
            "Ali",
            "Al",
            "works payments",
            "Zellij tracing",
            "東京大学",
            "量子物理",
            "education 東京大学",
            "works works",
            "unrelated",
        ] {
            assert_eq!(
                db.knowledge_context_evidence(query, 4, 800),
                legacy_knowledge_context_evidence(&db, query, 4, 800),
                "indexed result changed legacy semantics for {query:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn knowledge_lookup_plan_uses_fts_index() -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(Arc::new(|| 1_000))?;
        let plan = db
            .with_conn("test.plan", |conn| {
                let mut stmt =
                    conn.prepare(&format!("EXPLAIN QUERY PLAN {KNOWLEDGE_CONTEXT_SQL}"))?;
                let rows = stmt.query_map(
                    rusqlite::params![
                        r#"["alice","acme"]"#,
                        None::<i64>,
                        None::<i64>,
                        r#""alice" OR "acme""#
                    ],
                    |row| row.get::<_, String>(3),
                )?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .map_err(|_| "query plan failed")?;
        assert!(
            plan.iter()
                .any(|step| step.contains("knowledge_entities_fts")
                    && step.contains("VIRTUAL TABLE INDEX")),
            "query plan did not use knowledge FTS: {plan:?}"
        );
        assert!(
            plan.iter()
                .any(|step| step.contains("SEARCH k USING INTEGER PRIMARY KEY")),
            "FTS candidates were not joined by entity rowid: {plan:?}"
        );
        Ok(())
    }

    #[test]
    fn knowledge_index_tracks_update_and_source_deletion_atomically(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(Arc::new(|| 1_000))?;
        let quote = "Alice works at Acme. sourceonlytoken";
        let (event_id, _) = db
            .capture(&NewEvent {
                ts: 100,
                source: "capture",
                kind: "text",
                app_bundle_id: Some("test"),
                window_title: None,
                content: quote,
                content_hash: quote,
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            })
            .ok_or("capture fixture failed")?;
        db.with_conn_mut("test.knowledge", |conn| {
            shogun_memory::knowledge::persist(
                conn,
                event_id,
                &shogun_memory::knowledge::Candidate {
                    kind: shogun_memory::knowledge::Kind::Employment,
                    name: "Alice".into(),
                    context: "works at Acme.".into(),
                    quote: quote.into(), ..Default::default() },
                100,
            )
        })
        .map_err(|_| "knowledge insert failed")?;
        db.with_conn("test.index", |conn| {
            assert_eq!(
                conn.query_row(
                    "SELECT count(*) FROM knowledge_entities_fts WHERE knowledge_entities_fts MATCH 'sourceonlytoken'",
                    [],
                    |row| row.get::<_, i64>(0),
                )?,
                0,
                "source-only quote text must never enter the index"
            );
            conn.execute(
                "UPDATE knowledge_entities SET name='Bob',context='works at Bravo.'",
                [],
            )?;
            assert_eq!(
                conn.query_row(
                    "SELECT count(*) FROM knowledge_entities_fts WHERE knowledge_entities_fts MATCH 'alice'",
                    [],
                    |row| row.get::<_, i64>(0),
                )?,
                0
            );
            assert_eq!(
                conn.query_row(
                    "SELECT count(*) FROM knowledge_entities_fts WHERE knowledge_entities_fts MATCH 'bravo'",
                    [],
                    |row| row.get::<_, i64>(0),
                )?,
                1
            );
            conn.execute("DELETE FROM event_log WHERE id=?1", [event_id])?;
            assert_eq!(
                conn.query_row(
                    "SELECT count(*) FROM knowledge_entities_fts WHERE knowledge_entities_fts MATCH 'bravo'",
                    [],
                    |row| row.get::<_, i64>(0),
                )?,
                0,
                "deleted source must leave no searchable entity text"
            );
            Ok::<_, rusqlite::Error>(())
        })
        .map_err(|_| "knowledge index check failed")?;
        Ok(())
    }

    #[test]
    fn requested_relation_kind_ranks_before_newer_same_name_evidence(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(Arc::new(|| 1_000))?;
        let education = record_kind(
            &db,
            "Alice Ng",
            "studies physics at North College.",
            100,
            shogun_memory::knowledge::Kind::Education,
        )?;
        let employment = record(&db, "Alice Ng", "works at Acme.", 200)?;
        for query in ["my education Alice Ng", "Alice Ng school"] {
            let evidence = db.knowledge_context_evidence(query, 1, 800);
            assert_eq!(evidence.len(), 1);
            assert_eq!(evidence[0].event_id, education);
        }
        assert_eq!(
            db.knowledge_context_evidence("Alice Ng work", 1, 800)[0].event_id,
            employment
        );
        assert!(
            db.knowledge_context_evidence("my education", 4, 800)
                .is_empty(),
            "kind alone must never dump unrelated people"
        );
        Ok(())
    }

    #[test]
    fn named_knowledge_is_bounded_attributed_and_reaches_both_context_paths(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(Arc::new(|| 3 * 86_400_000))?;
        let alice = record(&db, "Alice", "worked at Acme in 2019.", 100)?;
        record(&db, "Bob", "works at OtherCo.", 200)?;
        let pack = db.assemble_context("Alice", 1, 80);
        assert_eq!(pack.evidence.len(), 1);
        assert_eq!(pack.evidence[0].event_id, alice);
        assert_eq!(pack.evidence[0].source, "capture");
        assert_eq!(pack.evidence[0].ts, 100);
        assert!(pack.evidence[0]
            .title
            .as_deref()
            .is_some_and(|title| title.contains("not proof of owner identity or current status")));
        assert!(pack.evidence[0].excerpt.contains("2019"));
        assert!(!pack.evidence[0].excerpt.contains("OtherCo"));
        let (compressed, _, _) = db.assemble_context_compressed_within(
            "Alice",
            1,
            80,
            &[],
            &shogun_fusion::compress::CompressionConfig {
                enabled: true,
                budget_tokens: 1_000,
                ..Default::default()
            },
            crate::daemon::CompressBudget::Unbounded,
        );
        assert!(compressed.evidence.iter().any(|item| item.event_id == alice
            && item
                .title
                .as_deref()
                .is_some_and(|title| title.starts_with("Possibly employment about"))));
        assert!(db
            .knowledge_context_evidence("Alice today", 4, 80)
            .is_empty());
        assert!(db
            .knowledge_context_evidence("a meeting with Alice", 4, 80)
            .is_empty());
        for n in 0..6 {
            record(&db, &format!("Alice{n}"), "works at Acme.", 300 + n)?;
        }
        let broad = db.knowledge_context_evidence("works Acme", 20, 12);
        assert_eq!(broad.len(), 4);
        assert!(broad.iter().all(|item| item.excerpt.chars().count() <= 12));
        Ok(())
    }

    #[test]
    fn knowledge_misses_leave_retrieval_unchanged_and_stale_quotes_are_withheld(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(Arc::new(|| 1_000))?;
        let id = record(&db, "Alice", "works at Acme.", 100)?;
        let original = vec![Evidence {
            event_id: 99,
            ts: 1,
            source: "user".into(),
            title: None,
            excerpt: "original evidence".into(),
            frame_id: None,
        }];
        for query in ["", "the", "unrelated", "Ali", "works works"] {
            let mut evidence = original.clone();
            assert!(!db.prioritize_knowledge_evidence(query, 4, 80, &mut evidence));
            assert_eq!(evidence, original);
        }
        db.with_conn("test.quote", |conn| {
            conn.execute("UPDATE knowledge_provenance SET quote = NULL", [])
        })
        .map_err(|_| "quote update failed")?;
        assert!(db.knowledge_context_evidence("Alice", 4, 80).is_empty());
        db.with_conn("test.quote", |conn| {
            conn.execute(
                "UPDATE knowledge_provenance SET quote = 'Alice works at Acme.'",
                [],
            )
        })
        .map_err(|_| "quote update failed")?;
        db.with_conn("test.source", |conn| {
            conn.execute(
                "UPDATE event_log SET content = 'Source was corrected' WHERE id = ?1",
                [id],
            )
        })
        .map_err(|_| "source update failed")?;
        assert!(db.knowledge_context_evidence("Alice", 4, 80).is_empty());
        Ok(())
    }

    #[test]
    #[ignore = "release-mode latency measurement"]
    fn benchmark_indexed_knowledge_lookup_at_growing_sizes(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(Arc::new(|| 100_000))?;
        let mut previous_size = 0;
        for size in [1_000, 10_000, 50_000] {
            db.with_conn_mut("test.benchmark.seed", |conn| {
                let tx = conn.transaction()?;
                for row in previous_size..size {
                    let name = if row + 1 == size {
                        format!("NeedleProject{size}")
                    } else {
                        format!("ArchivePerson{row}")
                    };
                    let context = format!("works on archived task number {row}.");
                    let quote = format!("{name} {context}");
                    tx.execute(
                        "INSERT INTO event_log(ts,source,kind,content,content_hash,last_seen_at)
                         VALUES(?1,'capture','text',?2,?3,?1)",
                        rusqlite::params![row as i64 + 1, quote, format!("bench-{row}")],
                    )?;
                    let event_id = tx.last_insert_rowid();
                    tx.execute(
                        "INSERT INTO knowledge_entities(kind,name,context,extraction_key,confidence,created_at,updated_at)
                         VALUES('activity',?1,?2,?3,0.6,?4,?4)",
                        rusqlite::params![name, context, format!("bench-key-{row}"), row as i64 + 1],
                    )?;
                    let entity_id = tx.last_insert_rowid();
                    tx.execute(
                        "INSERT INTO knowledge_provenance(entity_id,event_id,quote) VALUES(?1,?2,?3)",
                        rusqlite::params![entity_id, event_id, quote],
                    )?;
                }
                tx.commit()
            })
            .map_err(|_| "benchmark seed failed")?;
            previous_size = size;
            let query = format!("NeedleProject{size}");
            for _ in 0..20 {
                assert_eq!(db.knowledge_context_evidence(&query, 4, 800).len(), 1);
            }
            let mut samples = Vec::with_capacity(200);
            for _ in 0..200 {
                let started = std::time::Instant::now();
                assert_eq!(db.knowledge_context_evidence(&query, 4, 800).len(), 1);
                samples.push(started.elapsed().as_nanos());
            }
            samples.sort_unstable();
            let p50 = samples[samples.len() / 2];
            let p95 = samples[samples.len() * 95 / 100];
            println!(
                "knowledge context indexed lookup rows={size}: p50={:.3}ms p95={:.3}ms",
                p50 as f64 / 1_000_000.0,
                p95 as f64 / 1_000_000.0
            );
            assert!(
                p95 < std::time::Duration::from_millis(500).as_nanos(),
                "local search SLO exceeded at {size} rows: p50={p50}ns p95={p95}ns"
            );
        }
        Ok(())
    }
}
