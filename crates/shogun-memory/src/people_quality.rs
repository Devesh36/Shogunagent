//! Quality rules for `people` rows: a readable description, never captured UI text, and never
//! the owner or a non-contact. Pure checks plus one sweep the Dream Cycle runs.
//!
//! A person's `relationship_summary` is written *for the owner* ("Cofounder at X; you coordinate
//! launches over Discord"). The verbatim quote stays in provenance as evidence only.

use std::sync::{OnceLock, RwLock};

use rusqlite::Connection;

use crate::state::StateTable;

const MAX_DESCRIPTION_CHARS: usize = 280;
const MIN_DESCRIPTION_CHARS: usize = 15;
const MAX_CHANNEL_CHARS: usize = 40;

fn owners() -> &'static RwLock<Vec<String>> {
    static OWNERS: OnceLock<RwLock<Vec<String>>> = OnceLock::new();
    OWNERS.get_or_init(|| RwLock::new(Vec::new()))
}

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// The owner's own names (Settings profile name, aliases). A person row for one of these is the
/// owner, not a contact. Replaces the previous set.
pub fn set_owner_names<I: IntoIterator<Item = S>, S: AsRef<str>>(names: I) {
    let list: Vec<String> = names
        .into_iter()
        .map(|n| n.as_ref().split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|n| !n.is_empty())
        .collect();
    if let Ok(mut guard) = owners().write() {
        *guard = list;
    }
}

/// The owner's names as last set, spelled as given (the self row is created from them).
pub fn owner_names() -> Vec<String> {
    owners().read().map(|g| g.clone()).unwrap_or_default()
}

/// Exact (normalised) match on a full name, also written without spaces ("anantsinghal"), or the
/// owner's first name alone ("Anant" for "Anant Singhal"). Deliberately not fuzzy: a different
/// person sharing a surname stays a contact.
pub fn is_owner(name: &str) -> bool {
    let n = norm(name);
    !n.is_empty() && (is_owner_full(&n) || owners().read().is_ok_and(|g| {
        g.iter().map(|o| norm(o)).any(|o| o.split(' ').next().is_some_and(|first| first == n && o.contains(' ')))
    }))
}

/// [`is_owner`] without the first-name rule: one of the owner's own full names.
fn is_owner_full(name: &str) -> bool {
    let n = norm(name);
    let compact = |s: &str| s.replace(' ', "");
    !n.is_empty() && owners().read().is_ok_and(|g| g.iter().map(|o| norm(o)).any(|o| o == n || compact(&o) == compact(&n)))
}

/// Class the owner's own `people` rows `self`, so no list shows them (`entity_pages::visible_sql`):
/// a row named as the owner, or carrying one of the owner's full names as an alias (a first name
/// alone does not count there: another person may share it). Idempotent; returns rows changed.
pub fn mark_owner_rows(conn: &Connection) -> Result<usize, rusqlite::Error> {
    let rows: Vec<(i64, String, Option<String>)> = {
        let mut stmt = conn.prepare("SELECT id, display_name, aliases FROM people WHERE COALESCE(person_class, '') <> 'self'")?;
        let it = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        it.collect::<Result<_, _>>()?
    };
    let mut marked = 0;
    for (id, name, aliases) in rows {
        if is_owner(&name) || crate::entity_names::parse_aliases(aliases.as_deref()).iter().any(|a| is_owner_full(a)) {
            marked += conn.execute("UPDATE people SET person_class = 'self' WHERE id = ?1", [id])?;
        }
    }
    Ok(marked)
}

pub(crate) fn has_clock_time(s: &str) -> bool {
    let b = s.as_bytes();
    (1..b.len().saturating_sub(2)).any(|i| {
        b[i] == b':' && b[i - 1].is_ascii_digit() && b[i + 1].is_ascii_digit() && b[i + 2].is_ascii_digit()
    })
}

