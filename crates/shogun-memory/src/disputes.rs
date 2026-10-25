//! Contradiction flags on state (issue #253): disagreements are kept and shown, never overwritten.
//!
//! A world model that lives for years cannot let a new reading silently win over an old one. When
//! two open commitments read as the same promise but name different due dates ("send the report by
//! Friday", then "send the report by Monday"), both rows stay and a `state_disputes` row records
//! the disagreement until someone resolves it. Until then Fusion passes the pair as "possibly"
//! (see `shogun-fusion`), and the Context Health pane and the Memory API list it.
//!
//! **Deterministic only.** Same promise = same direction and a high overlap of content words
//! (Latin) or character bigrams (Japanese), after the date phrase itself is taken out. No model
//! call — the same rule as the rest of the capture path (invariant 5). A later stage may add a
//! Batch-lane judgement; this one would rather miss a dispute than invent one.

use std::collections::HashSet;

use rusqlite::{params, Connection, OptionalExtension};

use crate::state::CommitmentStatus;

/// Overlap at or above which two promise sentences are the same promise.
pub const SAME_PROMISE_SIMILARITY: f64 = 0.6;

/// Two due dates this far apart (or more) disagree. Both are end-of-day instants, so anything
/// short of a day is the same day seen across a DST change.
const DIFFERENT_DAY_MS: i64 = 12 * 60 * 60 * 1000;

/// The highest confidence a commitment in an open dispute is read back at: just under Fusion's
/// High band (0.8, `shogun_fusion::confidence`), so a disputed row is always passed as
/// "possibly:" and never as fact, however well-evidenced either side is on its own.
pub const DISPUTED_CONFIDENCE_CAP: f64 = 0.79;

/// SQL for a commitment's confidence as Fusion may see it: capped at [`DISPUTED_CONFIDENCE_CAP`]
/// while the row sits in an open dispute. Expects the row's table to be `commitments`.
///
/// A row the user edited (`user_pinned`, V49) is never capped: their statement stands, and the
/// disagreeing reading is the one passed as "possibly" (issue #290).
pub const COMMITMENT_CONFIDENCE_SQL: &str = "CASE WHEN commitments.user_pinned = 0 AND EXISTS (
        SELECT 1 FROM state_disputes d
         WHERE d.status = 'open' AND d.state_table = 'commitments'
           AND (d.earlier_id = commitments.id OR d.later_id = commitments.id))
    THEN MIN(commitments.confidence, 0.79) ELSE commitments.confidence END";

/// How many recent open commitments a new one is compared against.
const COMPARE_WINDOW: i64 = 200;

/// Words that carry no identity for a promise: pronouns, auxiliaries, and every word a deadline is
/// written with. Removing the deadline words is what lets "by Friday" and "by Monday" leave the
/// same promise behind.
const STOPWORDS: &[&str] = &[
    "i", "i'll", "ill", "we", "we'll", "you", "he", "she", "they", "it", "i'm", "will", "shall",
    "would", "can", "could", "should", "must", "going", "gonna", "to", "the", "a", "an", "and",
    "or", "of", "for", "with", "at", "in", "on", "by", "before", "until", "till", "due", "this",
    "next", "that", "my", "your", "our", "their", "his", "her", "me", "you", "get", "back", "be",
    "do", "is", "are", "am", "so", "just", "please", "then", "today", "tonight", "tomorrow", "day",
    "days", "week", "weeks", "month", "end", "eod", "eow", "eom", "after", "monday", "tuesday",
    "wednesday", "thursday", "friday", "saturday", "sunday", "mon", "tue", "tues", "wed", "thu",
    "thur", "thurs", "fri", "sat", "sun", "january", "february", "march", "april", "may", "june",
    "july", "august", "september", "sept", "october", "november", "december", "jan", "feb", "mar",
    "apr", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

/// Japanese deadline phrases, removed before comparing. Longest first so "明後日" goes before "明日".
const JA_DATE_PHRASES: &[&str] = &[
    "明後日", "あさって", "明日", "あした", "今日中", "今日", "本日", "今夜", "今週中", "今週末",
    "週末", "来週の", "来週", "月末", "までに", "まで", "日後", "曜日", "曜",
];

const JA_WEEKDAY_CHARS: &[char] = &['月', '火', '水', '木', '金', '土', '日'];

/// The identity-bearing tokens of a promise sentence: Latin content words plus bigrams of the
/// non-Latin text, with every deadline phrase removed.
pub fn promise_tokens(text: &str) -> HashSet<String> {
    let lower = text.to_lowercase();
    let mut tokens: HashSet<String> = lower
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '\''))
        .filter(|w| w.len() > 1 && !STOPWORDS.contains(w))
        .filter(|w| !w.chars().any(|c| c.is_ascii_digit()))
        .map(str::to_string)
        .collect();

    let mut ja = lower.clone();
    for phrase in JA_DATE_PHRASES {
        ja = ja.replace(phrase, " ");
    }
    // A weekday character left behind by "金曜" → "金 " is part of the date, not the promise.
    let ja: Vec<char> = ja
        .chars()
        .map(|c| if c.is_ascii() || JA_WEEKDAY_CHARS.contains(&c) { ' ' } else { c })
        .collect();
    for pair in ja.windows(2) {
        let word = |c: char| !c.is_whitespace() && !is_punct(c);
        if word(pair[0]) && word(pair[1]) {
            tokens.insert(pair.iter().collect());
        }
    }
    tokens
}

