//! Local outcome evidence for commitments and open loops (plan E49).
//!
//! The on-device rules ([`crate::extract`]) only ever *open* low-confidence candidates. This module
//! reads what happens to them afterwards: a later message in the **same thread** that says the
//! thing happened ("Sent the Q3 budget to Oren."), was called off ("Never mind about the deck, it
//! is no longer needed.") or moved ("Update: the memo for Oren moves to Thursday.").
//!
//! Each such sentence that names an open row of the thread is recorded in `loop_updates` (V104)
//! as a typed update — `resolved`, `cancelled` or `redated` — with the event and the exact quote,
//! and then:
//!
//! - a **local candidate** (below the dashboard line, not the user's, never Batch-reviewed) is
//!   closed or re-dated in place, with the quote linked as provenance. Its confidence is never
//!   touched: local evidence may retire or move a local candidate, never lift it (owner decision
//!   2026-10-02, `LOCAL_RULE_MAX_CONFIDENCE`);
//! - a row the **loop consolidator** owns (≥ 0.5, or Batch-reviewed) or the **user** owns (pinned
//!   or edited) is left as it is. The update waits, unread, for the next consolidator call
//!   ([`crate::loops::consolidation_input`] shows it under "Outcomes noticed" with its source), and
//!   only a resolution or re-list that quotes it changes the row (owner decision 2026-10-14).
//!
//! The rules are deterministic and explainable, like the rest of the capture path: fixed cue
//! phrases, a question / future / negation veto, the same thread, and at least two shared content
//! words with the row (the artifact and the person), with a tie refused.
//!
//! **Text without a thread** (screen capture, a note ingested with no `thread_key`) never changes
//! a row here, local candidate or not ([`reconcile_unthreaded_event`]). Nothing says which row it
//! is about; its time is when it was *seen*, not when it was said (an old "Sent …" scrolled into
//! view, a date read against the wrong day); and the same text comes back frame after frame. The
//! one thing it may do is leave a lead for the consolidator: a resolution or cancellation (never a
//! re-date) that names exactly one open row the consolidator owns — at least
//! [`MIN_SHARED_WORDS_UNTHREADED`] shared content words, at least half of its own, and no other
//! open row sharing [`MIN_SHARED_WORDS`] — is recorded unread and unapplied, once per row and
//! quote. The consolidator sees it with its source and changes the row only through a quote of its
//! own; a local candidate is left to its thread.
//!
//! The pass runs in the Dream Cycle's StateUpdate job and the hourly maintenance
//! ([`crate::recompute::recompute_overdue_and_staleness_detailed`]) over the events inserted since
//! its last run (`loop_update_scan`), so it is idempotent and bounded per run.

use std::collections::HashSet;

use rusqlite::{params, Connection, OptionalExtension};

use crate::state::StateTable;

/// How far back the first pass after the upgrade looks for outcomes.
pub const FIRST_SCAN_LOOKBACK_MS: i64 = 30 * 86_400_000;
/// Events one pass reads; the rest wait for the next pass.
pub const SCAN_BATCH: i64 = 5_000;
/// Content words an outcome must share with the row it closes.
pub const MIN_SHARED_WORDS: usize = 2;
/// Content words an outcome in text without a thread must share with the one row it names.
pub const MIN_SHARED_WORDS_UNTHREADED: usize = 3;
/// The confidence below which a row is a local candidate the local path may change.
pub const DASHBOARD_LINE: f64 = 0.5;

/// What a later sentence says happened to an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateKind {
    /// It was done: sent, delivered, answered, decided.
    Resolved,
    /// It was called off.
    Cancelled,
    /// Its due date moved.
    Redated,
}

impl UpdateKind {
    pub fn as_str(self) -> &'static str {
        match self {
            UpdateKind::Resolved => "resolved",
            UpdateKind::Cancelled => "cancelled",
            UpdateKind::Redated => "redated",
        }
    }
}

/// One outcome sentence: its kind, the exact span, and for a re-date the new due instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome<'a> {
    pub kind: UpdateKind,
    pub quote: &'a str,
    pub due_at: Option<i64>,
}

