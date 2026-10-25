//! Commit, list and delete imports (V52).
//!
//! Commit is the only write. It re-validates and re-previews the request inside the transaction —
//! there is no server-side "pending preview" to go stale or to leak — and stores only the selected
//! items, each pointing at one new `memory_imports` row. Provenance is that row, labelled
//! `imported:<platform>`; never an event, so an imported claim cannot pass for observed capture.

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use super::package::{parse_package, Section};
use super::preview::{preview, ProposedAction};
use super::{ImportError, ImportRequest, Selection, Via, FORMAT, PEOPLE_CONFIDENCE, PEOPLE_KEY_PREFIX};

/// What a commit stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommitOutcome {
    pub import_id: i64,
    pub provenance: &'static str,
    /// Items stored.
    pub stored: usize,
    /// Of those, how many are linked to an existing record (`merge_into`).
    pub linked: usize,
}

/// Persist the selected items of `request` under a new import record. Nothing is written unless
/// every selected id is valid; one transaction.
pub fn commit(
    conn: &mut Connection,
    request: &ImportRequest,
    via: Via,
    now: i64,
) -> Result<CommitOutcome, ImportError> {
    commit_as(conn, request, via, now, &crate::actor::current_write())
}

/// [`commit`] as `ctx` (E2-T4): the people rows an import creates record `ctx.actor` as their
/// writer and land in `ctx.namespace` (V93). [`commit`] itself writes as the call's current
/// context ([`crate::actor::current_write`]).
pub fn commit_as(
    conn: &mut Connection,
    request: &ImportRequest,
    via: Via,
    now: i64,
    ctx: &crate::actor::WriteContext,
) -> Result<CommitOutcome, ImportError> {
    let selection = request
        .selection
        .as_ref()
        .ok_or(ImportError::Invalid("invalid selection: select is required"))?;
    let package = parse_package(&request.package_json).map_err(ImportError::Package)?;
    let tx = conn.transaction()?;
    let proposed = preview(&tx, &package)?;

    let chosen: Vec<&super::preview::ProposedItem> = match selection {
        Selection::Default => proposed.items.iter().filter(|i| i.selected_by_default).collect(),
        Selection::Ids(ids) => {
            let mut chosen: Vec<&super::preview::ProposedItem> = Vec::new();
            for id in ids {
                let Some(item) = proposed.items.iter().find(|i| &i.item.id == id) else {
                    return Err(ImportError::Invalid(if proposed.rejected.iter().any(|r| &r.id == id) {
                        "invalid selection: item was rejected"
                    } else {
                        "invalid selection: unknown item"
                    }));
                };
                if matches!(item.action, ProposedAction::DuplicateSkip { .. }) {
                    return Err(ImportError::Invalid("invalid selection: item is a duplicate"));
                }
                if !chosen.iter().any(|c| c.item.id == item.item.id) {
                    chosen.push(item);
                }
            }
            chosen
        }
    };
    if chosen.is_empty() {
        return Err(ImportError::Invalid("invalid selection: nothing selected"));
    }

    tx.execute(
        "INSERT INTO memory_imports
            (origin, provenance, format, via, generated_at, coverage_note,
             package_bytes, package_xxh64, item_count, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            proposed.platform.as_str(),
            proposed.provenance,
            FORMAT,
            via.as_str(),
            proposed.generated_at,
            proposed.coverage_note,
            i64::try_from(proposed.package_bytes).unwrap_or(i64::MAX),
            proposed.package_xxh64,
            i64::try_from(chosen.len()).unwrap_or(i64::MAX),
            now,
        ],
    )?;
    let import_id = tx.last_insert_rowid();
    let mut linked = 0usize;
    for p in &chosen {
        let (linked_table, linked_id) = match &p.action {
            ProposedAction::MergeInto { table, id, .. } => {
                linked += 1;
                (Some(*table), Some(*id))
            }
            ProposedAction::New
                if request.people_to_state && p.item.section == Section::Entity && p.item.kind.as_deref() == Some("person") =>
            {
                (Some("people"), Some(insert_person(&tx, import_id, p, now, ctx)?))
            }
            _ => (None, None),
        };
        let aliases = (!p.item.aliases.is_empty())
            .then(|| serde_json::to_string(&p.item.aliases).unwrap_or_else(|_| "[]".into()));
        tx.execute(
            "INSERT INTO memory_import_items
                (import_id, package_item, section, kind, name, target, summary, aliases, status,
                 confidence, requested_confidence, evidence, match_key, linked_table, linked_id,
                 created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                import_id,
                p.item.id,
                p.item.section.as_str(),
                p.item.kind,
                p.item.name,
                p.item.target,
                p.item.summary,
                aliases,
                p.item.status,
                p.confidence,
                p.item.claimed.as_str(),
                p.item.evidence,
                p.item.match_key(),
                linked_table,
                linked_id,
                now,
            ],
        )?;
    }
    tx.commit()?;
    Ok(CommitOutcome { import_id, provenance: proposed.provenance, stored: chosen.len(), linked })
}

