//! The user merges two records that are one person / project — the inverse of
//! [`crate::state_split`] (issue #1362, FR-ST-10's other direction).
//!
//! Automatic resolution fuses rows only on certainty: an exact normalised name or alias
//! ([`crate::entity_dedupe::dedupe_exact`]) or a group the review model named and the lexical
//! check agreed with ([`crate::entity_dedupe::apply_groups`]). Everything the lexical rule merely
//! *relates* ([`crate::entity_names::related`]: a shared word, one name a prefix of another) stays
//! two rows until the person decides. This module is that decision:
//!
//! - [`candidates`] lists the related pairs nothing merged, newest first, for the Memory page's
//!   "Possible duplicates" and the `state.merge.candidates` read;
//! - [`merge`] folds the rows the person chose with the one merge there is
//!   ([`crate::entity_dedupe::merge`]: provenance, links and aliases move to the survivor, nothing
//!   is deleted from the event log, confidence is never raised);
//! - [`keep_separate`] records the opposite answer in `state_merge_blocks`, the same table a split
//!   writes, so the pair is neither suggested again nor fused by any automatic pass.
//!
//! Nothing here merges on its own: every call carries the ids the person named.

use std::collections::HashSet;

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::entity_names as names;
use crate::state::StateTable;
use crate::state_edit::EditError;

/// Most candidate pairs one read returns, newest first.
pub const MAX_CANDIDATES: usize = 20;
/// Most rows one merge may fold.
pub const MAX_MERGE_IDS: usize = 10;

/// Two rows the lexical rule relates that nothing merged. `ids[0]` is the older row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Candidate {
    pub table: &'static str,
    pub ids: [i64; 2],
    pub names: [String; 2],
    /// Distinct clock hours each row was named in (the review's `mention_count`).
    pub mention_counts: [i64; 2],
    /// `created_at` (unix ms) of each row.
    pub first_seen: [i64; 2],
    /// Provenance links of each row.
    pub evidence_counts: [i64; 2],
}

/// A parsed `state.merge` request: `{"table", "ids"}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeRequest {
    pub table: StateTable,
    /// Distinct, ascending, at least two.
    pub ids: Vec<i64>,
}

/// What a merge did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct MergeOutcome {
    pub survivor_id: i64,
    pub absorbed: usize,
}

/// A parsed `state.keep_separate` request: `{"table", "id", "other_id"}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeepSeparateRequest {
    pub table: StateTable,
    pub id: i64,
    pub other_id: i64,
}

fn parse_table(body: &serde_json::Value) -> Result<StateTable, EditError> {
    body.get("table")
        .and_then(serde_json::Value::as_str)
        .and_then(StateTable::parse)
        .filter(|t| matches!(t, StateTable::People | StateTable::Projects))
        .ok_or(EditError::Invalid("table must be people or projects"))
}

fn parse_id(body: &serde_json::Value, field: &'static str, missing: &'static str) -> Result<i64, EditError> {
    body.get(field).and_then(serde_json::Value::as_i64).filter(|id| *id > 0).ok_or(EditError::Invalid(missing))
}

/// Parse the shared `state.merge` body every face sends.
pub fn parse_merge(body: &str) -> Result<MergeRequest, EditError> {
    let body: serde_json::Value = serde_json::from_str(body).map_err(|_| EditError::Invalid("body is not JSON"))?;
    let table = parse_table(&body)?;
    let raw = body.get("ids").and_then(serde_json::Value::as_array).ok_or(EditError::Invalid("ids must be a list"))?;
    let mut ids: Vec<i64> = raw
        .iter()
        .map(|v| v.as_i64().filter(|id| *id > 0).ok_or(EditError::Invalid("ids must be positive integers")))
        .collect::<Result<_, _>>()?;
    ids.sort_unstable();
    ids.dedup();
    if ids.len() < 2 {
        return Err(EditError::Invalid("name at least two different records"));
    }
    if ids.len() > MAX_MERGE_IDS {
        return Err(EditError::Invalid("too many records for one merge"));
    }
    Ok(MergeRequest { table, ids })
}

/// Parse the shared `state.keep_separate` body.
pub fn parse_keep_separate(body: &str) -> Result<KeepSeparateRequest, EditError> {
    let body: serde_json::Value = serde_json::from_str(body).map_err(|_| EditError::Invalid("body is not JSON"))?;
    let table = parse_table(&body)?;
    let id = parse_id(&body, "id", "id must be a positive integer")?;
    let other_id = parse_id(&body, "other_id", "other_id must be a positive integer")?;
    if id == other_id {
        return Err(EditError::Invalid("name two different records"));
    }
    Ok(KeepSeparateRequest { table, id, other_id })
}

