//! `[Track]` on a Recap next action (FR-MT-17, issue #253).
//!
//! The Recap lists "who owes what" as *candidates*: the model read them out of the transcript and
//! nothing about them is confirmed. The one honest way for such a candidate to become a High-band
//! fact is the user saying so. Tracking one is that act — a user-explicit edit (§6.4 FR-ST-03) —
//! and only then does a commitment enter state at confidence 1.0.
//!
//! **What is recorded.** The tap itself becomes an `event_log` row (`source = 'meeting'`,
//! `kind = 'track'`, attached to the session): that row is the provenance the commitment rests on,
//! as §6.4 requires of an explicit edit. It keeps the meeting source so the Batch lane's source
//! filter still holds — the action text came out of a transcript (A-2 decision). The session's
//! transcript row, when there is one, is linked too at a lower weight, so "Why?" can walk from the
//! commitment down to what was said (FR-MT-18).
//!
//! **Only candidates.** The text must be one of the session's stored next actions. An API caller
//! cannot use this to write arbitrary 1.0 commitments; it can only confirm what the Recap offered.
//!
//! **Idempotent.** Tracking the same action twice returns the first commitment.
//!
//! **Confirms rather than duplicates.** If nightly local extraction already inferred the same
//! promise from this meeting (a low-confidence row citing one of the session's events), that row
//! is promoted to 1.0 instead of a second row being written beside it.
//!
//! **Owner.** The action's owner is resolved against `people` (display name or alias). No owner or
//! a first-person owner means the user's own commitment; a named owner makes it theirs, with the
//! person as counterparty when one matches. The caller may override both — that is still the
//! user's explicit choice. A disagreement with an existing reading of the same promise is kept as
//! a dispute (`disputes::check_new_commitment`), never overwritten.

use rusqlite::{params, Connection, OptionalExtension};

use crate::event_log::{content_hash, insert, NewEvent};
use crate::state::{
    insert_commitment, CommitmentDirection, CommitmentStatus, NewCommitment, Provenance,
};
use crate::MemoryError;

/// The `kind` of the event a `[Track]` tap writes.
pub const TRACK_KIND: &str = "track";

/// Weight of the transcript link: supporting evidence, not the reason the row exists.
const TRANSCRIPT_WEIGHT: f64 = 0.5;

/// Owners that mean the user themself.
const SELF_OWNERS: &[&str] =
    &["me", "i", "myself", "self", "私", "わたし", "自分", "僕", "ぼく", "俺", "おれ"];

/// A `[Track]` request. `direction` / `counterparty_id` override the owner resolution.
#[derive(Debug, Clone)]
pub struct TrackRequest<'a> {
    pub session_id: i64,
    pub text: &'a str,
    pub direction: Option<CommitmentDirection>,
    pub counterparty_id: Option<i64>,
    pub now: i64,
}

/// What a `[Track]` produced.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Tracked {
    pub commitment_id: i64,
    /// True when this action had already been tracked; nothing new was written.
    pub already_tracked: bool,
    /// True when an inferred row for the same promise was confirmed instead of a new row written.
    pub promoted: bool,
    /// A dispute raised against an existing reading of the same promise, if any.
    pub dispute_id: Option<i64>,
}

