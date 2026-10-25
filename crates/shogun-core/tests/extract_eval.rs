//! Extraction eval over the Dream Cycle's model-free runners (issue #317; docs/extract-eval.md).
//!
//! Two runners, both through public shogun-core API and nothing else — `dreamcycle::jobs` is not
//! modified for this:
//!
//! - **local rules**: [`LocalRuleClassifier`] over each golden event as `event_log` stores it
//!   (the Degraded / no-Batch night);
//! - **Batch replay**: checked-in synthetic Batch results fed through
//!   [`parse_batch_classification`], with the day split by the product's Batch source policy —
//!   cloud-eligible events are written to an in-memory `event_log`, read back through the
//!   product's partition, folded into **sessions** (one request per app / window / hour, zero-base
//!   plan L1) and parsed from the replay, whose items name their support as anchors into the
//!   session's lines (L2); `meeting` events go to local rules, as in the real Consolidation. The
//!   replay responses are hand-shaped, not model output: this scores the parse path, the anchor
//!   resolution and the scorer, not a model (docs/extract-eval.md §3).
//!
//! **Mail triage is off in this replay** (zero-base plan L0(b)). The golden days hold `gmail`
//! events (day-01's Oren Vale mail among them), and in the product those rows leave the
//! consolidation lane while mail triage is on — their commitments and open loops come out of
//! Stage B instead (`shogun_core::mail_batch`, same parser, same persistence path). The
//! consolidation replay here is built with the default [`SessionPolicy`] (no runtime exclusion,
//! the triage-off shape — `build_batch_sessions_with_stats`), so the gate still scores the mail
//! goldens through the parse path and stays meaningful; `the_mail_source_leaves_the_replay_only_under_the_triage_on_policy`
//! pins that the triage-on policy would remove exactly those rows.
//!
//! The floors are deliberately loose, like `shogun-memory/tests/retrieval_eval.rs`: they catch a
//! broken pipeline or a large regression, not a point of F1. The measured numbers are in the PR
//! and in docs/extract-eval.md; tighten the floors from measurement, not by guess.
//!
//! `db` feature: `dreamcycle::jobs` exists only there. CI runs this in the
//! `cargo test -p shogun-core --features daemon-server` step (a superset of `db`); print the
//! report with
//! `cargo test -p shogun-core --features db --test extract_eval -- --nocapture`.
#![cfg(feature = "db")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use extract_eval::golden::{self, BandLabel, SynthDay, Table};
use extract_eval::runners::{self, Classified, ReplayFixture};
use extract_eval::score::Score;
use extract_eval::{lint, report};
use shogun_core::daemon::Db;
use shogun_core::dreamcycle::jobs::{
    build_batch_sessions_with_policy, build_batch_sessions_with_stats, parse_batch_classification,
    AnchorStats, BatchSession, Classifier, LocalRuleClassifier, SessionPolicy, MAIL_SOURCE,
};
use shogun_core::llm::anthropic::BatchResult;
use shogun_memory::event_log::{EventText, NewEvent};

fn days() -> Vec<SynthDay> {
    golden::golden_days().expect("golden days parse")
}

fn fixtures() -> Vec<ReplayFixture> {
    runners::replay_fixtures().expect("replay fixtures parse")
}

/// Events as the Dream Cycle reads them: `event_log`'s stored text.
fn stored<'a>(events: impl IntoIterator<Item = &'a golden::Event>) -> Vec<EventText> {
    events
        .into_iter()
        .map(|e| EventText { id: e.id, content: runners::stored_text(&e.text) })
        .collect()
}

fn local_rules(day: &SynthDay) -> Classified {
    LocalRuleClassifier.classify(&stored(&day.events))
}

/// One golden day in the product's `event_log`, folded into the sessions the consolidation lane
/// would send. Row ids are the store's; `golden_id` maps them back to the day's event ids.
struct Night {
    sessions: Vec<BatchSession>,
    golden_id: HashMap<i64, i64>,
    cloud_rows: usize,
}

