//! As-of retrieval: `SearchOptions::until_ts`, the upper time bound.
//!
//! These are deterministic behaviour tests, not wall-clock gates, so they run on every
//! `cargo test` (the rule is stated in `tests/search_slo.rs`).
//!
//! What is being pinned down is the thing a floor alone could never express: "what was the answer
//! *as of* then". `since_ts` bounds the past, `until_ts` bounds the future, and the interesting
//! case is a fact that was revised — an as-of query must return the value that was current at the
//! bound, not the one that is current now.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use shogun_memory::search::{self, SearchDepth, SearchOptions};
use shogun_memory::event_log;

const MINUTE: i64 = 60_000;

/// Write one event and return its row id.
fn write(conn: &rusqlite::Connection, ts: i64, content: &str) -> i64 {
    event_log::insert(
        conn,
        &event_log::NewEvent {
            ts,
            source: "capture",
            kind: "text",
            app_bundle_id: Some("com.apple.mail"),
            window_title: Some("Mail"),
            content,
            content_hash: &format!("hash-{ts}"),
            dwell_ms: 0,
            display_id: None,
            window_bounds: None,
        },
    )
    .unwrap()
}

/// A log in which one project's database choice is revised twice.
fn revised_corpus(conn: &rusqlite::Connection) -> (i64, i64, i64) {
    let first = write(conn, 10 * MINUTE, "meridian will store its records in PostgreSQL");
    // Filler between the revisions so the answers are not adjacent rows.
    for i in 0..20 {
        write(
            conn,
            (11 + i) * MINUTE,
            &format!("meridian standup notes, nothing blocking [{i}]"),
        );
    }
    let second = write(conn, 40 * MINUTE, "meridian will store its records in SQLite");
    for i in 0..20 {
        write(
            conn,
            (41 + i) * MINUTE,
            &format!("meridian retrospective agenda item [{i}]"),
        );
    }
    let third = write(conn, 70 * MINUTE, "meridian will store its records in DuckDB");
    (first, second, third)
}

fn search_window(
    conn: &rusqlite::Connection,
    query: &str,
    since_ts: Option<i64>,
    until_ts: Option<i64>,
) -> Vec<i64> {
    let opts = SearchOptions {
        since_ts,
        until_ts,
        depth: SearchDepth::WarmOnly,
        ..Default::default()
    };
    search::search_hybrid_with_options(conn, query, None, 0, &opts, 10)
        .unwrap()
        .hits
        .into_iter()
        .map(|h| h.event_id)
        .collect()
}

#[test]
fn an_unbounded_search_is_unchanged_by_the_new_option() {
    let conn = shogun_memory::open_in_memory().unwrap();
    let (first, second, third) = revised_corpus(&conn);

    let got = search_window(&conn, "meridian store records", None, None);
    // All three revisions are reachable when no bound is set. This is the regression guard: the
    // default must reproduce the shipped behaviour exactly.
    for id in [first, second, third] {
        assert!(got.contains(&id), "unbounded search lost {id}: {got:?}");
    }
}

#[test]
fn an_as_of_bound_excludes_everything_said_after_it() {
    let conn = shogun_memory::open_in_memory().unwrap();
    let (first, second, third) = revised_corpus(&conn);

    // As of minute 50: the SQLite revision has happened, DuckDB has not.
    let got = search_window(&conn, "meridian store records", None, Some(50 * MINUTE));
    assert!(got.contains(&first), "first revision missing: {got:?}");
    assert!(got.contains(&second), "second revision missing: {got:?}");
    assert!(
        !got.contains(&third),
        "as-of query returned a fact from after its bound: {got:?}"
    );
}

#[test]
fn a_closed_window_excludes_both_ends() {
    let conn = shogun_memory::open_in_memory().unwrap();
    let (first, second, third) = revised_corpus(&conn);

    // Minutes 30..50 contain only the second revision.
    let got = search_window(
        &conn,
        "meridian store records",
        Some(30 * MINUTE),
        Some(50 * MINUTE),
    );
    assert!(got.contains(&second), "window lost its only answer: {got:?}");
    assert!(!got.contains(&first), "window reached before its floor: {got:?}");
    assert!(!got.contains(&third), "window reached past its ceiling: {got:?}");
}

#[test]
fn a_bound_before_the_first_event_returns_nothing() {
    let conn = shogun_memory::open_in_memory().unwrap();
    revised_corpus(&conn);

    let got = search_window(&conn, "meridian store records", None, Some(MINUTE));
    assert!(
        got.is_empty(),
        "a window that predates the log should be empty, got {got:?}"
    );
}

#[test]
fn an_inverted_window_returns_nothing_rather_than_ignoring_its_bounds() {
    let conn = shogun_memory::open_in_memory().unwrap();
    revised_corpus(&conn);

    // since > until. Silently widening this to an unbounded search would hide the caller's bug
    // behind a plausible-looking answer.
    let got = search_window(
        &conn,
        "meridian store records",
        Some(60 * MINUTE),
        Some(20 * MINUTE),
    );
    assert!(got.is_empty(), "inverted window should select nothing: {got:?}");
}

#[test]
fn the_floor_alone_still_behaves_as_it_always_did() {
    let conn = shogun_memory::open_in_memory().unwrap();
    let (first, _second, third) = revised_corpus(&conn);

    let got = search_window(&conn, "meridian store records", Some(60 * MINUTE), None);
    assert!(got.contains(&third), "floor lost the newest fact: {got:?}");
    assert!(!got.contains(&first), "floor reached below itself: {got:?}");
}

#[test]
fn an_as_of_query_answers_with_the_value_that_was_current_then() {
    let conn = shogun_memory::open_in_memory().unwrap();
    let (_first, second, third) = revised_corpus(&conn);

    // The whole point of the feature. Asking as of minute 50 must be able to reach the SQLite
    // revision without the DuckDB one in the running, which is not something ranking could do —
    // the newest row has to be out of the candidate set entirely.
    let as_of = search_window(&conn, "meridian store records", None, Some(50 * MINUTE));
    let now = search_window(&conn, "meridian store records", None, None);

    assert!(as_of.contains(&second) && !as_of.contains(&third), "{as_of:?}");
    assert!(now.contains(&third), "present-tense query lost the newest fact: {now:?}");
}