fn is_punct(c: char) -> bool {
    matches!(c, '、' | '。' | '！' | '？' | '「' | '」' | '・' | '（' | '）' | '：')
}

/// Jaccard overlap of two token sets; 0 when either is empty (nothing to be the same promise).
pub fn similarity(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let shared = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    shared / union
}

/// Who owes a commitment, as a dispute value: `mine`, `theirs`, or `theirs:<person id>`.
fn owner_value(direction: &str, counterparty: Option<i64>) -> String {
    match (direction, counterparty) {
        ("theirs", Some(id)) => format!("theirs:{id}"),
        (d, _) => d.to_string(),
    }
}

/// Whether two owner readings disagree. An unknown counterparty on someone else's side is not a
/// disagreement with a known one — only mine-vs-theirs, or two different known people, are.
fn owners_disagree(a: (&str, Option<i64>), b: (&str, Option<i64>)) -> bool {
    if a.0 != b.0 {
        return true;
    }
    a.0 == "theirs" && matches!((a.1, b.1), (Some(x), Some(y)) if x != y)
}

/// Compare a just-inserted commitment against the open ones before it, and record a dispute when
/// one of them is the same promise read differently: a different due date (`due_at`), or a
/// different owner (`owner` — issue #253: the Recap said Bob, earlier mail said you).
/// Returns the dispute id, if one was raised. Neither commitment is changed.
pub fn check_new_commitment(
    conn: &Connection,
    new_id: i64,
    now: i64,
) -> Result<Option<i64>, rusqlite::Error> {
    let Some((direction, counterparty, description, due)) = conn
        .query_row(
            "SELECT direction, counterparty_id, description, due_at FROM commitments WHERE id = ?1",
            [new_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                ))
            },
        )
        .optional()?
    else {
        return Ok(None);
    };
    let mine = promise_tokens(&description);
    if mine.is_empty() {
        return Ok(None);
    }

    let mut stmt = conn.prepare(
        "SELECT id, direction, counterparty_id, description, due_at FROM commitments
          WHERE id != ?1 AND status IN ('open', 'overdue')
          ORDER BY last_evidence_at DESC, id DESC
          LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![new_id, COMPARE_WINDOW], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<i64>>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, Option<i64>>(4)?,
        ))
    })?;
    for row in rows {
        let (other_id, other_dir, other_cp, other_desc, other_due) = row?;
        let (field, earlier_value, later_value) = if other_dir == direction {
            if owners_disagree((&other_dir, other_cp), (&direction, counterparty)) {
                ("owner", owner_value(&other_dir, other_cp), owner_value(&direction, counterparty))
            } else {
                match (other_due, due) {
                    // same day: agreement, not a dispute
                    (Some(o), Some(n)) if (o - n).abs() >= DIFFERENT_DAY_MS => {
                        ("due_at", o.to_string(), n.to_string())
                    }
                    _ => continue,
                }
            }
        } else {
            ("owner", owner_value(&other_dir, other_cp), owner_value(&direction, counterparty))
        };
        if similarity(&mine, &promise_tokens(&other_desc)) < SAME_PROMISE_SIMILARITY {
            continue;
        }
        // Most recent same-promise row only: one new reading raises at most one dispute.
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO state_disputes
               (state_table, field, earlier_id, later_id, earlier_value, later_value, created_at)
             VALUES ('commitments', ?1, ?2, ?3, ?4, ?5, ?6)",
            params![field, other_id, new_id, earlier_value, later_value, now],
        )?;
        return Ok((inserted != 0).then(|| conn.last_insert_rowid()));
    }
    Ok(None)
}