impl Night {
    fn load(day: &SynthDay) -> Self {
        let db = Db::open_in_memory(Arc::new(|| 0)).expect("in-memory db");
        let mut golden_id = HashMap::new();
        for e in &day.events {
            let hash = format!("golden-{}", e.id);
            let (row, touched) = db
                .capture(&NewEvent {
                    ts: e.ts_ms,
                    source: &e.source,
                    kind: "text",
                    app_bundle_id: Some(&e.app),
                    window_title: Some(&e.window),
                    content: &e.text,
                    content_hash: &hash,
                    dwell_ms: 0,
                    display_id: None,
                    window_bounds: None,
                })
                .expect("golden event stored");
            assert!(!touched, "{}: event {} collapsed into an earlier row", day.id, e.id);
            golden_id.insert(row, e.id);
        }
        // The product's window read: source-filtered cloud half plus its grouping metadata.
        // Default policy = mail triage off (see the module note).
        let window = db.try_events_in_range_partitioned(0, i64::MAX).expect("partition");
        let (sessions, _) = build_batch_sessions_with_stats(&window.cloud, &window.cloud_meta);
        Self { sessions, golden_id, cloud_rows: window.cloud.len() }
    }

    /// The same day under a runtime policy, as the scheduler would build it, with the store's
    /// ids of the day's `gmail` rows (the goldens give mail an app, so a mail session is known by
    /// its rows' source, not by its key).
    fn sessions_under(day: &SynthDay, policy: &SessionPolicy) -> (Vec<BatchSession>, BTreeSet<i64>) {
        let db = Db::open_in_memory(Arc::new(|| 0)).expect("in-memory db");
        for e in &day.events {
            db.capture(&NewEvent {
                ts: e.ts_ms,
                source: &e.source,
                kind: "text",
                app_bundle_id: Some(&e.app),
                window_title: Some(&e.window),
                content: &e.text,
                content_hash: &format!("golden-{}", e.id),
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            })
            .expect("golden event stored");
        }
        let window = db.try_events_in_range_partitioned(0, i64::MAX).expect("partition");
        let gmail = window.cloud_meta.iter().filter(|m| m.source == MAIL_SOURCE).map(|m| m.id).collect();
        (build_batch_sessions_with_policy(&window.cloud, &window.cloud_meta, policy).0, gmail)
    }

    fn expected_ids(&self) -> Vec<String> {
        self.sessions.iter().map(|s| s.custom_id.clone()).collect()
    }

    /// A session's events as golden ids, in time order.
    fn session_events(&self, session: &BatchSession) -> Vec<i64> {
        session.event_ids().iter().map(|row| self.golden_id[row]).collect()
    }

    /// Parse a replay against this night's sessions; candidates keyed by golden event id.
    fn parse(&self, results: &[BatchResult]) -> Result<(Classified, AnchorStats), shogun_core::llm::LlmError> {
        let parsed = parse_batch_classification(&self.expected_ids(), &self.sessions, results)?;
        let by_event = parsed
            .by_event
            .into_iter()
            .map(|(row, cands)| (self.golden_id[&row], cands))
            .collect();
        Ok((by_event, parsed.anchors))
    }
}

fn batch_results(fixture: &ReplayFixture) -> Vec<BatchResult> {
    fixture
        .results
        .iter()
        .map(|r| BatchResult { custom_id: r.custom_id.clone(), text: r.text(), error: r.error.clone() })
        .collect()
}

fn replay(day: &SynthDay, fixture: &ReplayFixture) -> (Classified, AnchorStats) {
    let night = Night::load(day);
    let (_, local_only) = runners::partition_for_batch(day);
    let (mut out, anchors) = night
        .parse(&batch_results(fixture))
        .expect("the replay parses as a complete, successful Batch");
    out.extend(LocalRuleClassifier.classify(&stored(local_only)));
    (out, anchors)
}

