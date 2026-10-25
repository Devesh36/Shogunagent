//! E2-T4 / E2-T5: every write records its actor and namespace (V92 / V93), an empty actor is
//! refused at this layer, and default reads never return an agent's scratch rows.

use rusqlite::Connection;

use crate::actor::{
    current_read, with_call_scope, with_read_scope, Actor, CallScope, Namespace, ReadScope,
    WriteContext,
};
use crate::event_log::{self, NewEvent};
use crate::state::{self, CommitmentDirection, CommitmentStatus, NewCommitment, Provenance};
use crate::state_edit::{self, CreateRequest, StatePatch};
use crate::MemoryError;

fn event<'a>(text: &'a str, hash: &'a str) -> NewEvent<'a> {
    NewEvent {
        ts: 1_700_000_000_000,
        source: "user",
        kind: "note",
        app_bundle_id: None,
        window_title: None,
        content: text,
        content_hash: hash,
        dwell_ms: 0,
        display_id: None,
        window_bounds: None,
    }
}

fn agent_scratch() -> WriteContext {
    WriteContext::scratch(Actor::agent("codex").unwrap()).unwrap()
}

fn row_marks(conn: &Connection, table: &str, id: i64, actor_column: &str) -> (Option<String>, String) {
    conn.query_row(
        &format!("SELECT {actor_column}, namespace FROM {table} WHERE id = ?1"),
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

fn commitment<'a>(text: &'a str) -> NewCommitment<'a> {
    NewCommitment {
        direction: CommitmentDirection::Mine,
        counterparty_id: None,
        description: text,
        due_at: None,
        status: CommitmentStatus::Open,
        project_id: None,
        confidence: 0.9,
        now: 1_700_000_000_000,
    }
}

#[test]
fn an_event_records_its_writer_and_namespace() {
    let conn = crate::open_in_memory().unwrap();
    let owner = event_log::insert(&conn, &event("owner note", "h1")).unwrap();
    assert_eq!(row_marks(&conn, "event_log", owner, "actor"), (Some("human:owner".into()), "world".into()));

    let ctx = WriteContext::world(Actor::agent("claude-code").unwrap());
    let agent = event_log::insert_as(&conn, &event("agent note", "h2"), &ctx).unwrap();
    assert_eq!(row_marks(&conn, "event_log", agent, "actor"), (Some("agent:claude-code".into()), "world".into()));

    let scratch = event_log::insert_as(&conn, &event("scratch", "h3"), &agent_scratch()).unwrap();
    assert_eq!(
        row_marks(&conn, "event_log", scratch, "actor"),
        (Some("agent:codex".into()), "agent_scratch:codex".into())
    );
}

#[test]
fn a_write_without_an_actor_is_refused_before_any_row() {
    assert!(matches!(WriteContext::new("", Namespace::World), Err(MemoryError::EmptyActor)));
    assert!(matches!(WriteContext::new("  ", Namespace::World), Err(MemoryError::EmptyActor)));
    assert!(matches!(Actor::agent(""), Err(MemoryError::InvalidActor)));
}

#[test]
fn dedup_touch_stays_within_the_namespace() {
    let conn = crate::open_in_memory().unwrap();
    let (world, touched) = event_log::insert_or_touch(&conn, &event("same text", "h")).unwrap();
    assert!(!touched);
    // The same text in the agent's scratch is a new row, not a touch of the world row.
    let (scratch, touched) = event_log::insert_or_touch_as(&conn, &event("same text", "h"), &agent_scratch()).unwrap();
    assert!(!touched);
    assert_ne!(world, scratch);
    // And the world row is touched by another world write.
    let (again, touched) = event_log::insert_or_touch(&conn, &event("same text", "h")).unwrap();
    assert!(touched);
    assert_eq!(again, world);
}

#[test]
fn state_rows_record_their_writer_and_default_reads_hide_scratch() {
    let mut conn = crate::open_in_memory().unwrap();
    let ev = event_log::insert(&conn, &event("I will send the deck by Friday", "e1")).unwrap();
    let prov = [Provenance::new(ev)];
    let owner_id = state::insert_commitment(&mut conn, &commitment("send the deck"), &prov).unwrap();
    assert_eq!(
        row_marks(&conn, "commitments", owner_id, "actor_last"),
        (Some("human:owner".into()), "world".into())
    );

    let ctx = agent_scratch();
    let scratch_id = state::insert_with_provenance_as(
        &mut conn,
        state::StateTable::Commitments,
        &prov,
        &ctx,
        |tx| {
            tx.execute(
                "INSERT INTO commitments (direction, description, status, confidence, created_at, updated_at, last_evidence_at)
                 VALUES ('mine', 'agent scratch item', 'open', 0.9, 1, 1, 1)",
                [],
            )?;
            Ok(tx.last_insert_rowid())
        },
    )
    .unwrap();
    assert_eq!(
        row_marks(&conn, "commitments", scratch_id, "actor_last"),
        (Some("agent:codex".into()), "agent_scratch:codex".into())
    );

    // Default read: world only.
    let ids: Vec<i64> = state::list_commitments(&conn).unwrap().iter().map(|c| c.id).collect();
    assert_eq!(ids, vec![owner_id]);
    assert!(state::get_commitment(&conn, scratch_id).unwrap().is_none());
    assert!(state::get_commitment(&conn, owner_id).unwrap().is_some());

    // Explicit scope.
    let both = state::list_commitments_scoped(&conn, ReadScope::WITH_SCRATCH).unwrap();
    assert_eq!(both.len(), 2);
    assert!(state::get_commitment_scoped(&conn, scratch_id, ReadScope::WITH_SCRATCH).unwrap().is_some());

    // The call scope a face sets.
    let seen = with_read_scope(ReadScope::WITH_SCRATCH, || {
        assert_eq!(current_read(), ReadScope::WITH_SCRATCH);
        state::list_commitments(&conn).unwrap().len()
    });
    assert_eq!(seen, 2);
    assert_eq!(state::list_commitments(&conn).unwrap().len(), 1);
}

