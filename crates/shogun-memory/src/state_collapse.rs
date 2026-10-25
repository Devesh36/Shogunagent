//! Collapse repeated sightings of one commitment / open loop that the window-scoped identity in
//! [`crate::memory_identity`] keeps apart.
//!
//! The same sentence on one page shows up under many window titles (tab badges, "High memory
//! usage", "Audio playing", group names), so a single message became dozens of rows. Two paths:
//!
//! * [`find_twin`] — at write time, a local (below-Batch) candidate that restates an unresolved or
//!   recent row from the same app within [`HORIZON_MS`] links to it instead of creating a twin.
//! * [`collapse_existing`] — idempotent maintenance that folds duplicates already stored.
//!
//! Neither path ever raises confidence: repeated sightings are not new evidence (2026-10-02 rule).
//! Rows the user edited stay authoritative; rows with a different person, project or due date, a
//! different explicit thread, or a non-screen provenance (connector items have real identity) are
//! never merged.

use std::collections::{BTreeSet, HashMap, HashSet};

use rusqlite::{params, Connection};

use crate::state::StateTable;

/// A twin must have been seen within this long before the new sighting.
pub const HORIZON_MS: i64 = 14 * 86_400_000;

/// Minimum folded length for a text to be specific enough to identify across windows.
const MIN_CHARS: usize = 20;
const MIN_TOKENS: usize = 4;
const PREFIX_MIN_CHARS: usize = 30;
/// A shorter, generic text ("please review") folds only as a recurring cluster of this size:
/// one repeat could be two real requests, many are the same on-screen element.
const SHORT_MIN_CHARS: usize = 10;
const SHORT_CLUSTER: usize = 3;
/// Conservative near-dup bar (token Jaccard).
const JACCARD_MIN: f64 = 0.9;

/// Lowercase, punctuation/emoji to spaces, whitespace collapsed. A trailing `…` truncation mark
/// disappears with the punctuation.
pub fn fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.extend(ch.to_lowercase());
        } else {
            pending_space = true;
        }
    }
    out
}

/// Is this folded text specific enough that the same text elsewhere is the same item?
pub fn specific(folded: &str) -> bool {
    folded.chars().count() >= MIN_CHARS && (folded.split(' ').count() >= MIN_TOKENS || has_cjk(folded))
}

/// Japanese / Chinese text, where a short span already carries a whole clause.
pub fn has_cjk(s: &str) -> bool {
    s.chars().any(|c| matches!(c, '\u{3040}'..='\u{30ff}' | '\u{3400}'..='\u{9fff}'))
}

/// Same item, by folded text: equal, one a long prefix of the other (a truncated preview), or
/// near-identical token sets.
pub fn similar(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if short.chars().count() >= PREFIX_MIN_CHARS && long.starts_with(short) {
        return true;
    }
    let ta: HashSet<&str> = a.split(' ').collect();
    let tb: HashSet<&str> = b.split(' ').collect();
    if ta.len() < 5 || tb.len() < 5 {
        return false;
    }
    let inter = ta.intersection(&tb).count() as f64;
    inter / ((ta.len() + tb.len()) as f64 - inter) >= JACCARD_MIN
}

/// Two long tokens of the text, cut to 8 chars, for a cheap SQL prefilter.
fn anchors(folded: &str) -> (String, String) {
    let mut tokens: Vec<&str> = folded.split(' ').collect();
    tokens.sort_by_key(|t| std::cmp::Reverse(t.chars().count()));
    let cut = |t: &str| t.chars().take(8).collect::<String>();
    let first = tokens.first().map(|t| cut(t)).unwrap_or_default();
    let second = tokens.get(1).map(|t| cut(t)).unwrap_or_else(|| first.clone());
    (first, second)
}


const BROWSERS: &[&str] = &["google chrome", "chrome", "brave", "safari", "firefox", "arc", "microsoft edge", "opera"];
const TAB_NOISE: &[&str] = &["high memory usage", "audio playing", "part of group", "camera recording", "microphone recording", "sharing your screen", "memory usage"];

