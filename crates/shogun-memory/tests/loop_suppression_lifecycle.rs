//! New consolidator rows honor correction/deletion intent using grounded source time.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use rusqlite::Connection;
use serde_json::{json, Value};
use shogun_memory::{
    actor::{Actor, WriteContext},
    event_log::{insert, NewEvent},
    loops::apply_as,
    state::StateTable,
    state_edit::{delete_state_as, suppression_digest, update_state_as, StatePatch},
};

fn scratch(id: &str) -> WriteContext {
    WriteContext::scratch(Actor::agent(id).unwrap()).unwrap()
}

fn event(conn: &Connection, ts: i64, hash: &str) -> i64 {
    insert(
        conn,
        &NewEvent {
            ts,
            source: "capture",
            kind: "text",
            content: "Please send Toru the launch deck",
            content_hash: hash,
            app_bundle_id: None,
            window_title: None,
            dwell_ms: 0,
            display_id: None,
            window_bounds: None,
        },
    )
    .unwrap()
}

fn answer(event: i64, kind: &str) -> Value {
    json!({"open": [{"kind": kind, "horizon": "short", "confidence": 0.9,
        "source_event_id": event, "source_quote": "Please send Toru the launch deck",
        "text": "Send Toru the launch deck"}]})
}

#[test]
fn consolidator_deletions_block_old_evidence_allow_new_and_use_explicit_destination() {
    for (kind, table) in [
        ("commitment", StateTable::Commitments),
        ("loop", StateTable::OpenLoops),
    ] {
        // Independent destinations exercise the INSERT path. Existing-row identity/re-list
        // lookups have their own scope contract and are deliberately not exercised as inserts.
        for other in [WriteContext::owner(), scratch("beta")] {
            let mut conn = shogun_memory::open_in_memory().unwrap();
            let alpha = scratch("alpha");
            let old = event(&conn, 1_000, "old");
            let fresh = event(&conn, 5_000, "fresh");
            assert_eq!(
                apply_as(&mut conn, &answer(old, kind), &[old], &[], 1_000, &alpha)
                    .unwrap()
                    .opened,
                1
            );
            let id: i64 = conn
                .query_row(&format!("SELECT id FROM {}", table.as_str()), [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert!(delete_state_as(&mut conn, table, id, 2_000, &WriteContext::owner()).unwrap());
            // The ambient context is still owner/world; only apply_as's alpha context is correct.
            let replay =
                apply_as(&mut conn, &answer(old, kind), &[old], &[], 9_000, &alpha).unwrap();
            assert_eq!((replay.opened, replay.relisted, replay.dropped), (0, 0, 1));
            assert_eq!(
                apply_as(
                    &mut conn,
                    &answer(fresh, kind),
                    &[fresh],
                    &[],
                    9_000,
                    &alpha
                )
                .unwrap()
                .opened,
                1
            );
            let id: i64 = conn
                .query_row(&format!("SELECT id FROM {}", table.as_str()), [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert!(delete_state_as(&mut conn, table, id, 6_000, &alpha).unwrap());
            assert_eq!(
                apply_as(&mut conn, &answer(old, kind), &[old], &[], 9_000, &other)
                    .unwrap()
                    .opened,
                1
            );
            let namespace: String = conn
                .query_row(
                    &format!("SELECT namespace FROM {}", table.as_str()),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(namespace, other.namespace_sql());
        }
    }
}

#[test]
fn consolidator_does_not_recreate_rewritten_old_wording() {
    for (kind, table) in [
        ("commitment", StateTable::Commitments),
        ("loop", StateTable::OpenLoops),
    ] {
        let mut conn = shogun_memory::open_in_memory().unwrap();
        let alpha = scratch("alpha");
        let old = event(&conn, 1_000, "old");
        assert_eq!(
            apply_as(&mut conn, &answer(old, kind), &[old], &[], 1_000, &alpha)
                .unwrap()
                .opened,
            1
        );
        let id: i64 = conn
            .query_row(&format!("SELECT id FROM {}", table.as_str()), [], |r| {
                r.get(0)
            })
            .unwrap();
        let patch = StatePatch {
            description: Some("Send Toru the corrected launch deck".into()),
            ..Default::default()
        };
        assert!(update_state_as(&mut conn, table, id, &patch, 2_000, &alpha).unwrap());
        let replay = apply_as(&mut conn, &answer(old, kind), &[old], &[], 9_000, &alpha).unwrap();
        assert_eq!((replay.opened, replay.relisted, replay.dropped), (0, 0, 1));
        let description: String = conn
            .query_row(
                &format!("SELECT description FROM {}", table.as_str()),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(description, "Send Toru the corrected launch deck");
    }
}

#[test]
fn consolidator_retains_legacy_global_suppression_until_its_original_cutoff() {
    for (kind, table) in [
        ("commitment", StateTable::Commitments),
        ("loop", StateTable::OpenLoops),
    ] {
        for destination in [WriteContext::owner(), scratch("alpha"), scratch("beta")] {
            let mut conn = shogun_memory::open_in_memory().unwrap();
            let old = event(&conn, 2_000, "old");
            let fresh = event(&conn, 2_001, "fresh");
            conn.execute(
                "INSERT INTO state_suppressions (state_table, match_digest, reason, suppressed_at)
                VALUES (?1, ?2, 'deleted', 2000)",
                rusqlite::params![
                    table.as_str(),
                    suppression_digest(table, "Send Toru the launch deck")
                ],
            )
            .unwrap();
            assert_eq!(
                apply_as(
                    &mut conn,
                    &answer(old, kind),
                    &[old],
                    &[],
                    9_000,
                    &destination
                )
                .unwrap()
                .opened,
                0
            );
            assert_eq!(
                apply_as(
                    &mut conn,
                    &answer(fresh, kind),
                    &[fresh],
                    &[],
                    9_000,
                    &destination
                )
                .unwrap()
                .opened,
                1
            );
        }
    }
}