/// Text that is screen furniture rather than a sentence about a person: timestamps, handles,
/// quote marks, ellipses, known UI labels, or too short to describe anyone.
pub fn looks_like_chrome(s: &str) -> bool {
    let t = s.trim();
    let n = t.chars().count();
    if n < MIN_DESCRIPTION_CHARS || t.split_whitespace().count() < 3 {
        return true;
    }
    if t.chars().any(chrome_char) || t.contains("...") {
        return true;
    }
    if has_clock_time(t) {
        return true;
    }
    let l = t.to_lowercase();
    const MARKERS: [&str; 14] = [
        "replying to", "verified account", "server tag", "navigation", "marked as", "stopped without",
        " ago", "sign in", "log in", "search for", "suggestion", "notification", "unread", "click ",
    ];
    MARKERS.iter().any(|m| l.contains(m))
}

/// Quote marks, ellipses, separators, a handle's `@`, a non-breaking space: screen furniture.
fn chrome_char(c: char) -> bool {
    matches!(c, '"' | '\u{201c}' | '\u{201d}' | '\u{2026}' | '|' | '\u{2022}' | '\u{203a}' | '@' | '\u{a0}')
}

/// A description fit to store, or `None`. `evidence` are the verbatim spans it must not be a copy of.
/// Stray furniture characters are stripped and the rest judged; UI phrases and clock times
/// still reject the whole description (they mark a captured line, not a sentence).
pub fn clean_description(raw: &str, evidence: &[&str]) -> Option<String> {
    let stripped: String = raw.replace("...", " ").chars().map(|c| if chrome_char(c) { ' ' } else { c }).collect();
    let text = stripped.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() > MAX_DESCRIPTION_CHARS || looks_like_chrome(&text) {
        return None;
    }
    let l = text.to_lowercase();
    if evidence.iter().any(|e| {
        let e = e.trim().to_lowercase();
        e.chars().count() >= 12 && (l.contains(&e) || e.contains(&l))
    }) || evidence.iter().any(|e| crate::sanitize::copies_words(&text, e))
    {
        return None;
    }
    crate::sanitize::persist_generated(&text).is_some_and(|c| c.text == text).then_some(text)
}

/// A short channel word ("Discord", "email"), or `None`.
pub fn clean_channel(raw: &str) -> Option<String> {
    let t = raw.trim();
    (!t.is_empty() && t.chars().count() <= MAX_CHANNEL_CHARS && !looks_like_chrome_channel(t)).then(|| t.to_string())
}

fn looks_like_chrome_channel(t: &str) -> bool {
    t.chars().any(|c| c.is_control() || matches!(c, '"' | '@' | '|')) || has_clock_time(t)
}