/// Create the pinned `people` row for a new imported person. Provenance is the import record (the
/// item row links to it); no event is invented. Pinned so the orphan sweep (no capture evidence)
/// and automatic passes leave it alone; the key prefix lets undo remove exactly these rows.
fn insert_person(
    tx: &Connection,
    import_id: i64,
    p: &super::preview::ProposedItem,
    now: i64,
    ctx: &crate::actor::WriteContext,
) -> Result<i64, rusqlite::Error> {
    let aliases = (!p.item.aliases.is_empty())
        .then(|| serde_json::to_string(&p.item.aliases).unwrap_or_else(|_| "[]".into()));
    tx.execute(
        "INSERT INTO people (display_name, aliases, relationship_summary, extraction_key, confidence,
                             base_confidence, user_pinned, created_at, updated_at, last_evidence_at,
                             actor_last, namespace)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5, 1, ?6, ?6, ?6, ?7, ?8)",
        params![
            p.item.name,
            aliases,
            p.item.summary,
            format!("{PEOPLE_KEY_PREFIX}{import_id}:{}", p.item.id),
            PEOPLE_CONFIDENCE,
            now,
            ctx.actor_sql(),
            ctx.namespace_sql(),
        ],
    )?;
    let id = tx.last_insert_rowid();
    let _ = crate::search_documents::project_source_after_write(
        tx,
        &crate::search_documents::SourceRef::Person(id),
        now,
    );
    Ok(id)
}

/// One import record, as the list shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ImportSummary {
    pub id: i64,
    pub origin: String,
    pub provenance: String,
    pub via: String,
    pub generated_at: Option<String>,
    pub coverage_note: Option<String>,
    pub item_count: i64,
    pub created_at: i64,
}

/// One stored imported claim.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StoredItem {
    pub id: i64,
    pub import_id: i64,
    /// Provenance label (`imported:<platform>`) — every read surface shows it.
    pub provenance: String,
    pub package_item: String,
    pub section: String,
    pub kind: Option<String>,
    pub name: String,
    pub target: Option<String>,
    pub summary: Option<String>,
    pub aliases: Vec<String>,
    pub status: String,
    pub confidence: f64,
    pub requested_confidence: String,
    pub evidence: Option<String>,
    pub linked_table: Option<String>,
    pub linked_id: Option<i64>,
    pub created_at: i64,
    /// Set by read surfaces from the shared confidence rule; every import is below the fact band.
    pub possibly: bool,
}

/// One import with its items.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ImportDetail {
    #[serde(flatten)]
    pub summary: ImportSummary,
    pub items: Vec<StoredItem>,
}

const SUMMARY_COLS: &str =
    "id, origin, provenance, via, generated_at, coverage_note, item_count, created_at";

fn summary_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ImportSummary> {
    Ok(ImportSummary {
        id: r.get(0)?,
        origin: r.get(1)?,
        provenance: r.get(2)?,
        via: r.get(3)?,
        generated_at: r.get(4)?,
        coverage_note: r.get(5)?,
        item_count: r.get(6)?,
        created_at: r.get(7)?,
    })
}

/// Every import, newest first.
pub fn list_imports(conn: &Connection) -> Result<Vec<ImportSummary>, rusqlite::Error> {
    let mut stmt = conn.prepare(&format!("SELECT {SUMMARY_COLS} FROM memory_imports ORDER BY id DESC"))?;
    let rows = stmt.query_map([], summary_row)?;
    rows.collect()
}