/// One side of a dispute: the commitment and the latest event it rests on.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DisputedCommitment {
    pub id: i64,
    pub description: String,
    pub due_at: Option<i64>,
    pub confidence: f64,
    /// The newest event this row cites (state_provenance), for "why do you think so".
    pub evidence_event_id: Option<i64>,
    pub evidence_ts: i64,
    /// `mine` or `theirs` — what an `owner` dispute disagrees about.
    pub direction: String,
    pub counterparty_id: Option<i64>,
    /// The counterparty's display name, when the row names a known person.
    pub counterparty_name: Option<String>,
}

/// An open disagreement between two commitments.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Dispute {
    pub id: i64,
    pub field: String,
    pub earlier: DisputedCommitment,
    pub later: DisputedCommitment,
    pub created_at: i64,
}

fn disputed_commitment(
    conn: &Connection,
    id: i64,
) -> Result<Option<DisputedCommitment>, rusqlite::Error> {
    conn.query_row(
        "SELECT c.id, c.description, c.due_at, c.confidence, c.last_evidence_at,
                (SELECT MAX(p.event_id) FROM state_provenance p
                  WHERE p.state_table = 'commitments' AND p.state_id = c.id),
                c.direction, c.counterparty_id,
                (SELECT display_name FROM people WHERE people.id = c.counterparty_id)
           FROM commitments c
          WHERE c.id = ?1 AND c.status IN ('open', 'overdue')",
        [id],
        |r| {
            Ok(DisputedCommitment {
                id: r.get(0)?,
                description: r.get(1)?,
                due_at: r.get(2)?,
                confidence: r.get(3)?,
                evidence_ts: r.get(4)?,
                evidence_event_id: r.get(5)?,
                direction: r.get(6)?,
                counterparty_id: r.get(7)?,
                counterparty_name: r.get(8)?,
            })
        },
    )
    .optional()
}

/// Open disputes whose two rows are both still open, newest first. A dispute whose row was
/// completed, cancelled or pruned in the meantime has nothing left to decide and is not listed.
pub fn list_open(conn: &Connection, limit: usize) -> Result<Vec<Dispute>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, field, earlier_id, later_id, created_at FROM state_disputes
          WHERE status = 'open' AND state_table = 'commitments'
          ORDER BY created_at DESC, id DESC",
    )?;
    let heads: Vec<(i64, String, i64, i64, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
        .collect::<Result<_, _>>()?;
    let mut out = Vec::new();
    for (id, field, earlier_id, later_id, created_at) in heads {
        if out.len() >= limit {
            break;
        }
        let (Some(earlier), Some(later)) =
            (disputed_commitment(conn, earlier_id)?, disputed_commitment(conn, later_id)?)
        else {
            continue;
        };
        out.push(Dispute { id, field, earlier, later, created_at });
    }
    Ok(out)
}

/// Ids of commitments that sit in an open dispute — what Fusion must not treat as fact.
pub fn disputed_commitment_ids(conn: &Connection) -> Result<HashSet<i64>, rusqlite::Error> {
    let mut ids = HashSet::new();
    for d in list_open(conn, usize::MAX)? {
        ids.insert(d.earlier.id);
        ids.insert(d.later.id);
    }
    Ok(ids)
}

/// How a dispute is settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// The earlier reading is right; the later commitment is cancelled.
    KeepEarlier,
    /// The later reading is right; the earlier commitment is cancelled.
    KeepLater,
    /// Neither is a real commitment; both are cancelled.
    DismissBoth,
    /// They were two different promises after all; both stay.
    KeepBoth,
}

impl Resolution {
    pub fn as_str(self) -> &'static str {
        match self {
            Resolution::KeepEarlier => "kept_earlier",
            Resolution::KeepLater => "kept_later",
            Resolution::DismissBoth => "dismissed_both",
            Resolution::KeepBoth => "kept_both",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "kept_earlier" | "keep_earlier" | "earlier" => Some(Resolution::KeepEarlier),
            "kept_later" | "keep_later" | "later" => Some(Resolution::KeepLater),
            "dismissed_both" | "dismiss_both" | "dismiss" => Some(Resolution::DismissBoth),
            "kept_both" | "keep_both" | "both" => Some(Resolution::KeepBoth),
            _ => None,
        }
    }
}

