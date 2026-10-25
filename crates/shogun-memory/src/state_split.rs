//! The user splits a wrong entity merge — 名寄せ correction (FR-ST-10, issue #290).
//!
//! Resolution fuses evidence into one `people` / `projects` row by an exact key: the knowledge
//! extractor's `kind:name:context` key ([`crate::knowledge::persist`]), an exact channel identity
//! ([`crate::identity::observe`]), or a name / alias when a Recap owner is resolved
//! ([`crate::recap_track::resolve_owner`]). When that fuses two real people (two "Alex"es in the
//! same company), the user picks the evidence that belongs to someone else and gives it a name:
//!
//! - a **new row** is created carrying the chosen provenance events (moved, not copied) and, for
//!   people, the chosen aliases / addresses / handles (moved out of the original's lists);
//! - commitments and open loops pointing at the original whose *every* provenance event moved are
//!   re-pointed at the new row — they were grounded only in the other entity's evidence. A row
//!   with mixed evidence keeps its link (it cannot be decided from provenance alone);
//! - **both** rows become the user's statement: confidence 1.0 and `user_pinned` (FR-ST-21), so no
//!   automatic pass re-scores or overwrites them;
//! - the decision is recorded (`state_merge_blocks` + `state_split_events`, V50, ids only) so the
//!   resolvers do not fuse them again: an extractor about to link a moved event back to the
//!   original links it where the user put it ([`split_redirect`]), and a name that matches both
//!   rows is treated as ambiguous rather than silently picking one ([`is_merge_blocked`]).
//!
//! What is not re-pointed: `projects.participants`, `threads.participants` / `threads.project_id`
//! and meeting participants are id lists derived by the Dream Cycle, not grounded per event; the
//! next derivation reads the corrected rows.

use std::collections::BTreeSet;

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::state::StateTable;
use crate::state_edit::{clean_text, source_ref, EditError, MAX_NAME_CHARS};

/// Most events one split may move — a bound on the request, not on memory.
pub const MAX_SPLIT_EVENTS: usize = 5_000;
/// Most events the evidence listing returns (newest first); the rest are counted.
pub const MAX_EVIDENCE_EVENTS: usize = 500;
/// Chain bound when following split after split of the same event.
const MAX_REDIRECT_HOPS: usize = 16;

/// A parsed `state.split` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitRequest {
    pub table: StateTable,
    pub id: i64,
    /// Name of the new entity.
    pub name: String,
    /// Provenance events to move, deduplicated, ascending.
    pub event_ids: Vec<i64>,
    /// People only: stored alias / email / handle values to move (compared case-insensitively).
    pub aliases: Vec<String>,
}

/// What a split did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SplitOutcome {
    pub new_id: i64,
    pub moved_events: usize,
    pub moved_aliases: usize,
    pub moved_references: usize,
}

/// Parse the one request shape every face sends:
/// `{"table": "people"|"projects", "id": N, "name": "...", "event_ids": [..], "aliases": [..]}`.
pub fn parse_request(body: &str) -> Result<SplitRequest, EditError> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| EditError::Invalid("invalid split request"))?;
    let obj = value.as_object().ok_or(EditError::Invalid("invalid split request"))?;
    for key in obj.keys() {
        if !matches!(key.as_str(), "table" | "id" | "name" | "event_ids" | "aliases") {
            return Err(EditError::Invalid("invalid field"));
        }
    }
    let table = obj
        .get("table")
        .and_then(|v| v.as_str())
        .and_then(StateTable::parse)
        .ok_or(EditError::Invalid("invalid table"))?;
    if !matches!(table, StateTable::People | StateTable::Projects) {
        return Err(EditError::Invalid("invalid table for split"));
    }
    let id = obj
        .get("id")
        .and_then(|v| v.as_i64())
        .filter(|id| *id > 0)
        .ok_or(EditError::Invalid("invalid id"))?;
    let name = match obj.get("name") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(_) => return Err(EditError::Invalid("invalid field type")),
        None => return Err(EditError::Invalid("invalid split: name required")),
    };
    let raw_events = match obj.get("event_ids") {
        Some(serde_json::Value::Array(a)) => a,
        Some(_) => return Err(EditError::Invalid("invalid field type")),
        None => return Err(EditError::Invalid("invalid split: no events chosen")),
    };
    let mut events = BTreeSet::new();
    for v in raw_events {
        let id = v.as_i64().filter(|id| *id > 0).ok_or(EditError::Invalid("invalid event id"))?;
        events.insert(id);
    }
    if events.is_empty() {
        return Err(EditError::Invalid("invalid split: no events chosen"));
    }
    if events.len() > MAX_SPLIT_EVENTS {
        return Err(EditError::Invalid("invalid split: too many events"));
    }
    let mut aliases: Vec<String> = Vec::new();
    match obj.get("aliases") {
        None | Some(serde_json::Value::Null) => {}
        Some(serde_json::Value::Array(a)) => {
            for v in a {
                let s = v.as_str().ok_or(EditError::Invalid("invalid field type"))?.trim();
                if s.is_empty() {
                    return Err(EditError::Invalid("invalid alias"));
                }
                if !aliases.iter().any(|a| a.eq_ignore_ascii_case(s)) {
                    aliases.push(s.to_string());
                }
            }
        }
        Some(_) => return Err(EditError::Invalid("invalid field type")),
    }
    if !aliases.is_empty() && table != StateTable::People {
        return Err(EditError::Invalid("invalid field for table"));
    }
    Ok(SplitRequest { table, id, name, event_ids: events.into_iter().collect(), aliases })
}