#[derive(Debug, thiserror::Error)]
pub enum TrackError {
    #[error("meeting not found")]
    NotFound,
    #[error("not one of this meeting's suggested next actions")]
    NotACandidate,
    #[error(transparent)]
    Memory(#[from] MemoryError),
}

impl From<rusqlite::Error> for TrackError {
    fn from(e: rusqlite::Error) -> Self {
        TrackError::Memory(MemoryError::Sqlite(e))
    }
}

#[derive(serde::Deserialize)]
struct StoredAction {
    text: String,
    #[serde(default)]
    owner: Option<String>,
}

fn stored_actions(conn: &Connection, session_id: i64) -> Result<Vec<StoredAction>, rusqlite::Error> {
    Ok(crate::meeting_recaps::get(conn, session_id)?
        .and_then(|r| serde_json::from_str(&r.next_actions_json).ok())
        .unwrap_or_default())
}

/// The dedup key of one tracked action: session + the stored action text.
fn track_key(session_id: i64, text: &str) -> String {
    content_hash(&format!("recap-track\u{1f}{session_id}\u{1f}{text}"))
}

/// The commitment an action of this session was tracked as, if it still exists.
fn tracked_commitment(
    conn: &Connection,
    session_id: i64,
    text: &str,
) -> Result<Option<i64>, rusqlite::Error> {
    conn.query_row(
        "SELECT c.id FROM event_log e
           JOIN state_provenance p ON p.event_id = e.id AND p.state_table = 'commitments'
           JOIN commitments c ON c.id = p.state_id
          WHERE e.session_id = ?1 AND e.source = ?2 AND e.kind = ?3 AND e.content_hash = ?4
          ORDER BY c.id LIMIT 1",
        params![session_id, crate::meeting_index::SOURCE, TRACK_KIND, track_key(session_id, text)],
        |r| r.get(0),
    )
    .optional()
}

/// For each of the session's next actions (in stored order), the commitment it was tracked as.
pub fn tracked_for_session(
    conn: &Connection,
    session_id: i64,
) -> Result<Vec<Option<i64>>, rusqlite::Error> {
    stored_actions(conn, session_id)?
        .iter()
        .map(|a| tracked_commitment(conn, session_id, &a.text))
        .collect()
}

/// Resolve a Recap owner to a direction and, for someone else, the matching person.
pub fn resolve_owner(
    conn: &Connection,
    owner: Option<&str>,
) -> Result<(CommitmentDirection, Option<i64>), rusqlite::Error> {
    let Some(owner) = owner.map(str::trim).filter(|o| !o.is_empty()) else {
        return Ok((CommitmentDirection::Mine, None));
    };
    let lower = owner.to_lowercase();
    if SELF_OWNERS.contains(&lower.as_str()) {
        return Ok((CommitmentDirection::Mine, None));
    }
    let mut stmt = conn.prepare("SELECT id, display_name, aliases FROM people ORDER BY confidence DESC, id")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?))
    })?;
    let mut matches = Vec::new();
    for row in rows {
        let (id, name, aliases) = row?;
        let aliases: Vec<String> =
            aliases.as_deref().and_then(|a| serde_json::from_str(a).ok()).unwrap_or_default();
        if name.to_lowercase() == lower || aliases.iter().any(|a| a.to_lowercase() == lower) {
            matches.push(id);
        }
    }
    let Some(&first) = matches.first() else {
        return Ok((CommitmentDirection::Theirs, None));
    };
    // The user split these people apart (FR-ST-10): a name both answer to is ambiguous, and
    // picking one would re-fuse their commitments. Leave the counterparty for the user.
    for &other in &matches[1..] {
        if crate::state_split::is_merge_blocked(conn, crate::state::StateTable::People, first, other)? {
            return Ok((CommitmentDirection::Theirs, None));
        }
    }
    Ok((CommitmentDirection::Theirs, Some(first)))
}