/// Sentences that open with what was done ("Sent the plan to Oren.", "Done: …", "Decided: …").
const RESOLVED_OPENERS: &[&str] = &[
    "sent ", "done:", "done,", "done -", "delivered ", "replied ", "answered ", "finished ", "completed ",
    "decided:", "decided ", "settled ", "got ", "received ", "shipped ", "submitted ", "approved ", "resolved ",
    "handled ", "merged ", "closed out ",
];
/// The same, said about someone else or the thing itself ("Oren sent the plan over", "the plan
/// arrived from Oren", "review of the deck done and sent back").
const RESOLVED_ANYWHERE: &[&str] = &[
    " sent ", " arrived", " delivered", " is done", " done and ", "has been sent", " came through", " came in",
];
/// The thing was called off.
const CANCELLED: &[&str] = &[
    "never mind", "no longer needed", "not needed anymore", "no longer necessary", "dropping ", "called off",
    "cancelled", "canceled", "scrap ", "scratch ", "is off now", "not happening",
];
/// The due date moved; the new date follows the cue.
const REDATED: &[&str] = &[
    "moves to ", "moved to ", "now due ", "is now due ", "slips to ", "slipped to ", "pushed to ", "pushed back to ",
    "postponed to ", "postponed until ", "rescheduled to ", "rescheduled for ", "delayed to ", "delayed until ",
    "deferred to ", "new deadline is ", "new due date is ",
];
/// An outcome is stated, not planned, asked, denied or remembered from long ago.
const NOT_DONE: &[&str] = &[
    "will ", "'ll ", "going to", " not ", "n't", "if ", "unless ", "still ", " yet", "should ", "need to",
    "needs to", "please", "last month", "last year", "back in ", " ago", "reminder", "nudge", "chase",
];
/// A move or a cancellation is stated, not wondered about.
const NOT_STATED: &[&str] = &["if ", "unless ", " not ", "n't", "might ", "may ", "maybe ", "could "];

/// Words that say nothing about which item a sentence is about: function words, the cue verbs
/// themselves and every word a date is written with.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "from", "about", "that", "this", "your", "you", "are", "was", "were", "will",
    "have", "has", "had", "all", "now", "any", "not", "its", "our", "their", "they", "them", "she", "him",
    "her", "his", "can", "could", "would", "should", "just", "still", "yet", "also", "been", "into", "onto",
    "over", "then", "than", "thanks", "thank", "update", "heads", "change", "moves", "moved", "slips",
    "slipped", "due", "done", "sent", "send", "sends", "got", "never", "mind", "need", "needed", "longer",
    "dropping", "decided", "decide", "settled", "settle", "whether", "ships", "ship", "goes", "out", "back",
    "please", "hi", "hello", "hey", "best", "cheers", "user", "promise", "sure", "it'll", "i'll", "i'm",
    "is", "it", "to", "of", "on", "in", "by", "be", "me", "my", "we", "us", "or", "an", "a", "i",
    "today", "tomorrow", "tonight", "week", "next", "monday", "tuesday", "wednesday", "thursday", "friday",
    "saturday", "sunday", "january", "february", "march", "april", "may", "june", "july", "august",
    "september", "october", "november", "december",
];

/// Sentence spans of `text`: split on newlines and `. ! ?`, trimmed, each an exact slice, with
/// whether it ended in a question mark.
fn spans(text: &str) -> Vec<(&str, bool)> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, c) in text.char_indices() {
        if matches!(c, '\n' | '.' | '!' | '?' | '。' | '！' | '？') {
            let s = text[start..i].trim();
            if !s.is_empty() {
                out.push((s, c == '?' || c == '？'));
            }
            start = i + c.len_utf8();
        }
    }
    let tail = text[start..].trim();
    if !tail.is_empty() {
        out.push((tail, false));
    }
    out
}

/// The sentence without a leading bullet; and without a short label too ("user: ", "Oren Vale: ").
fn bodies(lower: &str) -> (&str, &str) {
    let bare = lower.trim_start_matches(|c: char| c.is_whitespace() || matches!(c, '-' | '*' | '•' | '·'));
    let labelled = match bare.split_once(": ") {
        Some((label, rest)) if label.split_whitespace().count() <= 3 => rest.trim_start(),
        _ => bare,
    };
    (bare, labelled)
}

fn any_in(haystack: &str, cues: &[&str]) -> bool {
    cues.iter().any(|c| haystack.contains(c))
}

/// The new due instant a re-date sentence names, read against when it was said: the words after
/// the cue, read as a deadline ("moves to Thursday" → "by Thursday").
pub fn redated_due(lower: &str, said_at_ms: i64) -> Option<i64> {
    REDATED.iter().find_map(|cue| {
        let at = lower.find(cue)? + cue.len();
        crate::due::due_at(&format!("by {}", &lower[at..]), said_at_ms)
    })
}

/// The outcome sentences of `text` (English), said at `said_at_ms`. A question, a plan, a
/// negation or a memory of long ago is not an outcome; a re-date needs a date the parser reads.
pub fn detect(text: &str, said_at_ms: i64) -> Vec<Outcome<'_>> {
    let mut out = Vec::new();
    for (span, question) in spans(text) {
        if question {
            continue;
        }
        let lower = format!(" {}", span.to_lowercase());
        let (bare, body) = bodies(lower.trim_start());
        let stated = !any_in(&lower, NOT_STATED);
        if stated && any_in(&lower, REDATED) {
            if let Some(due) = redated_due(&lower, said_at_ms) {
                out.push(Outcome { kind: UpdateKind::Redated, quote: span, due_at: Some(due) });
            }
            continue;
        }
        if stated && any_in(&lower, CANCELLED) {
            out.push(Outcome { kind: UpdateKind::Cancelled, quote: span, due_at: None });
            continue;
        }
        let opener = |s: &str| RESOLVED_OPENERS.iter().any(|o| s.starts_with(o));
        if !any_in(&lower, NOT_DONE) && (opener(bare) || opener(body) || any_in(&lower, RESOLVED_ANYWHERE)) {
            out.push(Outcome { kind: UpdateKind::Resolved, quote: span, due_at: None });
        }
    }
    out
}