fn total(days: &[SynthDay], mut run: impl FnMut(&SynthDay) -> Classified) -> Score {
    let mut s = Score::default();
    for day in days {
        s.merge(&runners::score(day, &run(day)));
    }
    s
}

fn fixture_for<'a>(fixtures: &'a [ReplayFixture], day: &SynthDay) -> &'a ReplayFixture {
    fixtures.iter().find(|f| f.day == day.id).expect("every golden day has a replay fixture")
}

#[test]
fn replay_fixtures_are_synthetic_and_cover_each_session_once() {
    let days = days();
    let fixtures = fixtures();
    assert_eq!(fixtures.len(), days.len());
    for day in &days {
        let f = fixture_for(&fixtures, day);
        assert_eq!(f.format, runners::REPLAY_FORMAT);
        let findings = lint::lint_replay(f, day);
        assert!(findings.is_empty(), "replay lint: {findings:?}");
        let night = Night::load(day);
        let (cloud, _) = runners::partition_for_batch(day);
        // One result per session, no session twice, and the fixture's `events` are exactly the
        // golden events the lane folded into that session (so a reader can tell what the model
        // saw). Every session event is cloud-eligible: no meeting text behind any id.
        let mut ids: Vec<&str> = f.results.iter().map(|r| r.custom_id.as_str()).collect();
        ids.sort_unstable();
        let mut expected: Vec<String> = night.expected_ids();
        expected.sort();
        assert_eq!(ids, expected.iter().map(String::as_str).collect::<Vec<_>>(), "{}: one result per session", day.id);
        for r in &f.results {
            let session = night.sessions.iter().find(|s| s.custom_id == r.custom_id).unwrap();
            assert_eq!(r.events, night.session_events(session), "{}: session {} membership", day.id, r.custom_id);
            for id in &r.events {
                assert!(cloud.contains(id), "{}: {id} is not cloud-eligible", day.id);
            }
        }
        let folded: BTreeSet<i64> = night.sessions.iter().flat_map(|s| night.session_events(s)).collect();
        assert!(folded.iter().all(|id| cloud.contains(id)), "{}: a session holds a non-cloud event", day.id);
    }
}

#[test]
fn the_mail_source_leaves_the_replay_only_under_the_triage_on_policy() {
    let mut gmail_days = 0;
    for day in &days() {
        let (off, gmail) = Night::sessions_under(day, &SessionPolicy::for_mail_triage(false));
        assert_eq!(off, Night::load(day).sessions, "{}: the replay is the triage-off shape", day.id);
        let (on, _) = Night::sessions_under(day, &SessionPolicy::for_mail_triage(true));
        let holds_mail = |s: &BatchSession| s.event_ids().iter().any(|id| gmail.contains(id));
        let mail: Vec<&BatchSession> = off.iter().filter(|s| holds_mail(s)).collect();
        if !mail.is_empty() {
            gmail_days += 1;
        }
        assert_eq!(on.len() + mail.len(), off.len(), "{}: only gmail sessions leave", day.id);
        assert!(on.iter().all(|s| !holds_mail(s)), "{}: no gmail row is sent under triage on", day.id);
        for s in &on {
            assert!(off.contains(s), "{}: a non-gmail session changed under the policy", day.id);
        }
    }
    assert!(gmail_days > 0, "the goldens must hold gmail events for this gate to mean anything");
}

/// The measurement the zero-base plan is about (L1): rows in, requests out. Printed per day with
/// `--nocapture`; the floor only says sessions are fewer than rows on a day with repeated windows.
#[test]
fn golden_days_fold_into_fewer_requests_than_rows() {
    let mut rows = 0;
    let mut sessions = 0;
    for day in &days() {
        let night = Night::load(day);
        println!(
            "[extract-eval/sessions] {}: cloud rows {} -> sessions {}",
            day.id,
            night.cloud_rows,
            night.sessions.len()
        );
        rows += night.cloud_rows;
        sessions += night.sessions.len();
        for s in &night.sessions {
            assert!(!s.lines.is_empty());
            assert!(s.chars() <= shogun_core::dreamcycle::jobs::SESSION_MAX_CHARS);
        }
    }
    assert!(sessions < rows, "rows {rows} -> sessions {sessions}");
}