#[test]
fn state_create_update_and_delete_carry_the_actor() {
    let mut conn = crate::open_in_memory().unwrap();
    let now: i64 = 1_700_000_000_000;
    let ctx = WriteContext::world(Actor::agent("claude-code").unwrap());
    let req = CreateRequest {
        table: state::StateTable::OpenLoops,
        description: "ask Aiko about the forecast".into(),
        due_at: None,
        person_id: None,
    };
    let id = state_edit::create_state_as(&mut conn, &req, now, &ctx).unwrap().unwrap();
    assert_eq!(
        row_marks(&conn, "open_loops", id, "actor_last"),
        (Some("agent:claude-code".into()), "world".into())
    );

    let editor = WriteContext::world(Actor::agent("codex").unwrap());
    let patch = StatePatch { status: Some("closed".into()), ..StatePatch::default() };
    assert!(state_edit::update_state_as(&mut conn, state::StateTable::OpenLoops, id, &patch, now + 1, &editor).unwrap());
    assert_eq!(row_marks(&conn, "open_loops", id, "actor_last").0.as_deref(), Some("agent:codex"));

    // A call scope stands in for the explicit context on the un-suffixed functions.
    let scoped = with_call_scope(CallScope { write: ctx.clone(), read: ReadScope::WORLD_ONLY }, || {
        state_edit::create_state(&mut conn, &req, now + 2).unwrap().unwrap()
    });
    assert_eq!(row_marks(&conn, "open_loops", scoped, "actor_last").0.as_deref(), Some("agent:claude-code"));

    assert!(state_edit::delete_state_as(&mut conn, state::StateTable::OpenLoops, id, now + 3, &editor).unwrap());
    let (deleted_at, reason): (i64, String) = conn
        .query_row(
            "SELECT deleted_at, reason FROM export_tombstones WHERE kind = 'open_loops' AND id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(deleted_at, now + 3);
    assert_eq!(reason, "deleted_by:agent:codex");
}

#[test]
fn delete_everything_empties_the_tenancy_tables() {
    let mut conn = crate::open_in_memory().unwrap();
    let now: i64 = 1_700_000_000_000;
    let ev = event_log::insert(&conn, &event("evidence", "e")).unwrap();
    conn.execute_batch(&format!(
        "INSERT INTO tool_runs (ts, actor, tool_id, outcome) VALUES ({now}, 'agent:a', 'gmail.search', 'ok');
         INSERT INTO principal_meta (key, value) VALUES ('export_cursor', '12');
         INSERT INTO relations (subject_kind, subject_id, predicate, object_kind, object_id, valid_from,
                                confidence, provenance_event_id, created_at)
         VALUES ('people', 1, 'works_on', 'projects', 1, {now}, 0.9, {ev}, {now});
         INSERT INTO export_tombstones (kind, id, deleted_at) VALUES ('people', 9, {now});"
    ))
    .unwrap();
    crate::maintenance::delete_all(&mut conn).unwrap();
    for table in ["tool_runs", "principal_meta", "relations", "export_tombstones"] {
        let n: i64 = conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0, "{table} not emptied");
    }
}

#[test]
fn a_scratch_event_never_reaches_a_world_only_search() {
    let conn = crate::open_in_memory().unwrap();
    let world = event_log::insert(&conn, &event("zebrafish budget review with Aiko", "w1")).unwrap();
    let scratch = event_log::insert_as(&conn, &event("zebrafish scratch note of the agent", "s1"), &agent_scratch()).unwrap();
    let options = crate::search::FederatedSearchOptions::default();
    let now = 1_700_000_001_000;

    let hits = crate::search::search_federated(&conn, "zebrafish", None, now, &options, 10).unwrap();
    let ids: Vec<i64> = hits.hits.iter().flat_map(crate::search::hit_event_ids).collect();
    assert!(ids.contains(&world), "world hit missing: {ids:?}");
    assert!(!ids.contains(&scratch), "scratch hit leaked: {ids:?}");
    assert!(hits.raw_event_ranks.iter().all(|(id, _)| *id != scratch));

    let both = with_read_scope(ReadScope::WITH_SCRATCH, || {
        crate::search::search_federated(&conn, "zebrafish", None, now, &options, 10).unwrap()
    });
    let ids: Vec<i64> = both.hits.iter().flat_map(crate::search::hit_event_ids).collect();
    assert!(ids.contains(&world) && ids.contains(&scratch), "{ids:?}");

    let out = event_log::out_of_scope_ids(&conn, &[world, scratch], ReadScope::WORLD_ONLY).unwrap();
    assert_eq!(out.into_iter().collect::<Vec<_>>(), vec![scratch]);
}