/// The content words of `text`: lower-cased alphanumeric tokens of three or more characters (two
/// when they hold a digit, "q3"), stop words and date words removed.
pub fn content_words(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .map(|w| w.trim_matches('\''))
        .filter(|w| w.chars().count() >= 3 || (w.chars().count() == 2 && w.chars().any(|c| c.is_ascii_digit())))
        .filter(|w| !STOPWORDS.contains(w))
        .map(str::to_string)
        .collect()
}

/// An open row of the event's thread an outcome may be about.
#[derive(Debug, Clone)]
struct Target {
    table: StateTable,
    id: i64,
    description: String,
    due_at: Option<i64>,
    /// A local candidate: below the dashboard line, not the user's, never Batch-reviewed.
    local: bool,
}

/// Open rows with evidence in `thread_key` said before `before_ts` (not on `event_id` itself).
fn thread_targets(conn: &Connection, thread_key: &str, event_id: i64, before_ts: i64) -> Result<Vec<Target>, rusqlite::Error> {
    targets(conn, Some(thread_key), event_id, before_ts)
}

/// Open rows with evidence said before `before_ts` (not on `event_id` itself), in `thread_key`
/// when one is given, anywhere otherwise.
fn targets(conn: &Connection, thread_key: Option<&str>, event_id: i64, before_ts: i64) -> Result<Vec<Target>, rusqlite::Error> {
    let mut out = Vec::new();
    for (table, open, due) in [
        (StateTable::Commitments, "('open', 'overdue')", "s.due_at"),
        (StateTable::OpenLoops, "('open')", "NULL"),
    ] {
        let t = table.as_str();
        let sql = format!(
            "SELECT s.id, s.description, {due},
                    s.confidence < ?4 AND s.user_pinned = 0 AND s.user_edited_at IS NULL AND s.wording_edited = 0
                      AND s.horizon IS NULL
                      AND NOT EXISTS (SELECT 1 FROM state_provenance r WHERE r.state_table = '{t}'
                                       AND r.state_id = s.id AND r.batch_reviewed = 1)
               FROM {t} s
              WHERE s.status IN {open} AND s.aged_at IS NULL AND s.merged_into IS NULL
                AND EXISTS (SELECT 1 FROM state_provenance p JOIN event_log e ON e.id = p.event_id
                             WHERE p.state_table = '{t}' AND p.state_id = s.id
                               AND (?1 IS NULL OR e.thread_key = ?1) AND e.id != ?2 AND e.ts <= ?3)
              ORDER BY s.id"
        );
        let rows = conn
            .prepare(&sql)?
            .query_map(params![thread_key, event_id, before_ts, DASHBOARD_LINE], |r| {
                Ok(Target { table, id: r.get(0)?, description: r.get(1)?, due_at: r.get(2)?, local: r.get(3)? })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        out.extend(rows);
    }
    Ok(out)
}

/// The one row an outcome names: the most shared content words, at least [`MIN_SHARED_WORDS`] and
/// at least half of the outcome's own; a tie is refused (the sentence could be about either).
fn pick<'t>(targets: &'t [Target], outcome: &Outcome<'_>) -> Option<&'t Target> {
    let words = content_words(outcome.quote);
    if words.is_empty() {
        return None;
    }
    let mut best: Option<(&Target, usize)> = None;
    let mut tied = false;
    for target in targets {
        if outcome.kind == UpdateKind::Redated && target.table != StateTable::Commitments {
            continue;
        }
        let shared = content_words(&target.description).intersection(&words).count();
        if shared < MIN_SHARED_WORDS || 2 * shared < words.len() {
            continue;
        }
        match best {
            Some((_, n)) if n > shared => {}
            Some((_, n)) if n == shared => tied = true,
            _ => {
                best = Some((target, shared));
                tied = false;
            }
        }
    }
    if tied {
        return None;
    }
    best.map(|(t, _)| t)
}

/// What one pass did, counts only.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Reconciled {
    pub scanned: usize,
    /// Updates recorded (applied or left for the consolidator).
    pub recorded: usize,
    /// Local candidates closed.
    pub closed: usize,
    /// Local candidates re-dated.
    pub redated: usize,
}

const SAME_DAY_MS: i64 = 12 * 3_600_000;

/// Record `outcome` from `event_id` against `target`, and apply it when the target is a local
/// candidate. Returns `(recorded, applied)`. A repeat of (row, event, kind) records nothing.
fn record(conn: &Connection, target: &Target, event_id: i64, outcome: &Outcome<'_>, now: i64) -> Result<(bool, bool), rusqlite::Error> {
    if outcome.kind == UpdateKind::Redated {
        // A "move" to the day it already holds is no news.
        if let (Some(old), Some(new)) = (target.due_at, outcome.due_at) {
            if (old - new).abs() < SAME_DAY_MS {
                return Ok((false, false));
            }
        }
    }
    let Some(quote) = crate::sanitize::persist_generated(outcome.quote) else { return Ok((false, false)) };
    let quote = quote.text.into_owned();
    let t = target.table.as_str();
    let added = conn.execute(
        "INSERT OR IGNORE INTO loop_updates (kind, state_table, state_id, event_id, quote, due_at, previous_due_at,
                                             applied, created_at)
         SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8 WHERE EXISTS (SELECT 1 FROM event_log WHERE id = ?4)",
        params![outcome.kind.as_str(), t, target.id, event_id, quote, outcome.due_at, target.due_at, now],
    )?;
    if added == 0 || !target.local {
        return Ok((added > 0, false));
    }
    let ctx = crate::actor::current_write();
    let changed = match (outcome.kind, target.table) {
        (UpdateKind::Redated, StateTable::Commitments) => conn.execute(
            "UPDATE commitments SET due_at = ?2, updated_at = MAX(updated_at, ?3), actor_last = ?4,
                    status = CASE WHEN status = 'overdue' THEN 'open' ELSE status END
              WHERE id = ?1 AND status IN ('open', 'overdue')",
            params![target.id, outcome.due_at, now, ctx.actor_sql()],
        )?,
        (UpdateKind::Redated, _) => 0,
        (kind, table) => {
            let status = match (kind, table) {
                (UpdateKind::Resolved, StateTable::Commitments) => "done",
                (_, StateTable::Commitments) => "cancelled",
                _ => "closed",
            };
            conn.execute(
                &format!("UPDATE {t} SET status = ?2, updated_at = MAX(updated_at, ?3), actor_last = ?4 WHERE id = ?1"),
                params![target.id, status, now, ctx.actor_sql()],
            )?
        }
    };
    if changed == 0 {
        return Ok((true, false));
    }
    // The outcome's words are evidence of what happened to the row, never a Batch review.
    conn.execute(
        "INSERT INTO state_provenance (state_table, state_id, event_id, weight, quote, batch_reviewed)
         SELECT ?1, ?2, ?3, 1.0, ?4, 0 WHERE NOT EXISTS
           (SELECT 1 FROM state_provenance WHERE state_table = ?1 AND state_id = ?2 AND event_id = ?3)",
        params![t, target.id, event_id, quote],
    )?;
    conn.execute(
        "UPDATE loop_updates SET applied = 1 WHERE state_table = ?1 AND state_id = ?2 AND event_id = ?3 AND kind = ?4",
        params![t, target.id, event_id, outcome.kind.as_str()],
    )?;
    Ok((true, true))
}