struct Row {
    id: i64,
    name: String,
    keys: HashSet<String>,
    locked: bool,
    updated_at: i64,
    created_at: i64,
    mention_count: i64,
    evidence_count: i64,
}

/// The rows a merge may touch: every person / project except those the review classed as not
/// one at all (a cursor name, a tool, a task label) — a wrong row is deleted, not merged.
fn load(conn: &Connection, table: StateTable) -> Result<Vec<Row>, rusqlite::Error> {
    let (t, name, class) = match table {
        StateTable::People => ("people", "display_name", "COALESCE(person_class, '') = 'not_a_person'"),
        StateTable::Projects => ("projects", "name", "COALESCE(project_class, '') = 'not_a_project'"),
        _ => return Ok(Vec::new()),
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT a.id, a.{name}, a.aliases, {locked}, a.updated_at, a.created_at, a.mention_count,
                (SELECT COUNT(*) FROM state_provenance p WHERE p.state_table = '{t}' AND p.state_id = a.id)
           FROM {t} a
          WHERE NOT ({class})
          ORDER BY a.id",
        locked = names::LOCKED_SQL,
    ))?;
    let rows = stmt
        .query_map([], |r| {
            let name: String = r.get(1)?;
            let aliases = names::parse_aliases(r.get::<_, Option<String>>(2)?.as_deref());
            let keys = std::iter::once(&name)
                .chain(aliases.iter())
                .map(|n| names::key(n))
                .filter(|k| !k.is_empty())
                .collect();
            Ok(Row {
                id: r.get(0)?,
                name,
                keys,
                locked: r.get(3)?,
                updated_at: r.get(4)?,
                created_at: r.get(5)?,
                mention_count: r.get(6)?,
                evidence_count: r.get(7)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The pairs the lexical rule relates ([`names::related`]) that nothing merged: not two rows the
/// user edited by hand (neither may absorb the other), not a pair the user split or kept apart.
/// Newest activity first, at most [`MAX_CANDIDATES`]. Empty for a table that cannot be merged.
pub fn candidates(conn: &Connection, table: StateTable) -> Result<Vec<Candidate>, rusqlite::Error> {
    let t = match table {
        StateTable::People => "people",
        StateTable::Projects => "projects",
        _ => return Ok(Vec::new()),
    };
    let rows = load(conn, table)?;
    let mut pairs: Vec<(i64, &Row, &Row)> = Vec::new();
    for (i, a) in rows.iter().enumerate() {
        for b in &rows[i + 1..] {
            if (a.locked && b.locked) || !names::related(&a.keys, &b.keys) {
                continue;
            }
            if crate::state_split::is_merge_blocked(conn, table, a.id, b.id)? {
                continue;
            }
            pairs.push((a.updated_at.max(b.updated_at), a, b));
        }
    }
    pairs.sort_by_key(|(seen, a, b)| (std::cmp::Reverse(*seen), a.id, b.id));
    Ok(pairs
        .into_iter()
        .take(MAX_CANDIDATES)
        .map(|(_, a, b)| Candidate {
            table: t,
            ids: [a.id, b.id],
            names: [a.name.clone(), b.name.clone()],
            mention_counts: [a.mention_count, b.mention_count],
            first_seen: [a.created_at, b.created_at],
            evidence_counts: [a.evidence_count, b.evidence_count],
        })
        .collect())
}

fn exists(conn: &Connection, table: StateTable, id: i64) -> Result<bool, rusqlite::Error> {
    Ok(conn
        .query_row(&format!("SELECT 1 FROM {} WHERE id = ?1", table.as_str()), [id], |_| Ok(()))
        .optional()?
        .is_some())
}

/// Fold the rows the person named into one ([`crate::entity_dedupe::merge`] decides the survivor:
/// the row the user edited, else the owner's own, else the oldest). `Ok(None)` when a row does not
/// exist. `Invalid` when the merge is refused: the user split (or kept apart) two of them, or two
/// of them were edited by hand.
pub fn merge(conn: &mut Connection, req: &MergeRequest, now: i64) -> Result<Option<MergeOutcome>, EditError> {
    if !matches!(req.table, StateTable::People | StateTable::Projects) {
        return Err(EditError::Invalid("table must be people or projects"));
    }
    for id in &req.ids {
        if !exists(conn, req.table, *id)? {
            return Ok(None);
        }
    }
    let absorbed = crate::entity_dedupe::merge(conn, req.table, &req.ids, None, now)?;
    if absorbed == 0 {
        return Err(EditError::Invalid(
            "these records cannot be merged: you split them apart, or edited both by hand",
        ));
    }
    let mut survivor = None;
    for id in &req.ids {
        if exists(conn, req.table, *id)? {
            survivor = Some(*id);
        }
    }
    let survivor_id = survivor.ok_or(EditError::Invalid("merge left no record"))?;
    Ok(Some(MergeOutcome { survivor_id, absorbed }))
}

/// Record that two rows are different people / projects: they are not suggested again and no
/// automatic pass fuses them ([`crate::state_split::is_merge_blocked`]). `Ok(None)` when a row
/// does not exist. Idempotent.
pub fn keep_separate(conn: &Connection, req: &KeepSeparateRequest, now: i64) -> Result<Option<()>, EditError> {
    if !matches!(req.table, StateTable::People | StateTable::Projects) {
        return Err(EditError::Invalid("table must be people or projects"));
    }
    if !exists(conn, req.table, req.id)? || !exists(conn, req.table, req.other_id)? {
        return Ok(None);
    }
    conn.execute(
        "INSERT OR IGNORE INTO state_merge_blocks (state_table, kept_id, split_id, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![req.table.as_str(), req.id, req.other_id, now],
    )?;
    Ok(Some(()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn add(conn: &Connection, table: &str, name: &str, aliases: Option<&str>, updated: i64, locked: bool) -> i64 {
        let (name_col, extra, extra_v) = if table == "people" {
            ("display_name", "", "")
        } else {
            ("name", "status,", "'active',")
        };
        conn.execute(
            &format!(
                "INSERT INTO {table}({name_col},aliases,{extra}confidence,base_confidence,created_at,updated_at,last_evidence_at,user_pinned,mention_count)
                 VALUES(?1,?2,{extra_v}0.6,0.6,?3,?3,?3,?4,2)"
            ),
            params![name, aliases, updated, i64::from(locked)],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn link(conn: &Connection, table: &str, id: i64, n: i64) {
        let event = crate::event_log::insert(
            conn,
            &crate::event_log::NewEvent {
                ts: n,
                source: "capture",
                kind: "text",
                app_bundle_id: Some("app"),
                window_title: None,
                content: &format!("event {n}"),
                content_hash: &format!("h{n}"),
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap();
        conn.execute(
            "INSERT INTO state_provenance(state_table,state_id,event_id) VALUES(?1,?2,?3)",
            params![table, id, event],
        )
        .unwrap();
    }

    fn ids(c: &[Candidate]) -> Vec<[i64; 2]> {
        c.iter().map(|c| c.ids).collect()
    }

    #[test]
    fn related_pairs_are_candidates_newest_first_with_counts() {
        let conn = crate::open_in_memory().unwrap();
        let alex = add(&conn, "people", "Alex", None, 100, false);
        let alex_m = add(&conn, "people", "Alex Morgan", None, 300, false);
        let bea = add(&conn, "people", "Bea", None, 200, false);
        let bea_t = add(&conn, "people", "Bea Tanaka", Some(r#"["bea.t"]"#), 150, false);
        add(&conn, "people", "Zed", None, 400, false);
        link(&conn, "people", alex, 1);
        link(&conn, "people", alex, 2);
        link(&conn, "people", bea_t, 3);

        let found = candidates(&conn, StateTable::People).unwrap();
        assert_eq!(ids(&found), vec![[alex, alex_m], [bea, bea_t]]);
        let first = &found[0];
        assert_eq!(first.table, "people");
        assert_eq!(first.names, ["Alex".to_string(), "Alex Morgan".to_string()]);
        assert_eq!(first.evidence_counts, [2, 0]);
        assert_eq!(first.first_seen, [100, 300]);
        assert_eq!(first.mention_counts, [2, 2]);
        assert_eq!(found[1].evidence_counts, [0, 1]);
    }

    #[test]
    fn two_user_edited_rows_and_rows_classed_as_nobody_are_not_offered() {
        let conn = crate::open_in_memory().unwrap();
        add(&conn, "people", "Alex", None, 100, true);
        add(&conn, "people", "Alex Morgan", None, 200, true);
        let kim = add(&conn, "people", "Kim", None, 100, false);
        let kim_l = add(&conn, "people", "Kim Lee", None, 200, false);
        conn.execute("UPDATE people SET person_class = 'not_a_person' WHERE id = ?1", [kim_l]).unwrap();
        let found = candidates(&conn, StateTable::People).unwrap();
        assert!(found.is_empty(), "{found:?}");
        conn.execute("UPDATE people SET person_class = 'contact' WHERE id = ?1", [kim_l]).unwrap();
        assert_eq!(ids(&candidates(&conn, StateTable::People).unwrap()), vec![[kim, kim_l]]);
    }

    #[test]
    fn keep_separate_removes_the_pair_and_blocks_a_later_merge() {
        let mut conn = crate::open_in_memory().unwrap();
        let a = add(&conn, "projects", "Atlas", None, 100, false);
        let b = add(&conn, "projects", "Atlas Mobile", None, 200, false);
        assert_eq!(ids(&candidates(&conn, StateTable::Projects).unwrap()), vec![[a, b]]);

        let req = parse_keep_separate(&format!(r#"{{"table":"projects","id":{b},"other_id":{a}}}"#)).unwrap();
        assert_eq!(keep_separate(&conn, &req, 500).unwrap(), Some(()));
        // Idempotent: the UNIQUE row is ignored, not an error.
        assert_eq!(keep_separate(&conn, &req, 600).unwrap(), Some(()));
        assert!(candidates(&conn, StateTable::Projects).unwrap().is_empty());

        let merge_req = parse_merge(&format!(r#"{{"table":"projects","ids":[{a},{b}]}}"#)).unwrap();
        assert!(matches!(merge(&mut conn, &merge_req, 700), Err(EditError::Invalid(_))));
        assert_eq!(crate::entity_dedupe::dedupe_exact(&mut conn, 800).unwrap(), 0);
        assert!(crate::state_split::is_merge_blocked(&conn, StateTable::Projects, a, b).unwrap());
    }

    #[test]
    fn merge_folds_into_the_older_row_and_moves_evidence() {
        let mut conn = crate::open_in_memory().unwrap();
        let alex = add(&conn, "people", "Alex", None, 100, false);
        let alex_m = add(&conn, "people", "Alex Morgan", Some(r#"["alexm"]"#), 300, false);
        link(&conn, "people", alex, 1);
        link(&conn, "people", alex_m, 2);

        let req = parse_merge(&format!(r#"{{"table":"people","ids":[{alex_m},{alex},{alex}]}}"#)).unwrap();
        assert_eq!(req.ids, vec![alex, alex_m]);
        let outcome = merge(&mut conn, &req, 900).unwrap().unwrap();
        assert_eq!(outcome, MergeOutcome { survivor_id: alex, absorbed: 1 });

        let (name, aliases): (String, Option<String>) = conn
            .query_row("SELECT display_name, aliases FROM people WHERE id = ?1", [alex], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!(name, "Alex Morgan");
        assert!(aliases.unwrap_or_default().contains("alexm"));
        let links: i64 = conn
            .query_row("SELECT COUNT(*) FROM state_provenance WHERE state_table = 'people' AND state_id = ?1", [alex], |r| r.get(0))
            .unwrap();
        assert_eq!(links, 2);
        assert!(!exists(&conn, StateTable::People, alex_m).unwrap());
        assert!(candidates(&conn, StateTable::People).unwrap().is_empty());
    }

    #[test]
    fn a_missing_row_is_none_and_bad_bodies_are_invalid() {
        let mut conn = crate::open_in_memory().unwrap();
        let alex = add(&conn, "people", "Alex", None, 100, false);
        let req = parse_merge(&format!(r#"{{"table":"people","ids":[{alex},9999]}}"#)).unwrap();
        assert_eq!(merge(&mut conn, &req, 1).unwrap(), None);
        let req = parse_keep_separate(&format!(r#"{{"table":"people","id":{alex},"other_id":9999}}"#)).unwrap();
        assert_eq!(keep_separate(&conn, &req, 1).unwrap(), None);

        for body in [
            r#"{"table":"commitments","ids":[1,2]}"#,
            r#"{"table":"people","ids":[1]}"#,
            r#"{"table":"people","ids":[1,1]}"#,
            r#"{"table":"people","ids":[1,0]}"#,
            r#"{"table":"people"}"#,
            "not json",
        ] {
            assert!(matches!(parse_merge(body), Err(EditError::Invalid(_))), "{body}");
        }
        for body in [
            r#"{"table":"people","id":1,"other_id":1}"#,
            r#"{"table":"people","id":1}"#,
            r#"{"table":"open_loops","id":1,"other_id":2}"#,
        ] {
            assert!(matches!(parse_keep_separate(body), Err(EditError::Invalid(_))), "{body}");
        }
    }

    #[test]
    fn two_edited_rows_are_refused_not_merged() {
        let mut conn = crate::open_in_memory().unwrap();
        let a = add(&conn, "people", "Alex", None, 100, true);
        let b = add(&conn, "people", "Alex Morgan", None, 200, true);
        let req = parse_merge(&format!(r#"{{"table":"people","ids":[{a},{b}]}}"#)).unwrap();
        assert!(matches!(merge(&mut conn, &req, 1), Err(EditError::Invalid(_))));
        assert!(exists(&conn, StateTable::People, a).unwrap() && exists(&conn, StateTable::People, b).unwrap());
    }
}