/// A window title with only the browser's own decoration removed (tab badges, memory warning,
/// audio indicator, tab group, browser and profile name). Anything the page itself names stays, so
/// "PR #812" and "PR #813", or "Inbox - Work" and "Inbox - Personal", remain different windows.
pub fn page_title(title: &str) -> String {
    let marked = [" – ", " — ", " - ", " | "].iter().fold(title.to_string(), |t, sep| t.replace(sep, "\u{1}"));
    let mut kept: Vec<&str> = Vec::new();
    for seg in marked.split('\u{1}') {
        let lower = seg.trim().to_lowercase();
        if BROWSERS.contains(&lower.as_str()) {
            break; // the profile name follows the browser name
        }
        let is_size = lower.split_whitespace().count() == 2 && lower.ends_with(['b']) && lower.split_whitespace().next().is_some_and(|n| n.parse::<f64>().is_ok());
        if TAB_NOISE.iter().any(|n| lower.starts_with(n)) || is_size {
            continue;
        }
        kept.push(seg);
    }
    crate::memory_identity::stable_title(&kept.join(" - ")).to_lowercase()
}

/// A recent same-app row restating `description`, if any. `app` is the app of the event the new
/// candidate came from; `accept` is the caller's last word on a textual match (sender check).
/// Returns `(id, description)`.
#[allow(clippy::too_many_arguments)]
pub fn find_twin(
    conn: &Connection,
    table: StateTable,
    discriminator: &str,
    value: &str,
    description: &str,
    app: &str,
    now: i64,
    accept: &dyn Fn(i64) -> bool,
) -> Result<Option<(i64, String)>, rusqlite::Error> {
    let folded = fold(description);
    if app.is_empty() || !specific(&folded) {
        return Ok(None);
    }
    let (a1, a2) = anchors(&folded);
    let mut stmt = conn.prepare(&format!(
        "SELECT s.id, s.description FROM {t} s
          WHERE s.{discriminator} = ?1 AND s.updated_at >= ?2
            AND (instr(lower(s.description), ?3) > 0 OR instr(lower(s.description), ?4) > 0)
            AND EXISTS (SELECT 1 FROM state_provenance p JOIN event_log e ON e.id = p.event_id
                         WHERE p.state_table = ?5 AND p.state_id = s.id AND e.app_bundle_id = ?6)
          ORDER BY s.id",
        t = table.as_str()
    ))?;
    let rows = stmt
        .query_map(params![value, now - HORIZON_MS, a1, a2, table.as_str(), app], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows.into_iter().find(|(id, d)| similar(&folded, &fold(d)) && accept(*id)))
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CollapseReport {
    pub commitments_merged: usize,
    pub open_loops_merged: usize,
}

struct Row {
    id: i64,
    disc: String,
    folded: String,
    status: String,
    confidence: f64,
    pinned: bool,
    wording_edited: bool,
    created_at: i64,
    counterparty: Option<i64>,
    project: Option<i64>,
    due: Option<i64>,
    apps: BTreeSet<String>,
    titles: BTreeSet<String>,
    threads: BTreeSet<String>,
    /// Some provenance is not a screen capture, or the row is tied to something outside this pass.
    protected: bool,
}

/// Fold stored duplicates into one row each. Idempotent; one transaction per merged group. The
/// survivor keeps its own confidence (the highest in the group), so nothing is ever raised. Rows
/// the horizon timer aged or the consolidator merged (V83) are kept as they are: never folded,
/// never removed, and never able to close an open twin.
pub fn collapse_existing(conn: &mut Connection, now: i64) -> Result<CollapseReport, rusqlite::Error> {
    let mut total = CollapseReport::default();
    // A merge fills dates and widens apps, which can admit one more row; a few passes converge.
    for _ in 0..8 {
        let pass = CollapseReport {
            commitments_merged: collapse_table(conn, StateTable::Commitments, now)?,
            open_loops_merged: collapse_table(conn, StateTable::OpenLoops, now)?,
        };
        total.commitments_merged += pass.commitments_merged;
        total.open_loops_merged += pass.open_loops_merged;
        if pass == CollapseReport::default() {
            break;
        }
    }
    Ok(total)
}

fn load_rows(conn: &Connection, table: StateTable) -> Result<Vec<Row>, rusqlite::Error> {
    let t = table.as_str();
    let (disc, resolved_cols) = match table {
        StateTable::Commitments => ("direction", "counterparty_id, project_id, due_at"),
        _ => ("kind", "counterparty_id, project_id, NULL"),
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT id, {disc}, description, status, confidence, user_pinned, wording_edited, created_at,
                {resolved_cols} FROM {t} WHERE aged_at IS NULL AND merged_into IS NULL ORDER BY id"
    ))?;
    let mut rows: Vec<Row> = stmt
        .query_map([], |r| {
            Ok(Row {
                id: r.get(0)?,
                disc: r.get(1)?,
                folded: fold(&r.get::<_, String>(2)?),
                status: r.get(3)?,
                confidence: r.get(4)?,
                pinned: r.get::<_, i64>(5)? != 0,
                wording_edited: r.get::<_, i64>(6)? != 0,
                created_at: r.get(7)?,
                counterparty: r.get(8)?,
                project: r.get(9)?,
                due: r.get(10)?,
                apps: BTreeSet::new(),
                titles: BTreeSet::new(),
                threads: BTreeSet::new(),
                protected: false,
            })
        })?
        .collect::<Result<_, _>>()?;
    let mut by_id: HashMap<i64, usize> = rows.iter().enumerate().map(|(i, r)| (r.id, i)).collect();
    let mut ev = conn.prepare(
        "SELECT p.state_id, e.source, e.app_bundle_id, e.window_title, e.thread_key
           FROM state_provenance p JOIN event_log e ON e.id = p.event_id
          WHERE p.state_table = ?1",
    )?;
    let mut q = ev.query([t])?;
    while let Some(r) = q.next()? {
        let Some(&i) = by_id.get(&r.get::<_, i64>(0)?) else { continue };
        let source: String = r.get(1)?;
        let app: Option<String> = r.get(2)?;
        let title: Option<String> = r.get(3)?;
        let thread: Option<String> = r.get(4)?;
        let row = &mut rows[i];
        if !crate::memory_identity::is_screen_source(&source) {
            row.protected = true;
        }
        if let Some(t) = title.as_deref().map(page_title).filter(|t| !t.is_empty()) {
            row.titles.insert(t);
        }
        if let Some(a) = app.as_deref().filter(|a| !a.is_empty()) {
            row.apps.insert(a.to_string());
        }
        if let Some(k) = crate::memory_identity::explicit_thread(&source, app.as_deref(), title.as_deref(), thread.as_deref()) {
            row.threads.insert(k);
        }
    }
    by_id.clear();
    // A dispute the user already resolved, or a mail-triage link, points at the row id: leave those
    // rows alone. Open disputes between duplicates are an artifact of the duplication and go with it.
    let linked = |sql: &str| -> Result<HashSet<i64>, rusqlite::Error> {
        conn.prepare(sql)?.query_map([t], |r| r.get(0))?.collect()
    };
    let mut tied = linked(
        "SELECT earlier_id FROM state_disputes WHERE state_table = ?1 AND status = 'resolved'
         UNION SELECT later_id FROM state_disputes WHERE state_table = ?1 AND status = 'resolved'",
    )?;
    if table == StateTable::OpenLoops {
        tied.extend(
            conn.prepare("SELECT open_loop_id FROM mail_triage WHERE open_loop_id IS NOT NULL")?
                .query_map([], |r| r.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?,
        );
    }
    for row in &mut rows {
        if tied.contains(&row.id) {
            row.protected = true;
        }
    }
    Ok(rows)
}

#[allow(clippy::nonminimal_bool)] // reads as the list of reasons two rows may fold
fn compatible(survivor: &Row, row: &Row) -> bool {
    let same_opt = |a: Option<i64>, b: Option<i64>| a.is_none() || b.is_none() || a == b;
    survivor.disc == row.disc
        && !survivor.protected
        && !row.protected
        && !(survivor.pinned && row.pinned)
        && (row.created_at - survivor.created_at).abs() <= HORIZON_MS
        && !survivor.apps.is_disjoint(&row.apps)
        && !survivor.titles.is_disjoint(&row.titles)
        && (survivor.threads.is_empty() || row.threads.is_empty() || survivor.threads == row.threads)
        && same_opt(survivor.counterparty, row.counterparty)
        && same_opt(survivor.project, row.project)
        // The same sentence resolved to a different due date on another day ("today") is still the
        // same sentence; with different wording a different date is a different item.
        && (survivor.folded == row.folded || same_opt(survivor.due, row.due))
        && row.folded.chars().count() >= SHORT_MIN_CHARS
        && similar(&survivor.folded, &row.folded)
}

fn collapse_table(conn: &mut Connection, table: StateTable, now: i64) -> Result<usize, rusqlite::Error> {
    let rows = load_rows(conn, table)?;
    // Greedy clusters: a row joins the first cluster in which it is compatible with every member.
    let mut clusters: Vec<Vec<usize>> = Vec::new();
    'row: for (i, row) in rows.iter().enumerate() {
        for c in clusters.iter_mut() {
            if c.iter().all(|&m| compatible(&rows[m], row)) {
                c.push(i);
                continue 'row;
            }
        }
        clusters.push(vec![i]);
    }
    let mut merged = 0;
    for cluster in clusters.into_iter().filter(|c| c.len() > 1) {
        if !specific(&rows[cluster[0]].folded) && cluster.len() < SHORT_CLUSTER {
            continue;
        }
        let mut members: Vec<&Row> = cluster.iter().map(|&i| &rows[i]).collect();
        // Survivor: the user's row, then the best-supported one, then the oldest.
        members.sort_by(|a, b| {
            (b.pinned, b.wording_edited)
                .cmp(&(a.pinned, a.wording_edited))
                .then(b.confidence.total_cmp(&a.confidence))
                .then(a.id.cmp(&b.id))
        });
        let (survivor, absorbed) = (members[0], &members[1..]);
        let tx = conn.savepoint()?;
        fold_into(&tx, table, survivor, absorbed, now)?;
        tx.commit()?;
        let _ = crate::search_documents::project_source_after_write(conn, &crate::state_edit::source_ref(table, survivor.id), now);
        for a in absorbed {
            let _ = crate::search_documents::project_source_after_write(conn, &crate::state_edit::source_ref(table, a.id), now);
        }
        merged += absorbed.len();
    }
    Ok(merged)
}