/// Read the outcomes of one event (its content, time and thread) and record / apply them.
pub fn reconcile_event(conn: &Connection, event_id: i64, now: i64) -> Result<Reconciled, rusqlite::Error> {
    let mut done = Reconciled { scanned: 1, ..Reconciled::default() };
    let row: Option<(String, i64, Option<String>)> = conn
        .query_row("SELECT content, ts, thread_key FROM event_log WHERE id = ?1", [event_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .optional()?;
    let Some((content, ts, thread)) = row else { return Ok(done) };
    let Some(thread) = thread else { return reconcile_unthreaded_event(conn, event_id, &content, ts, now) };
    let outcomes = detect(&content, ts);
    if outcomes.is_empty() {
        return Ok(done);
    }
    for outcome in &outcomes {
        // Re-read per outcome: an earlier outcome of this event may have closed a row.
        let targets = thread_targets(conn, &thread, event_id, ts)?;
        let Some(target) = pick(&targets, outcome) else { continue };
        let (recorded, applied) = record(conn, target, event_id, outcome, now)?;
        done.recorded += usize::from(recorded);
        if applied {
            match outcome.kind {
                UpdateKind::Redated => done.redated += 1,
                _ => done.closed += 1,
            }
        }
        if applied {
            let _ = crate::search_documents::project_source_after_write(
                conn,
                &crate::state_edit::source_ref(target.table, target.id),
                now,
            );
        }
    }
    Ok(done)
}

/// The one open row anywhere a thread-less outcome names: at least
/// [`MIN_SHARED_WORDS_UNTHREADED`] shared content words and half of the outcome's own, and no other
/// open row sharing even [`MIN_SHARED_WORDS`] (two rows it could be about is one too many).
fn pick_unthreaded<'t>(targets: &'t [Target], outcome: &Outcome<'_>) -> Option<&'t Target> {
    let words = content_words(outcome.quote);
    let mut close = targets
        .iter()
        .map(|t| (t, content_words(&t.description).intersection(&words).count()))
        .filter(|(_, shared)| *shared >= MIN_SHARED_WORDS);
    let (target, shared) = close.next()?;
    if close.next().is_some() || shared < MIN_SHARED_WORDS_UNTHREADED || 2 * shared < words.len() {
        return None;
    }
    Some(target)
}

/// An event without a thread (see the module notes): never applied, never a re-date, never about a
/// local candidate; a resolution or cancellation that names exactly one consolidator-owned row is
/// recorded unread for the consolidator, once per (row, kind, quote) however often the text is
/// captured again. Sources the Batch lane may not see (meeting text) are skipped: the consolidator
/// would never be shown them.
fn reconcile_unthreaded_event(conn: &Connection, event_id: i64, content: &str, ts: i64, now: i64) -> Result<Reconciled, rusqlite::Error> {
    let mut done = Reconciled { scanned: 1, ..Reconciled::default() };
    let outcomes: Vec<Outcome<'_>> = detect(content, ts).into_iter().filter(|o| o.kind != UpdateKind::Redated).collect();
    if outcomes.is_empty() {
        return Ok(done);
    }
    let source: Option<String> = conn.query_row("SELECT source FROM event_log WHERE id = ?1", [event_id], |r| r.get(0)).optional()?;
    if source.is_some_and(|s| crate::event_log::BATCH_EXCLUDED_SOURCES.contains(&s.as_str())) {
        return Ok(done);
    }
    for outcome in &outcomes {
        let targets = targets(conn, None, event_id, ts)?;
        let Some(target) = pick_unthreaded(&targets, outcome) else { continue };
        if target.local {
            continue; // a local candidate changes only on its own thread's words
        }
        let Some(quote) = crate::sanitize::persist_generated(outcome.quote) else { continue };
        let seen: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM loop_updates WHERE state_table = ?1 AND state_id = ?2 AND kind = ?3 AND quote = ?4)",
            params![target.table.as_str(), target.id, outcome.kind.as_str(), quote.text.as_ref()],
            |r| r.get(0),
        )?;
        if seen {
            continue; // the same words captured again
        }
        let (recorded, applied) = record(conn, target, event_id, outcome, now)?;
        debug_assert!(!applied, "a consolidator-owned row is never applied locally");
        done.recorded += usize::from(recorded);
    }
    Ok(done)
}