#[test]
fn local_rule_classifier_gate() {
    let s = total(&days(), local_rules);
    println!("{}", report::markdown("LocalRuleClassifier (Dream Cycle, no Batch)", &s));

    // Local rules never produce entities, so people / projects are all omissions — by design.
    assert_eq!(s.item(Table::People).tp + s.item(Table::Projects).tp, 0);
    // Loose floors on what local rules do produce.
    assert!(s.item(Table::Commitments).recall().unwrap_or(0.0) >= 0.70, "commitment recall");
    assert!(s.item(Table::Commitments).precision().unwrap_or(0.0) >= 0.55, "commitment precision");
    assert!(s.item(Table::OpenLoops).recall().unwrap_or(0.0) >= 0.70, "open-loop recall");
    assert!(s.item(Table::OpenLoops).precision().unwrap_or(0.0) >= 0.60, "open-loop precision");
    // Every local candidate cites the event it came from, and that event says it.
    assert_eq!(s.provenance.dangling, 0);
    assert_eq!(s.provenance.validity(), Some(1.0), "local-rule provenance must be exact");
    // FR-ST-20: local rules are capped under the Medium edge, so nothing lands above Low.
    for band in [BandLabel::Medium, BandLabel::High] {
        assert_eq!(s.calibration.get(&band).map_or(0, |b| b.n), 0, "local rules above Low");
    }
}

#[test]
fn batch_replay_gate() {
    let days = days();
    let fixtures = fixtures();
    let mut anchors = AnchorStats::default();
    let s = total(&days, |d| {
        let (classified, a) = replay(d, fixture_for(&fixtures, d));
        anchors.resolved += a.resolved;
        anchors.unresolved += a.unresolved;
        anchors.line_mismatch += a.line_mismatch;
        classified
    });
    println!("{}", report::markdown("Batch replay (sessions + anchors through parse_batch_classification; local rules for meetings)", &s));
    println!(
        "[extract-eval/anchors] resolved {} unresolved {} line_mismatch {}",
        anchors.resolved, anchors.unresolved, anchors.line_mismatch
    );

    let items = s.items_total();
    assert!(items.recall().unwrap_or(0.0) >= 0.70, "replay row recall");
    assert!(items.precision().unwrap_or(0.0) >= 0.70, "replay row precision");
    assert!(s.fields_total().f1().unwrap_or(0.0) >= 0.70, "replay field F1");
    // Entities do reach the scorer through the parse path.
    assert!(s.item(Table::People).tp > 0 && s.item(Table::Projects).tp > 0);
    // Anchors: the fixtures carry one deliberately unresolvable anchor (day-02); it is dropped
    // and counted, never attributed to an event.
    assert!(anchors.resolved > 0);
    assert!(anchors.unresolved >= 1, "the unresolvable anchor must be counted");
    // One item carries a deliberately wrong `line`: the anchor check rejects the number, the
    // unique anchor still names the line, and the rejection is counted.
    assert_eq!(anchors.line_mismatch, 1, "the wrong-line item is counted exactly once");
    // Provenance is recovered from the anchors: every citation names a real event (none
    // dangling), the recovered spans are literal source text (every entity citation is valid), and
    // the fixture's one invented commitment — a real anchor under a made-up description — is still
    // caught as unsupported by the scorer.
    assert_eq!(s.provenance.dangling, 0);
    assert!(s.provenance.unsupported >= 1, "the unsupported hallucination must be caught");
    assert!(s.provenance.validity().unwrap_or(0.0) >= 0.90, "replay provenance validity");
    // FR-ST-20 connection: Batch rows land in Medium, and Medium must keep its promise
    // (right at least as often as the Medium edge), or "possibly:" is misleading.
    assert!(s.calibration.get(&BandLabel::Medium).map_or(0, |b| b.n) > 0);
    assert_eq!(s.band_promise_violations(), vec![], "a band is right less often than it promises");
}

