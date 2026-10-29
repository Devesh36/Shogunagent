//! Public correction/export surfaces must obey the same scratch read scope as state lists.
#![allow(clippy::expect_used)]

use rusqlite::{params, Connection};
use shogun_memory::{
    acl::{grant_scratch, revoke_scratch, ScratchAccess},
    actor::{with_call_scope, Actor, CallScope, Namespace, ReadScope, WriteContext},
    event_log::{insert, NewEvent},
    state::StateTable,
    state_edit::{export, export_entity, to_markdown, WorldModel},
};

fn fixture() -> Connection {
    let conn = shogun_memory::open_in_memory().expect("store");
    for (id, namespace, label) in [
        (1, "world", "public"),
        (2, "agent_scratch:alpha", "alpha-private"),
        (3, "agent_scratch:beta", "beta-private"),
    ] {
        conn.execute("INSERT INTO people(id,display_name,confidence,created_at,updated_at,last_evidence_at,namespace,user_pinned) VALUES(?1,?2,1,1,1,1,?3,1)", params![id,label,namespace]).expect("person");
        conn.execute("INSERT INTO projects(id,name,status,confidence,created_at,updated_at,last_evidence_at,namespace,user_pinned) VALUES(?1,?2,'active',1,1,1,1,?3,1)", params![id,label,namespace]).expect("project");
        conn.execute("INSERT INTO commitments(id,direction,description,status,confidence,created_at,updated_at,last_evidence_at,namespace,user_pinned) VALUES(?1,'mine',?2,'open',1,1,1,1,?3,1)", params![id,label,namespace]).expect("commitment");
        conn.execute("INSERT INTO open_loops(id,kind,description,opened_at,status,confidence,created_at,updated_at,last_evidence_at,namespace,user_pinned) VALUES(?1,'other',?2,1,'open',1,1,1,1,?3,1)", params![id,label,namespace]).expect("loop");
    }
    conn
}

fn as_agent<T>(scope: ReadScope, f: impl FnOnce() -> T) -> T {
    with_call_scope(
        CallScope {
            write: WriteContext::world(Actor::agent("alpha").expect("actor")),
            read: scope,
        },
        f,
    )
}

fn assert_ids(model: &WorldModel, expected: &[i64]) {
    for (table, rows) in model.tables() {
        let mut actual = rows.iter().map(|r| r.id).collect::<Vec<_>>();
        actual.sort_unstable();
        assert_eq!(actual, expected, "{}", table.as_str());
    }
}

#[test]
fn default_export_excludes_scratch_from_json_and_markdown() {
    let conn = fixture();
    let model = export(&conn).expect("export");
    assert_ids(&model, &[1]);
    for output in [
        serde_json::to_string(&model).expect("json"),
        to_markdown(&model, 1),
    ] {
        assert!(!output.contains("private"), "scratch text leaked");
    }
    for table in [StateTable::People, StateTable::Projects] {
        assert!(export_entity(&conn, table, 1).expect("public").is_some());
        assert!(export_entity(&conn, table, 2).expect("hidden").is_none());
    }
}

#[test]
fn export_respects_grants_revocation_and_explicit_world_only_reads() {
    let conn = fixture();
    let agent = Actor::agent("alpha").expect("actor");
    let other = Namespace::scratch("beta").expect("namespace");
    as_agent(ReadScope::WITH_SCRATCH, || {
        assert_ids(&export(&conn).expect("export"), &[1, 2]);
        for access in [ScratchAccess::Read, ScratchAccess::Write] {
            grant_scratch(&conn, &agent, &other, access, &Actor::owner(), 2).expect("grant");
            assert_ids(&export(&conn).expect("granted export"), &[1, 2, 3]);
            as_agent(ReadScope::WORLD_ONLY, || {
                assert_ids(&export(&conn).expect("world"), &[1])
            });
            for table in [StateTable::People, StateTable::Projects] {
                assert!(export_entity(&conn, table, 3)
                    .expect("granted entity")
                    .is_some());
            }
            revoke_scratch(&conn, &agent, &other).expect("revoke");
            assert_ids(&export(&conn).expect("revoked export"), &[1, 2]);
            for table in [StateTable::People, StateTable::Projects] {
                assert!(export_entity(&conn, table, 3)
                    .expect("hidden entity")
                    .is_none());
                assert!(export_entity(&conn, table, 99)
                    .expect("absent entity")
                    .is_none());
            }
        }
    });
    with_call_scope(
        CallScope {
            write: WriteContext::owner(),
            read: ReadScope::WITH_SCRATCH,
        },
        || {
            assert_ids(&export(&conn).expect("owner export"), &[1, 2, 3]);
        },
    );
    assert_ids(&export(&conn).expect("scope restored"), &[1]);
}

#[test]
fn visible_state_does_not_export_hidden_source_identifiers_or_counts() {
    let conn = fixture();
    let event = insert(
        &conn,
        &NewEvent {
            ts: 1,
            source: "capture",
            kind: "text",
            app_bundle_id: None,
            window_title: None,
            content: "Synthetic private evidence",
            content_hash: "private-export-fixture",
            dwell_ms: 0,
            display_id: None,
            window_bounds: None,
        },
    )
    .expect("event");
    conn.execute(
        "UPDATE event_log SET namespace='agent_scratch:beta' WHERE id=?1",
        [event],
    )
    .expect("event namespace");
    for table in [
        StateTable::People,
        StateTable::Projects,
        StateTable::Commitments,
        StateTable::OpenLoops,
    ] {
        conn.execute(
            "INSERT INTO state_provenance(state_table,state_id,event_id) VALUES(?1,1,?2)",
            params![table.as_str(), event],
        )
        .expect("link");
    }
    as_agent(ReadScope::WITH_SCRATCH, || {
        for (_, rows) in export(&conn).expect("export").tables() {
            assert!(rows.iter().all(|r| r.provenance.is_empty()));
        }
        for table in [StateTable::People, StateTable::Projects] {
            let row = export_entity(&conn, table, 1)
                .expect("entity")
                .expect("visible state");
            assert!(row.provenance.is_empty());
            assert_eq!(row.evidence_count, Some(0));
        }
        grant_scratch(
            &conn,
            &Actor::agent("alpha").expect("actor"),
            &Namespace::scratch("beta").expect("namespace"),
            ScratchAccess::Read,
            &Actor::owner(),
            2,
        )
        .expect("grant");
        for (_, rows) in export(&conn).expect("granted export").tables() {
            assert_eq!(
                rows.iter().find(|r| r.id == 1).expect("row").provenance,
                vec![event]
            );
        }
        assert_eq!(
            export_entity(&conn, StateTable::People, 1)
                .expect("entity")
                .expect("visible")
                .evidence_count,
            Some(1)
        );
    });
}