/// An open commitment extraction already inferred from this meeting that reads as the same
/// promise — the row a `[Track]` should confirm rather than duplicate.
fn inferred_same_promise(
    conn: &Connection,
    session_id: i64,
    direction: CommitmentDirection,
    text: &str,
) -> Result<Option<i64>, rusqlite::Error> {
    let mine = crate::disputes::promise_tokens(text);
    let mut stmt = conn.prepare(
        "SELECT DISTINCT c.id, c.description FROM commitments c
           JOIN state_provenance p ON p.state_table = 'commitments' AND p.state_id = c.id
           JOIN event_log e ON e.id = p.event_id
          WHERE e.session_id = ?1 AND e.kind != ?2 AND c.direction = ?3
            AND c.status IN ('open', 'overdue') AND c.confidence < 1.0
          ORDER BY c.id",
    )?;
    let rows = stmt.query_map(params![session_id, TRACK_KIND, direction.as_str()], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (id, description) = row?;
        let sim = crate::disputes::similarity(&mine, &crate::disputes::promise_tokens(&description));
        if sim >= crate::disputes::SAME_PROMISE_SIMILARITY {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

/// Track one Recap next action as a commitment (FR-MT-17).
pub fn track(conn: &mut Connection, req: &TrackRequest<'_>) -> Result<Tracked, TrackError> {
    let session = crate::session::get(conn, req.session_id)?.ok_or(TrackError::NotFound)?;
    let wanted = req.text.trim();
    let action = stored_actions(conn, session.id)?
        .into_iter()
        .find(|a| a.text.trim() == wanted && !wanted.is_empty())
        .ok_or(TrackError::NotACandidate)?;

    if let Some(id) = tracked_commitment(conn, session.id, &action.text)? {
        return Ok(Tracked { commitment_id: id, already_tracked: true, promoted: false, dispute_id: None });
    }

    let (resolved_direction, resolved_counterparty) = resolve_owner(conn, action.owner.as_deref())?;
    let direction = req.direction.unwrap_or(resolved_direction);
    // A counterparty resolved for the other direction does not carry over to an override.
    let counterparty_id = req
        .counterparty_id
        .or(if direction == resolved_direction { resolved_counterparty } else { None });
    // The deadline is read against when it was said, not when it was tracked.
    let due_at = crate::due::due_at(&action.text, session.started_at);

    let transcript_event: Option<i64> = conn
        .query_row(
            "SELECT id FROM event_log WHERE session_id = ?1 AND source = ?2 AND kind = 'transcript'
              ORDER BY id LIMIT 1",
            params![session.id, crate::meeting_index::SOURCE],
            |r| r.get(0),
        )
        .optional()?;
    let promote = inferred_same_promise(conn, session.id, direction, &action.text)?;

    let tx = conn.savepoint()?;
    let hash = track_key(session.id, &action.text);
    let event_id = insert(
        &tx,
        &NewEvent {
            ts: req.now,
            source: crate::meeting_index::SOURCE,
            kind: TRACK_KIND,
            app_bundle_id: None,
            window_title: session.title.as_deref(),
            content: &action.text,
            content_hash: &hash,
            dwell_ms: 0,
            display_id: None,
            window_bounds: None,
        },
    )?;
    crate::session::attach_event(&tx, session.id, event_id)?;
    tx.commit()?;

    let mut provenance = vec![Provenance::new(event_id)];
    if let Some(t) = transcript_event {
        provenance.push(Provenance { event_id: t, weight: TRANSCRIPT_WEIGHT });
    }

    let (commitment_id, promoted) = match promote {
        Some(id) => {
            let tx = conn.savepoint()?;
            tx.execute(
                "UPDATE commitments
                    SET confidence = 1.0, base_confidence = 1.0, description = ?2,
                        due_at = COALESCE(due_at, ?3),
                        counterparty_id = COALESCE(?4, counterparty_id),
                        updated_at = ?5, last_evidence_at = ?5
                  WHERE id = ?1",
                params![id, action.text, due_at, counterparty_id, req.now],
            )?;
            for p in &provenance {
                tx.execute(
                    "INSERT OR IGNORE INTO state_provenance (state_table, state_id, event_id, weight)
                     VALUES ('commitments', ?1, ?2, ?3)",
                    params![id, p.event_id, p.weight],
                )?;
            }
            tx.commit()?;
            let _ = crate::search_documents::project_source_after_write(
                conn,
                &crate::search_documents::SourceRef::Commitment(id),
                req.now,
            );
            (id, true)
        }
        None => {
            let id = insert_commitment(
                conn,
                &NewCommitment {
                    direction,
                    counterparty_id,
                    description: &action.text,
                    due_at,
                    status: CommitmentStatus::Open,
                    project_id: None,
                    confidence: 1.0,
                    now: req.now,
                },
                &provenance,
            )?;
            (id, false)
        }
    };
    let dispute_id = crate::disputes::check_new_commitment(conn, commitment_id, req.now)?;
    Ok(Tracked { commitment_id, already_tracked: false, promoted, dispute_id })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{close, open, NewSession};

    const DAY: i64 = 24 * 60 * 60 * 1000;
    /// Wednesday 2026-09-23 10:00 UTC.
    const WED: i64 = 20_719 * DAY + 10 * 3_600_000;

    fn meeting(conn: &Connection, actions: &str) -> i64 {
        let id = open(
            conn,
            &NewSession {
                kind: "meeting",
                started_at: WED,
                title: Some("Launch sync"),
                app_bundle_id: Some("us.zoom.xos"),
                calendar_occurrence_id: None,
                confidence: 0.7,
                provenance: "{}",
            },
        )
        .unwrap();
        close(conn, id, WED + 3_600_000).unwrap();
        crate::meeting_recaps::save(conn, id, "summary", "[]", actions, None, "m", WED + 3_700_000)
            .unwrap();
        id
    }

    fn person(conn: &mut Connection, name: &str, aliases: &str) -> i64 {
        crate::state::insert_person(
            conn,
            &crate::state::NewPerson {
                display_name: name,
                aliases_json: Some(aliases),
                confidence: 0.9,
                now: 1,
                ..Default::default()
            },
            &[Provenance::new(any_event(conn))],
        )
        .unwrap()
    }

    fn any_event(conn: &Connection) -> i64 {
        insert(
            conn,
            &NewEvent {
                ts: 1,
                source: "capture",
                kind: "text",
                app_bundle_id: None,
                window_title: None,
                content: "x",
                content_hash: "x",
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap()
    }

    fn req(session_id: i64, text: &str) -> TrackRequest<'_> {
        TrackRequest { session_id, text, direction: None, counterparty_id: None, now: WED + 4_000_000 }
    }

    fn commitment(conn: &Connection, id: i64) -> (String, Option<i64>, Option<i64>, f64) {
        conn.query_row(
            "SELECT direction, counterparty_id, due_at, confidence FROM commitments WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap()
    }

    /// FR-ST-10: after the user splits two people apart, a name both answer to is ambiguous —
    /// resolving it to either would fuse their commitments again.
    #[test]
    fn a_name_shared_by_split_people_resolves_to_no_one() {
        let mut conn = crate::open_in_memory().unwrap();
        let ev = |conn: &Connection, h: &str| {
            insert(
                conn,
                &NewEvent {
                    ts: 1,
                    source: "capture",
                    kind: "text",
                    app_bundle_id: None,
                    window_title: None,
                    content: h,
                    content_hash: h,
                    dwell_ms: 0,
                    display_id: None,
                    window_bounds: None,
                },
            )
            .unwrap()
        };
        let (e1, e2) = (ev(&conn, "a"), ev(&conn, "b"));
        let alex = crate::state::insert_person(
            &mut conn,
            &crate::state::NewPerson {
                display_name: "Alex",
                aliases_json: Some(r#"["Alex M"]"#),
                confidence: 0.9,
                now: 1,
                ..Default::default()
            },
            &[Provenance::new(e1), Provenance::new(e2)],
        )
        .unwrap();
        assert_eq!(resolve_owner(&conn, Some("alex")).unwrap().1, Some(alex));
        let split = crate::state_split::split(
            &mut conn,
            &crate::state_split::SplitRequest {
                table: crate::state::StateTable::People,
                id: alex,
                name: "Alex".into(),
                event_ids: vec![e2],
                aliases: vec!["Alex M".into()],
            },
            10,
        )
        .unwrap()
        .unwrap();
        assert_eq!(resolve_owner(&conn, Some("Alex")).unwrap(), (CommitmentDirection::Theirs, None));
        // the alias that moved still names exactly one of them
        assert_eq!(resolve_owner(&conn, Some("alex m")).unwrap().1, Some(split.new_id));
    }

    #[test]
    fn untracked_candidates_never_reach_state() {
        let conn = crate::open_in_memory().unwrap();
        let sid = meeting(&conn, r#"[{"text":"Send the deck by Friday","owner":null}]"#);
        let n: i64 = conn.query_row("SELECT count(*) FROM commitments", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0, "a Recap candidate is not state until the user tracks it");
        assert_eq!(tracked_for_session(&conn, sid).unwrap(), vec![None]);
    }

    #[test]
    fn tracking_writes_a_confirmed_commitment_resting_on_the_tap() {
        let mut conn = crate::open_in_memory().unwrap();
        let sid = meeting(&conn, r#"[{"text":"Send the deck by Friday","owner":"Me"}]"#);
        let t = track(&mut conn, &req(sid, "Send the deck by Friday")).unwrap();
        assert!(!t.already_tracked && !t.promoted);
        let (direction, counterparty, due, confidence) = commitment(&conn, t.commitment_id);
        assert_eq!(direction, "mine");
        assert_eq!(counterparty, None);
        assert_eq!(confidence, 1.0);
        assert!(due.is_some(), "the deadline is read against the meeting's time");
        let kind: String = conn
            .query_row(
                "SELECT e.kind FROM state_provenance p JOIN event_log e ON e.id = p.event_id
                  WHERE p.state_table = 'commitments' AND p.state_id = ?1 AND p.weight = 1.0",
                [t.commitment_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kind, TRACK_KIND);
        assert_eq!(tracked_for_session(&conn, sid).unwrap(), vec![Some(t.commitment_id)]);

        let again = track(&mut conn, &req(sid, "  Send the deck by Friday ")).unwrap();
        assert!(again.already_tracked);
        assert_eq!(again.commitment_id, t.commitment_id);
    }

    #[test]
    fn a_named_owner_becomes_their_commitment_with_the_person() {
        let mut conn = crate::open_in_memory().unwrap();
        let alice = person(&mut conn, "Alice Tanaka", r#"["Alice"]"#);
        let sid = meeting(
            &conn,
            r#"[{"text":"Review the contract","owner":"alice"},{"text":"Book the room","owner":"Bob"}]"#,
        );
        let a = track(&mut conn, &req(sid, "Review the contract")).unwrap();
        assert_eq!(commitment(&conn, a.commitment_id).0, "theirs");
        assert_eq!(commitment(&conn, a.commitment_id).1, Some(alice));
        let b = track(&mut conn, &req(sid, "Book the room")).unwrap();
        assert_eq!(commitment(&conn, b.commitment_id).0, "theirs");
        assert_eq!(commitment(&conn, b.commitment_id).1, None, "no match is no guess");
    }

    #[test]
    fn the_caller_can_override_the_owner() {
        let mut conn = crate::open_in_memory().unwrap();
        let sid = meeting(&conn, r#"[{"text":"Review the contract","owner":"Bob"}]"#);
        let mut r = req(sid, "Review the contract");
        r.direction = Some(CommitmentDirection::Mine);
        let t = track(&mut conn, &r).unwrap();
        assert_eq!(commitment(&conn, t.commitment_id).0, "mine");
    }

    #[test]
    fn only_the_recaps_own_candidates_can_be_tracked() {
        let mut conn = crate::open_in_memory().unwrap();
        let sid = meeting(&conn, r#"[{"text":"Send the deck","owner":null}]"#);
        assert!(matches!(track(&mut conn, &req(sid, "Wire $1M")), Err(TrackError::NotACandidate)));
        assert!(matches!(track(&mut conn, &req(sid + 99, "Send the deck")), Err(TrackError::NotFound)));
    }

    #[test]
    fn an_inferred_row_from_the_same_meeting_is_confirmed_not_duplicated() {
        let mut conn = crate::open_in_memory().unwrap();
        let sid = meeting(&conn, r#"[{"text":"Send the budget deck to finance","owner":"Me"}]"#);
        let said = any_event(&conn);
        crate::session::attach_event(&conn, sid, said).unwrap();
        let inferred = insert_commitment(
            &mut conn,
            &NewCommitment {
                direction: CommitmentDirection::Mine,
                counterparty_id: None,
                description: "I'll send the budget deck to finance",
                due_at: None,
                status: CommitmentStatus::Open,
                project_id: None,
                confidence: 0.4,
                now: WED,
            },
            &[Provenance::new(said)],
        )
        .unwrap();
        let t = track(&mut conn, &req(sid, "Send the budget deck to finance")).unwrap();
        assert!(t.promoted);
        assert_eq!(t.commitment_id, inferred);
        assert_eq!(commitment(&conn, inferred).3, 1.0);
        let n: i64 = conn.query_row("SELECT count(*) FROM commitments", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn a_different_owner_for_the_same_promise_is_kept_as_a_dispute() {
        let mut conn = crate::open_in_memory().unwrap();
        let sid = meeting(&conn, r#"[{"text":"Send the budget deck to finance","owner":"Bob"}]"#);
        // An earlier reading from elsewhere: the user promised it.
        let ev = any_event(&conn);
        let earlier = insert_commitment(
            &mut conn,
            &NewCommitment {
                direction: CommitmentDirection::Mine,
                counterparty_id: None,
                description: "I'll send the budget deck to finance",
                due_at: None,
                status: CommitmentStatus::Open,
                project_id: None,
                confidence: 0.6,
                now: WED - DAY,
            },
            &[Provenance::new(ev)],
        )
        .unwrap();
        let t = track(&mut conn, &req(sid, "Send the budget deck to finance")).unwrap();
        assert!(!t.promoted, "a row from another source is not this meeting's to confirm");
        let d = t.dispute_id.expect("owner disagreement is recorded");
        let (field, e, l): (String, i64, i64) = conn
            .query_row(
                "SELECT field, earlier_id, later_id FROM state_disputes WHERE id = ?1",
                [d],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((field.as_str(), e, l), ("owner", earlier, t.commitment_id));
    }
}
