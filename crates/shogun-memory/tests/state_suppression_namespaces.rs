//! Suppression is a deletion/correction effect on a destination namespace, not an event filter.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use rusqlite::{params, Connection};
use shogun_memory::{
    actor::{with_call_scope, Actor, CallScope, ReadScope, WriteContext},
    event_log::{insert, NewEvent},
    extract::{persist_candidates, Candidate},
    knowledge,
    state::{CommitmentDirection, OpenLoopKind, StateTable},
    state_edit::{delete_state_as, is_suppressed_in_namespace, update_state_as, StatePatch},
};

fn scratch(id: &str) -> WriteContext {
    WriteContext::scratch(Actor::agent(id).unwrap()).unwrap()
}

fn as_writer<T>(ctx: &WriteContext, f: impl FnOnce() -> T) -> T {
    with_call_scope(
        CallScope {
            write: ctx.clone(),
            read: ReadScope::WITH_SCRATCH,
        },
        f,
    )
}

fn event(conn: &Connection, ts: i64, hash: &str, content: &str) -> i64 {
    insert(
        conn,
        &NewEvent {
            ts,
            source: "capture",
            kind: "text",
            content,
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

fn candidate(table: StateTable) -> Candidate {
    match table {
        StateTable::Commitments => Candidate::Commitment {
            direction: CommitmentDirection::Mine,
            description: "Send the launch deck".into(),
            confidence: 0.3,
            evidence: None,
        },
        StateTable::OpenLoops => Candidate::OpenLoop {
            kind: OpenLoopKind::FollowUp,
            description: "Send the launch deck".into(),
            confidence: 0.3,
            evidence: None,
        },
        _ => unreachable!(),
    }
}

#[test]
fn private_corrections_and_deletions_preserve_same_word_neighbors_and_fresh_evidence() {
    for table in [StateTable::Commitments, StateTable::OpenLoops] {
        for edit in [false, true] {
            let mut conn = shogun_memory::open_in_memory().unwrap();
            let old = event(&conn, 1_000, "old", "Send the launch deck");
            let fresh = event(&conn, 5_000, "fresh", "Send the launch deck");
            let contexts = [WriteContext::owner(), scratch("alpha"), scratch("beta")];
            let candidates = [candidate(table)];
            let ids: Vec<i64> = contexts
                .iter()
                .map(|ctx| {
                    as_writer(ctx, || {
                        persist_candidates(&mut conn, old, &candidates, 1_000, 1_000).unwrap()[0]
                    })
                })
                .collect();

            // The actor may edit its private row while its call's destination is world.
            // Recording ctx.namespace instead of the row's namespace would suppress world.
            let editor = WriteContext::world(Actor::agent("alpha").unwrap());
            if edit {
                let patch = StatePatch {
                    description: Some("Send the corrected launch deck".into()),
                    ..StatePatch::default()
                };
                assert!(update_state_as(&mut conn, table, ids[1], &patch, 2_000, &editor).unwrap());
            } else {
                assert!(delete_state_as(&mut conn, table, ids[1], 2_000, &editor).unwrap());
            }
            for i in [0, 2] {
                let stored: (String, String) = conn
                    .query_row(
                        &format!(
                            "SELECT description, namespace FROM {} WHERE id = ?1",
                            table.as_str()
                        ),
                        [ids[i]],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .unwrap();
                assert_eq!(
                    stored,
                    ("Send the launch deck".into(), contexts[i].namespace_sql())
                );
                assert_eq!(
                    shogun_memory::state::provenance_count(&conn, table, ids[i]).unwrap(),
                    1
                );
            }

            for (i, ctx) in contexts.iter().enumerate() {
                // The event is world; the destination is independently selected by the writer.
                let replay = as_writer(ctx, || {
                    persist_candidates(&mut conn, old, &candidates, 9_000, 9_000).unwrap()
                });
                assert_eq!(
                    replay.len(),
                    usize::from(i != 1),
                    "{table:?}, edit={edit}, {ctx:?}"
                );
            }
            assert_eq!(
                as_writer(&contexts[1], || {
                    persist_candidates(&mut conn, fresh, &candidates, 5_000, 9_000).unwrap()
                })
                .len(),
                1,
                "later evidence remains eligible in the affected namespace"
            );
            let recorded: (String, String) = conn
                .query_row(
                    "SELECT namespace, reason FROM state_suppressions_scoped",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(
                recorded,
                (
                    "agent_scratch:alpha".into(),
                    if edit { "edited" } else { "deleted" }.into()
                )
            );
            assert_eq!(
                conn.query_row("SELECT count(*) FROM state_suppressions", [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
    }
}

#[test]
fn extracted_people_and_projects_replay_only_outside_the_deleted_namespace() {
    for (kind, table, name, content) in [
        (
            knowledge::Kind::Person,
            StateTable::People,
            "Dana Reyes",
            "Met Dana Reyes from Acme about the launch",
        ),
        (
            knowledge::Kind::Project,
            StateTable::Projects,
            "Atlas",
            "Working on Atlas at Acme for the launch",
        ),
    ] {
        for destination in [WriteContext::owner(), scratch("beta")] {
            let mut conn = shogun_memory::open_in_memory().unwrap();
            let old = event(&conn, 1_000, "old", content);
            let fresh = event(&conn, 5_000, "fresh", content);
            let alpha = scratch("alpha");
            let candidate = knowledge::Candidate {
                kind,
                name: name.into(),
                context: "Acme".into(),
                quote: content.into(),
                ..Default::default()
            };
            let id = as_writer(&alpha, || {
                knowledge::persist(&mut conn, old, &candidate, 1_000)
            })
            .unwrap()
            .unwrap();
            let key: String = conn
                .query_row(
                    &format!(
                        "SELECT extraction_key FROM {} WHERE id = ?1",
                        table.as_str()
                    ),
                    [id],
                    |r| r.get(0),
                )
                .unwrap();
            // Owner's destination is world, but the affected row belongs to alpha.
            assert!(delete_state_as(&mut conn, table, id, 2_000, &WriteContext::owner()).unwrap());
            assert!(as_writer(&alpha, || knowledge::persist(
                &mut conn, old, &candidate, 9_000
            ))
            .unwrap()
            .is_none());
            let fresh_id = as_writer(&alpha, || {
                knowledge::persist(&mut conn, fresh, &candidate, 9_000)
            })
            .unwrap()
            .unwrap();
            assert!(delete_state_as(&mut conn, table, fresh_id, 6_000, &alpha).unwrap());
            let other = as_writer(&destination, || {
                knowledge::persist(&mut conn, old, &candidate, 9_000)
            })
            .unwrap()
            .unwrap();
            let namespace: String = conn
                .query_row(
                    &format!("SELECT namespace FROM {} WHERE id = ?1", table.as_str()),
                    [other],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(namespace, destination.namespace_sql());
            assert!(
                is_suppressed_in_namespace(&conn, table, &key, 6_000, &alpha.namespace_sql())
                    .unwrap()
            );
            assert!(
                !is_suppressed_in_namespace(&conn, table, &key, 6_001, &alpha.namespace_sql())
                    .unwrap()
            );
        }
    }
}

#[test]
fn repeated_deletions_keep_cutoffs_independent_and_never_extend_legacy_rows() {
    let mut conn = shogun_memory::open_in_memory().unwrap();
    let old = event(&conn, 1_000, "old", "Send the launch deck");
    let table = StateTable::Commitments;
    let candidates = [candidate(table)];
    // Seed the exact legacy format: it remains a global cutoff of 500, not 2,000.
    let digest = shogun_memory::state_edit::suppression_digest(table, "Send the launch deck");
    conn.execute(
        "INSERT INTO state_suppressions (state_table, match_digest, reason, suppressed_at)
        VALUES ('commitments', ?1, 'deleted', 500)",
        [digest],
    )
    .unwrap();
    let alpha = scratch("alpha");
    for deleted_at in [2_000, 1_500] {
        // A direct user-created row can be removed again, even with an older supplied time.
        let req = shogun_memory::state_edit::CreateRequest {
            table,
            description: "Send the launch deck".into(),
            due_at: None,
            person_id: None,
        };
        let id = shogun_memory::state_edit::create_state_as(&mut conn, &req, 1_000, &alpha)
            .unwrap()
            .unwrap();
        assert!(delete_state_as(&mut conn, table, id, deleted_at, &alpha).unwrap());
    }
    let cutoff: i64 = conn
        .query_row(
            "SELECT suppressed_at FROM state_suppressions_scoped",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cutoff, 2_000);
    let legacy: i64 = conn
        .query_row("SELECT suppressed_at FROM state_suppressions", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(legacy, 500);
    for namespace in ["world", "agent_scratch:alpha", "agent_scratch:beta"] {
        assert!(
            is_suppressed_in_namespace(&conn, table, "Send the launch deck", 500, namespace)
                .unwrap()
        );
        assert_eq!(
            is_suppressed_in_namespace(&conn, table, "Send the launch deck", 501, namespace)
                .unwrap(),
            namespace == "agent_scratch:alpha"
        );
    }
    assert_eq!(
        as_writer(&scratch("beta"), || persist_candidates(
            &mut conn,
            old,
            &candidates,
            9_000,
            9_000
        ))
        .unwrap()
        .len(),
        1
    );
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM state_suppressions_scoped WHERE namespace = ?1",
            params!["agent_scratch:beta"],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}
