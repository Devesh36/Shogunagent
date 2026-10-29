#![allow(clippy::expect_used)]

use std::error::Error;

use rusqlite::Connection;
use shogun_memory::{
    acl::{grant_scratch, revoke_scratch, ScratchAccess},
    actor::{with_call_scope, Actor, CallScope, Namespace, ReadScope, WriteContext},
    state::{self, NewPerson, NewProject, ProjectStatus, Provenance, StateTable},
    state_edit::{self, CreateRequest, StatePatch},
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

fn writer(id: &str) -> TestResult<WriteContext> {
    Ok(WriteContext::world(Actor::agent(id)?))
}

fn fixture(conn: &mut Connection, table: StateTable, namespace: Namespace) -> TestResult<i64> {
    let ctx = WriteContext {
        actor: Actor::owner(),
        namespace,
    };
    let id = with_call_scope(
        CallScope {
            write: ctx.clone(),
            read: ReadScope::WORLD_ONLY,
        },
        || -> TestResult<i64> {
            conn.execute("INSERT INTO event_log (ts, source, kind, content, content_hash, last_seen_at) VALUES (1, 'synthetic', 'text', 'review the draft', 'fixture', 1)", [])?;
            let provenance = [Provenance::new(conn.last_insert_rowid())];
            Ok(match table {
                StateTable::People => state::insert_person(
                    conn,
                    &NewPerson {
                        display_name: "Synthetic Person",
                        confidence: 0.8,
                        now: 1,
                        ..Default::default()
                    },
                    &provenance,
                )?,
                StateTable::Projects => state::insert_project(
                    conn,
                    &NewProject {
                        name: "Synthetic Project",
                        status: ProjectStatus::Active,
                        summary: None,
                        participants_json: None,
                        sources_json: None,
                        confidence: 0.8,
                        now: 1,
                    },
                    &provenance,
                )?,
                _ => {
                    let id = state_edit::create_state_as(conn, &request(table, None), 1, &ctx)
                        .expect("state operation")
                        .expect("fixture created");
                    conn.execute("INSERT INTO state_provenance (state_table, state_id, event_id, weight) VALUES (?1, ?2, ?3, 1)", rusqlite::params![table.as_str(), id, provenance[0].event_id])?;
                    id
                }
            })
        },
    )?;
    Ok(id)
}

fn request(table: StateTable, person_id: Option<i64>) -> CreateRequest {
    CreateRequest {
        table,
        description: "review the draft".into(),
        due_at: None,
        person_id,
    }
}

fn patch(table: StateTable) -> StatePatch {
    match table {
        StateTable::People | StateTable::Projects => StatePatch {
            name: Some("Corrected name".into()),
            ..Default::default()
        },
        _ => StatePatch {
            description: Some("review the final draft".into()),
            ..Default::default()
        },
    }
}

fn changes(conn: &Connection) -> TestResult<i64> {
    Ok(conn.query_row("SELECT total_changes()", [], |r| r.get(0))?)
}

const TABLES: [StateTable; 4] = [
    StateTable::People,
    StateTable::Projects,
    StateTable::Commitments,
    StateTable::OpenLoops,
];

#[test]
fn foreign_scratch_mutations_need_write_grants_and_denials_do_not_write() -> TestResult {
    for table in TABLES {
        let mut conn = shogun_memory::open_in_memory()?;
        let namespace = Namespace::scratch("alice")?;
        let id = fixture(&mut conn, table, namespace.clone())?;
        let bob = writer("bob")?;
        // No grant and read-only grants must produce the same absent response, including retries.
        for access in [None, Some(ScratchAccess::Read)] {
            if let Some(access) = access {
                grant_scratch(&conn, &bob.actor, &namespace, access, &Actor::owner(), 2)?;
            }
            let before = changes(&conn)?;
            for _ in 0..2 {
                assert!(
                    !state_edit::update_state_as(&mut conn, table, id, &patch(table), 3, &bob)
                        .expect("state operation"),
                    "{table:?}"
                );
                assert!(
                    !state_edit::delete_state_as(&mut conn, table, id, 3, &bob)
                        .expect("state operation"),
                    "{table:?}"
                );
                assert!(!state_edit::update_state_as(
                    &mut conn,
                    table,
                    id + 100,
                    &patch(table),
                    3,
                    &bob
                )
                .expect("state operation"));
                assert!(
                    !state_edit::delete_state_as(&mut conn, table, id + 100, 3, &bob)
                        .expect("state operation")
                );
            }
            assert_eq!(
                changes(&conn)?,
                before,
                "denial must not change rows, provenance, suppression, indexes or tombstones"
            );
        }
        grant_scratch(
            &conn,
            &bob.actor,
            &namespace,
            ScratchAccess::Write,
            &Actor::owner(),
            4,
        )?;
        assert!(
            state_edit::update_state_as(&mut conn, table, id, &patch(table), 5, &bob)
                .expect("state operation")
        );
        let edited: (i64, f64, String) = conn.query_row(
            &format!(
                "SELECT user_pinned, confidence, actor_last FROM {} WHERE id=?1",
                table.as_str()
            ),
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        assert_eq!(edited, (1, 1.0, "agent:bob".into()));
        assert_eq!(state::provenance_count(&conn, table, id)?, 1);
        revoke_scratch(&conn, &bob.actor, &namespace)?;
        let before = changes(&conn)?;
        assert!(
            !state_edit::update_state_as(&mut conn, table, id, &patch(table), 6, &bob)
                .expect("state operation")
        );
        assert!(
            !state_edit::delete_state_as(&mut conn, table, id, 6, &bob).expect("state operation")
        );
        assert_eq!(changes(&conn)?, before);
        grant_scratch(
            &conn,
            &bob.actor,
            &namespace,
            ScratchAccess::Write,
            &Actor::owner(),
            7,
        )?;
        assert!(
            state_edit::delete_state_as(&mut conn, table, id, 8, &bob).expect("state operation")
        );
        assert!(
            !state_edit::delete_state_as(&mut conn, table, id, 9, &bob).expect("state operation")
        );
        assert_eq!(state::provenance_count(&conn, table, id)?, 0);
        let reason: String = conn.query_row(
            "SELECT reason FROM export_tombstones WHERE kind=?1 AND id=?2",
            rusqlite::params![table.as_str(), id],
            |r| r.get(0),
        )?;
        assert_eq!(reason, "deleted_by:agent:bob");
        if matches!(table, StateTable::Commitments | StateTable::OpenLoops) {
            assert!(state_edit::is_suppressed_in_namespace(
                &conn,
                table,
                "review the draft",
                1,
                &namespace.as_str(),
            )?);
            assert!(state_edit::is_suppressed_in_namespace(
                &conn,
                table,
                "review the final draft",
                1,
                &namespace.as_str(),
            )?);
            assert!(!state_edit::is_suppressed(&conn, table, "review the draft", 1)?);
            assert!(!state_edit::is_suppressed(&conn, table, "review the final draft", 1)?);
        }
    }
    Ok(())
}

#[test]
fn world_own_scratch_and_owner_mutations_stay_available() -> TestResult {
    for table in TABLES {
        for (namespace, ctx) in [
            (Namespace::World, writer("bob")?),
            (Namespace::scratch("bob")?, writer("bob")?),
            (Namespace::scratch("alice")?, WriteContext::owner()),
        ] {
            let mut conn = shogun_memory::open_in_memory()?;
            let id = fixture(&mut conn, table, namespace)?;
            assert!(
                state_edit::update_state_as(&mut conn, table, id, &patch(table), 2, &ctx)
                    .expect("state operation")
            );
            assert!(state_edit::delete_state_as(&mut conn, table, id, 3, &ctx)
                .expect("state operation"));
        }
    }
    Ok(())
}

#[test]
fn create_checks_destination_write_access() -> TestResult {
    for table in [StateTable::Commitments, StateTable::OpenLoops] {
        let mut conn = shogun_memory::open_in_memory()?;
        let namespace = Namespace::scratch("alice")?;
        let ctx = WriteContext {
            actor: Actor::agent("bob")?,
            namespace: namespace.clone(),
        };
        for access in [None, Some(ScratchAccess::Read)] {
            if let Some(access) = access {
                grant_scratch(&conn, &ctx.actor, &namespace, access, &Actor::owner(), 1)?;
            }
            let before = changes(&conn)?;
            assert!(
                state_edit::create_state_as(&mut conn, &request(table, None), 2, &ctx).is_err()
            );
            assert_eq!(changes(&conn)?, before);
        }
        grant_scratch(
            &conn,
            &ctx.actor,
            &namespace,
            ScratchAccess::Write,
            &Actor::owner(),
            3,
        )?;
        assert!(
            state_edit::create_state_as(&mut conn, &request(table, None), 4, &ctx)
                .expect("state operation")
                .is_some()
        );
        revoke_scratch(&conn, &ctx.actor, &namespace)?;
        assert!(state_edit::create_state_as(&mut conn, &request(table, None), 5, &ctx).is_err());
        assert!(
            state_edit::create_state_as(&mut conn, &request(table, None), 6, &writer("bob")?)
                .expect("state operation")
                .is_some()
        );
        assert!(state_edit::create_state_as(
            &mut conn,
            &request(table, None),
            6,
            &WriteContext::scratch(Actor::agent("bob")?)?
        )
        .expect("state operation")
        .is_some());
    }
    Ok(())
}

#[test]
fn create_cannot_reference_an_unreadable_person() -> TestResult {
    for table in [StateTable::Commitments, StateTable::OpenLoops] {
        let mut conn = shogun_memory::open_in_memory()?;
        let namespace = Namespace::scratch("alice")?;
        let person = fixture(&mut conn, StateTable::People, namespace.clone())?;
        let bob = writer("bob")?;
        let before = changes(&conn)?;
        assert_eq!(
            state_edit::create_state_as(&mut conn, &request(table, Some(person)), 2, &bob)
                .expect("state operation"),
            None
        );
        assert_eq!(
            state_edit::create_state_as(&mut conn, &request(table, Some(person + 100)), 2, &bob)
                .expect("state operation"),
            None
        );
        assert_eq!(changes(&conn)?, before);
        grant_scratch(
            &conn,
            &bob.actor,
            &namespace,
            ScratchAccess::Read,
            &Actor::owner(),
            3,
        )?;
        assert!(
            state_edit::create_state_as(&mut conn, &request(table, Some(person)), 4, &bob)
                .expect("state operation")
                .is_some()
        );
        revoke_scratch(&conn, &bob.actor, &namespace)?;
        let before = changes(&conn)?;
        assert_eq!(
            state_edit::create_state_as(&mut conn, &request(table, Some(person)), 5, &bob)
                .expect("state operation"),
            None
        );
        assert_eq!(changes(&conn)?, before);
    }
    Ok(())
}
