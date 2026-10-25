//! The end-to-end coverage the audit found missing (PII-004).
//!
//! These are regression tests, not measurements: they assert the properties the design *claims*,
//! so that a refactor moving a write off the sanitized path fails CI instead of going unnoticed.
//! The measurement itself is the binary.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pii_bench::corpus::{Category, Expectation};
use pii_bench::{run_all, Boundary};

#[test]
fn every_case_runs_through_every_boundary() {
    let outcomes = run_all().expect("benchmark runs");
    let cases = pii_bench::CASES.len();
    assert_eq!(outcomes.len(), cases * Boundary::ALL.len());
}

#[test]
fn targeted_secrets_never_survive_the_content_column() {
    // The redactor's declared contract. If this fails, secret masking is broken.
    let outcomes = run_all().expect("benchmark runs");
    for o in outcomes.iter().filter(|o| {
        o.boundary == Boundary::Content && o.category == Category::TargetedSecret
    }) {
        assert!(
            !o.leaked(),
            "{} survived the content column (row={} fts={})",
            o.case_id,
            o.survived_in_row,
            o.survived_in_fts
        );
    }
}

#[test]
fn look_alike_prose_is_never_mangled() {
    // Precision is the design's central constraint: corrupting real memory is worse than missing
    // a secret, because the product is built on that memory.
    let outcomes = run_all().expect("benchmark runs");
    for o in outcomes
        .iter()
        .filter(|o| matches!(o.expectation, Expectation::Untouched))
    {
        assert!(
            o.leaked(),
            "{} at {} was masked but should pass through untouched",
            o.case_id,
            o.boundary.as_str()
        );
    }
}

#[test]
fn the_fts_index_never_leaks_what_the_row_masked() {
    let outcomes = run_all().expect("benchmark runs");
    for o in outcomes.iter() {
        if !o.survived_in_row && o.survived_in_fts {
            panic!(
                "{} at {}: masked in the row but still findable via FTS",
                o.case_id,
                o.boundary.as_str()
            );
        }
    }
}

/// PII-001, pinned as a test: the assertion the `window_title` fix must satisfy.
#[test]
fn targeted_secrets_never_survive_the_window_title_column() {
    let outcomes = run_all().expect("benchmark runs");
    for o in outcomes.iter().filter(|o| {
        o.boundary == Boundary::WindowTitle && o.category == Category::TargetedSecret
    }) {
        assert!(
            !o.leaked(),
            "{} survived window_title (row={} fts={})",
            o.case_id,
            o.survived_in_row,
            o.survived_in_fts
        );
    }
}

