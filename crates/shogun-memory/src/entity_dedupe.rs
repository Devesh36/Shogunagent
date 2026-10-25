//! One row per real person or project.
//!
//! Three things live here:
//!
//! * [`roster`] — the compact list of people and projects the hourly review is shown, so the model
//!   can map a mention to an existing row (`existing_id`) instead of creating a twin.
//! * [`dedupe_exact`] — a deterministic merge of rows that share a normalised name or alias
//!   ("mikel" / "Mikel", "Toru" / "toru" with that alias). No model.
//! * [`apply_groups`] — merges the groups a model call named ("which of these are the same
//!   person?"), after a lexical sanity check.
//!
//! [`merge`] is the one merge: a single transaction that moves provenance and links onto the
//! survivor, unions aliases, keeps the most recent description and the fullest proper-cased name.
//! The survivor is the row the user edited, else the owner's own row, else the oldest. Two user-edited rows are never merged,
//! and a pair the user split apart (`state_merge_blocks`) is never fused. Confidence is never raised.

use std::collections::{HashMap, HashSet};

use rusqlite::{params, Connection};
use serde::Deserialize;

use crate::entity_names as names;
use crate::state::StateTable;

struct Cols {
    table: &'static str,
    name: &'static str,
    desc: &'static str,
    /// SQL: the row is the owner's own (`person_class = 'self'`); never for projects.
    is_self: &'static str,
}

fn cols(table: StateTable) -> Option<Cols> {
    match table {
        StateTable::People => Some(Cols {
            table: "people",
            name: "display_name",
            desc: "relationship_summary",
            is_self: "COALESCE(person_class, '') = 'self'",
        }),
        StateTable::Projects => Some(Cols { table: "projects", name: "name", desc: "summary", is_self: "0" }),
        _ => None,
    }
}

#[derive(Clone)]
struct Row {
    id: i64,
    name: String,
    aliases: Vec<String>,
    desc: String,
    locked: bool,
    updated_at: i64,
    is_self: bool,
}

impl Row {
    /// Every normalised spelling this row answers to.
    fn keys(&self) -> HashSet<String> {
        std::iter::once(&self.name).chain(self.aliases.iter()).map(|n| names::key(n)).filter(|k| !k.is_empty()).collect()
    }
}