/// One import and its items (package order). `None` when it does not exist.
pub fn get_import(conn: &Connection, id: i64) -> Result<Option<ImportDetail>, rusqlite::Error> {
    let Some(summary) = conn
        .query_row(&format!("SELECT {SUMMARY_COLS} FROM memory_imports WHERE id = ?1"), [id], summary_row)
        .optional()?
    else {
        return Ok(None);
    };
    let mut stmt = conn.prepare(
        "SELECT i.id, i.import_id, m.provenance, i.package_item, i.section, i.kind, i.name, i.target,
                i.summary, i.aliases, i.status, i.confidence, i.requested_confidence, i.evidence,
                i.linked_table, i.linked_id, i.created_at
           FROM memory_import_items i JOIN memory_imports m ON m.id = i.import_id
          WHERE i.import_id = ?1 ORDER BY i.id",
    )?;
    let items = stmt
        .query_map([id], |r| {
            let aliases: Option<String> = r.get(9)?;
            Ok(StoredItem {
                id: r.get(0)?,
                import_id: r.get(1)?,
                provenance: r.get(2)?,
                package_item: r.get(3)?,
                section: r.get(4)?,
                kind: r.get(5)?,
                name: r.get(6)?,
                target: r.get(7)?,
                summary: r.get(8)?,
                aliases: aliases
                    .and_then(|a| serde_json::from_str(&a).ok())
                    .unwrap_or_default(),
                status: r.get(10)?,
                confidence: r.get(11)?,
                requested_confidence: r.get(12)?,
                evidence: r.get(13)?,
                linked_table: r.get(14)?,
                linked_id: r.get(15)?,
                created_at: r.get(16)?,
                possibly: true,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(ImportDetail { summary, items }))
}

/// Delete one import and everything it stored — the undo for a commit. Linked records are not
/// touched (they were never changed); people the import itself created are removed. `false` when it does not exist.
pub fn delete_import(conn: &mut Connection, id: i64) -> Result<bool, rusqlite::Error> {
    let tx = conn.transaction()?;
    // People this import created (not ones it merely linked), unless the user has edited them since.
    let created: Vec<i64> = tx
        .prepare("SELECT id FROM people WHERE extraction_key LIKE ?1 ESCAPE '\\' AND user_edited_at IS NULL")?
        .query_map([format!("{PEOPLE_KEY_PREFIX}{id}:%")], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for person in created {
        // A captured row merged into the imported one moved its evidence and links here (the
        // pinned import row survives every merge). Those are the user's own captures: keep the row
        // as an ordinary captured person instead of deleting them with the import.
        let captured: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM state_provenance WHERE state_table = 'people' AND state_id = ?1)
                 OR EXISTS(SELECT 1 FROM commitments WHERE counterparty_id = ?1)
                 OR EXISTS(SELECT 1 FROM open_loops WHERE counterparty_id = ?1)",
            [person],
            |r| r.get(0),
        )?;
        if captured {
            tx.execute("UPDATE people SET user_pinned = 0, extraction_key = NULL WHERE id = ?1", [person])?;
        } else {
            crate::state_edit::remove_state_row(&tx, crate::state::StateTable::People, person)?;
        }
    }
    tx.execute("DELETE FROM memory_import_items WHERE import_id = ?1", [id])?;
    let removed = tx.execute("DELETE FROM memory_imports WHERE id = ?1", [id])?;
    tx.commit()?;
    Ok(removed > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_import::package::Section;
    use crate::memory_import::parse_request;

    const PACKAGE: &str = r#"{"format":"shogun-memory-import/v1","generated_at":"2026-09-20T10:00:00Z",
        "source":{"platform":"claude","coverage_note":"export of 12 chats"},
        "profile":{"preferences":[{"value":"Short answers","confidence":"high","evidence":"asked twice","status":"active"}]},
        "entities":[
          {"name":"Atlas","type":"project","summary":"Mobile app","aliases":["Project Atlas"],"confidence":"medium","evidence":"e","status":"active"},
          {"name":"atlas","type":"project","confidence":"low","evidence":"e","status":"active"},
          {"name":"Leaky","type":"tool","summary":"password=hunter2hunter2","confidence":"low","evidence":"e","status":"active"}
        ],
        "open_loops":[{"summary":"Send the deck","status":"open","confidence":"medium","evidence":"e"}]}"#;

    fn request(select: &str) -> ImportRequest {
        parse_request(&format!(r#"{{"package":{PACKAGE}{select}}}"#)).unwrap()
    }

    fn count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn commit_stores_only_the_selection_with_import_provenance() {
        let mut conn = crate::open_in_memory().unwrap();
        let out = commit(&mut conn, &request(r#","select":["entities/0","open_loops/0"]"#), Via::Api, 42).unwrap();
        assert_eq!(out.stored, 2);
        assert_eq!(out.linked, 0);
        assert_eq!(out.provenance, "imported:claude");
        assert_eq!(count(&conn, "memory_imports"), 1);
        assert_eq!(count(&conn, "memory_import_items"), 2);
        // No capture event, no state row, no provenance to an event.
        for table in ["event_log", "people", "projects", "open_loops", "state_provenance", "knowledge_entities"] {
            assert_eq!(count(&conn, table), 0, "{table}");
        }
        let detail = get_import(&conn, out.import_id).unwrap().unwrap();
        assert_eq!(detail.summary.via, "api");
        assert_eq!(detail.summary.origin, "claude");
        assert_eq!(detail.summary.item_count, 2);
        let atlas = &detail.items[0];
        assert_eq!((atlas.package_item.as_str(), atlas.name.as_str()), ("entities/0", "Atlas"));
        assert_eq!(atlas.aliases, ["Project Atlas"]);
        assert_eq!(atlas.provenance, "imported:claude");
        assert_eq!(atlas.confidence, crate::memory_import::CONFIDENCE_MEDIUM);
        assert_eq!(Section::parse(&atlas.section), Some(Section::Entity));

        // Re-importing the same package now proposes those two as already imported.
        let again = parse_package(&request("").package_json).unwrap();
        let pv = preview(&conn, &again).unwrap();
        let atlas = pv.items.iter().find(|i| i.item.id == "entities/0").unwrap();
        assert_eq!(
            atlas.action,
            ProposedAction::DuplicateSkip { reason: "already_imported", of: format!("import:{}", out.import_id) }
        );
    }

    #[test]
    fn default_selection_skips_duplicates_and_rejected_items_and_caps_confidence() {
        let mut conn = crate::open_in_memory().unwrap();
        let out = commit(&mut conn, &request(r#","select":"default""#), Via::Ui, 1).unwrap();
        // preferences/0, entities/0, open_loops/0 — entities/1 is a duplicate, entities/2 rejected.
        assert_eq!(out.stored, 3);
        let max: f64 = conn.query_row("SELECT max(confidence) FROM memory_import_items", [], |r| r.get(0)).unwrap();
        assert_eq!(max, crate::memory_import::CONFIDENCE_MAX, "high claim stored at the cap");
        let stored: Vec<String> = {
            let mut s = conn.prepare("SELECT name || coalesce(summary,'') FROM memory_import_items").unwrap();
            s.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
        };
        assert!(stored.iter().all(|t| !t.contains("hunter2")));
    }

    #[test]
    fn a_bad_selection_writes_nothing() {
        let mut conn = crate::open_in_memory().unwrap();
        for (select, why) in [
            ("", "invalid selection: select is required"),
            (r#","select":[]"#, "invalid selection: nothing selected"),
            (r#","select":["entities/9"]"#, "invalid selection: unknown item"),
            (r#","select":["entities/2"]"#, "invalid selection: item was rejected"),
            (r#","select":["entities/0","entities/1"]"#, "invalid selection: item is a duplicate"),
        ] {
            let err = commit(&mut conn, &request(select), Via::Api, 1).unwrap_err();
            assert_eq!(err.to_string(), why);
        }
        let bad = parse_request(r#"{"package":{"format":"nope"},"select":"default"}"#).unwrap();
        assert!(matches!(commit(&mut conn, &bad, Via::Api, 1), Err(ImportError::Package(_))));
        assert_eq!(count(&conn, "memory_imports"), 0);
        assert_eq!(count(&conn, "memory_import_items"), 0);
    }

    #[test]
    fn merge_links_without_touching_the_observed_record_and_delete_undoes_the_import() {
        let mut conn = crate::open_in_memory().unwrap();
        conn.execute(
            "INSERT INTO knowledge_entities (kind, name, context, extraction_key, confidence, created_at, updated_at)
             VALUES ('tool', 'Leaky', 'ctx', 'k', 0.4, 1, 1)",
            [],
        )
        .unwrap();
        let pkg = r#"{"format":"shogun-memory-import/v1","source":{"platform":"computer_history"},
            "entities":[{"name":"leaky","type":"tool","confidence":"high","evidence":null,"status":"active"}]}"#;
        let req = parse_request(&format!(r#"{{"package":{pkg},"select":"default"}}"#)).unwrap();
        let out = commit(&mut conn, &req, Via::Ui, 5).unwrap();
        assert_eq!((out.stored, out.linked, out.provenance), (1, 1, "imported:computer_history"));
        let (table, conf): (String, f64) = conn
            .query_row("SELECT linked_table, confidence FROM memory_import_items", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!(table, "knowledge_entities");
        assert_eq!(conf, crate::memory_import::CONFIDENCE_MAX);
        let observed: f64 =
            conn.query_row("SELECT confidence FROM knowledge_entities", [], |r| r.get(0)).unwrap();
        assert_eq!(observed, 0.4, "the observed record keeps its own confidence");

        assert_eq!(list_imports(&conn).unwrap().len(), 1);
        assert!(delete_import(&mut conn, out.import_id).unwrap());
        assert!(!delete_import(&mut conn, out.import_id).unwrap());
        assert_eq!(count(&conn, "memory_import_items"), 0);
        assert_eq!(count(&conn, "knowledge_entities"), 1);
        assert!(get_import(&conn, out.import_id).unwrap().is_none());
    }

    const PEOPLE: &str = r#"{"format":"shogun-memory-import/v1","source":{"platform":"other"},
        "entities":[
          {"name":"Alex Morgan","type":"person","summary":"Design lead","aliases":["Alex M"],"confidence":"low","evidence":"e","status":"active"},
          {"name":"Atlas","type":"project","confidence":"low","evidence":"e","status":"active"}]}"#;

    #[test]
    fn people_to_state_creates_pinned_people_idempotently_and_undo_removes_them() {
        let mut conn = crate::open_in_memory().unwrap();
        let req = |flag: bool| {
            parse_request(&format!(r#"{{"package":{PEOPLE},"select":"default","people_to_state":{flag}}}"#)).unwrap()
        };
        let mut off = crate::open_in_memory().unwrap();
        commit(&mut off, &req(false), Via::Api, 1).unwrap();
        assert_eq!(count(&off, "people"), 0, "off by default");
        let first = commit(&mut conn, &req(true), Via::Api, 2).unwrap();
        assert_eq!(count(&conn, "people"), 1, "projects stay weak claims");
        let (conf, pinned, aliases): (f64, i64, String) = conn
            .query_row("SELECT confidence, user_pinned, aliases FROM people", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        assert_eq!((conf, pinned, aliases.as_str()), (0.8, 1, r#"["Alex M"]"#));
        let linked: i64 = conn
            .query_row("SELECT count(*) FROM memory_import_items WHERE linked_table='people' AND import_id=?1", [first.import_id], |r| r.get(0))
            .unwrap();
        assert_eq!(linked, 1);
        // Re-import: every item is already imported, so nothing is selected and nothing duplicates.
        assert!(commit(&mut conn, &req(true), Via::Api, 3).is_err());
        assert_eq!(count(&conn, "people"), 1);
        assert!(delete_import(&mut conn, first.import_id).unwrap());
        assert_eq!(count(&conn, "people"), 0);
    }

    #[test]
    fn undo_keeps_an_imported_person_that_captured_evidence_was_merged_into() {
        let mut conn = crate::open_in_memory().unwrap();
        let req = parse_request(&format!(r#"{{"package":{PEOPLE},"select":"default","people_to_state":true}}"#)).unwrap();
        let out = commit(&mut conn, &req, Via::Api, 2).unwrap();
        let imported: i64 = conn.query_row("SELECT id FROM people", [], |r| r.get(0)).unwrap();
        // A captured "alex morgan" row merges into the pinned imported one and moves its evidence there.
        let ev = crate::event_log::insert(&conn, &crate::event_log::NewEvent {
            ts: 3, source: "capture", kind: "text", app_bundle_id: Some("app"), window_title: None,
            content: "alex morgan replied", content_hash: "h", dwell_ms: 0, display_id: None, window_bounds: None,
        }).unwrap();
        let captured = crate::state::insert_person(
            &mut conn,
            &crate::state::NewPerson { display_name: "alex morgan", confidence: 0.6, now: 3, ..Default::default() },
            &[crate::state::Provenance::new(ev)],
        ).unwrap();
        assert_eq!(crate::entity_dedupe::merge(&mut conn, crate::state::StateTable::People, &[imported, captured], None, 4).unwrap(), 1);
        assert!(delete_import(&mut conn, out.import_id).unwrap());
        let (pinned, key, links): (i64, Option<String>, i64) = conn
            .query_row("SELECT user_pinned, extraction_key, (SELECT count(*) FROM state_provenance WHERE state_table='people' AND state_id=people.id) FROM people", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        assert_eq!((pinned, key, links), (0, None, 1), "the captured evidence survives the undo");
    }
}