/// One alias / address / handle on a person.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AliasEntry {
    /// `alias` (a name it was seen under), `email`, or `handle` (`<channel>:<value>`).
    pub kind: &'static str,
    /// The stored value — what a split request names to move it.
    pub value: String,
}

/// One provenance event, metadata only (never its content).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EvidenceEvent {
    pub id: i64,
    pub ts: i64,
    pub source: String,
    pub kind: String,
}

/// What a split can choose from.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Evidence {
    pub table: &'static str,
    pub id: i64,
    pub title: String,
    pub aliases: Vec<AliasEntry>,
    /// Newest first, at most [`MAX_EVIDENCE_EVENTS`].
    pub events: Vec<EvidenceEvent>,
    /// All provenance events, including any beyond the listed ones.
    pub event_count: usize,
}

fn json_list(raw: Option<String>) -> Vec<String> {
    raw.and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok()).unwrap_or_default()
}

/// A person's raw `aliases` / `emails` / `handles` JSON columns.
type IdentityLists = (Option<String>, Option<String>, Option<String>);

/// The (column, kind) pairs a person's identity lists live in.
const ALIAS_COLUMNS: [(&str, &str); 3] = [("aliases", "alias"), ("emails", "email"), ("handles", "handle")];

/// A row's evidence: its aliases (people) and provenance events. `None` when the row is gone or
/// the table cannot be split.
pub fn evidence(conn: &Connection, table: StateTable, id: i64) -> Result<Option<Evidence>, rusqlite::Error> {
    let (title, aliases) = match table {
        StateTable::People => {
            let row: Option<(String, IdentityLists)> = conn
                .query_row(
                    "SELECT display_name, aliases, emails, handles FROM people WHERE id = ?1",
                    [id],
                    |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?, r.get(3)?))),
                )
                .optional()?;
            let Some((title, (a, e, h))) = row else { return Ok(None) };
            let mut aliases = Vec::new();
            for ((_, kind), raw) in ALIAS_COLUMNS.iter().zip([a, e, h]) {
                for value in json_list(raw) {
                    aliases.push(AliasEntry { kind, value });
                }
            }
            (title, aliases)
        }
        StateTable::Projects => {
            let title: Option<String> = conn
                .query_row("SELECT name FROM projects WHERE id = ?1", [id], |r| r.get(0))
                .optional()?;
            let Some(title) = title else { return Ok(None) };
            (title, Vec::new())
        }
        StateTable::Commitments | StateTable::OpenLoops => return Ok(None),
    };
    let event_count: i64 = conn.query_row(
        "SELECT count(DISTINCT event_id) FROM state_provenance WHERE state_table = ?1 AND state_id = ?2",
        params![table.as_str(), id],
        |r| r.get(0),
    )?;
    let mut stmt = conn.prepare(
        "SELECT DISTINCT e.id, e.ts, e.source, e.kind
           FROM state_provenance p JOIN event_log e ON e.id = p.event_id
          WHERE p.state_table = ?1 AND p.state_id = ?2
          ORDER BY e.ts DESC, e.id DESC LIMIT ?3",
    )?;
    let events = stmt
        .query_map(params![table.as_str(), id, MAX_EVIDENCE_EVENTS as i64], |r| {
            Ok(EvidenceEvent { id: r.get(0)?, ts: r.get(1)?, source: r.get(2)?, kind: r.get(3)? })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(Evidence {
        table: table.as_str(),
        id,
        title,
        aliases,
        events,
        event_count: usize::try_from(event_count).unwrap_or(0),
    }))
}

