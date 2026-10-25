//! State maintenance passes for the Dream Cycle (FR-ST-21, FR-DC-03). Pure DB effects, no model
//! call — these run in both the Full and Degraded sequences so state does not rot when a full cycle
//! is missed. All updates are idempotent given the same `now` (they recompute an absolute value from
//! stored evidence times, never accumulate).

use rusqlite::{params, Connection};

/// One day in milliseconds (open-loop staleness is measured in whole days).
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// A commitment flipped `open` → `overdue` by THIS maintenance pass (C-3). The status transition
/// itself is the notification watermark: a row is returned exactly once — the pass that flips it —
/// and never again, because nothing ever moves a commitment back to `open`. No extra "notified"
/// column, no migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewlyOverdue {
    pub id: i64,
    /// The state summary (already the derived description, never raw capture text).
    pub description: String,
}

/// Recompute overdue status and open-loop staleness from `now_ms` (FR-ST-21):
/// - an `open` commitment whose `due_at` is in the past becomes `overdue`;
/// - every `open` open loop's `staleness_days` is set to whole days since `opened_at`.
///
/// Both apply to user-pinned rows too (issue #290): they are the passage of time measured against
/// the row's own dates — including a due date the user set — not a competing reading of it.
///
/// First, the outcomes later messages report for open rows of their thread are read and the local
/// candidates among those rows closed or re-dated ([`crate::loop_updates::reconcile`], E49).
///
/// Returns `(commitments_flagged_overdue, open_loops_whose_staleness_changed)`. Idempotent:
/// re-running with the same `now` flags nothing new and writes nothing.
pub fn recompute_overdue_and_staleness(
    conn: &mut Connection,
    now_ms: i64,
) -> Result<(usize, usize), rusqlite::Error> {
    let (newly, loops) = recompute_overdue_and_staleness_detailed(conn, now_ms)?;
    Ok((newly.len(), loops))
}