#[test]
fn sanitizing_the_title_does_not_leave_the_secret_in_the_thread_key() {
    // The half of PII-001 that is easy to miss: `thread_key` is "{source}:{app}:{title}", so the
    // title is embedded in the key string itself. Masking only the stored column would leave the
    // secret sitting in `event_log.thread_key`, `threads.thread_key` and `threads.title`.
    let conn = shogun_memory::open_in_memory().expect("db");
    let secret = "sk-ant-api03-AAAABBBBCCCCDDDDEEEEFFFF";
    let title = format!("Terminal — export KEY={secret}");
    let hash = shogun_memory::event_log::content_hash("body");
    shogun_memory::event_log::insert(
        &conn,
        &shogun_memory::event_log::NewEvent {
            ts: 1_000,
            source: "capture",
            kind: "text",
            app_bundle_id: Some("com.apple.Terminal"),
            window_title: Some(&title),
            content: "body",
            content_hash: &hash,
            dwell_ms: 0,
            display_id: None,
            window_bounds: None,
        },
    )
    .expect("insert");

    let (stored_title, key): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT window_title, thread_key FROM event_log WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("row");
    assert!(
        !stored_title.unwrap_or_default().contains(secret),
        "secret survived event_log.window_title"
    );
    assert!(
        !key.clone().unwrap_or_default().contains(secret),
        "secret survived event_log.thread_key"
    );

    if let Some(k) = key {
        let (t_title, t_key): (Option<String>, String) = conn
            .query_row(
                "SELECT title, thread_key FROM threads WHERE thread_key = ?1",
                [&k],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("thread row");
        assert!(!t_title.unwrap_or_default().contains(secret), "secret survived threads.title");
        assert!(!t_key.contains(secret), "secret survived threads.thread_key");
    }
}

#[test]
fn titles_differing_only_by_a_secret_now_group_together() {
    // These produce one thread_key, because the differing part is masked to the same token. It
    // is a grouping change that can only affect titles that contained a secret.
    let conn = shogun_memory::open_in_memory().expect("db");
    for (i, tok) in ["AAAABBBBCCCCDDDDEEEE", "111122223333444455556"].iter().enumerate() {
        let title = format!("Safari — dashboard?access_token={tok}");
        let body = format!("visit {i}");
        let hash = shogun_memory::event_log::content_hash(&body);
        shogun_memory::event_log::insert(
            &conn,
            &shogun_memory::event_log::NewEvent {
                ts: 1_000 + i as i64,
                source: "capture",
                kind: "text",
                app_bundle_id: Some("com.apple.Safari"),
                window_title: Some(&title),
                content: &body,
                content_hash: &hash,
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .expect("insert");
    }
    let distinct: i64 = conn
        .query_row("SELECT count(DISTINCT thread_key) FROM event_log", [], |r| r.get(0))
        .expect("count");
    assert_eq!(distinct, 1, "two visits to the same window should be one thread");
}

#[test]
fn national_ids_and_cards_never_leave_the_device() {
    let outcomes = run_all().expect("benchmark runs");
    let checked = outcomes
        .iter()
        .filter(|o| o.boundary == Boundary::Egress && o.category == Category::HighRiskIdentifier)
        .inspect(|o| {
            assert!(
                !o.leaked(),
                "{} left the device through the egress path",
                o.case_id
            );
        })
        .count();
    assert!(checked > 0, "no high-risk identifiers were exercised");
}

#[test]
fn the_db_path_still_keeps_what_it_is_designed_to_keep() {
    let outcomes = run_all().expect("benchmark runs");
    for o in outcomes
        .iter()
        .filter(|o| o.boundary == Boundary::Content && o.category == Category::HighRiskIdentifier)
    {
        assert_eq!(
            o.expectation,
            Expectation::SurvivesByDesign,
            "{} must be characterised on the DB path, not scored as a defect",
            o.case_id
        );
    }
}

#[test]
fn look_alikes_survive_the_egress_path_untouched() {
    // Precision. A Luhn or SSN detector that eats order numbers and timestamps is worse than no
    // detector at all, because it silently removes text the product legitimately holds.
    let outcomes = run_all().expect("benchmark runs");
    for o in outcomes
        .iter()
        .filter(|o| o.boundary == Boundary::Egress && o.category == Category::LookAlike)
    {
        assert!(
            o.survived_in_row,
            "{} was masked at egress but is ordinary text",
            o.case_id
        );
    }
}

#[test]
fn secrets_are_masked_at_egress_as_well_as_on_write() {
    let outcomes = run_all().expect("benchmark runs");
    for o in outcomes
        .iter()
        .filter(|o| o.boundary == Boundary::Egress && o.category == Category::TargetedSecret)
    {
        assert!(!o.leaked(), "{} survived the egress path", o.case_id);
    }
}

#[test]
fn a_key_pasted_in_the_wrong_case_is_still_a_key() {
    // PII-004: issuer prefixes match case-insensitively, so an uppercase spelling must not reach
    // disk verbatim.
    let outcomes = run_all().expect("benchmark runs");
    for o in outcomes.iter().filter(|o| {
        o.boundary == Boundary::Content
            && matches!(o.case_id, "uppercased-prefix" | "mixed-case-prefix" | "uppercased-github-pat")
    }) {
        assert!(
            !o.leaked(),
            "{} survived: issuer prefixes are case-sensitive again",
            o.case_id
        );
    }
}