fn load(conn: &Connection, c: &Cols) -> Result<Vec<Row>, rusqlite::Error> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id, {name}, aliases, COALESCE({desc}, ''), {locked}, updated_at, {is_self} FROM {t} ORDER BY id",
        name = c.name,
        desc = c.desc,
        is_self = c.is_self,
        locked = names::LOCKED_SQL,
        t = c.table
    ))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Row {
                id: r.get(0)?,
                name: r.get(1)?,
                aliases: names::parse_aliases(r.get::<_, Option<String>>(2)?.as_deref()),
                desc: r.get(3)?,
                locked: r.get(4)?,
                updated_at: r.get(5)?,
                is_self: r.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Fold the rows `ids` into one. The survivor is the user-edited row if there is one, else the
/// oldest; `canonical` (the model's preferred spelling) is used as the display name only if it is
/// one of the rows' own names or aliases. Returns how many rows were absorbed. Two user-edited
/// rows, or rows the user split apart, are left alone.
pub fn merge(
    conn: &mut Connection,
    table: StateTable,
    ids: &[i64],
    canonical: Option<&str>,
    now: i64,
) -> Result<usize, rusqlite::Error> {
    let Some(c) = cols(table) else { return Ok(0) };
    let all = load(conn, &c)?;
    let mut members: Vec<Row> = Vec::new();
    for id in ids {
        if let Some(row) = all.iter().find(|r| r.id == *id) {
            if !members.iter().any(|m| m.id == row.id) {
                members.push(row.clone());
            }
        }
    }
    if members.len() < 2 || members.iter().filter(|m| m.locked).count() > 1 {
        return Ok(0);
    }
    // Any pair the user split apart blocks the whole group: merging A and C, then C and B, would
    // fuse A and B again through C.
    let ids: Vec<i64> = members.iter().map(|m| m.id).collect();
    if crate::memory_identity::any_pair_blocked(conn, table, &ids)? {
        return Ok(0);
    }
    // The owner's own row survives too: a person row folded into it must not take it over.
    members.sort_by_key(|m| (!m.locked, !m.is_self, m.id));
    let survivor = members[0].clone();
    let absorbed: Vec<Row> = members[1..].to_vec();
    let group: Vec<&Row> = std::iter::once(&survivor).chain(absorbed.iter()).collect();

    // Display name: a person's grows by extension (`names::better_name`); a project, or a row the
    // user edited, keeps the survivor's own (the others' names become aliases below).
    let display = if survivor.locked {
        survivor.name.clone()
    } else if table == StateTable::Projects {
        // Never renamed: the first real name in the group (a repository path or a work item never
        // names the project when a real name is there), proper-cased if a member spells it so.
        let real = |n: &&str| crate::entity_pages::repo_path_project(n).is_none() && !crate::entity_pages::looks_like_artifact(n);
        let name = group.iter().map(|m| m.name.as_str()).find(real).unwrap_or(&survivor.name);
        group.iter().map(|m| m.name.as_str()).find(|n| names::better_case(name, n)).unwrap_or(name).to_string()
    } else {
        let mut spellings: Vec<&str> = absorbed.iter().map(|m| m.name.as_str()).collect();
        let canonical = canonical.map(str::trim).filter(|n| {
            let k = names::key(n);
            group.iter().any(|m| m.keys().contains(&k))
        });
        spellings.extend(canonical);
        names::fullest(&survivor.name, spellings)
    };
    // A row the user edited keeps its own aliases, emails and handles: one they removed must not
    // come back with a merge.
    let aliases = if survivor.locked {
        names::union(survivor.aliases.iter().cloned(), &names::key(&display))
    } else {
        names::union(
            group.iter().flat_map(|m| std::iter::once(m.name.clone()).chain(m.aliases.iter().cloned())),
            &names::key(&display),
        )
    };
    let aliases_json = if aliases.is_empty() { None } else { serde_json::to_string(&aliases).ok() };
    // Description: the most recently updated non-empty one; the user's own wording stays.
    let description = if survivor.locked {
        None
    } else {
        group
            .iter()
            .filter(|m| !m.desc.trim().is_empty())
            .max_by_key(|m| (m.updated_at, m.desc.len()))
            .map(|m| m.desc.clone())
    };

    let people = table == StateTable::People;
    let mut emails: Vec<String> = Vec::new();
    let mut handles: Vec<String> = Vec::new();
    let mut channel: Option<String> = None;
    let mut last_interaction: Option<i64> = None;
    let mut created = i64::MAX;
    let mut evidence = 0_i64;
    for m in &group {
        let (c_at, e_at): (i64, i64) = conn.query_row(
            &format!("SELECT created_at, last_evidence_at FROM {} WHERE id = ?1", c.table),
            [m.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        created = created.min(c_at);
        evidence = evidence.max(e_at);
        if people {
            let (em, ha, ch, li): (Option<String>, Option<String>, Option<String>, Option<i64>) = conn.query_row(
                "SELECT emails, handles, interaction_channel, last_interaction_at FROM people WHERE id = ?1",
                [m.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
            if !survivor.locked || m.id == survivor.id {
                emails.extend(names::parse_aliases(em.as_deref()));
                handles.extend(names::parse_aliases(ha.as_deref()));
            }
            channel = channel.or(ch);
            last_interaction = last_interaction.max(li);
        }
    }

    let tx = conn.savepoint()?;
    for a in &absorbed {
        // Where both rows cite one event, the survivor's link keeps the better evidence: a quote
        // over none, the Batch-review mark over none.
        tx.execute(
            "UPDATE state_provenance AS s
                SET quote = CASE WHEN a.batch_reviewed > s.batch_reviewed AND a.quote IS NOT NULL THEN a.quote
                                 ELSE COALESCE(s.quote, a.quote) END,
                    batch_reviewed = MAX(s.batch_reviewed, a.batch_reviewed)
               FROM state_provenance AS a
              WHERE s.state_table = ?2 AND s.state_id = ?1 AND a.state_table = ?2 AND a.state_id = ?3
                AND a.event_id = s.event_id
                AND (a.batch_reviewed > s.batch_reviewed OR (s.quote IS NULL AND a.quote IS NOT NULL))",
            params![survivor.id, c.table, a.id],
        )?;
        // Provenance union: the absorbed row's links (with quotes and the Batch-review mark) move
        // unless the survivor already cites that event; the rest go with the row.
        tx.execute(
            "UPDATE state_provenance SET state_id = ?1
              WHERE state_table = ?2 AND state_id = ?3
                AND event_id NOT IN (SELECT event_id FROM state_provenance WHERE state_table = ?2 AND state_id = ?1)",
            params![survivor.id, c.table, a.id],
        )?;
        if people {
            for t in ["commitments", "open_loops"] {
                tx.execute(&format!("UPDATE {t} SET counterparty_id = ?1 WHERE counterparty_id = ?2"), params![survivor.id, a.id])?;
            }
        } else {
            for t in ["commitments", "open_loops", "threads"] {
                tx.execute(&format!("UPDATE {t} SET project_id = ?1 WHERE project_id = ?2"), params![survivor.id, a.id])?;
            }
        }
        crate::state_edit::remove_state_row(&tx, table, a.id)?;
    }
    if people {
        let emails = names::union(emails, "");
        let handles = names::union(handles, "");
        let json = |v: &[String]| if v.is_empty() { None } else { serde_json::to_string(v).ok() };
        tx.execute(
            "UPDATE people SET display_name = ?2, aliases = ?3, relationship_summary = COALESCE(?4, relationship_summary),
                    emails = COALESCE(?5, emails), handles = COALESCE(?6, handles),
                    interaction_channel = COALESCE(interaction_channel, ?7),
                    last_interaction_at = CASE WHEN ?8 IS NULL THEN last_interaction_at
                                               WHEN last_interaction_at IS NULL THEN ?8
                                               ELSE MAX(last_interaction_at, ?8) END,
                    created_at = MIN(created_at, ?9), last_evidence_at = MAX(last_evidence_at, ?10),
                    updated_at = MAX(updated_at + 1, ?11)
              WHERE id = ?1",
            params![
                survivor.id, display, aliases_json, description, json(emails.as_slice()), json(handles.as_slice()), channel,
                last_interaction, created, evidence, now
            ],
        )?;
    } else {
        tx.execute(
            "UPDATE projects SET name = ?2, aliases = ?3, summary = COALESCE(?4, summary),
                    created_at = MIN(created_at, ?5), last_evidence_at = MAX(last_evidence_at, ?6),
                    updated_at = MAX(updated_at + 1, ?7)
              WHERE id = ?1",
            params![survivor.id, display, aliases_json, description, created, evidence, now],
        )?;
    }
    // The survivor's hours now include the absorbed rows' links; its page is rebuilt from them.
    crate::entity_pages::recount(&tx, table, survivor.id)?;
    tx.commit()?;
    for m in &group {
        let _ = crate::search_documents::project_source_after_write(conn, &crate::state_edit::source_ref(table, m.id), now);
    }
    Ok(absorbed.len())
}

/// Merge every set of rows that share a normalised name, or whose name is another's alias.
/// Idempotent; returns how many rows were absorbed.
pub fn dedupe_exact(conn: &mut Connection, now: i64) -> Result<usize, rusqlite::Error> {
    let mut total = 0;
    for table in [StateTable::People, StateTable::Projects] {
        let Some(c) = cols(table) else { continue };
        let rows = load(conn, &c)?;
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, r) in rows.iter().enumerate() {
            let k = names::key(&r.name);
            if !k.is_empty() {
                by_name.entry(k).or_default().push(i);
            }
        }
        let mut parent: Vec<usize> = (0..rows.len()).collect();
        fn find(parent: &mut [usize], mut i: usize) -> usize {
            while parent[i] != i {
                parent[i] = parent[parent[i]];
                i = parent[i];
            }
            i
        }
        for (i, r) in rows.iter().enumerate() {
            // A name joins rows with the same name; an alias only when it is a full name ("Toru
            // Tano"), never a bare first name that two different people can share.
            let own = names::key(&r.name);
            for k in r.keys().into_iter().filter(|k| *k == own || k.contains(' ')) {
                for &j in by_name.get(&k).map(Vec::as_slice).unwrap_or_default() {
                    let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                    if a != b {
                        parent[b] = a;
                    }
                }
            }
            // A repository path ("owner/repo") folds into the one project its repo part names.
            let repo = (table == StateTable::Projects)
                .then(|| crate::entity_pages::repo_path_project(&r.name))
                .flatten()
                .map(names::key);
            if let Some(repo) = repo {
                let named: Vec<usize> = (0..rows.len()).filter(|&j| j != i && rows[j].keys().contains(&repo)).collect();
                if let [j] = named[..] {
                    let (a, b) = (find(&mut parent, j), find(&mut parent, i));
                    if a != b {
                        parent[b] = a;
                    }
                }
            }
        }
        let mut groups: HashMap<usize, Vec<i64>> = HashMap::new();
        for (i, r) in rows.iter().enumerate() {
            let root = find(&mut parent, i);
            groups.entry(root).or_default().push(r.id);
        }
        let mut groups: Vec<Vec<i64>> = groups.into_values().filter(|g| g.len() > 1).collect();
        groups.sort();
        for ids in groups {
            total += merge(conn, table, &ids, None, now)?;
        }
    }
    Ok(total)
}

#[derive(Deserialize)]
struct WireGroups {
    #[serde(default)]
    groups: Vec<WireGroup>,
}

#[derive(Deserialize)]
struct WireGroup {
    #[serde(default)]
    ids: Vec<i64>,
    #[serde(default)]
    canonical_name: Option<String>,
}

/// Two rows are lexically related ([`names::related`]): the sanity check on a model group.
fn related(a: &Row, b: &Row) -> bool {
    names::related(&a.keys(), &b.keys())
}

/// Apply the groups a model named (`{"groups":[{"ids":[..],"canonical_name":".."}]}`). Each group
/// is split into its lexically connected parts first, so a wrong id cannot drag an unrelated row in.
/// Returns how many rows were absorbed. Unparseable input applies nothing.
pub fn apply_groups(conn: &mut Connection, table: StateTable, json: &str, now: i64) -> Result<usize, rusqlite::Error> {
    let Some(c) = cols(table) else { return Ok(0) };
    let Ok(wire) = serde_json::from_str::<WireGroups>(json) else { return Ok(0) };
    let mut total = 0;
    for group in wire.groups {
        // Earlier merges in this loop may have removed rows; read fresh each time.
        let rows = load(conn, &c)?;
        let members: Vec<Row> = group
            .ids
            .iter()
            .filter_map(|id| rows.iter().find(|r| r.id == *id).cloned())
            .fold(Vec::new(), |mut acc: Vec<Row>, r| {
                if !acc.iter().any(|m| m.id == r.id) {
                    acc.push(r);
                }
                acc
            });
        let mut seen = vec![false; members.len()];
        for start in 0..members.len() {
            if seen[start] {
                continue;
            }
            let mut part = vec![start];
            seen[start] = true;
            let mut i = 0;
            while i < part.len() {
                for j in 0..members.len() {
                    if !seen[j] && related(&members[part[i]], &members[j]) {
                        seen[j] = true;
                        part.push(j);
                    }
                }
                i += 1;
            }
            if part.len() > 1 {
                let ids: Vec<i64> = part.iter().map(|&k| members[k].id).collect();
                total += merge(conn, table, &ids, group.canonical_name.as_deref(), now)?;
            }
        }
    }
    Ok(total)
}

fn one_line(s: &str, max_chars: usize) -> String {
    let flat: String = s.chars().map(|c| if c.is_control() || c == '|' { ' ' } else { c }).collect();
    flat.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(max_chars).collect()
}

/// One `id | name | also known as | about` line per row, stopping before `max_bytes`. `about_chars`
/// says how much of a row's description to show.
fn roster_lines(rows: &[&Row], about_chars: &dyn Fn(&Row) -> usize, max_bytes: usize) -> String {
    let mut out = String::new();
    for r in rows {
        let line = format!(
            "{} | {} | {} | {}",
            r.id,
            one_line(&r.name, 80),
            r.aliases.iter().take(6).map(|a| one_line(a, 40)).collect::<Vec<_>>().join(", "),
            one_line(&r.desc, about_chars(r))
        );
        if out.len() + line.len() + 1 > max_bytes {
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&line);
    }
    out
}

/// The roster rides in the shared `system` half of every request: keep it well under the relay's limit.
const ROSTER_MAX_BYTES: usize = 24_000;

/// How much of a description the review sees: all of it for an entry the window mentions (the model
/// rewrites it, so it must read what it is rewriting), a glance for the rest.
const ABOUT_MENTIONED: usize = 300;
const ABOUT_GLANCE: usize = 80;

const ROSTER_PEOPLE: usize = 100;
const ROSTER_PROJECTS: usize = 40;

/// Does the window text (already [`names::key`]-normalised) mention this row by name or alias?
fn mentioned(r: &Row, text_key: &str) -> bool {
    r.keys().iter().any(|k| k.chars().count() >= names::min_part_chars(k) && text_key.contains(k.as_str()))
}

/// Rows to show: those the window mentions (by name or alias), then the most recently updated.
fn pick<'a>(rows: &'a [Row], text_key: &str, cap: usize) -> Vec<&'a Row> {
    let mut by_recent: Vec<&Row> = rows.iter().collect();
    by_recent.sort_by_key(|r| std::cmp::Reverse((r.updated_at, r.id)));
    let mut out: Vec<&Row> = by_recent.iter().copied().filter(|r| mentioned(r, text_key)).take(cap).collect();
    for r in by_recent {
        if out.len() >= cap {
            break;
        }
        if !out.iter().any(|o| o.id == r.id) {
            out.push(r);
        }
    }
    out.sort_by_key(|r| r.id);
    out
}

/// The people and projects shown to the hourly review, as the text appended to its instruction:
/// `id | name | also known as | about`. Empty when there is nothing yet. `window` is the text of
/// the events about to be reviewed; entries it mentions come first.
pub fn roster<'a>(conn: &Connection, window: impl IntoIterator<Item = &'a str>) -> Result<String, rusqlite::Error> {
    let mut text_key = String::new();
    for text in window {
        if text_key.len() > 4_000_000 {
            break;
        }
        text_key.push_str(&names::key(text));
        text_key.push('\n');
    }
    let mut out = String::new();
    for (table, cap, title) in [
        (StateTable::People, ROSTER_PEOPLE, "People already known"),
        (StateTable::Projects, ROSTER_PROJECTS, "Projects already known"),
    ] {
        let Some(c) = cols(table) else { continue };
        // The owner's own row is not someone a mention can be mapped to.
        let rows: Vec<Row> = load(conn, &c)?.into_iter().filter(|r| !r.is_self).collect();
        if rows.is_empty() {
            continue;
        }
        let picked = pick(&rows, &text_key, cap);
        out.push_str(&format!("{title} (id | name | also known as | about):\n{}\n\n", roster_lines(
            &picked,
            &|r| if mentioned(r, &text_key) { ABOUT_MENTIONED } else { ABOUT_GLANCE },
            ROSTER_MAX_BYTES,
        )));
    }
    Ok(out.trim_end().to_string())
}

/// Rows of `table` as the dedupe prompt's data (`id | name | also known as | about`, one per line),
/// with how many there are; `None` when fewer than three exist (nothing worth asking about).
pub fn dedupe_input(conn: &Connection, table: StateTable) -> Result<Option<String>, rusqlite::Error> {
    let Some(c) = cols(table) else { return Ok(None) };
    let mut rows = load(conn, &c)?;
    if rows.len() < 3 {
        return Ok(None);
    }
    rows.sort_by_key(|r| std::cmp::Reverse((r.updated_at, r.id)));
    rows.truncate(200);
    rows.sort_by_key(|r| r.id);
    let refs: Vec<&Row> = rows.iter().collect();
    Ok(Some(roster_lines(&refs, &|_| 160, 40_000)))
}

/// Cheap change detector for the people and projects tables: row counts and the newest id. New
/// rows are what can be duplicates; a new link on an old row cannot. The dedupe call is only worth
/// making again when this moved.
pub fn fingerprint(conn: &Connection) -> Result<u64, rusqlite::Error> {
    let mut h: u64 = 1469598103934665603;
    for t in ["people", "projects"] {
        let (n, newest): (i64, i64) =
            conn.query_row(&format!("SELECT COUNT(*), COALESCE(MAX(id), 0) FROM {t}"), [], |r| Ok((r.get(0)?, r.get(1)?)))?;
        for v in [n, newest] {
            h = (h ^ v as u64).wrapping_mul(1099511628211);
        }
    }
    Ok(h)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn add(conn: &Connection, table: &str, name: &str, aliases: Option<&str>, desc: Option<&str>, updated: i64, locked: bool) -> i64 {
        let (name_col, desc_col) = if table == "people" { ("display_name", "relationship_summary") } else { ("name", "summary") };
        let extra = if table == "projects" { "status," } else { "" };
        let extra_v = if table == "projects" { "'active'," } else { "" };
        conn.execute(
            &format!(
                "INSERT INTO {table}({name_col},aliases,{desc_col},{extra}confidence,base_confidence,created_at,updated_at,last_evidence_at,user_pinned)
                 VALUES(?1,?2,?3,{extra_v}0.6,0.6,?4,?4,?4,?5)"
            ),
            params![name, aliases, desc, updated, i64::from(locked)],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn event(conn: &Connection, n: i64) -> i64 {
        crate::event_log::insert(
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
        .unwrap()
    }

    fn link(conn: &Connection, table: &str, id: i64, event: i64) {
        conn.execute("INSERT INTO state_provenance(state_table,state_id,event_id,weight) VALUES(?1,?2,?3,1.0)", params![table, id, event]).unwrap();
    }

    fn names_of(conn: &Connection, table: &str) -> Vec<String> {
        let col = if table == "people" { "display_name" } else { "name" };
        conn.prepare(&format!("SELECT {col} FROM {table} ORDER BY id")).unwrap().query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap()
    }

    #[test]
    fn case_variants_merge_into_the_oldest_with_the_proper_cased_name_and_both_sources() {
        let mut conn = crate::open_in_memory().unwrap();
        let (e1, e2, e3) = (event(&conn, 1), event(&conn, 2), event(&conn, 3));
        let a = add(&conn, "people", "mikel", None, Some("Old note."), 10, false);
        let b = add(&conn, "people", "Mikel", None, Some("Cofounder; you ship the notch together."), 20, false);
        link(&conn, "people", a, e1);
        link(&conn, "people", a, e2);
        link(&conn, "people", b, e2);
        link(&conn, "people", b, e3);
        conn.execute("INSERT INTO commitments(direction,counterparty_id,description,status,confidence,created_at,updated_at,last_evidence_at) VALUES('mine',?1,'Send the deck','open',0.6,1,1,1)", [b]).unwrap();
        assert_eq!(dedupe_exact(&mut conn, 100).unwrap(), 1);
        assert_eq!(names_of(&conn, "people"), ["Mikel"]);
        let (id, desc, aliases): (i64, String, Option<String>) = conn
            .query_row("SELECT id, relationship_summary, aliases FROM people", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        assert_eq!(id, a, "the oldest row survives");
        assert_eq!(desc, "Cofounder; you ship the notch together.", "the most recent description wins");
        assert!(names::parse_aliases(aliases.as_deref()).is_empty(), "the old spelling is the same name once normalised: no alias needed");
        let events: i64 = conn.query_row("SELECT COUNT(DISTINCT event_id) FROM state_provenance WHERE state_table='people' AND state_id=?1", [a], |r| r.get(0)).unwrap();
        assert_eq!(events, 3, "provenance is the union, an event cited by both counts once");
        let cp: i64 = conn.query_row("SELECT counterparty_id FROM commitments", [], |r| r.get(0)).unwrap();
        assert_eq!(cp, a, "links follow the survivor");
        assert_eq!(dedupe_exact(&mut conn, 200).unwrap(), 0, "idempotent");
    }

    #[test]
    fn a_full_name_alias_joins_rows_but_a_shared_first_name_never_does() {
        let mut conn = crate::open_in_memory().unwrap();
        add(&conn, "people", "Torutesu", Some(r#"["Toru Tano"]"#), None, 10, false);
        add(&conn, "people", "toru tano", Some(r#"["Toru"]"#), None, 11, false);
        add(&conn, "people", "Toru Sato", Some(r#"["Toru"]"#), None, 12, false);
        add(&conn, "people", "Toru", None, None, 13, true);
        add(&conn, "people", "Dana Reyes", None, None, 14, false);
        // "Torutesu" lists the full name "Toru Tano": one person. The bare first name "Toru" that
        // Toru Tano, Toru Sato and the pinned "Toru" share fuses nobody.
        assert_eq!(dedupe_exact(&mut conn, 100).unwrap(), 1);
        assert_eq!(names_of(&conn, "people"), ["Torutesu", "Toru Sato", "Toru", "Dana Reyes"], "a handle is not extended by a full name: kept, the name an alias");
    }

    #[test]
    fn a_split_pair_blocks_the_whole_group_even_through_a_third_row() {
        let mut conn = crate::open_in_memory().unwrap();
        add(&conn, "people", "ALEX", None, None, 1, false);
        let a = add(&conn, "people", "Alex", None, None, 2, false);
        let b = add(&conn, "people", "alex", None, None, 3, false);
        conn.execute("INSERT INTO state_merge_blocks(state_table,kept_id,split_id,created_at) VALUES('people',?1,?2,1000)", params![a, b]).unwrap();
        assert_eq!(dedupe_exact(&mut conn, 100).unwrap(), 0);
        assert_eq!(names_of(&conn, "people").len(), 3);
    }

    #[test]
    fn a_locked_survivor_keeps_its_own_aliases_emails_and_handles() {
        let mut conn = crate::open_in_memory().unwrap();
        let mine = add(&conn, "people", "Dana", None, None, 1, true);
        let other = add(&conn, "people", "dana", Some(r#"["Dana R"]"#), None, 2, false);
        conn.execute("UPDATE people SET emails='[\"dana@old.example\"]', handles='[\"dr\"]' WHERE id=?1", [other]).unwrap();
        assert_eq!(merge(&mut conn, StateTable::People, &[mine, other], None, 100).unwrap(), 1);
        let (aliases, emails, handles): (Option<String>, Option<String>, Option<String>) = conn
            .query_row("SELECT aliases, emails, handles FROM people WHERE id=?1", [mine], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        assert_eq!((aliases, emails, handles), (None, None, None), "what the user removed does not come back");
    }

    #[test]
    fn the_survivor_keeps_the_better_link_to_an_event_both_rows_cite() {
        let mut conn = crate::open_in_memory().unwrap();
        let e = event(&conn, 1);
        let a = add(&conn, "people", "mikel", None, None, 1, false);
        let b = add(&conn, "people", "Mikel", None, None, 2, false);
        link(&conn, "people", a, e);
        conn.execute("INSERT INTO state_provenance(state_table,state_id,event_id,weight,quote,batch_reviewed) VALUES('people',?1,?2,1.0,'event 1',1)", params![b, e]).unwrap();
        assert_eq!(dedupe_exact(&mut conn, 100).unwrap(), 1);
        let (quote, reviewed): (Option<String>, i64) = conn
            .query_row("SELECT quote, batch_reviewed FROM state_provenance WHERE state_table='people' AND state_id=?1", [a], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!((quote.as_deref(), reviewed), (Some("event 1"), 1));
    }

    #[test]
    fn two_character_cjk_names_are_related_and_mentioned() {
        let mut conn = crate::open_in_memory().unwrap();
        let full = add(&conn, "people", "山田 太郎", None, None, 1, false);
        let family = add(&conn, "people", "山田", None, None, 2, false);
        let other = add(&conn, "people", "佐藤 花子", None, None, 3, false);
        let rows = load(&conn, &cols(StateTable::People).unwrap()).unwrap();
        let bare = rows.iter().find(|r| r.id == family).unwrap();
        assert!(mentioned(bare, &names::key("山田さんから連絡")), "a two-character name is mentioned");
        let json = format!(r#"{{"groups":[{{"ids":[{full},{family},{other}]}}]}}"#);
        assert_eq!(apply_groups(&mut conn, StateTable::People, &json, 100).unwrap(), 1);
        assert_eq!(names_of(&conn, "people"), ["山田 太郎", "佐藤 花子"]);
    }

    #[test]
    fn the_user_edited_row_survives_and_two_user_edited_rows_never_merge() {
        let mut conn = crate::open_in_memory().unwrap();
        let old = add(&conn, "people", "dana", None, Some("Auto."), 10, false);
        let mine = add(&conn, "people", "Dana", None, Some("My wording."), 5, true);
        assert_eq!(merge(&mut conn, StateTable::People, &[old, mine], Some("Dana Reyes"), 100).unwrap(), 1);
        let (id, name, desc): (i64, String, String) = conn
            .query_row("SELECT id, display_name, relationship_summary FROM people", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        assert_eq!((id, name.as_str(), desc.as_str()), (mine, "Dana", "My wording."), "locked name and wording untouched");

        let other = add(&conn, "people", "dana", None, None, 11, true);
        assert_eq!(merge(&mut conn, StateTable::People, &[mine, other], None, 100).unwrap(), 0);
        assert_eq!(names_of(&conn, "people").len(), 2);
    }

    #[test]
    fn a_pair_the_user_split_apart_stays_two_rows() {
        let mut conn = crate::open_in_memory().unwrap();
        let a = add(&conn, "people", "Alex", None, None, 1, false);
        let b = add(&conn, "people", "alex", None, None, 1, false);
        conn.execute("INSERT INTO state_merge_blocks(state_table,kept_id,split_id,created_at) VALUES('people',?1,?2,1000)", params![a, b]).unwrap();
        assert_eq!(dedupe_exact(&mut conn, 100).unwrap(), 0);
        assert_eq!(names_of(&conn, "people").len(), 2);
    }

    #[test]
    fn a_repository_path_folds_into_the_project_it_names() {
        let mut conn = crate::open_in_memory().unwrap();
        add(&conn, "projects", "Torutesu/ShogunAI-", None, None, 1, false);
        add(&conn, "projects", "Shogun AI", Some(r#"["ShogunAI"]"#), None, 2, false);
        add(&conn, "projects", "Acme/tools", None, None, 3, false);
        assert_eq!(dedupe_exact(&mut conn, 100).unwrap(), 1);
        assert_eq!(names_of(&conn, "projects"), ["Shogun AI", "Acme/tools"], "the real name, not the older repo path; an unmatched path stays");
    }

    #[test]
    fn projects_merge_the_same_way() {
        let mut conn = crate::open_in_memory().unwrap();
        add(&conn, "projects", "shogunai", None, None, 1, false);
        add(&conn, "projects", "ShogunAI", None, Some("The notch OS."), 2, false);
        add(&conn, "projects", "Kestrel", None, None, 3, false);
        assert_eq!(dedupe_exact(&mut conn, 100).unwrap(), 1);
        assert_eq!(names_of(&conn, "projects"), ["ShogunAI", "Kestrel"]);
    }

    #[test]
    fn model_groups_merge_related_names_and_ignore_unrelated_ids_and_garbage() {
        let mut conn = crate::open_in_memory().unwrap();
        let toru = add(&conn, "people", "toru", None, None, 1, false);
        let tano = add(&conn, "people", "toru tano", None, Some("Cofounder."), 2, false);
        let handle = add(&conn, "people", "Torutesu", None, None, 3, false);
        let dana = add(&conn, "people", "Dana Reyes", None, None, 4, false);
        let json = format!(r#"{{"groups":[{{"ids":[{toru},{tano},{handle},{dana},9999],"canonical_name":"Toru Tano"}}]}}"#);
        assert_eq!(apply_groups(&mut conn, StateTable::People, &json, 100).unwrap(), 2);
        let left = names_of(&conn, "people");
        assert_eq!(left, ["Toru Tano", "Dana Reyes"]);
        let aliases: Option<String> = conn.query_row("SELECT aliases FROM people WHERE id=?1", [toru], |r| r.get(0)).unwrap();
        assert_eq!(names::parse_aliases(aliases.as_deref()), ["toru", "Torutesu"]);
        assert_eq!(apply_groups(&mut conn, StateTable::People, "not json", 100).unwrap(), 0);
        // An unrelated pair named by the model is not fused.
        let maria = add(&conn, "people", "Maria Lopez", None, None, 5, false);
        let json = format!(r#"{{"groups":[{{"ids":[{dana},{maria}],"canonical_name":"Dana Reyes"}}]}}"#);
        assert_eq!(apply_groups(&mut conn, StateTable::People, &json, 100).unwrap(), 0);
    }

    #[test]
    fn roster_lists_mentioned_entries_first_with_aliases_and_a_short_about() {
        let conn = crate::open_in_memory().unwrap();
        let long = "Cofounder at Select KK; you build ShogunAI together over Discord and many long calls about the notch panel design.";
        add(&conn, "people", "Toru Tano", Some(r#"["toru","Torutesu"]"#), Some(long), 1, false);
        add(&conn, "people", "Dana Reyes", None, Some(long), 2, false);
        for n in 0..120 {
            add(&conn, "people", &format!("Person {n}"), None, None, 100 + n, false);
        }
        let text = roster(&conn, ["ping from torutesu about the build"]).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "People already known (id | name | also known as | about):");
        assert!(text.contains(&format!("| Toru Tano | toru, Torutesu | {long}")), "a mentioned entry shows its whole description: {text}");
        if let Some(dana) = text.lines().find(|l| l.contains("| Dana Reyes |")) {
            assert!(dana.ends_with("many long c"), "an entry the window does not mention shows a glance: {dana}");
        }
        let people_lines = lines.iter().filter(|l| l.starts_with(|c: char| c.is_ascii_digit())).count();
        assert_eq!(people_lines, ROSTER_PEOPLE);
        assert_eq!(roster(&crate::open_in_memory().unwrap(), []).unwrap(), "");
    }

    #[test]
    fn dedupe_input_needs_three_rows_and_fingerprint_moves_with_the_data() {
        let conn = crate::open_in_memory().unwrap();
        add(&conn, "people", "A One", None, None, 1, false);
        add(&conn, "people", "B Two", None, None, 2, false);
        assert_eq!(dedupe_input(&conn, StateTable::People).unwrap(), None);
        let before = fingerprint(&conn).unwrap();
        add(&conn, "people", "C Three", None, None, 3, false);
        assert!(dedupe_input(&conn, StateTable::People).unwrap().is_some());
        assert_ne!(fingerprint(&conn).unwrap(), before);
    }
}