/// [`recompute_overdue_and_staleness`], but returning WHICH commitments this pass flipped to
/// overdue, so the caller can notify each exactly once (C-3). The select and the flip run on the
/// same predicate inside one transaction, so the returned rows are precisely the flipped rows —
/// an already-`overdue` row matches neither and is never re-reported.
pub fn recompute_overdue_and_staleness_detailed(
    conn: &mut Connection,
    now_ms: i64,
) -> Result<(Vec<NewlyOverdue>, usize), rusqlite::Error> {
    // Later messages in a thread close or move its local candidates first (E49), so a commitment
    // moved to next week is not flagged overdue on its old date. Best effort: the pass rolls back
    // on its own and is retried next run; it never holds up the overdue flip.
    let _ = crate::loop_updates::reconcile(conn, now_ms);
    let tx = conn.savepoint()?;
    let newly: Vec<NewlyOverdue> = {
        let mut stmt = tx.prepare(
            "SELECT id, description FROM commitments
              WHERE status = 'open' AND due_at IS NOT NULL AND due_at < ?1
              ORDER BY id",
        )?;
        let rows = stmt
            .query_map(params![now_ms], |r| Ok(NewlyOverdue { id: r.get(0)?, description: r.get(1)? }))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    tx.execute(
        "UPDATE commitments SET status = 'overdue', updated_at = ?1
         WHERE status = 'open' AND due_at IS NOT NULL AND due_at < ?1",
        params![now_ms],
    )?;
    // Ageing is not activity (#765, #899): `updated_at` stays put, or the Evening Wrap's "Still
    // open" list and the Dream Cycle's changed count would read every open loop as touched this
    // hour. A loop whose whole-day count did not move is not written at all, so its search entry
    // is not rebuilt every hour either (the projection's triggers fire on any UPDATE).
    // `staleness_days` is NOT NULL (V1), so `IS NOT` is plain inequality here: it writes a count
    // that went down (an `opened_at` moved later) as well as one that went up.
    let loops = tx.execute(
        "UPDATE open_loops
         SET staleness_days = MAX(0, (?1 - opened_at) / ?2)
         WHERE status = 'open' AND staleness_days IS NOT MAX(0, (?1 - opened_at) / ?2)",
        params![now_ms, DAY_MS],
    )?;
    tx.commit()?;
    Ok((newly, loops))
}

/// The four state tables carry a `confidence` and a `last_evidence_at`; decay lowers confidence for
/// rows not re-evidenced recently.
const CONFIDENCE_TABLES: &[&str] = &["people", "projects", "commitments", "open_loops"];

/// Age-decay confidence for every state row (FR-ST-21): one half-life of silence halves it.
///
/// `confidence` is **derived**, not accumulated — it is recomputed from the row's
/// `base_confidence` (its value at `last_evidence_at`) times `0.5^(elapsed / half_life_ms)`. That
/// distinction is the whole point: multiplying the stored value would compound every time the pass
/// runs, so an hourly maintenance job would drive a month-old row to zero in a couple of days
/// instead of halving it (see `V6__base_confidence.sql`). Deriving it means running the pass twice,
/// or a hundred times, lands on the same number — and a row that gets fresh evidence, which moves
/// `last_evidence_at` forward, climbs back toward its base instead of staying flattened.
///
/// Computed in Rust (not SQL `pow`, which the bundled SQLite may lack) and clamped to `[0, 1]` so
/// the schema CHECK always holds. Returns the number of rows whose confidence actually changed.
pub fn decay_confidence(
    conn: &mut Connection,
    now_ms: i64,
    half_life_ms: i64,
) -> Result<usize, rusqlite::Error> {
    if half_life_ms <= 0 {
        return Ok(0);
    }
    let half_life = half_life_ms as f64;
    let mut changed = 0usize;
    let tx = conn.savepoint()?;
    for table in CONFIDENCE_TABLES {
        let rows: Vec<(i64, f64, f64, i64)> = {
            // A row the user edited is pinned (V49): their statement does not age. A row the loop
            // consolidator stored (V83 `horizon`) keeps the confidence it was stored at; its
            // horizon timer retires it instead (`loops::age_by_horizon`).
            let horizon = if matches!(*table, "commitments" | "open_loops") { " AND horizon IS NULL" } else { "" };
            let sql = format!("SELECT id, confidence, base_confidence, last_evidence_at FROM {table}
                          WHERE user_pinned = 0{horizon}");
            let mut stmt = tx.prepare(&sql)?;
            let mapped =
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };
        for (id, confidence, base, last_evidence_at) in rows {
            // A `last_evidence_at` at or after `now` (clock skew, or evidence recorded this tick)
            // means no elapsed time, not negative decay.
            let elapsed = (now_ms - last_evidence_at).max(0) as f64;
            let target = (base * 0.5_f64.powf(elapsed / half_life)).clamp(0.0, 1.0);
            // only write when it meaningfully moves, to keep the changed-count honest
            if (target - confidence).abs() > 1e-9 {
                // Ageing is not activity: `updated_at` stays put. The Evening Wrap ("done
                // today", "still open") and the Dream Cycle's changed count read it as "something
                // happened to this row", and an hourly pass that moved it made every row look
                // touched today (#765). The search projection still hears about the new
                // confidence: its BEFORE UPDATE triggers fire on any column.
                let sql = format!("UPDATE {table} SET confidence = ?1 WHERE id = ?2");
                tx.execute(&sql, params![target, id])?;
                changed += 1;
            }
        }
    }
    tx.commit()?;
    let _ = crate::search_documents::reconcile_invalidated(conn, 100, now_ms);
    Ok(changed)
}

/// Delete state candidates that have faded to nothing (state-table GC).
///
/// Every capture with a cue inserts a Low-confidence `commitments` / `open_loops` row, and
/// [`decay_confidence`] only ever drives those rows *toward* zero — nothing removed them, so a
/// year of use left tens of thousands of near-zero rows that `context_actions`, the brief and
/// every chat memory read scanned on each call. A row is pruned when it is still unresolved
/// (`open` / `overdue`), its confidence is below `min_confidence`, and no evidence has touched it
/// for `silence_ms`. Resolved rows (`done` / `closed` / `cancelled`) are the user's own record
/// and are retained while any source remains. A bounded sweep also removes unpinned physical
/// orphans from all four tables, clearing incoming foreign keys without a deletion tombstone.
/// A future-due commitment stays; undated commitments and open loops retain the existing rule.
/// Provenance links go with a removed row; raw event text stays in `event_log`.
/// Returns the number of rows removed.
pub fn prune_faded(
    conn: &mut Connection,
    now_ms: i64,
    min_confidence: f64,
    silence_ms: i64,
) -> Result<usize, rusqlite::Error> {
    let cutoff = now_ms.saturating_sub(silence_ms.max(0));
    let mut removed = 0usize;
    let tx = conn.savepoint()?;
    for (table, open_statuses) in [
        ("commitments", "('open', 'overdue')"),
        ("open_loops", "('open')"),
    ] {
        let due_guard = if table == "commitments" {
            "AND (due_at IS NULL OR due_at < ?3)"
        } else {
            ""
        };
        let doomed = format!(
            "SELECT id FROM {table}
              WHERE status IN {open_statuses}
                AND user_pinned = 0
                AND confidence < ?1
                AND last_evidence_at < ?2
                {due_guard}"
        );
        let values = [&min_confidence as &dyn rusqlite::ToSql, &cutoff, &now_ms];
        let values = &values[..if table == "commitments" { 3 } else { 2 }];
        tx.execute(
            &format!(
                "DELETE FROM state_provenance
                  WHERE state_table = '{table}' AND state_id IN ({doomed})"
            ),
            values,
        )?;
        removed += tx.execute(
            &format!("DELETE FROM {table} WHERE id IN ({doomed})"),
            values,
        )?;
    }
    // Only physical orphans: an invalid quote on a retained source may still be repaired.
    // ponytail: remove at most 100 per hourly pass; increase only if measured backlog warrants it.
    let mut orphan_budget = 100;
    for table in crate::state::ALL_STATE_TABLES {
        let ids = tx.prepare(&format!(
            "SELECT id FROM {table} s WHERE user_pinned=0 AND NOT EXISTS (
               SELECT 1 FROM state_provenance p JOIN event_log e ON e.id=p.event_id
               WHERE p.state_table='{table}' AND p.state_id=s.id) ORDER BY id LIMIT ?1",
            table=table.as_str()
        ))?.query_map([orphan_budget], |row| row.get::<_, i64>(0))?.collect::<Result<Vec<_>, _>>()?;
        for id in ids {
            removed += crate::state_edit::remove_state_row(&tx, table, id)?;
            orphan_budget -= 1;
        }
        if orphan_budget == 0 { break; }
    }
    tx.commit()?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_log::{insert as insert_event, NewEvent};
    use crate::state::{
        insert_commitment, insert_open_loop, insert_person, CommitmentDirection, CommitmentStatus,
        NewCommitment, NewOpenLoop, NewPerson, OpenLoopKind, Provenance,
    };

    fn seed_event(conn: &Connection) -> i64 {
        insert_event(
            conn,
            &NewEvent {
                ts: 1,
                source: "capture",
                kind: "text",
                app_bundle_id: None,
                window_title: None,
                content: "evidence",
                content_hash: "h1",
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn orphan_sweep_is_bounded_preserves_pins_and_clears_foreign_keys() {
        let mut conn = crate::open_in_memory().unwrap();
        let e = seed_event(&conn);
        let task = seed_commitment(&mut conn, 0.8, 1);
        conn.execute_batch("INSERT INTO people (id,display_name,confidence,created_at,updated_at,last_evidence_at) VALUES (1,'orphan',0.8,1,1,1);
            INSERT INTO projects (id,name,status,confidence,created_at,updated_at,last_evidence_at) VALUES (1,'orphan','active',0.8,1,1,1);
            UPDATE commitments SET counterparty_id=1,project_id=1;").unwrap();
        for id in 2..=102 {
            conn.execute("INSERT INTO people (id,display_name,confidence,created_at,updated_at,last_evidence_at,user_pinned) VALUES (?1,'orphan',0.8,1,1,1,?2)", params![id, id == 102]).unwrap();
        }
        assert_eq!(prune_faded(&mut conn, 2, 0.05, 90 * DAY_MS).unwrap(), 100);
        assert_eq!(prune_faded(&mut conn, 2, 0.05, 90 * DAY_MS).unwrap(), 2);
        assert_eq!(conn.query_row("SELECT count(*) FROM people WHERE user_pinned=1", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
        let refs: (Option<i64>, Option<i64>) = conn.query_row("SELECT counterparty_id,project_id FROM commitments WHERE id=?1", [task], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!(refs, (None, None));
        assert_eq!(conn.query_row("SELECT count(*) FROM state_suppressions", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
        assert!(conn.query_row("SELECT 1 FROM event_log WHERE id=?1", [e], |_| Ok(())).is_ok());
    }

    /// Insert a commitment with `confidence`, evidenced at `now`, and return its id.
    fn seed_commitment(conn: &mut Connection, confidence: f64, now: i64) -> i64 {
        let e = seed_event(conn);
        insert_commitment(
            conn,
            &NewCommitment {
                direction: CommitmentDirection::Mine,
                counterparty_id: None,
                description: "ship it",
                due_at: None,
                status: CommitmentStatus::Open,
                project_id: None,
                confidence,
                now,
            },
            &[Provenance::new(e)],
        )
        .unwrap()
    }

    fn confidence_of(conn: &Connection, id: i64) -> f64 {
        conn.query_row("SELECT confidence FROM commitments WHERE id = ?1", [id], |r| r.get(0))
            .unwrap()
    }

    /// The on-device maintenance job runs hourly; decaying the *stored* confidence each time would
    /// compound the factors and drive a month-old row to zero within days.
    #[test]
    fn hourly_decay_over_a_half_life_halves_it_once_not_repeatedly() {
        const HOUR: i64 = 60 * 60 * 1000;
        const HALF_LIFE: i64 = 30 * 24 * HOUR;
        let mut conn = crate::open_in_memory().unwrap();
        let id = seed_commitment(&mut conn, 0.8, 0);

        // one pass per hour for exactly one half-life
        for h in 1..=(HALF_LIFE / HOUR) {
            decay_confidence(&mut conn, h * HOUR, HALF_LIFE).unwrap();
        }
        let after = confidence_of(&conn, id);
        assert!(
            (after - 0.4).abs() < 1e-6,
            "one half-life of silence must halve it once; got {after}"
        );
    }

    /// Idempotency is the property the module doc claims for every pass here.
    #[test]
    fn decay_is_idempotent_for_a_given_now() {
        let mut conn = crate::open_in_memory().unwrap();
        let id = seed_commitment(&mut conn, 0.9, 0);
        let now = 10_000;
        assert_eq!(decay_confidence(&mut conn, now, 40_000).unwrap(), 1);
        let once = confidence_of(&conn, id);
        // a second pass at the same instant must change nothing at all
        assert_eq!(decay_confidence(&mut conn, now, 40_000).unwrap(), 0);
        assert!((confidence_of(&conn, id) - once).abs() < 1e-12);
    }

    /// Ageing a row is not activity on it (#765). A commitment finished days ago must not come
    /// back as "done today" in the Evening Wrap, or as "changed" in the Dream Cycle's count,
    /// because the hourly decay pass lowered its confidence this morning.
    #[test]
    fn decay_leaves_updated_at_alone() {
        const DAY: i64 = 24 * 60 * 60 * 1000;
        let mut conn = crate::open_in_memory().unwrap();
        let e = seed_event(&conn);
        let done = insert_commitment(
            &mut conn,
            &NewCommitment {
                direction: CommitmentDirection::Mine,
                counterparty_id: None,
                description: "sent the deck",
                due_at: None,
                status: CommitmentStatus::Done,
                project_id: None,
                confidence: 0.8,
                now: 0,
            },
            &[Provenance::new(e)],
        )
        .unwrap();
        let updated_at = |conn: &Connection| -> i64 {
            conn.query_row("SELECT updated_at FROM commitments WHERE id = ?1", [done], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(updated_at(&conn), 0);

        let now = 5 * DAY;
        assert_eq!(decay_confidence(&mut conn, now, 30 * DAY).unwrap(), 1, "it did age");
        assert!(confidence_of(&conn, done) < 0.8);
        assert_eq!(updated_at(&conn), 0, "ageing is not activity");
        let today = now - DAY;
        assert_eq!(crate::state::count_commitments_done_since(&conn, today).unwrap(), 0);
        assert_eq!(crate::state::count_changed_since(&conn, today).unwrap(), 0);
    }

    /// Fresh evidence moves `last_evidence_at` forward, so the row climbs back toward its base
    /// rather than staying flattened by however long it had been quiet.
    #[test]
    fn new_evidence_restores_a_decayed_row() {
        let mut conn = crate::open_in_memory().unwrap();
        let id = seed_commitment(&mut conn, 0.8, 0);
        let half_life = 1_000;
        decay_confidence(&mut conn, 3_000, half_life).unwrap();
        assert!(confidence_of(&conn, id) < 0.2, "three half-lives of silence");

        conn.execute("UPDATE commitments SET last_evidence_at = ?1 WHERE id = ?2", params![3_000, id])
            .unwrap();
        decay_confidence(&mut conn, 3_000, half_life).unwrap();
        assert!(
            (confidence_of(&conn, id) - 0.8).abs() < 1e-9,
            "evidence at `now` means no elapsed time, so the base is restored"
        );
    }

    #[test]
    fn past_due_open_commitment_becomes_overdue() {
        let mut conn = crate::open_in_memory().unwrap();
        let e = seed_event(&conn);
        insert_commitment(
            &mut conn,
            &NewCommitment {
                direction: CommitmentDirection::Mine,
                counterparty_id: None,
                description: "ship it",
                due_at: Some(1_000),
                status: CommitmentStatus::Open,
                project_id: None,
                confidence: 0.9,
                now: 1,
            },
            &[Provenance::new(e)],
        )
        .unwrap();
        let (overdue, _) = recompute_overdue_and_staleness(&mut conn, 2_000).unwrap();
        assert_eq!(overdue, 1);
        let status: String =
            conn.query_row("SELECT status FROM commitments", [], |r| r.get(0)).unwrap();
        assert_eq!(status, "overdue");
        // idempotent: a second pass flags nothing new
        let (again, _) = recompute_overdue_and_staleness(&mut conn, 2_000).unwrap();
        assert_eq!(again, 0);
    }

    /// C-3 dedup: the detailed pass returns a commitment exactly once — the pass that flips it
    /// `open` → `overdue`. The status transition is the watermark; there is no path back to
    /// `open`, so an already-flipped row can never be reported (and so notified) again.
    #[test]
    fn newly_overdue_is_reported_once_and_never_repeats() {
        let mut conn = crate::open_in_memory().unwrap();
        let e = seed_event(&conn);
        let id = insert_commitment(
            &mut conn,
            &NewCommitment {
                direction: CommitmentDirection::Mine,
                counterparty_id: None,
                description: "send the deck",
                due_at: Some(1_000),
                status: CommitmentStatus::Open,
                project_id: None,
                confidence: 0.9,
                now: 1,
            },
            &[Provenance::new(e)],
        )
        .unwrap();

        // Not yet due: nothing to notify.
        let (before_due, _) = recompute_overdue_and_staleness_detailed(&mut conn, 500).unwrap();
        assert!(before_due.is_empty(), "not overdue yet: {before_due:?}");

        // First pass past the due time: reported exactly once, with its description.
        let (first, _) = recompute_overdue_and_staleness_detailed(&mut conn, 2_000).unwrap();
        assert_eq!(first, vec![NewlyOverdue { id, description: "send the deck".into() }]);

        // Every later pass — same instant or later — reports nothing for it again.
        for later in [2_000, 3_000, 100_000] {
            let (again, _) = recompute_overdue_and_staleness_detailed(&mut conn, later).unwrap();
            assert!(again.is_empty(), "already-notified row must never repeat: {again:?}");
        }
    }

    /// A commitment with no due date can never become overdue, so it is never reported.
    #[test]
    fn newly_overdue_ignores_undated_commitments() {
        let mut conn = crate::open_in_memory().unwrap();
        seed_commitment(&mut conn, 0.9, 0); // due_at: None
        let (newly, _) = recompute_overdue_and_staleness_detailed(&mut conn, i64::MAX).unwrap();
        assert!(newly.is_empty());
    }

    #[test]
    fn open_loop_staleness_tracks_elapsed_days() {
        let mut conn = crate::open_in_memory().unwrap();
        let e = seed_event(&conn);
        insert_open_loop(
            &mut conn,
            &NewOpenLoop {
                kind: OpenLoopKind::ReplyNeeded,
                description: "reply",
                counterparty_id: None,
                project_id: None,
                opened_at: 0,
                confidence: 0.6,
                now: 0,
            },
            &[Provenance::new(e)],
        )
        .unwrap();
        recompute_overdue_and_staleness(&mut conn, 3 * DAY_MS + 500).unwrap();
        let days: i64 =
            conn.query_row("SELECT staleness_days FROM open_loops", [], |r| r.get(0)).unwrap();
        assert_eq!(days, 3);
    }

    /// Ageing an open loop is not activity on it (#899). The hourly pass must not make every open
    /// loop look touched today, and must not rewrite a loop whose day count did not move.
    #[test]
    fn staleness_leaves_updated_at_alone_and_skips_unchanged_loops() {
        let mut conn = crate::open_in_memory().unwrap();
        let e = seed_event(&conn);
        let id = insert_open_loop(
            &mut conn,
            &NewOpenLoop {
                kind: OpenLoopKind::ReplyNeeded,
                description: "reply",
                counterparty_id: None,
                project_id: None,
                opened_at: 0,
                confidence: 0.6,
                now: 0,
            },
            &[Provenance::new(e)],
        )
        .unwrap();
        let row = |conn: &Connection| -> (i64, i64) {
            conn.query_row(
                "SELECT staleness_days, updated_at FROM open_loops WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };

        let now = 3 * DAY_MS + 500;
        let (_, changed) = recompute_overdue_and_staleness(&mut conn, now).unwrap();
        assert_eq!(changed, 1, "the day count moved");
        assert_eq!(row(&conn), (3, 0), "ageing is not activity");
        assert_eq!(crate::state::count_changed_since(&conn, now - DAY_MS).unwrap(), 0);
        assert!(
            crate::state::list_open_loops_active_since(&conn, now - DAY_MS).unwrap().is_empty(),
            "the Evening Wrap's Still open list does not show it as active today"
        );

        // An hour later the loop is still three days old: nothing is written.
        let (_, again) = recompute_overdue_and_staleness(&mut conn, now + 3_600_000).unwrap();
        assert_eq!(again, 0);
        assert_eq!(row(&conn), (3, 0));
    }

    #[test]
    fn confidence_halves_after_one_half_life() {
        let mut conn = crate::open_in_memory().unwrap();
        let e = seed_event(&conn);
        insert_person(
            &mut conn,
            &NewPerson { display_name: "Ann", confidence: 0.8, now: 0, ..Default::default() },
            &[Provenance::new(e)],
        )
        .unwrap();
        let half_life = 10 * DAY_MS;
        let changed = decay_confidence(&mut conn, half_life, half_life).unwrap();
        assert_eq!(changed, 1);
        let c: f64 = conn.query_row("SELECT confidence FROM people", [], |r| r.get(0)).unwrap();
        assert!((c - 0.4).abs() < 1e-6, "confidence should halve: {c}");
        // stays within the schema CHECK range
        assert!((0.0..=1.0).contains(&c));
    }

    #[test]
    fn freshly_evidenced_rows_barely_decay() {
        let mut conn = crate::open_in_memory().unwrap();
        let e = seed_event(&conn);
        insert_person(
            &mut conn,
            &NewPerson { display_name: "Now", confidence: 0.9, now: 1_000, ..Default::default() },
            &[Provenance::new(e)],
        )
        .unwrap();
        // now == last_evidence_at → elapsed 0, no decay (row excluded by the < now filter)
        let changed = decay_confidence(&mut conn, 1_000, 10 * DAY_MS).unwrap();
        assert_eq!(changed, 0);
        let c: f64 = conn.query_row("SELECT confidence FROM people", [], |r| r.get(0)).unwrap();
        assert!((c - 0.9).abs() < 1e-9);
    }
}

/// Repeated, distinct Batch-reviewed evidence stays below the High/fact band (0.8).
const CORROBORATION_CEILING: f64 = 0.75;

/// Shapes how fast corroboration accumulates. Larger = slower. Tuned so a second sighting is a
/// clear step up and a tenth is a marginal one.
const CORROBORATION_SHAPE: f64 = 2.0;

/// Strengthen existing state using distinct, live Batch-reviewed quotes. Local observations
/// alone never promote a memory. Replays are idempotent; pinned corrections are untouched.
/// Returns the number of rows whose base confidence or evidence age changed.
pub fn corroborate(conn: &mut Connection) -> Result<usize, rusqlite::Error> {
    let now_ms = conn.query_row(
        "SELECT MAX(
             COALESCE((SELECT MAX(updated_at) FROM people), 0),
             COALESCE((SELECT MAX(updated_at) FROM projects), 0),
             COALESCE((SELECT MAX(updated_at) FROM commitments), 0),
             COALESCE((SELECT MAX(updated_at) FROM open_loops), 0)) + 1",
        [],
        |row| row.get(0),
    )?;
    corroborate_at(conn, now_ms)
}

pub fn corroborate_at(
    conn: &mut Connection,
    now_ms: i64,
) -> Result<usize, rusqlite::Error> {
    let mut raised = 0usize;
    let tx = conn.savepoint()?;
    for table in CONFIDENCE_TABLES {
        // Commitments and open loops are owned by the loop consolidator (2026-10-14): its verdict
        // is their confidence, so repeated quotes must not lift a retired review row back up.
        if matches!(*table, "commitments" | "open_loops") {
            continue;
        }
        let sql = format!(
            "SELECT id, base_confidence, last_evidence_at FROM {table} WHERE user_pinned = 0
             AND id IN (SELECT state_id FROM state_provenance WHERE state_table='{table}'
                        AND batch_reviewed=1 AND quote IS NOT NULL GROUP BY state_id HAVING count(*)>=2)"
        );
        let rows: Vec<(i64, f64, i64)> = tx.prepare(&sql)?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?;
        for (id, base, last_evidence_at) in rows {
            let (evidence, newest_evidence_ts) = reviewed_evidence(&tx, table, id, now_ms)?;
            if evidence < 2 { continue; }
            let target = corroborated_confidence((evidence + 1) as f64).clamp(0.0, 1.0);
            if target > base || newest_evidence_ts > last_evidence_at {
                // Raise the visible value too so a standalone corroborate pass is not invisible;
                // the next decay recomputes it from the new base and ages it correctly.
                //
                // `last_evidence_at` must advance to the newest corroborating event, or the very
                // next decay pass ages the raised value from the ORIGINAL sighting and drops the
                // row straight back below the Medium boundary — silently undoing the promotion
                // this function exists to make (the corroborating events ARE evidence).
                tx.execute(
                    &format!(
                        "UPDATE {table} SET base_confidence = MAX(base_confidence, ?1),
                                            confidence = MAX(confidence, ?1),
                                            last_evidence_at = MAX(last_evidence_at, ?3),
                                            updated_at = ?4
                                      WHERE id = ?2"
                    ),
                    params![target, id, newest_evidence_ts, now_ms],
                )?;
                raised += 1;
            }
        }
    }
    tx.commit()?;
    let _ = crate::search_documents::reconcile_invalidated(conn, 100, now_ms);
    Ok(raised)
}

/// The confidence `evidence_count` independent sightings justify, with diminishing returns and a
/// hard ceiling below High. Pure, so the curve is testable on its own.
pub fn corroborated_confidence(evidence_count: f64) -> f64 {
    if evidence_count <= 1.0 {
        return 0.0; // a single sighting is not corroboration
    }
    // Asymptotic rather than linear: each additional sighting adds less than the one before, and
    // the ceiling is approached but never reached. More evidence should always help a little and
    // never harden into certainty — that judgement needs the model pass.
    let extra = evidence_count - 1.0;
    let scale = extra / (extra + CORROBORATION_SHAPE);
    0.5 + scale * (CORROBORATION_CEILING - 0.5)
}

/// Count distinct, still-valid Batch-reviewed quotes. Re-capturing or re-classifying the same
/// message cannot renew its age. Overlapping excerpts count once too.
fn reviewed_evidence(
    conn: &Connection, table: &str, id: i64, now_ms: i64,
) -> Result<(usize, i64), rusqlite::Error> {
    let mut stmt = conn.prepare_cached(
        "SELECT e.ts, e.content, p.quote FROM state_provenance p
         JOIN event_log e ON e.id=p.event_id
         WHERE p.state_table=?1 AND p.state_id=?2 AND p.batch_reviewed=1 AND p.quote IS NOT NULL
         ORDER BY e.ts, e.id"
    )?;
    let mut rows = stmt.query(params![table, id])?;
    let mut quotes: Vec<String> = Vec::new();
    let mut events = std::collections::HashSet::new();
    let mut evidence = 0;
    let mut newest = 0;
    while let Some(row) = rows.next()? {
        let ts: i64 = row.get(0)?;
        if ts > now_ms { continue; }
        let content = row.get_ref(1)?.as_str()?;
        let quote: String = row.get(2)?;
        if quote.chars().count() < 12 || !crate::state::source_contains_quote(content, &quote) {
            continue;
        }
        let normalized = crate::state::quote_identity(&quote);
        if quotes.iter().any(|seen| seen.contains(&normalized) || normalized.contains(seen)) {
            continue;
        }
        quotes.push(normalized);
        use std::hash::{Hash, Hasher};
        let mut body = twox_hash::XxHash64::with_seed(0);
        for word in content.split_whitespace() { word.hash(&mut body); }
        if !events.insert(body.finish()) { continue; }
        evidence += 1;
        newest = newest.max(ts);
    }
    Ok((evidence, newest))
}

/// Owner decision 2026-10-13: bring back below the Medium band every commitment and open loop that
/// reached it before review was required (2026-10-02), when corroboration raised a row
/// for being seen more than once. Those rows are the raw sentence the local rules picked out
/// (greetings, marketing copy, status rows), and nothing lowered them again.
///
/// A row keeps its confidence when the user pinned it, when a Batch-reviewed quote backs it, or
/// when its description is not a verbatim sentence of any linked event: that wording came from
/// the model (a Batch result from before review stamps existed), not from the local rules.
/// Nothing is deleted, and later Batch evidence can raise a capped row again. Idempotent.
/// Returns the number of rows lowered.
pub fn cap_unreviewed_actions(conn: &mut Connection) -> Result<usize, rusqlite::Error> {
    let cap = crate::extract::LOCAL_RULE_MAX_CONFIDENCE;
    let tx = conn.savepoint()?;
    let mut lowered = 0usize;
    for table in ["commitments", "open_loops"] {
        let rows: Vec<(i64, String)> = tx
            .prepare(&format!(
                "SELECT id, description FROM {table}
                 WHERE user_pinned = 0 AND (confidence > ?1 OR base_confidence > ?1)
                   AND id NOT IN (SELECT state_id FROM state_provenance WHERE state_table = '{table}'
                                  AND batch_reviewed = 1 AND quote IS NOT NULL)"
            ))?
            .query_map([cap], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        for (id, description) in rows {
            let sources: Vec<Option<String>> = tx
                .prepare(
                    "SELECT e.content FROM state_provenance p JOIN event_log e ON e.id = p.event_id
                     WHERE p.state_table = ?1 AND p.state_id = ?2",
                )?
                .query_map(params![table, id], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            if !sources.iter().flatten().any(|source| crate::state::source_contains_quote(source, &description)) {
                continue;
            }
            lowered += tx.execute(
                &format!(
                    "UPDATE {table} SET confidence = MIN(confidence, ?1),
                                        base_confidence = MIN(base_confidence, ?1)
                     WHERE id = ?2 AND user_pinned = 0"
                ),
                params![cap, id],
            )?;
        }
    }
    tx.commit()?;
    Ok(lowered)
}

#[cfg(test)]
mod corroboration_tests {
    use super::*;
    use crate::state::{
        insert_commitment, insert_open_loop, CommitmentDirection, CommitmentStatus, NewCommitment,
        NewOpenLoop, OpenLoopKind, Provenance,
    };

    #[test]
    fn unreviewed_raw_sentences_are_capped_and_reviewed_or_model_worded_or_pinned_rows_are_kept(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut conn = crate::open_in_memory()?;
        let event = crate::event_log::insert(&conn, &crate::event_log::NewEvent {
            ts: 1_000, source: "capture", kind: "text", app_bundle_id: None, window_title: None,
            content: "本日17時からよろしくお願いいたします。 I'll send the deck tomorrow.",
            content_hash: "cap", dwell_ms: 0, display_id: None, window_bounds: None,
        })?;
        let mut insert = |description: &str, confidence: f64| {
            insert_commitment(
                &mut conn,
                &NewCommitment {
                    direction: CommitmentDirection::Mine,
                    counterparty_id: None,
                    description,
                    due_at: None,
                    status: CommitmentStatus::Open,
                    project_id: None,
                    confidence,
                    now: 1_000,
                },
                &[Provenance::new(event)],
            )
        };
        let greeting = insert("本日17時からよろしくお願いいたします", 0.7)?; // raised by old corroboration
        let model_worded = insert("Send the deck", 0.7)?; // pre-review Batch wording
        let reviewed = insert("I'll send the deck tomorrow", 0.6)?;
        let pinned = insert("I'll send the deck", 0.7)?;
        let low = insert("I'll send the deck tom", 0.35)?;
        conn.execute(
            "UPDATE state_provenance SET quote = 'I''ll send the deck tomorrow', batch_reviewed = 1
             WHERE state_table = 'commitments' AND state_id = ?1",
            [reviewed],
        )?;
        conn.execute("UPDATE commitments SET user_pinned = 1 WHERE id = ?1", [pinned])?;

        assert_eq!(cap_unreviewed_actions(&mut conn)?, 1);
        assert_eq!(cap_unreviewed_actions(&mut conn)?, 0, "idempotent");
        let conf = |id: i64| -> f64 {
            conn.query_row("SELECT confidence FROM commitments WHERE id = ?1", [id], |r| r.get(0)).unwrap_or(-1.0)
        };
        assert_eq!(conf(greeting), crate::extract::LOCAL_RULE_MAX_CONFIDENCE);
        assert_eq!(conf(model_worded), 0.7);
        assert_eq!(conf(reviewed), 0.6);
        assert_eq!(conf(pinned), 0.7);
        assert_eq!(conf(low), 0.35);
        Ok(())
    }

    #[test]
    fn confidence_curve_stays_below_high_and_has_diminishing_returns() {
        assert_eq!(corroborated_confidence(1.0), 0.0);
        let two = corroborated_confidence(2.0);
        let three = corroborated_confidence(3.0);
        let four = corroborated_confidence(4.0);
        assert!(two < three && three < four);
        assert!(four - three < three - two);
        assert!(corroborated_confidence(1_000.0) < 0.8);
    }

    #[test]
    fn only_distinct_batch_reviewed_quotes_strengthen_and_refresh_one_row() -> Result<(), Box<dyn std::error::Error>> {
        use crate::extract::{persist_candidates_deduped, Candidate};
        let mut conn = crate::open_in_memory()?;
        let mut id = 0;
        for (i, quote, reviewed) in [
            (1, "I'll send the final deck after legal approval", true),
            (2, "I'll send the final deck after legal approval", true),
            (3, "Legal has approved; I am sending the final deck", false),
        ] {
            let event = crate::event_log::insert(&conn, &crate::event_log::NewEvent {
                ts: i * 1_000, source: "gmail", kind: "text", app_bundle_id: None,
                window_title: Some("Delivery chat"), content: quote, content_hash: &format!("review-{i}"),
                dwell_ms: 0, display_id: None, window_bounds: None,
            })?;
            conn.execute("UPDATE event_log SET thread_key='delivery-thread' WHERE id=?1", [event])?;
            let candidate = Candidate::Commitment {
                direction: CommitmentDirection::Mine, description: "Send final deck".into(),
                confidence: if reviewed { 0.6 } else { 0.35 }, evidence: Some(quote.into()),
            };
            id = persist_candidates_deduped(&mut conn, event, &[candidate], i * 1_000, i * 1_000)?[0];
        }
        assert_eq!(corroborate_at(&mut conn, 4_000)?, 0, "replay and unreviewed text cannot boost");
        persist_candidates_deduped(&mut conn, 3, &[Candidate::Commitment {
            direction: CommitmentDirection::Mine, description: "Send final deck".into(),
            confidence: 0.6, evidence: Some("Legal has approved; I am sending the final deck".into()),
        }], 3_000, 4_000)?;
        // Commitments and open loops belong to the loop consolidator: a second distinct reviewed
        // quote does not lift them (2026-10-14).
        assert_eq!(corroborate_at(&mut conn, 4_000)?, 0);
        let base: f64 = conn.query_row("SELECT base_confidence FROM commitments WHERE id=?1", [id], |r| r.get(0))?;
        assert!(base <= 0.6, "{base}");
        assert_eq!(conn.query_row("SELECT count(*) FROM commitments", [], |r| r.get::<_, i64>(0))?, 1);
        Ok(())
    }

    #[test]
    fn repeated_observations_never_promote_or_renew_state_evidence() {
        let mut conn = crate::open_in_memory().unwrap();
        let observations = [
            ("capture", "I'll send the deck. Mail sidebar A", "ax-a"),
            ("capture", "I'll send the deck. Mail sidebar B", "ax-b"),
            ("screen_ocr", "I'll send the deck. Mail sidebar B", "ocr"),
            ("gmail", "I'll send the deck.", "synced"),
        ];
        let events: Vec<i64> = observations
            .iter()
            .enumerate()
            .map(|(i, (source, content, hash))| {
                crate::event_log::insert(
                    &conn,
                    &crate::event_log::NewEvent {
                        ts: 2_000 + i as i64,
                        source,
                        kind: "text",
                        app_bundle_id: None,
                        window_title: None,
                        content,
                        content_hash: hash,
                        dwell_ms: 0,
                        display_id: None,
                        window_bounds: None,
                    },
                )
                .unwrap()
            })
            .collect();
        let provenance: Vec<_> = events.iter().map(|event| Provenance::new(*event)).collect();
        for (description, confidence, pinned) in [
            ("local rule", 0.35, false),
            ("Batch classified", 0.9, false),
            ("user pinned", 1.0, true),
        ] {
            let id = insert_commitment(
                &mut conn,
                &NewCommitment {
                    direction: CommitmentDirection::Mine,
                    counterparty_id: None,
                    description,
                    due_at: None,
                    status: CommitmentStatus::Open,
                    project_id: None,
                    confidence,
                    now: 1_000,
                },
                &provenance,
            )
            .unwrap();
            if pinned {
                conn.execute("UPDATE commitments SET user_pinned = 1 WHERE id = ?1", [id])
                    .unwrap();
            }
        }
        assert_eq!(corroborate_at(&mut conn, 5_000).unwrap(), 0);
        assert_eq!(corroborate(&mut conn).unwrap(), 0);
        let mut stmt = conn
            .prepare(
                "SELECT confidence, base_confidence, last_evidence_at
                 FROM commitments ORDER BY id",
            )
            .unwrap();
        let rows: Vec<(f64, f64, i64)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![(0.35, 0.35, 1_000), (0.9, 0.9, 1_000), (1.0, 1.0, 1_000)]
        );
        let links: i64 = conn
            .query_row(
                "SELECT count(*) FROM state_provenance WHERE state_table = 'commitments'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(links, 12, "every observation remains linked for source tracing");
    }

    #[test]
    fn pruning_preserves_future_and_due_now_commitments() {
        let mut conn = crate::open_in_memory().unwrap();
        let event = crate::event_log::insert(
            &conn,
            &crate::event_log::NewEvent {
                ts: 1,
                source: "capture",
                kind: "text",
                app_bundle_id: None,
                window_title: None,
                content: "source text stays",
                content_hash: "prune-boundary",
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap();
        let now = 200 * DAY_MS;
        let old = now - 120 * DAY_MS;
        let mut ids = Vec::new();
        for (description, due_at, pinned, confidence, evidence_at, status) in [
            ("future", Some(now + 1), false, 0.01, old, CommitmentStatus::Open),
            ("due now", Some(now), false, 0.01, old, CommitmentStatus::Open),
            ("past due", Some(now - 1), false, 0.01, old, CommitmentStatus::Overdue),
            ("undated", None, false, 0.01, old, CommitmentStatus::Open),
            ("pinned undated", None, true, 0.01, old, CommitmentStatus::Open),
            ("recent", None, false, 0.01, now - 10 * DAY_MS, CommitmentStatus::Open),
            ("credible", None, false, 0.4, old, CommitmentStatus::Open),
            ("done", None, false, 0.01, old, CommitmentStatus::Done),
        ] {
            let id = insert_commitment(
                &mut conn,
                &NewCommitment {
                    direction: CommitmentDirection::Mine,
                    counterparty_id: None,
                    description,
                    due_at,
                    status,
                    project_id: None,
                    confidence,
                    now: evidence_at,
                },
                &[Provenance::new(event)],
            )
            .unwrap();
            if pinned {
                conn.execute("UPDATE commitments SET user_pinned = 1 WHERE id = ?1", [id])
                    .unwrap();
            }
            ids.push(id);
        }
        let loop_id = insert_open_loop(
            &mut conn,
            &NewOpenLoop {
                kind: OpenLoopKind::ReplyNeeded,
                description: "old open loop",
                counterparty_id: None,
                project_id: None,
                opened_at: old,
                confidence: 0.01,
                now: old,
            },
            &[Provenance::new(event)],
        )
        .unwrap();
        assert_eq!(prune_faded(&mut conn, now, 0.05, 90 * DAY_MS).unwrap(), 3);
        let surviving: Vec<i64> = conn
            .prepare("SELECT id FROM commitments ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(surviving, vec![ids[0], ids[1], ids[4], ids[5], ids[6], ids[7]]);
        let links: Vec<i64> = conn
            .prepare(
                "SELECT state_id FROM state_provenance
                 WHERE state_table = 'commitments' ORDER BY state_id",
            )
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(links, surviving);
        let loop_count: i64 = conn
            .query_row("SELECT count(*) FROM open_loops WHERE id = ?1", [loop_id], |r| r.get(0))
            .unwrap();
        assert_eq!(loop_count, 0, "open-loop pruning scope stays unchanged");
        let source: String = conn
            .query_row("SELECT content FROM event_log WHERE id = ?1", [event], |r| r.get(0))
            .unwrap();
        assert_eq!(source, "source text stays");
        assert_eq!(prune_faded(&mut conn, now, 0.05, 90 * DAY_MS).unwrap(), 0);
    }
}