/// One pass over the events inserted since the last pass (at most [`SCAN_BATCH`]; threaded and
/// not, [`reconcile_event`]); the first pass after the upgrade starts [`FIRST_SCAN_LOOKBACK_MS`]
/// back. Idempotent.
pub fn reconcile(conn: &mut Connection, now: i64) -> Result<Reconciled, rusqlite::Error> {
    let tx = conn.savepoint()?;
    let watermark: Option<i64> = tx
        .query_row("SELECT last_event_id FROM loop_update_scan WHERE id = 1", [], |r| r.get(0))
        .optional()?;
    let from = match watermark {
        Some(id) => id,
        None => tx.query_row(
            "SELECT COALESCE((SELECT MIN(id) FROM event_log WHERE ts >= ?1) - 1, (SELECT MAX(id) FROM event_log), 0)",
            [now - FIRST_SCAN_LOOKBACK_MS],
            |r| r.get(0),
        )?,
    };
    let ids: Vec<i64> = tx
        .prepare("SELECT id FROM event_log WHERE id > ?1 ORDER BY id LIMIT ?2")?
        .query_map(params![from, SCAN_BATCH], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let mut total = Reconciled::default();
    for &id in &ids {
        let one = reconcile_event(&tx, id, now)?;
        total.scanned += one.scanned;
        total.recorded += one.recorded;
        total.closed += one.closed;
        total.redated += one.redated;
    }
    let last = ids.last().copied().unwrap_or(from);
    tx.execute(
        "INSERT INTO loop_update_scan (id, last_event_id) VALUES (1, ?1)
         ON CONFLICT (id) DO UPDATE SET last_event_id = MAX(last_event_id, excluded.last_event_id)",
        [last],
    )?;
    tx.commit()?;
    Ok(total)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::state::{
        insert_commitment_with_quote, insert_open_loop_with_quote, CommitmentDirection, CommitmentStatus, NewCommitment,
        NewOpenLoop, OpenLoopKind, Provenance,
    };

    const HOUR: i64 = 3_600_000;
    /// Monday 2026-09-07 10:00 UTC.
    const MON: i64 = 1_788_739_200_000 + 10 * HOUR;

    fn kinds(text: &str) -> Vec<(UpdateKind, &str)> {
        detect(text, MON).into_iter().map(|o| (o.kind, o.quote)).collect()
    }

    #[test]
    fn resolutions_cancellations_and_moves_are_read_with_their_exact_span() {
        for text in [
            "Sent the Q3 budget to Oren Vale.",
            "Done: the board memo is with Priya Nand now.",
            "Oren Vale sent the pricing deck over, thanks.",
            "Got the vendor contract from Kenji Arai.",
            "The latency report arrived from Dana Okafor, thanks.",
            "Replied to Mateo Ruiz about the customer FAQ.",
            "Finished the review of the design spec for Sven Morrow.",
            "Review of the test plan done and sent back to Noor Haddad.",
            "Decided: Heron ships with the rollout plan.",
            "Settled it, Kestrel goes out with the audit evidence.",
        ] {
            assert_eq!(kinds(text), vec![(UpdateKind::Resolved, text.trim_end_matches('.'))], "{text}");
        }
        for text in [
            "Never mind about the hiring scorecard for Felix Dorn, it is no longer needed.",
            "Dropping the partner brief for Amara Quist; the plan changed.",
        ] {
            assert_eq!(kinds(text)[0].0, UpdateKind::Cancelled, "{text}");
        }
        let moved = detect("Hi Oren,\n\nUpdate: the board memo for Oren Vale moves to Thursday.\n\nThanks", MON);
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].kind, UpdateKind::Redated);
        assert_eq!(moved[0].quote, "Update: the board memo for Oren Vale moves to Thursday");
        // Thursday after Monday 2026-09-07 is 2026-09-10.
        let day = crate::due::civil_from_days(moved[0].due_at.unwrap().div_euclid(86_400_000));
        assert_eq!((day.month, day.day), (9, 10));
        for text in ["Heads up, the memo for Oren slips to September 21.", "Change of plan, the memo for Oren is now due Friday."] {
            assert_eq!(kinds(text)[0].0, UpdateKind::Redated, "{text}");
        }
    }

    #[test]
    fn plans_questions_denials_and_old_news_are_not_outcomes() {
        for text in [
            "I'll send the Q3 budget to Oren by Friday.",
            "Has Oren sent the deck yet?",
            "Oren hasn't sent the deck.",
            "Still waiting on Oren for the deck.",
            "Last month I sent the renewal quote to Lucia as promised.",
            "Back in August Felix got the training schedule from me, all settled.",
            "I sent a reminder to Oren about the deck.",
            "If the memo slips to Friday, tell me.",
            "Maybe the launch moves to Thursday.",
            "Waiting for Priya to send the API migration notes.",
            "lunch at noon?",
        ] {
            assert!(detect(text, MON).is_empty(), "{text}: {:?}", detect(text, MON));
        }
        // A move with no date the parser reads is not a re-date.
        assert!(detect("The memo moves to the next phase.", MON).is_empty());
    }

    #[test]
    fn content_words_drop_function_words_cues_and_dates() {
        let words = content_words("Update: the Q3 budget for Oren Vale moves to Thursday.");
        let mut sorted: Vec<_> = words.into_iter().collect();
        sorted.sort();
        assert_eq!(sorted, vec!["budget", "oren", "q3", "vale"]);
    }

    fn event(conn: &Connection, n: i64, ts: i64, thread: Option<&str>, content: &str) -> i64 {
        let id = crate::event_log::insert(
            conn,
            &crate::event_log::NewEvent {
                ts,
                source: "agent",
                kind: "email",
                app_bundle_id: None,
                window_title: Some("loop"),
                content,
                content_hash: &format!("h{n}"),
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap();
        conn.execute("UPDATE event_log SET thread_key = ?2 WHERE id = ?1", params![id, thread]).unwrap();
        id
    }

    fn commitment(conn: &mut Connection, ev: i64, text: &str, due: Option<i64>, confidence: f64) -> i64 {
        insert_commitment_with_quote(
            conn,
            &NewCommitment {
                direction: CommitmentDirection::Mine,
                counterparty_id: None,
                description: text,
                due_at: due,
                status: CommitmentStatus::Open,
                project_id: None,
                confidence,
                now: MON,
            },
            &[Provenance::new(ev)],
            Some(text),
        )
        .unwrap()
    }

    fn status(conn: &Connection, table: &str, id: i64) -> (String, f64, Option<i64>) {
        let due = if table == "commitments" { "due_at" } else { "NULL" };
        conn.query_row(&format!("SELECT status, confidence, {due} FROM {table} WHERE id = ?1"), [id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap()
    }

    #[test]
    fn a_later_message_in_the_thread_closes_or_moves_a_local_candidate_without_lifting_it() {
        let mut conn = crate::open_in_memory().unwrap();
        let said = "I'll send the Q3 budget to Oren Vale by Friday.";
        let e1 = event(&conn, 1, MON, Some("t1"), said);
        let due = crate::due::due_at(said, MON);
        let c = commitment(&mut conn, e1, said, due, 0.35);
        let promise = "Can you confirm the numbers in the customer FAQ?";
        let e2 = event(&conn, 2, MON + HOUR, Some("t2"), promise);
        let l = insert_open_loop_with_quote(
            &mut conn,
            &NewOpenLoop { kind: OpenLoopKind::ReplyNeeded, description: promise, counterparty_id: None, project_id: None, opened_at: MON, confidence: 0.3, now: MON },
            &[Provenance::new(e2)],
            Some(promise),
        )
        .unwrap();
        // Moved, then done; the loop is answered in its own thread.
        event(&conn, 3, MON + 2 * HOUR, Some("t1"), "Update: the Q3 budget for Oren Vale moves to Thursday.");
        let out = reconcile(&mut conn, MON + 3 * HOUR).unwrap();
        assert_eq!((out.recorded, out.redated, out.closed), (1, 1, 0), "{out:?}");
        let (st, conf, due) = status(&conn, "commitments", c);
        assert_eq!((st.as_str(), conf), ("open", 0.35), "a move never lifts confidence");
        let day = crate::due::civil_from_days(due.unwrap().div_euclid(86_400_000));
        assert_eq!((day.month, day.day), (9, 10));
        let e4 = event(&conn, 4, MON + 4 * HOUR, Some("t1"), "Sent the Q3 budget to Oren Vale.");
        event(&conn, 5, MON + 4 * HOUR, Some("t2"), "Replied to Oren about the customer FAQ.");
        let out = reconcile(&mut conn, MON + 5 * HOUR).unwrap();
        assert_eq!(out.closed, 2, "{out:?}");
        assert_eq!(status(&conn, "commitments", c).0, "done");
        assert_eq!(status(&conn, "commitments", c).1, 0.35);
        assert_eq!(status(&conn, "open_loops", l).0, "closed");
        // The outcome's quote is the row's provenance, never a Batch review.
        let (quote, reviewed): (String, i64) = conn
            .query_row(
                "SELECT quote, batch_reviewed FROM state_provenance WHERE state_table = 'commitments' AND state_id = ?1 AND event_id = ?2",
                params![c, e4],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((quote.as_str(), reviewed), ("Sent the Q3 budget to Oren Vale", 0));
        // Idempotent: a second pass over nothing new records nothing.
        assert_eq!(reconcile(&mut conn, MON + 6 * HOUR).unwrap(), Reconciled::default());
        let updates: i64 = conn.query_row("SELECT count(*) FROM loop_updates WHERE applied = 1", [], |r| r.get(0)).unwrap();
        assert_eq!(updates, 3);
    }

    #[test]
    fn another_thread_an_unrelated_sentence_or_a_tie_changes_nothing() {
        let mut conn = crate::open_in_memory().unwrap();
        let said = "I'll send the Q3 budget to Oren Vale by Friday.";
        let e1 = event(&conn, 1, MON, Some("t1"), said);
        let c = commitment(&mut conn, e1, said, None, 0.35);
        event(&conn, 2, MON + HOUR, Some("t9"), "Sent the Q3 budget to Oren Vale.");
        event(&conn, 3, MON + HOUR, None, "Sent the Q3 budget to Oren Vale.");
        event(&conn, 4, MON + HOUR, Some("t1"), "Sent the pricing deck to Priya Nand.");
        // Two rows of one thread the same outcome fits equally: refused.
        let said2 = "I'll send the Q3 budget to Oren Vale by Monday.";
        let e5 = event(&conn, 5, MON, Some("t3"), said);
        let e6 = event(&conn, 6, MON, Some("t3"), said2);
        let a = commitment(&mut conn, e5, said, None, 0.35);
        let b = commitment(&mut conn, e6, said2, None, 0.35);
        event(&conn, 7, MON + HOUR, Some("t3"), "Sent the Q3 budget to Oren Vale.");
        let out = reconcile(&mut conn, MON + 2 * HOUR).unwrap();
        assert_eq!(out.closed, 0, "{out:?}");
        for id in [c, a, b] {
            assert_eq!(status(&conn, "commitments", id).0, "open");
        }
    }

    #[test]
    fn a_reviewed_or_pinned_row_is_left_to_the_consolidator_or_the_user() {
        let mut conn = crate::open_in_memory().unwrap();
        let said = "I'll send the Q3 budget to Oren Vale by Friday.";
        let e1 = event(&conn, 1, MON, Some("t1"), said);
        let reviewed = commitment(&mut conn, e1, said, None, 0.9);
        let said2 = "I'll send the board memo to Priya Nand by Friday.";
        let e2 = event(&conn, 2, MON, Some("t2"), said2);
        let pinned = commitment(&mut conn, e2, said2, None, 0.35);
        conn.execute("UPDATE commitments SET user_pinned = 1 WHERE id = ?1", [pinned]).unwrap();
        event(&conn, 3, MON + HOUR, Some("t1"), "Sent the Q3 budget to Oren Vale.");
        event(&conn, 4, MON + HOUR, Some("t2"), "Sent the board memo to Priya Nand.");
        let out = reconcile(&mut conn, MON + 2 * HOUR).unwrap();
        assert_eq!((out.recorded, out.closed), (2, 0), "{out:?}");
        assert_eq!(status(&conn, "commitments", reviewed).0, "open");
        assert_eq!(status(&conn, "commitments", pinned).0, "open");
        let unread: i64 = conn
            .query_row("SELECT count(*) FROM loop_updates WHERE applied = 0 AND read_at IS NULL", [], |r| r.get(0))
            .unwrap();
        assert_eq!(unread, 2, "kept for the consolidator");
    }

    #[test]
    fn the_first_pass_starts_a_bounded_window_back() {
        let mut conn = crate::open_in_memory().unwrap();
        let old = MON - 40 * 86_400_000;
        let said = "I'll send the Q3 budget to Oren Vale by Friday.";
        let e1 = event(&conn, 1, old, Some("t1"), said);
        let c = commitment(&mut conn, e1, said, None, 0.35);
        event(&conn, 2, old + HOUR, Some("t1"), "Sent the Q3 budget to Oren Vale.");
        let out = reconcile(&mut conn, MON).unwrap();
        assert_eq!(out.closed, 0, "older than the first-pass window: {out:?}");
        assert_eq!(status(&conn, "commitments", c).0, "open");
    }

    fn unread(conn: &Connection) -> Vec<(String, i64, String)> {
        conn.prepare("SELECT kind, state_id, quote FROM loop_updates WHERE applied = 0 AND read_at IS NULL ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    /// E49: text without a thread (screen capture) never closes or re-dates a row. A resolution
    /// that names exactly one consolidator-owned row is left unread for the consolidator, once;
    /// a local candidate, a re-date, a weak or ambiguous match and meeting text leave nothing.
    #[test]
    fn text_without_a_thread_only_leaves_an_unambiguous_lead_for_the_consolidator() {
        let mut conn = crate::open_in_memory().unwrap();
        let local_said = "I'll send the Q3 budget to Oren Vale by Friday.";
        let e1 = event(&conn, 1, MON, Some("t1"), local_said);
        let local = commitment(&mut conn, e1, local_said, None, 0.35);
        let reviewed_said = "I'll send the board memo to Priya Nand by Friday.";
        let e2 = event(&conn, 2, MON, Some("t2"), reviewed_said);
        let reviewed = commitment(&mut conn, e2, reviewed_said, crate::due::due_at(reviewed_said, MON), 0.9);

        // A local candidate is not touched by text without a thread, however clear.
        event(&conn, 3, MON + HOUR, None, "Sent the Q3 budget to Oren Vale.");
        // A re-date read off a screen is never taken: the capture time is not when it was said.
        event(&conn, 4, MON + HOUR, None, "Update: the board memo for Priya Nand moves to Thursday.");
        // Too few shared words to name the row.
        event(&conn, 5, MON + HOUR, None, "Sent it to Priya.");
        let out = reconcile(&mut conn, MON + 2 * HOUR).unwrap();
        assert_eq!((out.recorded, out.closed, out.redated), (0, 0, 0), "{out:?}");
        assert_eq!(status(&conn, "commitments", local).0, "open");
        assert!(unread(&conn).is_empty());

        // The resolution of the consolidator's row is left for it, unapplied; the row stays open.
        let done = "Sent the board memo to Priya Nand";
        event(&conn, 6, MON + 3 * HOUR, None, &format!("{done}. Next: lunch."));
        // The same words captured again in a later frame are not a second lead.
        event(&conn, 7, MON + 3 * HOUR + 60_000, None, &format!("{done}. Next: lunch at noon."));
        let out = reconcile(&mut conn, MON + 4 * HOUR).unwrap();
        assert_eq!((out.recorded, out.closed), (1, 0), "{out:?}");
        assert_eq!(unread(&conn), vec![("resolved".to_string(), reviewed, done.to_string())]);
        let (st, conf, _) = status(&conn, "commitments", reviewed);
        assert_eq!((st.as_str(), conf), ("open", 0.9), "only the consolidator closes it, through its own quote");
        let input = crate::loops::consolidation_input(&conn, MON + 4 * HOUR).unwrap().unwrap();
        assert!(input.text.contains(&format!("C{reviewed} | resolved |")), "{}", input.text);

        // Two open rows the words could be about: nothing.
        let other_said = "I'll send the board deck to Priya Nand by Monday.";
        let e8 = event(&conn, 8, MON + 5 * HOUR, Some("t3"), other_said);
        commitment(&mut conn, e8, other_said, None, 0.9);
        event(&conn, 9, MON + 6 * HOUR, None, "Sent the board stuff to Priya Nand.");
        let out = reconcile(&mut conn, MON + 7 * HOUR).unwrap();
        assert_eq!(out.recorded, 0, "{out:?}");
    }

    /// Meeting text never reaches the Batch lane, so it leaves no lead for the consolidator.
    #[test]
    fn meeting_text_without_a_thread_leaves_no_lead() {
        let mut conn = crate::open_in_memory().unwrap();
        let said = "I'll send the board memo to Priya Nand by Friday.";
        let e1 = event(&conn, 1, MON, Some("t1"), said);
        let reviewed = commitment(&mut conn, e1, said, None, 0.9);
        crate::event_log::insert(
            &conn,
            &crate::event_log::NewEvent {
                ts: MON + HOUR,
                source: "meeting",
                kind: "transcript",
                app_bundle_id: None,
                window_title: None,
                content: "Sent the board memo to Priya Nand.",
                content_hash: "m1",
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap();
        let out = reconcile(&mut conn, MON + 2 * HOUR).unwrap();
        assert_eq!(out.recorded, 0, "{out:?}");
        assert_eq!(status(&conn, "commitments", reviewed).0, "open");
    }
}
