#![allow(clippy::unwrap_used, clippy::expect_used)]

use rusqlite::Connection;
use shogun_memory::actor::{Actor, WriteContext};
use shogun_memory::event_log::{self, NewEvent};
use shogun_memory::state::{self, NewCommitment, NewPerson, Provenance};

use super::*;

fn event(conn: &Connection, text: &str) -> i64 {
    let hash = event_log::content_hash(text);
    event_log::insert(
        conn,
        &NewEvent {
            ts: 1_000,
            source: "capture",
            kind: "text",
            app_bundle_id: Some("com.example.notes"),
            window_title: Some("Notes"),
            content: text,
            content_hash: &hash,
            dwell_ms: 0,
            display_id: None,
            window_bounds: None,
        },
    )
    .unwrap()
}

fn person(conn: &mut Connection, name: &str, confidence: f64) -> i64 {
    let ev = event(conn, &format!("{name} joined the planning call."));
    let id = state::insert_person(
        conn,
        &NewPerson { display_name: name, confidence, now: 1_000, ..NewPerson::default() },
        &[Provenance::new(ev)],
    )
    .unwrap();
    // Pinned so the list rule shows it whatever its mention count.
    conn.execute("UPDATE people SET user_pinned = 1 WHERE id = ?1", [id]).unwrap();
    id
}

fn commitment(conn: &mut Connection, text: &str, confidence: f64) -> i64 {
    let ev = event(conn, text);
    let id = state::insert_commitment(
        conn,
        &NewCommitment {
            direction: state::CommitmentDirection::Mine,
            counterparty_id: None,
            description: text,
            due_at: None,
            status: state::CommitmentStatus::Open,
            project_id: None,
            confidence,
            now: 1_000,
        },
        &[Provenance::new(ev)],
    )
    .unwrap();
    conn.execute(
        "UPDATE state_provenance SET quote = ?1 WHERE state_table = 'commitments' AND state_id = ?2",
        rusqlite::params![text, id],
    )
    .unwrap();
    id
}

#[test]
fn slugs_are_stable_unique_and_safe() {
    assert_eq!(slug("Oren Vale", 12, "person"), "oren-vale-12");
    assert_eq!(slug("  Oren   Vale!! ", 12, "person"), "oren-vale-12");
    assert_eq!(slug("Heron Rollout / Q3", 4, "project"), "heron-rollout-q3-4");
    assert_eq!(slug("佐伯 真", 7, "person"), "佐伯-真-7");
    assert_eq!(slug("!!!", 9, "person"), "person-9");
    assert_eq!(slug("../../etc", 3, "project"), "etc-3", "no path separators or dots survive");
    assert_eq!(slug_id("oren-vale-12"), Some(12));
    let long = "x".repeat(500);
    assert!(slug(&long, 1, "person").chars().count() <= SLUG_STEM_CHARS + 2);

    // A row's slug does not move when other rows come and go.
    let mut conn = shogun_memory::open_in_memory().unwrap();
    let oren = person(&mut conn, "Oren Vale", 0.9);
    let before = files(&conn, None).unwrap();
    let path = format!("{PEOPLE_DIR}/{}.md", slug("Oren Vale", oren, "person"));
    assert!(before.iter().any(|e| e.path == path));
    person(&mut conn, "Oren Vale", 0.9); // a namesake gets its own id, so its own file
    person(&mut conn, "Tamsin Ekberg", 0.9);
    let after = files(&conn, None).unwrap();
    assert!(after.iter().any(|e| e.path == path), "the first row keeps its path");
    let people_files = after.iter().filter(|e| e.path.starts_with(PEOPLE_DIR)).count();
    assert_eq!(people_files, 3);
}

#[test]
fn index_stays_within_200_lines_and_25_kb_however_large_the_model() {
    let mut conn = shogun_memory::open_in_memory().unwrap();
    for n in 0..MAX_ENTITY_FILES + 50 {
        let name = format!("Person {n} with a deliberately long display name to fill the index quickly {}", "z".repeat(40));
        person(&mut conn, &name, 0.9);
    }
    for limit in [None, Some(Namespace::scratch("codex").unwrap())] {
        let index = index_md(&conn, limit.as_ref()).unwrap();
        assert!(index.lines().count() <= INDEX_MAX_LINES, "{} lines", index.lines().count());
        assert!(index.len() <= INDEX_MAX_BYTES, "{} bytes", index.len());
        assert!(index.contains("more in people/"), "the overflow is named");
    }
    // An empty store still has a well-formed index.
    let empty = shogun_memory::open_in_memory().unwrap();
    let index = index_md(&empty, None).unwrap();
    assert!(index.starts_with("# Memory index"));
    assert!(index.contains("[Commitments](commitments.md): 0 open"));
}

#[test]
fn lists_carry_band_and_quote_and_never_a_low_row() {
    let mut conn = shogun_memory::open_in_memory().unwrap();
    commitment(&mut conn, "I'll send the rollout plan to Oren by Friday.", 0.9);
    commitment(&mut conn, "I'll draft the cost estimate next week.", 0.6);
    commitment(&mut conn, "I'll maybe look at the dashboard.", 0.3);
    let md = read(&conn, COMMITMENTS, None).unwrap().unwrap();
    assert!(md.contains("- I'll send the rollout plan to Oren by Friday. (id"), "{md}");
    assert!(md.contains("> I'll send the rollout plan to Oren by Friday. (event"), "{md}");
    assert!(md.contains("- possibly: I'll draft the cost estimate next week."), "{md}");
    assert!(!md.contains("dashboard"), "a Low row is never projected: {md}");
    assert!(read(&conn, OPEN_LOOPS, None).unwrap().unwrap().contains("None open."));
    // Directories and unknown paths are not files.
    assert_eq!(read(&conn, PEOPLE_DIR, None).unwrap(), None);
    assert_eq!(read(&conn, "/memories/people/nobody-99.md", None).unwrap(), None);
}

#[test]
fn people_pages_resolve_only_by_their_current_slug_and_scratch_only_for_its_owner() {
    let mut conn = shogun_memory::open_in_memory().unwrap();
    let id = person(&mut conn, "Oren Vale", 0.9);
    let page = read(&conn, &format!("{PEOPLE_DIR}/oren-vale-{id}.md"), None).unwrap().unwrap();
    assert!(page.starts_with("# Oren Vale"), "{page}");
    assert!(page.contains("Source events:"), "{page}");
    assert_eq!(read(&conn, &format!("{PEOPLE_DIR}/someone-else-{id}.md"), None).unwrap(), None);

    let codex = WriteContext::scratch(Actor::agent("codex").unwrap()).unwrap();
    shogun_memory::scratch_files::write(&conn, &codex, "progress.md", "step 1 done", 5).unwrap();
    let mine = files(&conn, Some(&codex.namespace)).unwrap();
    assert!(mine.iter().any(|e| e.path == "/memories/scratch/codex/progress.md"));
    assert_eq!(read(&conn, "/memories/scratch/codex/progress.md", Some(&codex.namespace)).unwrap().as_deref(), Some("step 1 done"));
    // Another agent sees neither the file in its listing nor its content.
    let claude = Namespace::scratch("claude").unwrap();
    assert!(files(&conn, Some(&claude)).unwrap().iter().all(|e| !e.path.contains("codex")));
    assert_eq!(read(&conn, "/memories/scratch/codex/progress.md", Some(&claude)).unwrap(), None);
    assert_eq!(read(&conn, "/memories/scratch/codex/progress.md", None).unwrap(), None);
}