fn linked_events(conn: &Connection, table: StateTable, id: i64) -> Result<BTreeSet<i64>, rusqlite::Error> {
    let mut stmt =
        conn.prepare("SELECT event_id FROM state_provenance WHERE state_table = ?1 AND state_id = ?2")?;
    let rows = stmt.query_map(params![table.as_str(), id], |r| r.get::<_, i64>(0))?;
    rows.collect()
}

fn max_event_ts(conn: &Connection, table: StateTable, id: i64) -> Result<Option<i64>, rusqlite::Error> {
    conn.query_row(
        "SELECT max(e.ts) FROM state_provenance p JOIN event_log e ON e.id = p.event_id
          WHERE p.state_table = ?1 AND p.state_id = ?2",
        params![table.as_str(), id],
        |r| r.get(0),
    )
}

/// Split `req.event_ids` (and, for people, `req.aliases`) off `req.id` into a new row named
/// `req.name`. `Ok(None)` when the row does not exist. At least one event must move (the new row
/// needs provenance, FR-ST-02) and at least one must stay (otherwise it is a rename — use
/// `state.update`). Every chosen event and alias must belong to the row.
pub fn split(conn: &mut Connection, req: &SplitRequest, now: i64) -> Result<Option<SplitOutcome>, EditError> {
    if !matches!(req.table, StateTable::People | StateTable::Projects) {
        return Err(EditError::Invalid("invalid table for split"));
    }
    if req.event_ids.is_empty() {
        return Err(EditError::Invalid("invalid split: no events chosen"));
    }
    if !req.aliases.is_empty() && req.table != StateTable::People {
        return Err(EditError::Invalid("invalid field for table"));
    }
    let name = clean_text(&req.name, MAX_NAME_CHARS, false)?.unwrap_or_default();
    let table = req.table;
    let t = table.as_str();

    let tx = conn.savepoint()?;
    let exists: Option<i64> =
        tx.query_row(&format!("SELECT id FROM {t} WHERE id = ?1"), [req.id], |r| r.get(0)).optional()?;
    if exists.is_none() {
        return Ok(None);
    }
    let linked = linked_events(&tx, table, req.id)?;
    if req.event_ids.iter().any(|e| !linked.contains(e)) {
        return Err(EditError::Invalid("invalid split: event not linked to this record"));
    }
    if linked.len() <= req.event_ids.len() {
        return Err(EditError::Invalid("invalid split: nothing would be left"));
    }

    // Aliases: take each chosen value out of the column it lives in.
    let mut moved_aliases = 0usize;
    let mut new_lists: [Vec<String>; 3] = Default::default();
    if table == StateTable::People {
        let lists: IdentityLists = tx.query_row(
            "SELECT aliases, emails, handles FROM people WHERE id = ?1",
            [req.id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let mut kept: [Vec<String>; 3] = [json_list(lists.0), json_list(lists.1), json_list(lists.2)];
        for wanted in &req.aliases {
            let mut found = false;
            for (list, moved) in kept.iter_mut().zip(new_lists.iter_mut()) {
                if let Some(pos) = list.iter().position(|v| v.trim().eq_ignore_ascii_case(wanted)) {
                    moved.push(list.remove(pos));
                    found = true;
                    break;
                }
            }
            if !found {
                return Err(EditError::Invalid("invalid split: alias not on this record"));
            }
            moved_aliases += 1;
        }
        if moved_aliases > 0 {
            let enc = |v: &Vec<String>| -> Option<String> {
                (!v.is_empty()).then(|| serde_json::to_string(v).unwrap_or_else(|_| "[]".into()))
            };
            tx.execute(
                "UPDATE people SET aliases = ?1, emails = ?2, handles = ?3 WHERE id = ?4",
                params![enc(&kept[0]), enc(&kept[1]), enc(&kept[2]), req.id],
            )?;
        }
    }

    // The new row: the user's statement, pinned.
    let enc = |v: &Vec<String>| -> Option<String> {
        (!v.is_empty()).then(|| serde_json::to_string(v).unwrap_or_else(|_| "[]".into()))
    };
    match table {
        StateTable::People => tx.execute(
            "INSERT INTO people
               (display_name, aliases, emails, handles, confidence, base_confidence, user_pinned,
                user_edited_at, created_at, updated_at, last_evidence_at)
             VALUES (?1, ?2, ?3, ?4, 1.0, 1.0, 1, ?5, ?5, ?5, ?5)",
            params![name, enc(&new_lists[0]), enc(&new_lists[1]), enc(&new_lists[2]), now],
        )?,
        _ => tx.execute(
            "INSERT INTO projects
               (name, status, confidence, base_confidence, user_pinned, user_edited_at,
                created_at, updated_at, last_evidence_at)
             SELECT ?1, status, 1.0, 1.0, 1, ?2, ?2, ?2, ?2 FROM projects WHERE id = ?3",
            params![name, now, req.id],
        )?,
    };
    let new_id = tx.last_insert_rowid();

    // Move the chosen provenance links.
    let mut moved_events = 0usize;
    for event_id in &req.event_ids {
        moved_events += tx.execute(
            "UPDATE state_provenance SET state_id = ?1
              WHERE state_table = ?2 AND state_id = ?3 AND event_id = ?4",
            params![new_id, t, req.id, event_id],
        )?;
    }

    // Commitments / open loops grounded only in moved evidence follow it.
    let moved: BTreeSet<i64> = req.event_ids.iter().copied().collect();
    let column = if table == StateTable::People { "counterparty_id" } else { "project_id" };
    let mut moved_references = 0usize;
    for child in [StateTable::Commitments, StateTable::OpenLoops] {
        let c = child.as_str();
        let ids: Vec<i64> = {
            let mut stmt = tx.prepare(&format!("SELECT id FROM {c} WHERE {column} = ?1"))?;
            let rows = stmt.query_map([req.id], |r| r.get::<_, i64>(0))?;
            rows.collect::<Result<_, _>>()?
        };
        for child_id in ids {
            let evidence = linked_events(&tx, child, child_id)?;
            if !evidence.is_empty() && evidence.is_subset(&moved) {
                moved_references += tx.execute(
                    &format!("UPDATE {c} SET {column} = ?1, updated_at = ?2 WHERE id = ?3"),
                    params![new_id, now, child_id],
                )?;
            }
        }
    }

    // Evidence times, then pin the original too: it is now the user's reading of who it is.
    let new_last = max_event_ts(&tx, table, new_id)?.unwrap_or(now);
    tx.execute(&format!("UPDATE {t} SET last_evidence_at = ?1 WHERE id = ?2"), params![new_last, new_id])?;
    let kept_last = max_event_ts(&tx, table, req.id)?;
    tx.execute(
        &format!(
            "UPDATE {t} SET confidence = 1.0, base_confidence = 1.0, user_pinned = 1,
                    user_edited_at = ?1, updated_at = ?1,
                    last_evidence_at = COALESCE(?2, last_evidence_at)
              WHERE id = ?3"
        ),
        params![now, kept_last, req.id],
    )?;

    // Record the decision so the resolvers do not fuse them again.
    tx.execute(
        "INSERT INTO state_merge_blocks (state_table, kept_id, split_id, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![t, req.id, new_id, now],
    )?;
    let block_id = tx.last_insert_rowid();
    for event_id in &req.event_ids {
        tx.execute(
            "INSERT OR IGNORE INTO state_split_events (block_id, event_id) VALUES (?1, ?2)",
            params![block_id, event_id],
        )?;
    }
    tx.commit()?;

    for id in [req.id, new_id] {
        let _ = crate::search_documents::project_source_after_write(conn, &source_ref(table, id), now);
    }
    Ok(Some(SplitOutcome { new_id, moved_events, moved_aliases, moved_references }))
}

/// Where an extractor should link `event_id` when it resolved it to `state_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitRedirect {
    /// No split moved this event away from the row: link as usual.
    Keep,
    /// The user moved this event to another row: link it there.
    To(i64),
    /// The user moved it to a row that no longer exists: link it nowhere.
    Drop,
}

/// SQL: row `?id` in `table` existed when the block was written (a reused id does not inherit it).
fn row_predates(conn: &Connection, table: &str, id: i64, block_at: i64) -> Result<bool, rusqlite::Error> {
    Ok(conn
        .query_row(
            &format!("SELECT 1 FROM {table} WHERE id = ?1 AND created_at <= ?2"),
            params![id, block_at],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Follow the user's splits for one event: if it was moved away from `state_id` (possibly more
/// than once — A→B, then B→C), return where it lives now. Only evidence captured at or before a
/// split is redirected, and a row created after the split (a reused id) never inherits it.
pub fn split_redirect(
    conn: &Connection,
    table: StateTable,
    state_id: i64,
    event_id: i64,
) -> Result<SplitRedirect, rusqlite::Error> {
    if !matches!(table, StateTable::People | StateTable::Projects) {
        return Ok(SplitRedirect::Keep);
    }
    let Some(event_ts) = crate::state_edit::event_ts(conn, event_id)? else {
        return Ok(SplitRedirect::Keep);
    };
    let t = table.as_str();
    let mut current = state_id;
    let mut last_block_at = None;
    for _ in 0..MAX_REDIRECT_HOPS {
        let next: Option<(i64, i64)> = conn
            .query_row(
                "SELECT b.split_id, b.created_at FROM state_merge_blocks b
                   JOIN state_split_events s ON s.block_id = b.id
                  WHERE b.state_table = ?1 AND b.kept_id = ?2 AND s.event_id = ?3
                    AND b.created_at >= ?4
                  ORDER BY b.created_at DESC, b.id DESC LIMIT 1",
                params![t, current, event_id, event_ts],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((next, at)) = next else { break };
        if last_block_at.is_none() && !row_predates(conn, t, current, at)? {
            // The row the extractor found is not the one the user split (its id was reused).
            return Ok(SplitRedirect::Keep);
        }
        current = next;
        last_block_at = Some(at);
    }
    match last_block_at {
        None => Ok(SplitRedirect::Keep),
        Some(at) if row_predates(conn, t, current, at)? => Ok(SplitRedirect::To(current)),
        Some(_) => Ok(SplitRedirect::Drop),
    }
}

/// Whether the user said rows `a` and `b` are not the same entity (either direction). A resolver
/// must never fuse such a pair, and a name that matches both is ambiguous.
pub fn is_merge_blocked(conn: &Connection, table: StateTable, a: i64, b: i64) -> Result<bool, rusqlite::Error> {
    if a == b || !matches!(table, StateTable::People | StateTable::Projects) {
        return Ok(false);
    }
    let t = table.as_str();
    let at: Option<i64> = conn
        .query_row(
            "SELECT max(created_at) FROM state_merge_blocks
              WHERE state_table = ?1
                AND ((kept_id = ?2 AND split_id = ?3) OR (kept_id = ?3 AND split_id = ?2))",
            params![t, a, b],
            |r| r.get(0),
        )?;
    let Some(at) = at else { return Ok(false) };
    Ok(row_predates(conn, t, a, at)? && row_predates(conn, t, b, at)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_log::{insert as insert_event, NewEvent};
    use crate::state::{
        insert_commitment, insert_person, CommitmentDirection, CommitmentStatus, NewCommitment,
        NewPerson, Provenance,
    };

    fn event(conn: &Connection, content: &str, ts: i64) -> i64 {
        insert_event(
            conn,
            &NewEvent {
                ts,
                source: "capture",
                kind: "text",
                app_bundle_id: None,
                window_title: None,
                content,
                content_hash: &format!("{content}{ts}"),
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap()
    }

    fn links(conn: &Connection, table: &str, id: i64) -> Vec<i64> {
        let mut stmt = conn
            .prepare("SELECT event_id FROM state_provenance WHERE state_table = ?1 AND state_id = ?2 ORDER BY event_id")
            .unwrap();
        stmt.query_map(params![table, id], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
    }

    fn req(table: StateTable, id: i64, name: &str, events: &[i64], aliases: &[&str]) -> SplitRequest {
        SplitRequest {
            table,
            id,
            name: name.into(),
            event_ids: events.to_vec(),
            aliases: aliases.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn pinned(conn: &Connection, table: &str, id: i64) -> (f64, i64) {
        conn.query_row(&format!("SELECT confidence, user_pinned FROM {table} WHERE id = ?1"), [id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap()
    }

    fn fused_alex(conn: &mut Connection) -> (i64, [i64; 4]) {
        let e = [
            event(conn, "a1", 100),
            event(conn, "a2", 200),
            event(conn, "b1", 300),
            event(conn, "b2", 400),
        ];
        let id = insert_person(
            conn,
            &NewPerson {
                display_name: "Alex",
                aliases_json: Some(r#"["Alex K","Alex M"]"#),
                emails_json: Some(r#"["alex.k@acme.com","alex.m@acme.com"]"#),
                handles_json: Some(r#"["slack:alexm"]"#),
                confidence: 0.95,
                now: 1,
                ..Default::default()
            },
            &e.map(Provenance::new),
        )
        .unwrap();
        (id, e)
    }

    #[test]
    fn split_moves_exactly_the_chosen_links_and_aliases() {
        let mut conn = crate::open_in_memory().unwrap();
        let (alex, e) = fused_alex(&mut conn);
        let out = split(
            &mut conn,
            &req(StateTable::People, alex, "Alex Morgan", &[e[2], e[3]], &["alex.m@acme.com", "SLACK:ALEXM", "Alex M"]),
            1_000,
        )
        .unwrap()
        .unwrap();
        assert_eq!((out.moved_events, out.moved_aliases), (2, 3));
        assert_eq!(links(&conn, "people", alex), vec![e[0], e[1]]);
        assert_eq!(links(&conn, "people", out.new_id), vec![e[2], e[3]]);

        let ev = evidence(&conn, StateTable::People, out.new_id).unwrap().unwrap();
        assert_eq!(ev.title, "Alex Morgan");
        let values: Vec<_> = ev.aliases.iter().map(|a| (a.kind, a.value.as_str())).collect();
        assert_eq!(values, vec![("alias", "Alex M"), ("email", "alex.m@acme.com"), ("handle", "slack:alexm")]);
        assert_eq!(ev.events.iter().map(|x| x.id).collect::<Vec<_>>(), vec![e[3], e[2]], "newest first");
        let kept = evidence(&conn, StateTable::People, alex).unwrap().unwrap();
        let values: Vec<_> = kept.aliases.iter().map(|a| a.value.as_str()).collect();
        assert_eq!(values, vec!["Alex K", "alex.k@acme.com"]);
        assert_eq!(kept.event_count, 2);

        // both sides are the user's statement now
        assert_eq!(pinned(&conn, "people", alex), (1.0, 1));
        assert_eq!(pinned(&conn, "people", out.new_id), (1.0, 1));
        let last: i64 = conn
            .query_row("SELECT last_evidence_at FROM people WHERE id = ?1", [out.new_id], |r| r.get(0))
            .unwrap();
        assert_eq!(last, 400);
        assert!(is_merge_blocked(&conn, StateTable::People, alex, out.new_id).unwrap());
        assert!(is_merge_blocked(&conn, StateTable::People, out.new_id, alex).unwrap());
    }

    #[test]
    fn split_refuses_what_does_not_belong_or_would_empty_the_row() {
        let mut conn = crate::open_in_memory().unwrap();
        let (alex, e) = fused_alex(&mut conn);
        let other = event(&conn, "x", 50);
        let bad = [
            req(StateTable::People, alex, "B", &[other], &[]),
            req(StateTable::People, alex, "B", &e, &[]),
            req(StateTable::People, alex, "B", &[], &["Alex K"]),
            req(StateTable::People, alex, "B", &[e[0]], &["nobody@x.com"]),
            req(StateTable::People, alex, "   ", &[e[0]], &[]),
            req(StateTable::Commitments, alex, "B", &[e[0]], &[]),
        ];
        for r in bad {
            assert!(matches!(split(&mut conn, &r, 5), Err(EditError::Invalid(_))), "{r:?}");
        }
        // nothing changed
        assert_eq!(links(&conn, "people", alex), e.to_vec());
        let n: i64 = conn.query_row("SELECT count(*) FROM people", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        assert_eq!(split(&mut conn, &req(StateTable::People, 999, "B", &[e[0]], &[]), 5).unwrap(), None);
    }

    #[test]
    fn references_grounded_only_in_moved_evidence_follow_it() {
        let mut conn = crate::open_in_memory().unwrap();
        let (alex, e) = fused_alex(&mut conn);
        let commit = |conn: &mut Connection, desc: &str, ev: &[i64]| {
            insert_commitment(
                conn,
                &NewCommitment {
                    direction: CommitmentDirection::Theirs,
                    counterparty_id: Some(alex),
                    description: desc,
                    due_at: None,
                    status: CommitmentStatus::Open,
                    project_id: None,
                    confidence: 0.7,
                    now: 1,
                },
                &ev.iter().copied().map(Provenance::new).collect::<Vec<_>>(),
            )
            .unwrap()
        };
        let theirs = commit(&mut conn, "send the invoice", &[e[2]]);
        let mixed = commit(&mut conn, "review the plan", &[e[1], e[3]]);
        let kept = commit(&mut conn, "book the room", &[e[0]]);
        let out = split(&mut conn, &req(StateTable::People, alex, "Alex Morgan", &[e[2], e[3]], &[]), 1_000)
            .unwrap()
            .unwrap();
        assert_eq!(out.moved_references, 1);
        let cp = |id: i64| -> Option<i64> {
            conn.query_row("SELECT counterparty_id FROM commitments WHERE id = ?1", [id], |r| r.get(0)).unwrap()
        };
        assert_eq!(cp(theirs), Some(out.new_id));
        assert_eq!(cp(mixed), Some(alex), "mixed evidence cannot be decided from provenance");
        assert_eq!(cp(kept), Some(alex));
    }

    /// The acceptance property: re-extracting the same events does not fuse them again.
    #[test]
    fn re_extraction_of_moved_events_does_not_re_merge() {
        let mut conn = crate::open_in_memory().unwrap();
        let content = "Met Alex from Acme about the launch";
        let e1 = event(&conn, content, 1_000);
        let e2 = event(&conn, &format!("{content} again"), 2_000);
        let cand = crate::knowledge::Candidate {
            kind: crate::knowledge::Kind::Person,
            name: "Alex".into(),
            context: "Acme".into(),
            quote: content.into(), ..Default::default() };
        let alex = crate::knowledge::persist(&mut conn, e1, &cand, 1_000).unwrap().unwrap();
        assert_eq!(crate::knowledge::persist(&mut conn, e2, &cand, 2_000).unwrap(), Some(alex));
        assert_eq!(links(&conn, "people", alex), vec![e1, e2]);

        let out = split(&mut conn, &req(StateTable::People, alex, "Alex (Acme sales)", &[e2], &[]), 3_000)
            .unwrap()
            .unwrap();
        // the Dream Cycle re-reads both events
        assert_eq!(crate::knowledge::persist(&mut conn, e1, &cand, 4_000).unwrap(), Some(alex));
        assert_eq!(crate::knowledge::persist(&mut conn, e2, &cand, 4_000).unwrap(), Some(out.new_id));
        assert_eq!(links(&conn, "people", alex), vec![e1]);
        assert_eq!(links(&conn, "people", out.new_id), vec![e2]);

        // a new mention after the split is new evidence and resolves by key as usual
        let e3 = event(&conn, &format!("{content} later"), 5_000);
        assert_eq!(crate::knowledge::persist(&mut conn, e3, &cand, 5_000).unwrap(), Some(alex));
    }

    #[test]
    fn a_split_of_a_split_follows_the_chain_and_a_deleted_target_drops() {
        let mut conn = crate::open_in_memory().unwrap();
        let (alex, e) = fused_alex(&mut conn);
        let b = split(&mut conn, &req(StateTable::People, alex, "B", &[e[2], e[3]], &[]), 1_000)
            .unwrap()
            .unwrap()
            .new_id;
        let c = split(&mut conn, &req(StateTable::People, b, "C", &[e[3]], &[]), 2_000)
            .unwrap()
            .unwrap()
            .new_id;
        let r = |state, ev| split_redirect(&conn, StateTable::People, state, ev).unwrap();
        assert_eq!(r(alex, e[0]), SplitRedirect::Keep);
        assert_eq!(r(alex, e[2]), SplitRedirect::To(b));
        assert_eq!(r(alex, e[3]), SplitRedirect::To(c));
        assert_eq!(r(b, e[3]), SplitRedirect::To(c));

        crate::state_edit::delete_state(&mut conn, StateTable::People, c, 3_000).unwrap();
        assert_eq!(
            split_redirect(&conn, StateTable::People, alex, e[3]).unwrap(),
            SplitRedirect::Drop,
            "the user's decision outlives the row they moved it to"
        );
    }

    #[test]
    fn a_reused_id_does_not_inherit_a_split() {
        let mut conn = crate::open_in_memory().unwrap();
        let (alex, e) = fused_alex(&mut conn);
        let b = split(&mut conn, &req(StateTable::People, alex, "B", &[e[3]], &[]), 1_000)
            .unwrap()
            .unwrap()
            .new_id;
        crate::state_edit::delete_state(&mut conn, StateTable::People, b, 2_000).unwrap();
        // SQLite hands the freed max id to the next row
        let ev = event(&conn, "z", 3_000);
        let reborn = insert_person(
            &mut conn,
            &NewPerson { display_name: "Zoe", confidence: 0.5, now: 3_000, ..Default::default() },
            &[Provenance::new(ev)],
        )
        .unwrap();
        assert_eq!(reborn, b);
        assert!(!is_merge_blocked(&conn, StateTable::People, alex, reborn).unwrap());
        assert_eq!(split_redirect(&conn, StateTable::People, alex, e[3]).unwrap(), SplitRedirect::Drop);
    }

    #[test]
    fn projects_split_by_events_and_keep_status() {
        let mut conn = crate::open_in_memory().unwrap();
        let e1 = event(&conn, "p1", 10);
        let e2 = event(&conn, "p2", 20);
        let p = crate::state::insert_project(
            &mut conn,
            &crate::state::NewProject {
                name: "Launch",
                status: crate::state::ProjectStatus::Waiting,
                summary: None,
                participants_json: None,
                sources_json: None,
                confidence: 0.6,
                now: 1,
            },
            &[Provenance::new(e1), Provenance::new(e2)],
        )
        .unwrap();
        assert!(matches!(
            split(&mut conn, &req(StateTable::Projects, p, "Launch EU", &[e2], &["x"]), 5),
            Err(EditError::Invalid(_))
        ));
        let out = split(&mut conn, &req(StateTable::Projects, p, "Launch EU", &[e2], &[]), 5).unwrap().unwrap();
        let status: String =
            conn.query_row("SELECT status FROM projects WHERE id = ?1", [out.new_id], |r| r.get(0)).unwrap();
        assert_eq!(status, "waiting");
        assert_eq!(links(&conn, "projects", out.new_id), vec![e2]);
        assert_eq!(pinned(&conn, "projects", p), (1.0, 1));
    }

    #[test]
    fn request_parser_is_strict() {
        let r = parse_request(
            r#"{"table":"people","id":3,"name":"Alex M","event_ids":[9,4,9],"aliases":["a@x.com","A@X.com"]}"#,
        )
        .unwrap();
        assert_eq!(r.event_ids, vec![4, 9]);
        assert_eq!(r.aliases, vec!["a@x.com".to_string()]);
        assert_eq!(r.table, StateTable::People);
        for bad in [
            "",
            r#"{"table":"commitments","id":1,"name":"x","event_ids":[1]}"#,
            r#"{"table":"people","id":1,"event_ids":[1]}"#,
            r#"{"table":"people","id":1,"name":"x"}"#,
            r#"{"table":"people","id":1,"name":"x","event_ids":[]}"#,
            r#"{"table":"people","id":1,"name":"x","event_ids":[0]}"#,
            r#"{"table":"people","id":1,"name":"x","event_ids":[1],"confidence":1}"#,
            r#"{"table":"projects","id":1,"name":"x","event_ids":[1],"aliases":["a"]}"#,
            r#"{"table":"people","id":1,"name":"x","event_ids":[1],"aliases":[" "]}"#,
        ] {
            assert!(parse_request(bad).is_err(), "{bad}");
        }
    }
}