fn fold_into(
    tx: &Connection,
    table: StateTable,
    survivor: &Row,
    absorbed: &[&Row],
    now: i64,
) -> Result<(), rusqlite::Error> {
    let t = table.as_str();
    let resolved_rank = |s: &str| match s {
        "done" | "closed" => 2,
        "cancelled" => 1,
        _ => 0,
    };
    let mut status = survivor.status.clone();
    let mut created = survivor.created_at;
    for a in absorbed {
        // Provenance union: keep every event once; the absorbed row's link (with its quote and
        // Batch-review mark) moves unless the survivor already cites that event.
        tx.execute(
            "UPDATE state_provenance SET state_id = ?1
              WHERE state_table = ?2 AND state_id = ?3
                AND event_id NOT IN (SELECT event_id FROM state_provenance WHERE state_table = ?2 AND state_id = ?1)",
            params![survivor.id, t, a.id],
        )?;
        created = created.min(a.created_at);
        if resolved_rank(&a.status) > resolved_rank(&status) {
            status = a.status.clone();
        }
        let fill = match table {
            StateTable::Commitments => "counterparty_id = COALESCE(counterparty_id, ?2), project_id = COALESCE(project_id, ?3), due_at = COALESCE(MAX(due_at, ?4), due_at, ?4)",
            _ => "counterparty_id = COALESCE(counterparty_id, ?2), project_id = COALESCE(project_id, ?3)",
        };
        let sql = format!("UPDATE {t} SET {fill} WHERE id = ?1 AND user_pinned = 0");
        if table == StateTable::Commitments {
            tx.execute(&sql, params![survivor.id, a.counterparty, a.project, a.due])?;
        } else {
            tx.execute(&sql, params![survivor.id, a.counterparty, a.project])?;
        }
        crate::state_edit::remove_state_row(tx, table, a.id)?;
    }
    // Unresolved: the latest sighting's due date decides open versus overdue.
    if resolved_rank(&status) == 0 && !survivor.pinned && table == StateTable::Commitments {
        status = tx.query_row(
            "SELECT CASE WHEN due_at IS NOT NULL AND due_at < ?2 THEN 'overdue' ELSE 'open' END FROM commitments WHERE id = ?1",
            params![survivor.id, now],
            |r| r.get(0),
        )?;
    }
    // Confidence and base_confidence are left exactly as the survivor had them.
    tx.execute(
        &format!(
            "UPDATE {t} SET status = ?2, created_at = ?3,
                    last_evidence_at = MAX(last_evidence_at, (SELECT COALESCE(MAX(e.ts), 0) FROM state_provenance p JOIN event_log e ON e.id = p.event_id WHERE p.state_table = ?4 AND p.state_id = ?1)),
                    updated_at = MAX(updated_at + 1, ?5)
              WHERE id = ?1"
        ),
        params![survivor.id, status, created, t, now],
    )?;
    if table == StateTable::OpenLoops {
        tx.execute("UPDATE open_loops SET opened_at = MIN(opened_at, ?2) WHERE id = ?1", params![survivor.id, created])?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::event_log::{insert, NewEvent};
    use crate::extract::{extract, persist_candidates_deduped};
    use crate::state::{insert_commitment, CommitmentDirection, CommitmentStatus, NewCommitment, Provenance};

    const SAID: &str = "for major PRs that have more than 5000 lines changed, I'll tag you";
    const DAY: i64 = 86_400_000;

    fn event(conn: &Connection, n: i64, ts: i64, app: &str, title: &str) -> i64 {
        insert(
            conn,
            &NewEvent {
                ts,
                source: "capture",
                kind: "text",
                app_bundle_id: Some(app),
                window_title: Some(title),
                content: SAID,
                content_hash: &format!("h{n}"),
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap()
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn same_sentence_under_other_window_titles_is_one_row_with_unchanged_confidence() {
        let mut conn = crate::open_in_memory().unwrap();
        let cands = extract(SAID);
        assert_eq!(cands.len(), 1);
        let titles = ["(2) Chat", "(2) Chat - High memory usage - 1.5 GB", "(2) Chat - Audio playing", "Chat - Part of group Work"];
        let mut ids = Vec::new();
        for (i, t) in titles.iter().enumerate() {
            let ts = 1_000 + i as i64 * 1_000;
            let ev = event(&conn, i as i64, ts, "com.google.Chrome", t);
            ids.extend(persist_candidates_deduped(&mut conn, ev, &cands, ts, ts).unwrap());
        }
        assert!(ids.windows(2).all(|w| w[0] == w[1]), "{ids:?}");
        assert_eq!(count(&conn, "SELECT count(*) FROM commitments"), 1);
        assert_eq!(count(&conn, "SELECT count(*) FROM state_provenance WHERE state_table='commitments'"), 4);
        let conf: f64 = conn.query_row("SELECT confidence FROM commitments", [], |r| r.get(0)).unwrap();
        assert!(conf < 0.5 && (conf - 0.35).abs() < 1e-9, "repeats must not raise confidence");
    }

    #[test]
    fn other_app_other_week_or_short_text_stays_separate() {
        let mut conn = crate::open_in_memory().unwrap();
        let cands = extract(SAID);
        let a = event(&conn, 1, 1_000, "com.google.Chrome", "Chat");
        let b = event(&conn, 2, 2_000, "com.tinyspeck.slackmacgap", "Chat");
        let c = event(&conn, 3, 1_000 + 15 * DAY, "com.google.Chrome", "Chat - Audio playing");
        for (ev, ts) in [(a, 1_000), (b, 2_000), (c, 1_000 + 15 * DAY)] {
            persist_candidates_deduped(&mut conn, ev, &cands, ts, ts).unwrap();
        }
        assert_eq!(count(&conn, "SELECT count(*) FROM commitments"), 3);
        let short = extract("I'll do it.");
        let s1 = event(&conn, 4, 5_000, "com.google.Chrome", "A");
        let s2 = event(&conn, 5, 6_000, "com.google.Chrome", "B");
        persist_candidates_deduped(&mut conn, s1, &short, 5_000, 5_000).unwrap();
        persist_candidates_deduped(&mut conn, s2, &short, 6_000, 6_000).unwrap();
        assert_eq!(count(&conn, "SELECT count(*) FROM commitments WHERE description LIKE 'I''ll do it%'"), 2);
    }

    fn add(conn: &mut Connection, ev: i64, conf: f64, due: Option<i64>, at: i64) -> i64 {
        add_text(conn, ev, SAID, conf, due, at)
    }

    fn add_text(conn: &mut Connection, ev: i64, text: &str, conf: f64, due: Option<i64>, at: i64) -> i64 {
        insert_commitment(
            conn,
            &NewCommitment {
                direction: CommitmentDirection::Mine,
                counterparty_id: None,
                description: text,
                due_at: due,
                status: CommitmentStatus::Open,
                project_id: None,
                confidence: conf,
                now: at,
            },
            &[Provenance::new(ev)],
        )
        .unwrap()
    }

    #[test]
    fn existing_duplicates_merge_keep_provenance_and_never_raise_confidence() {
        let mut conn = crate::open_in_memory().unwrap();
        let mut ids = Vec::new();
        for (i, (conf, due)) in [(0.3, None), (0.35, None), (0.3, None), (0.3, Some(9_999))].into_iter().enumerate() {
            let ev = event(&conn, i as i64, 1_000 + i as i64, "com.google.Chrome", &format!("Chat - High memory usage - {i}.5 GB - Google Chrome - Anant"));
            ids.push(add(&mut conn, ev, conf, due, 1_000 + i as i64));
        }
        let r = collapse_existing(&mut conn, 10_000).unwrap();
        // Rows 0..2 fold together; the dated one is a different item only if its due differs from
        // a dated sibling — here the others are undated, so it joins too and fills the due date.
        assert_eq!(r.commitments_merged, 3);
        assert_eq!(count(&conn, "SELECT count(*) FROM commitments"), 1);
        assert_eq!(count(&conn, "SELECT count(*) FROM state_provenance WHERE state_table='commitments'"), 4);
        let (conf, base): (f64, f64) = conn.query_row("SELECT confidence, base_confidence FROM commitments", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert!((conf - 0.35).abs() < 1e-9 && (base - 0.35).abs() < 1e-9, "survivor keeps the highest input, no more");
        assert_eq!(count(&conn, "SELECT due_at FROM commitments"), 9_999);
        assert_eq!(collapse_existing(&mut conn, 20_000).unwrap(), CollapseReport::default(), "idempotent");
    }

    #[test]
    fn different_due_dates_user_rows_and_connectors_stay_distinct() {
        let mut conn = crate::open_in_memory().unwrap();
        let e0 = event(&conn, 0, 1_000, "com.google.Chrome", "Chat");
        let e1 = event(&conn, 1, 2_000, "com.google.Chrome", "Chat - Audio playing");
        let e2 = event(&conn, 2, 3_000, "com.google.Chrome", "Chat - Audio playing");
        let e3 = event(&conn, 3, 4_000, "com.google.Chrome", "Chat - Audio playing");
        let longer = format!("{SAID} right now");
        add(&mut conn, e0, 0.3, Some(100), 1_000);
        add_text(&mut conn, e1, &longer, 0.3, Some(200), 2_000);
        let pinned_a = add(&mut conn, e2, 0.3, None, 3_000);
        let pinned_b = add(&mut conn, e3, 0.3, None, 4_000);
        conn.execute("UPDATE commitments SET user_pinned=1 WHERE id IN (?1, ?2)", [pinned_a, pinned_b]).unwrap();
        let mail = insert(&conn, &NewEvent { ts: 5_000, source: "gmail", kind: "message", app_bundle_id: Some("com.google.Chrome"), window_title: None, content: SAID, content_hash: "m", dwell_ms: 0, display_id: None, window_bounds: None }).unwrap();
        add(&mut conn, mail, 0.3, Some(100), 5_000);
        collapse_existing(&mut conn, 10_000).unwrap();
        // due 100 (same text) and due 200 (longer wording) never fold into each other; the two pinned rows are never fused with each
        // other; the mail row has a connector identity and is left alone.
        assert!(count(&conn, "SELECT count(*) FROM commitments WHERE user_pinned=1") == 2);
        // pinned A (+ the due-100 copy), pinned B (+ the longer due-200 wording), the mail row.
        assert_eq!(count(&conn, "SELECT count(*) FROM commitments"), 3);
        assert_eq!(count(&conn, "SELECT count(*) FROM commitments WHERE due_at=100 AND user_pinned=0"), 1, "connector row untouched");
    }

    #[test]
    fn same_sentence_with_day_relative_due_dates_folds_to_the_latest_due() {
        let mut conn = crate::open_in_memory().unwrap();
        for (i, due) in [(0, 5 * DAY), (1, 6 * DAY), (2, 7 * DAY)] {
            let ev = event(&conn, i, i * DAY, "com.google.Chrome", &format!("Chat - High memory usage - {i}.5 GB - Google Chrome - Anant"));
            add(&mut conn, ev, 0.3, Some(due), i * DAY);
        }
        // An open, auto-raised conflict between the copies goes with them.
        conn.execute("INSERT INTO state_disputes(state_table,field,earlier_id,later_id,created_at) VALUES('commitments','due_at',1,2,1)", []).unwrap();
        collapse_existing(&mut conn, 3 * DAY).unwrap();
        assert_eq!(count(&conn, "SELECT count(*) FROM commitments"), 1);
        assert_eq!(count(&conn, "SELECT due_at FROM commitments"), 7 * DAY);
        assert_eq!(count(&conn, "SELECT count(*) FROM state_disputes"), 0);
        assert_eq!(count(&conn, "SELECT count(*) FROM commitments WHERE status='open'"), 1, "latest due is in the future");
    }

    #[test]
    fn short_generic_text_folds_only_as_a_recurring_cluster() {
        for (n, expected) in [(2, 2), (4, 1)] {
            let mut conn = crate::open_in_memory().unwrap();
            for i in 0..n {
                let ev = event(&conn, i, i, "com.google.Chrome", &format!("Chat - High memory usage - {i}.5 GB - Google Chrome - Anant"));
                crate::state::insert_open_loop(
                    &mut conn,
                    &crate::state::NewOpenLoop { kind: crate::state::OpenLoopKind::ReviewPending, description: "please review", counterparty_id: None, project_id: None, opened_at: i, confidence: 0.3, now: i },
                    &[Provenance::new(ev)],
                )
                .unwrap();
            }
            collapse_existing(&mut conn, 100).unwrap();
            assert_eq!(count(&conn, "SELECT count(*) FROM open_loops"), expected, "{n} repeats");
        }
    }

    #[test]
    fn different_pages_in_one_app_stay_separate_and_browser_decoration_is_ignored() {
        assert_eq!(page_title("(2) PR #812 - acme/api - GitHub - High memory usage - 1.5 GB - Google Chrome - Anant"), page_title("PR #812 - acme/api - GitHub"));
        assert_ne!(page_title("PR #812 - acme/api - GitHub"), page_title("PR #813 - acme/api - GitHub"));
        assert_ne!(page_title("Inbox \u{2014} Work"), page_title("Inbox \u{2014} Personal"));
        let mut conn = crate::open_in_memory().unwrap();
        let cands = extract(SAID);
        for (i, t) in ["PR #812 - GitHub", "PR #813 - GitHub"].iter().enumerate() {
            let ev = event(&conn, i as i64, 1_000 + i as i64, "com.google.Chrome", t);
            persist_candidates_deduped(&mut conn, ev, &cands, 1_000, 1_000).unwrap();
        }
        assert_eq!(count(&conn, "SELECT count(*) FROM commitments"), 2);
        collapse_existing(&mut conn, 10_000).unwrap();
        assert_eq!(count(&conn, "SELECT count(*) FROM commitments"), 2);
    }

    #[test]
    fn near_duplicates_need_ninety_percent_overlap() {
        assert!(similar(&fold("I'll send the full combined report once all four are back"), &fold("I'll send the full combined report once all four are back…")));
        assert!(!similar(&fold("I'll send the report to Sam on Friday morning"), &fold("I'll send the report to Dana on Monday evening")));
        assert!(!specific(&fold("I'll do it")));
    }
}