/// The parse contract the replay depends on: one malformed item fails the whole window (the
/// window stays resumable), and a result for a session that was never sent is refused. Both are
/// failure-taxonomy rows (docs/extract-eval.md §2).
#[test]
fn batch_replay_failure_shapes() {
    let days = days();
    let fixtures = fixtures();
    let day = &days[0];
    let f = fixture_for(&fixtures, day);
    let night = Night::load(day);

    let mut fenced = batch_results(f);
    fenced[0].text = fenced[0].text.as_ref().map(|t| format!("```json\n{t}\n```"));
    assert!(night.parse(&fenced).is_err(), "a code-fenced item must fail the window");

    let mut missing = batch_results(f);
    missing.pop();
    assert!(night.parse(&missing).is_err(), "a missing item must fail the window");

    let mut unknown = batch_results(f);
    unknown[0].custom_id = "s0000000000000000".to_string();
    assert!(night.parse(&unknown).is_err(), "a result for a session that was never sent is refused");
}

/// The fixture item with the deliberately wrong `line` (docs/extract-eval.md, Batch replay): the
/// number names a line that does not start with the anchor, so the resolver ignores it and the
/// item is attributed to the event whose line the anchor uniquely names — never to the numbered
/// line. Provenance correctness is the bar: a stored quote comes from the event it cites.
#[test]
fn a_wrong_line_number_in_the_replay_is_overruled_by_its_anchor() {
    let days = days();
    let fixtures = fixtures();
    let mut seen = 0;
    for day in &days {
        let f = fixture_for(&fixtures, day);
        let night = Night::load(day);
        for r in &f.results {
            let Some(response) = &r.response else { continue };
            let session = night.sessions.iter().find(|s| s.custom_id == r.custom_id).unwrap();
            for array in ["commitments", "open_loops", "entities"] {
                for item in response[array].as_array().into_iter().flatten() {
                    let (Some(line), Some(anchor)) = (item["line"].as_u64(), item["anchor"].as_str()) else {
                        continue;
                    };
                    let numbered = session.lines.get(line as usize - 1).map(|l| l.sent.as_str()).unwrap_or("");
                    if numbered.starts_with(anchor) {
                        continue;
                    }
                    seen += 1;
                    let hit = session
                        .resolve_anchor(anchor, item["anchor_len"].as_u64().map(|n| n as usize), Some(line as usize), &[])
                        .expect("the wrong-line item still resolves through its unique anchor");
                    assert_ne!(hit.line, line as usize, "{}: the numbered line was not taken", day.id);
                    assert!(session.lines[hit.line - 1].sent.starts_with(anchor));
                    assert_eq!(hit.event_id, session.lines[hit.line - 1].event_id);
                    assert!(
                        day.event(night.golden_id[&hit.event_id]).unwrap().text.contains(&hit.quote),
                        "{}: the quote is text of the event it is attributed to",
                        day.id
                    );
                }
            }
        }
    }
    assert_eq!(seen, 1, "exactly one deliberately wrong-line item across the fixtures");
}

/// Fixture maintenance, not a gate: print every session of every golden day with its numbered
/// lines, so the `line` numbers in `crates/extract-eval/replay/*.json` can be (re)derived after a
/// golden regeneration. Run with
/// `cargo test -p shogun-core --features db --test extract_eval dump_session_lines -- --ignored --nocapture`.
#[test]
#[ignore = "prints the session bodies for fixture maintenance"]
fn dump_session_lines() {
    for day in &days() {
        let night = Night::load(day);
        for s in &night.sessions {
            println!("[session] {} {} events={:?}", day.id, s.custom_id, night.session_events(s));
            for (n, line) in s.lines.iter().enumerate() {
                println!("  [{}] {}", n + 1, line.sent);
            }
        }
    }
}