/// Settle an open dispute. Returns false when it is unknown or already resolved. The losing
/// commitment is cancelled, not deleted: its evidence stays in the log and it can be seen again.
pub fn resolve(
    conn: &mut Connection,
    dispute_id: i64,
    resolution: Resolution,
    now: i64,
) -> Result<bool, rusqlite::Error> {
    let Some((earlier, later)) = conn
        .query_row(
            "SELECT earlier_id, later_id FROM state_disputes
              WHERE id = ?1 AND status = 'open' AND state_table = 'commitments'",
            [dispute_id],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()?
    else {
        return Ok(false);
    };
    let cancel: &[i64] = match resolution {
        Resolution::KeepEarlier => &[later],
        Resolution::KeepLater => &[earlier],
        Resolution::DismissBoth => &[earlier, later],
        Resolution::KeepBoth => &[],
    };
    let tx = conn.transaction()?;
    for id in cancel {
        crate::state::set_commitment_status(&tx, *id, CommitmentStatus::Cancelled, now)?;
    }
    tx.execute(
        "UPDATE state_disputes SET status = 'resolved', resolution = ?1, resolved_at = ?2
          WHERE id = ?3",
        params![resolution.as_str(), now, dispute_id],
    )?;
    tx.commit()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_log::{insert as insert_event, NewEvent};
    use crate::extract::{extract, persist_candidates};

    const DAY: i64 = 24 * 60 * 60 * 1000;
    /// Wednesday 2026-09-23 10:00 UTC.
    const WED: i64 = 20_719 * DAY + 10 * 3_600_000;

    fn capture(conn: &mut Connection, text: &str, at: i64) -> Vec<i64> {
        let hash = crate::event_log::content_hash(text);
        let event = NewEvent {
            ts: at,
            source: "capture",
            kind: "text",
            app_bundle_id: None,
            window_title: None,
            content: text,
            content_hash: &hash,
            dwell_ms: 0,
            display_id: None,
            window_bounds: None,
        };
        let event_id = insert_event(conn, &event).unwrap();
        persist_candidates(conn, event_id, &extract(text), at, at).unwrap()
    }

    #[test]
    fn the_same_promise_with_a_moved_deadline_is_disputed_not_overwritten() {
        let mut conn = crate::open_in_memory().unwrap();
        let first = capture(&mut conn, "I'll send the report by Friday.", WED);
        let second = capture(&mut conn, "I'll send the report on Monday.", WED + 3_600_000);
        assert_eq!((first.len(), second.len()), (1, 1));

        let disputes = list_open(&conn, 10).unwrap();
        assert_eq!(disputes.len(), 1, "{disputes:?}");
        let d = &disputes[0];
        assert_eq!((d.earlier.id, d.later.id), (first[0], second[0]));
        assert_ne!(d.earlier.due_at, d.later.due_at);
        assert!(d.earlier.evidence_event_id.is_some() && d.later.evidence_event_id.is_some());

        // Neither row was touched: both are still open, each with its own date.
        let open: i64 = conn
            .query_row("SELECT count(*) FROM commitments WHERE status = 'open'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(open, 2);
        assert_eq!(disputed_commitment_ids(&conn).unwrap().len(), 2);
    }

    #[test]
    fn japanese_moved_deadlines_are_disputed_too() {
        let mut conn = crate::open_in_memory().unwrap();
        capture(&mut conn, "明日までに資料を送ります", WED);
        capture(&mut conn, "金曜までに資料を送ります", WED + 3_600_000);
        assert_eq!(list_open(&conn, 10).unwrap().len(), 1);
    }

    #[test]
    fn different_promises_or_the_same_day_are_not_disputes() {
        let mut conn = crate::open_in_memory().unwrap();
        capture(&mut conn, "I'll send the deck by Friday.", WED);
        capture(&mut conn, "I'll send the invoice by Monday.", WED + 1);
        capture(&mut conn, "明日までに資料を送ります", WED + 2);
        capture(&mut conn, "金曜までに請求書を送ります", WED + 3);
        // Same promise, same day, said twice.
        capture(&mut conn, "I'll review the contract tomorrow.", WED + 4);
        capture(&mut conn, "I'll review the contract by Thursday.", WED + 5);
        // No date on one side: new information, not a contradiction.
        capture(&mut conn, "I'll call the landlord.", WED + 6);
        capture(&mut conn, "I'll call the landlord on Monday.", WED + 7);
        assert!(list_open(&conn, 10).unwrap().is_empty(), "{:?}", list_open(&conn, 10));
    }

    #[test]
    fn owners_disagree_only_when_both_sides_are_known() {
        assert!(owners_disagree(("mine", None), ("theirs", None)));
        assert!(owners_disagree(("theirs", Some(1)), ("theirs", Some(2))));
        assert!(!owners_disagree(("theirs", Some(1)), ("theirs", None)), "unknown is not a claim");
        assert!(!owners_disagree(("theirs", Some(1)), ("theirs", Some(1))));
        assert!(!owners_disagree(("mine", None), ("mine", Some(3))));
        assert_eq!(owner_value("theirs", Some(7)), "theirs:7");
        assert_eq!(owner_value("mine", Some(7)), "mine");
    }

    #[test]
    fn resolving_cancels_the_losing_reading_and_closes_the_dispute() {
        let mut conn = crate::open_in_memory().unwrap();
        let a = capture(&mut conn, "I'll send the report by Friday.", WED)[0];
        let b = capture(&mut conn, "I'll send the report on Monday.", WED + 1)[0];
        let id = list_open(&conn, 10).unwrap()[0].id;

        assert!(resolve(&mut conn, id, Resolution::KeepLater, WED + 2).unwrap());
        let status = |id: i64| -> String {
            conn.query_row("SELECT status FROM commitments WHERE id = ?1", [id], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(status(a), "cancelled");
        assert_eq!(status(b), "open");
        assert!(list_open(&conn, 10).unwrap().is_empty());
        let again = resolve(&mut conn, id, Resolution::KeepEarlier, WED + 3).unwrap();
        assert!(!again, "already resolved");
    }

    #[test]
    fn a_dispute_whose_row_went_away_is_not_listed() {
        let mut conn = crate::open_in_memory().unwrap();
        let a = capture(&mut conn, "I'll send the report by Friday.", WED)[0];
        capture(&mut conn, "I'll send the report on Monday.", WED + 1);
        crate::state::set_commitment_status(&conn, a, CommitmentStatus::Done, WED + 2).unwrap();
        assert!(list_open(&conn, 10).unwrap().is_empty());
        assert!(disputed_commitment_ids(&conn).unwrap().is_empty());
    }

    #[test]
    fn fusion_reads_a_disputed_commitment_below_the_fact_band() {
        let mut conn = crate::open_in_memory().unwrap();
        let a = capture(&mut conn, "I'll send the report by Friday.", WED)[0];
        let b = capture(&mut conn, "I'll send the report on Monday.", WED + 1)[0];
        let calm = capture(&mut conn, "I'll call the landlord on Monday.", WED + 2)[0];
        conn.execute("UPDATE commitments SET confidence = 0.95", []).unwrap();

        let read = |conn: &Connection, id: i64| -> f64 {
            crate::state::list_commitments(conn)
                .unwrap()
                .into_iter()
                .find(|c| c.id == id)
                .map_or(-1.0, |c| c.confidence)
        };
        assert!(read(&conn, a) <= DISPUTED_CONFIDENCE_CAP);
        assert!(read(&conn, b) <= DISPUTED_CONFIDENCE_CAP);
        assert!((read(&conn, calm) - 0.95).abs() < 1e-9, "an undisputed row keeps its confidence");
        assert!(
            COMMITMENT_CONFIDENCE_SQL
                .contains(&format!("MIN(commitments.confidence, {DISPUTED_CONFIDENCE_CAP})")),
            "the SQL cap and the constant have drifted apart"
        );

        let id = list_open(&conn, 10).unwrap()[0].id;
        resolve(&mut conn, id, Resolution::KeepBoth, WED + 3).unwrap();
        assert!((read(&conn, a) - 0.95).abs() < 1e-9, "resolved: back to its own confidence");
    }

    #[test]
    fn resolutions_parse_from_the_api_words() {
        for r in [
            Resolution::KeepEarlier,
            Resolution::KeepLater,
            Resolution::DismissBoth,
            Resolution::KeepBoth,
        ] {
            assert_eq!(Resolution::parse(r.as_str()), Some(r));
        }
        assert_eq!(Resolution::parse("later"), Some(Resolution::KeepLater));
        assert_eq!(Resolution::parse("nonsense"), None);
    }
}