/// Dream Cycle sweep: clear chrome-like summaries (people and projects), remove the owner's own row,
/// and prune stale singletons and work-item projects ([`crate::entity_pages::prune`]). Rows the user
/// touched (`user_edited_at`, `wording_edited`, `user_pinned`) and the owner's `self` row (the names they
/// go by, [`crate::self_identity`]) are never selected, nor is a written
/// page (checked claim by claim when stored). Returns `(summaries_cleared, rows_removed)`. A
/// cleared summary is refilled by the next Batch review that sees the person again.
pub fn sweep_people(conn: &mut Connection, now: i64) -> Result<(usize, usize), rusqlite::Error> {
    let rows: Vec<(i64, String, Option<String>, bool)> = {
        let mut stmt = conn.prepare(
            "SELECT id, display_name, relationship_summary, page IS NOT NULL FROM people
              WHERE user_edited_at IS NULL AND wording_edited = 0 AND user_pinned = 0
                AND COALESCE(person_class, '') <> 'self'",
        )?;
        let it = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        it.collect::<Result<_, _>>()?
    };
    let (mut cleared, mut removed) = (0, 0);
    for (id, name, summary, paged) in rows {
        if is_owner(&name) {
            if crate::state_edit::delete_state(conn, StateTable::People, id, now).unwrap_or(false) {
                removed += 1;
            }
        } else if !paged && summary.as_deref().is_some_and(|s| !s.trim().is_empty() && looks_like_chrome(s)) {
            conn.execute("UPDATE people SET relationship_summary = NULL WHERE id = ?1", [id])?;
            let _ = crate::search_documents::project_source_after_write(conn, &crate::search_documents::SourceRef::Person(id), now);
            cleared += 1;
        }
    }
    let projects: Vec<(i64, String)> = {
        let mut stmt = conn.prepare(
            "SELECT id, summary FROM projects WHERE summary IS NOT NULL AND summary <> '' AND page IS NULL
              AND user_edited_at IS NULL AND wording_edited = 0 AND user_pinned = 0",
        )?;
        let it = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        it.collect::<Result<_, _>>()?
    };
    let mut n = 0;
    for (id, summary) in projects {
        if looks_like_chrome(&summary) {
            conn.execute("UPDATE projects SET summary = NULL WHERE id = ?1", [id])?;
            n += 1;
        }
    }
    removed += crate::entity_pages::prune(conn, now)?;
    Ok((cleared + n, removed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_is_rejected_and_prose_kept() {
        for bad in ["Server Tag: Foo, 10:00 PM", "Replying to Dana Reyes", "user navigation menu", "marked as ready for dev",
            "Verified account @dana 9 hours ago", "stopped without replying", "short", "He said \"ship it\" to everyone"] {
            assert!(looks_like_chrome(bad), "{bad}");
        }
        let ok = "Cofounder at Acme; you coordinate launches over Discord";
        assert!(!looks_like_chrome(ok));
        assert_eq!(clean_description(ok, &[]).as_deref(), Some(ok));
        assert!(clean_description(ok, &[ok]).is_none(), "a copy of the evidence is not a description");
        let message = "hey can you send me the final launch deck before friday so we can review it together with the board next week";
        let echoed = "Asked you: can you send me the final launch deck before friday so we can review it";
        assert!(clean_description(echoed, &[message]).is_some(), "a fifteen-word echo is still a description");
        let copied = "Asked you: can you send me the final launch deck before friday so we can review it together with the board next week";
        assert!(clean_description(copied, &[]).is_some());
        assert!(clean_description(copied, &[message]).is_none(), "a long run of the message is a quote");
        assert!(clean_description("Asked you for the final launch deck before Friday", &[message]).is_some());
        // Stray furniture is stripped, not fatal; UI phrases still are.
        assert_eq!(clean_description("Runs \u{201c}Comet\u{201d} with @bob at Acme…", &[]).as_deref(), Some("Runs Comet with bob at Acme"));
        assert!(clean_description("Replying to Dana about \"Comet\"", &[]).is_none());
    }

    /// One test owns the process-wide owner list so parallel tests cannot race it.
    #[test]
    fn owner_filter_persist_and_sweep() -> Result<(), Box<dyn std::error::Error>> {
        use crate::knowledge::{persist, Candidate, Kind, PersonClass};
        set_owner_names(["Pat Quill"]);
        assert!(is_owner("pat  quill") && is_owner("Pat"));
        assert!(!is_owner("Pat Other") && !is_owner("Quill"));

        let mut conn = crate::open_in_memory()?;
        let quote = "Dana Reyes: shipping the Acme launch checklist tonight";
        let event = |hash: &'static str| crate::event_log::NewEvent {
            ts: 1_000, source: "capture", kind: "text", app_bundle_id: Some("app"), window_title: None,
            content: quote, content_hash: hash, dwell_ms: 0, display_id: None, window_bounds: None,
        };
        let ev = crate::event_log::insert(&conn, &event("pq-1"))?;
        let cand = |name: &str, class, desc: Option<&str>| Candidate {
            kind: Kind::Person, name: name.into(), context: "Acme launch".into(), quote: quote.into(),
            description: desc.map(str::to_string), person_class: class, channel: Some("Discord".into()),
            ..Default::default()
        };
        let dana = |conn: &mut Connection, c: Candidate| -> Result<Option<i64>, crate::MemoryError> { persist(conn, ev, &c, 2_000) };
        // Non-people and the owner never become rows.
        assert_eq!(dana(&mut conn, cand("Dana Reyes", Some(PersonClass::NotAPerson), None))?, None);
        assert_eq!(dana(&mut conn, cand("Pat Quill", Some(PersonClass::Contact), None))?, None);
        // A contact stores the clean description, not the quote; chrome falls back to empty.
        let id = dana(&mut conn, cand("Dana Reyes", Some(PersonClass::Contact), Some("Replying to Dana, 10:00 PM")))?.ok_or("contact")?;
        let summary = |conn: &Connection| -> Result<Option<String>, rusqlite::Error> {
            conn.query_row("SELECT relationship_summary FROM people WHERE id=?1", [id], |r| r.get(0))
        };
        assert_eq!(summary(&conn)?, None);
        let good = "Runs the Acme launch with you; you coordinate over Discord";
        persist(&mut conn, ev, &cand("Dana Reyes", Some(PersonClass::Contact), Some(good)), 3_000)?;
        assert_eq!(summary(&conn)?.as_deref(), Some(good));
        let channel: Option<String> = conn.query_row("SELECT interaction_channel FROM people WHERE id=?1", [id], |r| r.get(0))?;
        assert_eq!(channel.as_deref(), Some("Discord"));
        // A user-edited row is never refreshed or swept.
        conn.execute("UPDATE people SET wording_edited=1, relationship_summary='x y z' WHERE id=?1", [id])?;
        persist(&mut conn, ev, &cand("Dana Reyes", Some(PersonClass::Contact), Some("Someone different entirely here")), 4_000)?;
        assert_eq!(summary(&conn)?.as_deref(), Some("x y z"));
        assert_eq!(sweep_people(&mut conn, 5_000)?, (0, 0));
        // Sweep: chrome summary cleared, owner row removed, edited row untouched.
        conn.execute("UPDATE people SET wording_edited=0 WHERE id=?1", [id])?;
        conn.execute("UPDATE people SET relationship_summary='Server Tag: Foo, 10:00 PM' WHERE id=?1", [id])?;
        conn.execute(
            "INSERT INTO people(display_name,confidence,base_confidence,created_at,updated_at,last_evidence_at) VALUES('Pat Quill',0.6,0.6,1,1,1)",
            [],
        )?;
        assert_eq!(sweep_people(&mut conn, 6_000)?, (1, 1));
        assert_eq!(summary(&conn)?, None);
        // The owner's row under a spelling the review kept (an alias without spaces) is classed
        // `self` and leaves every list; another person sharing the first name stays.
        conn.execute(
            "INSERT INTO people(display_name,aliases,extraction_key,confidence,base_confidence,created_at,updated_at,last_evidence_at,mention_count)
             VALUES('P. Quill','[\"patquill\"]','person:pq',0.85,0.85,1,1,1,3),
                   ('Pat Other','[\"Pat\"]','person:po',0.85,0.85,1,1,1,3)",
            [],
        )?;
        assert_eq!(mark_owner_rows(&conn)?, 1);
        assert_eq!(mark_owner_rows(&conn)?, 0, "idempotent");
        let id_of = |name: &str| -> Result<i64, rusqlite::Error> {
            conn.query_row("SELECT id FROM people WHERE display_name = ?1", [name], |r| r.get(0))
        };
        assert!(!crate::entity_pages::is_listed(&conn, StateTable::People, id_of("P. Quill")?)?);
        assert!(crate::entity_pages::is_listed(&conn, StateTable::People, id_of("Pat Other")?)?);
        set_owner_names(Vec::<String>::new());
        Ok(())
    }
}
