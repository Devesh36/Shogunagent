//! The corpora this crate generates.
//!
//! All of them are built from the same small vocabulary of projects, people and topics, so that
//! the background is genuinely *confusable* with the answers. A corpus of random strings would
//! make retrieval look perfect and measure nothing: every needle would be the only text in the
//! database containing its own words.
//!
//! Query text is written for the retrieval path that actually exists. The index is FTS5 with a
//! trigram tokenizer and [`shogun_memory::search::lexical_terms`] drops terms under three
//! characters and stopwords, then ORs the rest — so every question here carries at least one
//! distinctive term that its answer also carries, and the distractors share some of the others.

use crate::rng::Rng;
use crate::workload::{BenchEvent, BenchQuery, GeneratedWorkload, Workload};

/// Project code names. Deliberately not real words: a project name is the rare, high-bm25 term
/// that makes a question selective, exactly as a real project name would be.
const PROJECTS: &[&str] = &[
    "kestrel", "lantern", "meridian", "obsidian", "pinnacle", "quarry", "redwood", "sable",
    "tundra", "vantage", "willow", "zephyr",
];

const PEOPLE: &[&str] = &[
    "Priya", "Dana", "Marcus", "Yuki", "Ines", "Tobias", "Amara", "Rafael",
];

/// Apps a capture would plausibly come from, so `app_bundle_id` is not a constant column.
const APPS: &[(&str, &str)] = &[
    ("com.apple.Safari", "Safari"),
    ("com.tinyspeck.slackmacgap", "Slack"),
    ("com.apple.mail", "Mail"),
    ("com.microsoft.VSCode", "Code"),
    ("com.figma.Desktop", "Figma"),
];

/// Background sentences: on-topic enough to compete for the same terms as the answers, without
/// answering anything.
const CHATTER: &[&str] = &[
    "standup notes for {p}: nothing blocking, review queue is short",
    "{who} shared the latest {p} dashboard screenshots in the channel",
    "{p} retrospective is scheduled, agenda still needs owners",
    "someone asked whether {p} needs another round of design review",
    "{who} is drafting the {p} onboarding document this week",
    "the {p} integration job flaked again, rerunning it now",
    "{p} ticket triage moved four items into the backlog",
    "{who} mentioned {p} in passing during the planning call",
    "budget spreadsheet for {p} was updated with the new headcount",
    "{p} changelog entry still needs a proper description",
];

/// The decisions the query set asks about. Each is a distinct, checkable fact.
const DECISIONS: &[(&str, &str)] = &[
    (
        "database",
        "{p} will store its records in PostgreSQL rather than MySQL",
    ),
    (
        "deadline",
        "{p} ships to customers on the fourteenth of March",
    ),
    ("owner", "{who} takes over as the engineering owner of {p}"),
    (
        "pricing",
        "{p} renewal was settled at twelve thousand for the year",
    ),
    (
        "hosting",
        "{p} runs in the Frankfurt region for data residency",
    ),
    (
        "auth",
        "{p} authenticates users through the shared identity service",
    ),
];

fn app_for(rng: &mut Rng) -> (String, String) {
    let (bundle, title) = rng
        .pick(APPS)
        .copied()
        .unwrap_or(("com.apple.Safari", "Safari"));
    (bundle.to_string(), title.to_string())
}

fn fill(template: &str, project: &str, who: &str) -> String {
    template.replace("{p}", project).replace("{who}", who)
}

/// One background event. `ts` is milliseconds; callers pass a monotonically increasing value so
/// the log has a realistic time axis (and so Cold partitioning has something to bite on).
fn chatter_event(rng: &mut Rng, ts: i64, index: usize) -> BenchEvent {
    let project = rng.pick(PROJECTS).copied().unwrap_or("kestrel");
    let who = rng.pick(PEOPLE).copied().unwrap_or("Priya");
    let template = rng.pick(CHATTER).copied().unwrap_or(CHATTER[0]);
    let (bundle, title) = app_for(rng);
    // The index is part of the text so background events are distinct rows rather than accidental
    // duplicates — otherwise the "clean" corpus would silently contain a duplicate population and
    // its write-amplification number would be measuring the generator, not the memory layer.
    let content = format!("{} [note {}]", fill(template, project, who), index);
    BenchEvent {
        ts,
        source: "capture".to_string(),
        kind: "text".to_string(),
        app_bundle_id: Some(bundle),
        window_title: Some(format!("{title} — {project}")),
        content,
        dwell_ms: 0,
        fact_id: format!("chatter-{index}"),
    }
}

fn decision_event(ts: i64, project: &str, who: &str, topic: &str, template: &str) -> BenchEvent {
    BenchEvent {
        ts,
        source: "capture".to_string(),
        kind: "text".to_string(),
        app_bundle_id: Some("com.apple.mail".to_string()),
        window_title: Some(format!("Mail — {project} {topic}")),
        content: fill(template, project, who),
        dwell_ms: 0,
        fact_id: format!("decision-{project}-{topic}"),
    }
}

/// How many milliseconds separate one event from the next. One minute keeps a 100k corpus inside a
/// realistic couple of months rather than compressing it into an instant.
const TS_STEP_MS: i64 = 60_000;

/// Baseline corpus: unique events only, one answer per query, no duplicates and no contradictions.
///
/// This is the reference point every other number is read against — write amplification here
/// should be 1.0, and any duplicate collapse the layer reports is a false positive.
#[derive(Debug, Default, Clone, Copy)]
pub struct CleanWorkload;

impl Workload for CleanWorkload {
    fn name(&self) -> &'static str {
        "clean"
    }

    fn generate(&self, rng: &mut Rng, events: usize, queries: usize) -> GeneratedWorkload {
        // One answer-bearing event per query, planted at a reproducible position in the stream.
        let planned = queries.min(events);
        let mut slots: Vec<usize> = (0..events).collect();
        rng.shuffle(&mut slots);
        let mut plan: Vec<(usize, usize)> = slots
            .into_iter()
            .take(planned)
            .enumerate()
            .map(|(q, slot)| (slot, q))
            .collect();
        plan.sort_unstable();

        let mut out: Vec<BenchEvent> = Vec::with_capacity(events);
        let mut query_specs: Vec<(String, String, String)> = Vec::with_capacity(planned);
        let mut next = 0usize;
        for i in 0..events {
            let ts = (i as i64 + 1) * TS_STEP_MS;
            if next < plan.len() && plan[next].0 == i {
                let q = plan[next].1;
                // Cycle projects and topics so each query gets a distinct (project, topic) pair;
                // two queries answered by the same fact would double-count in recall.
                let project = PROJECTS[q % PROJECTS.len()];
                let (topic, template) = DECISIONS[(q / PROJECTS.len()) % DECISIONS.len()];
                let who = PEOPLE[q % PEOPLE.len()];
                // Past PROJECTS x DECISIONS distinct pairs, suffix the topic to keep them unique.
                let round = q / (PROJECTS.len() * DECISIONS.len());
                let topic = if round == 0 {
                    topic.to_string()
                } else {
                    format!("{topic}{round}")
                };
                let mut ev = decision_event(ts, project, who, &topic, template);
                if round > 0 {
                    ev.content = format!("{} (revision {})", ev.content, round);
                }
                query_specs.push((project.to_string(), topic, ev.fact_id.clone()));
                out.push(ev);
                next += 1;
            } else {
                out.push(chatter_event(rng, ts, i));
            }
        }

        let queries = query_specs
            .into_iter()
            .map(|(project, topic, fact_id)| BenchQuery {
                ask: format!("what did we decide about the {project} {topic}"),
                expected: vec![fact_id],
                superseded: Vec::new(),
            })
            .collect();

        GeneratedWorkload {
            name: self.name(),
            events: out,
            queries,
        }
    }
}

/// Fraction of a duplicate corpus that repeats an earlier event rather than being new.
const DUPLICATE_SHARE: f64 = 0.30;

/// How many of those repeats are *near* duplicates (reworded) rather than byte-identical.
///
/// The split is the point of this workload. Exact repeats hit
/// [`shogun_memory::event_log::insert_or_touch`]'s `content_hash` match and collapse; near repeats
/// carry the same fact in different words, hash differently, and become second rows. Reporting the
/// two collapse rates separately says exactly where the current dedup contract stops.
const NEAR_DUPLICATE_SHARE: f64 = 0.50;

/// Duplicate-heavy corpus: ~30% of writes repeat a fact already in the log, half verbatim and half
/// reworded.
#[derive(Debug, Default, Clone, Copy)]
pub struct DuplicateWorkload;

impl Workload for DuplicateWorkload {
    fn name(&self) -> &'static str {
        "duplicate"
    }

    fn generate(&self, rng: &mut Rng, events: usize, queries: usize) -> GeneratedWorkload {
        let unique_target = ((events as f64) * (1.0 - DUPLICATE_SHARE)).round() as usize;
        let base = CleanWorkload.generate(rng, unique_target.max(1), queries);

        let mut out: Vec<BenchEvent> = Vec::with_capacity(events);
        out.extend(base.events.iter().cloned());

        let mut i = out.len();
        while i < events {
            let ts = (i as i64 + 1) * TS_STEP_MS;
            let src = match rng.pick(&base.events) {
                Some(e) => e.clone(),
                None => break,
            };
            let mut dup = src.clone();
            dup.ts = ts;
            dup.dwell_ms = 1_000;
            if rng.chance(NEAR_DUPLICATE_SHARE) {
                // Reworded, same meaning, same fact. A human would call these one memory.
                dup.content = format!("(forwarded) {} — resending for visibility", src.content);
            }
            // `fact_id` is inherited either way: both forms carry the fact the original carried.
            out.push(dup);
            i += 1;
        }

        GeneratedWorkload {
            name: self.name(),
            events: out,
            queries: base.queries,
        }
    }
}

/// How many times a temporal fact changes its value over the corpus.
const REVISIONS: usize = 3;

/// The values a tracked project's database choice moves through, in order.
const TEMPORAL_VALUES: [&str; REVISIONS] = ["PostgreSQL", "SQLite", "PostgreSQL"];

/// Smallest corpus that can give every temporal query all of its revisions.
///
/// Query count is capped by the distinct projects available to the generator. Keeping this
/// calculation beside those private constants prevents validation and generation from drifting.
pub fn minimum_temporal_events(queries: usize) -> usize {
    queries.min(PROJECTS.len()).max(1) * REVISIONS
}

/// Maximum number of distinct temporal queries this vocabulary can generate.
pub fn temporal_project_count() -> usize {
    PROJECTS.len()
}

/// Temporal corpus: facts that are overwritten by later facts.
///
/// Each tracked project states its database choice three times — PostgreSQL, then SQLite, then
/// PostgreSQL again — and the query asks in the present tense. Only the last statement is a
/// correct answer; the earlier ones are `superseded`. Nothing here tries to *fix* that (v0.1
/// changes no production behaviour); it measures how often the current retrieval path hands back a
/// fact the user already overruled, which is the baseline a selective-update controller must beat.
#[derive(Debug, Default, Clone, Copy)]
pub struct TemporalWorkload;

impl Workload for TemporalWorkload {
    fn name(&self) -> &'static str {
        "temporal"
    }

    fn generate(&self, rng: &mut Rng, events: usize, queries: usize) -> GeneratedWorkload {
        let tracked = queries.min(PROJECTS.len()).max(1);
        let revision_events = minimum_temporal_events(queries);
        let filler = events.saturating_sub(revision_events);

        // Revisions are spread across the corpus rather than written back to back, so the stale and
        // the current statement sit far apart in the log — which is the situation that makes a
        // present-tense question hard in the first place.
        let mut planned: Vec<(usize, usize, usize)> = Vec::with_capacity(revision_events);
        for p in 0..tracked {
            for r in 0..REVISIONS {
                let band_start = r * events / REVISIONS;
                let band_end = (r + 1) * events / REVISIONS;
                let band_len = band_end - band_start;
                // Config validation guarantees `band_len >= tracked`. Even spacing therefore
                // gives every revision a unique position; modular jitter would collide for
                // small valid corpora and silently drop history (issue #221).
                let offset = p * band_len / tracked;
                planned.push((band_start + offset, p, r));
            }
        }
        planned.sort_unstable();

        let mut out: Vec<BenchEvent> = Vec::with_capacity(events);
        // Per project, the fact each revision asserted, in order.
        let mut asserted: Vec<Vec<String>> = vec![Vec::new(); tracked];
        let mut cursor = 0usize;
        let mut filler_written = 0usize;
        for i in 0..events {
            let ts = (i as i64 + 1) * TS_STEP_MS;
            if cursor < planned.len() && planned[cursor].0 == i {
                let (_, p, r) = planned[cursor];
                let project = PROJECTS[p % PROJECTS.len()];
                let value = TEMPORAL_VALUES[r];
                // Fact identity is the *value*, not the revision index. The sequence returns to an
                // earlier value (A -> B -> A), and restating A is not a new fact — it is the same
                // fact asserted again. Keying on `r` instead made the third statement a distinct
                // "current" fact, which the memory layer then correctly collapsed into the first
                // statement's row (identical text, identical content_hash), leaving the expected
                // fact mapped to no row at all and reporting a false recall of 0.
                let fact_id = format!("temporal-{project}-database-{}", value.to_lowercase());
                out.push(BenchEvent {
                    ts,
                    source: "capture".to_string(),
                    kind: "text".to_string(),
                    app_bundle_id: Some("com.tinyspeck.slackmacgap".to_string()),
                    window_title: Some(format!("Slack — {project}")),
                    content: format!(
                        "update: {project} will store its records in {value} from now on"
                    ),
                    dwell_ms: 0,
                    fact_id: fact_id.clone(),
                });
                asserted[p].push(fact_id);
                cursor += 1;
            } else if filler_written < filler {
                out.push(chatter_event(rng, ts, i));
                filler_written += 1;
            }
        }

        // Guarantee the requested corpus size. Positions clamped against the end of the stream can
        // leave the walk a few events short; padding here keeps "--events N" literally true, which
        // the determinism tests assert and which any cross-run comparison depends on.
        while out.len() < events {
            let ts = (out.len() as i64 + 1) * TS_STEP_MS;
            let index = out.len();
            out.push(chatter_event(rng, ts, index));
        }

        // The answer is whatever the last revision asserted. Anything asserted earlier that is not
        // that fact is superseded; an earlier assertion of the *same* fact is not stale, it is the
        // same answer stated twice.
        let queries = asserted
            .into_iter()
            .enumerate()
            .filter_map(|(p, facts)| {
                let current = facts.last()?.clone();
                let mut superseded: Vec<String> =
                    facts.iter().filter(|f| **f != current).cloned().collect();
                superseded.sort_unstable();
                superseded.dedup();
                let project = PROJECTS[p % PROJECTS.len()];
                Some(BenchQuery {
                    ask: format!("where does {project} store its records"),
                    expected: vec![current],
                    superseded,
                })
            })
            .collect();

        GeneratedWorkload {
            name: self.name(),
            events: out,
            queries,
        }
    }
}

// Workloads added for the LongMemEval category sweep and the slicer question.
//
// `research/COMPARISON.md` established that `clean` / `duplicate` / `temporal` reach roughly two
// of LongMemEval's six question categories, and that the corpus slicer could not be evaluated at
// all on a corpus with no redundancy in it. The four below close both gaps.

/// How many captures one "moment" produces in the redundant corpus.
///
/// Five is the shape real Accessibility capture has: the same window is re-read on a timer while
/// the user reads it, so one fact arrives several times within a few seconds.
const BURST: usize = 5;

/// Spacing inside a burst. Short enough that a whole burst lands in one `hour` bucket.
const BURST_STEP_MS: i64 = 5_000;

/// Share of burst repeats that are byte-identical rather than slightly changed.
///
/// A real re-read usually differs a little (the scroll position moved, a timestamp ticked), but
/// not always. Splitting the burst this way means the corpus exercises both the `content_hash`
/// collapse path and the near-duplicate path that defeats it.
const BURST_VERBATIM_SHARE: f64 = 0.40;

/// Redundant corpus: every fact is captured several times in a burst, as screen capture does.
///
/// This is the corpus `research/COMPARISON.md` named as the precondition for evaluating the
/// slicer. On `clean`, every fact exists exactly once, so any dropped event is an equally likely
/// answer and time-bucketing can only trade accuracy for cost at 1:1 — which is what the first
/// slicer run measured. Redundancy is the thing a slice is supposed to exploit: when a fact is
/// present five times inside one bucket, keeping one of the five costs nothing at all.
///
/// The measurement that matters here is the gap between
/// [`crate::slicer::SliceReport::answer_retention`] (distinct facts kept) and
/// [`crate::slicer::SliceReport::answer_event_retention`] (copies kept). On `clean` they are
/// equal; here they should separate, and the size of the gap is the redundancy available.
#[derive(Debug, Default, Clone, Copy)]
pub struct RedundantWorkload;

impl Workload for RedundantWorkload {
    fn name(&self) -> &'static str {
        "redundant"
    }

    fn generate(&self, rng: &mut Rng, events: usize, queries: usize) -> GeneratedWorkload {
        // Each moment becomes BURST captures, so the number of distinct moments is events / BURST.
        let moments = (events / BURST).max(1);
        let planned = queries.min(moments);

        let mut slots: Vec<usize> = (0..moments).collect();
        rng.shuffle(&mut slots);
        let mut plan: Vec<(usize, usize)> = slots
            .into_iter()
            .take(planned)
            .enumerate()
            .map(|(q, slot)| (slot, q))
            .collect();
        plan.sort_unstable();

        let mut out: Vec<BenchEvent> = Vec::with_capacity(events);
        let mut query_specs: Vec<(String, String, String)> = Vec::with_capacity(planned);
        let mut next = 0usize;

        for m in 0..moments {
            // Moments are spaced BURST steps apart so the corpus covers the same span of time as
            // a `clean` corpus of the same event count.
            let base_ts = (m as i64 + 1) * TS_STEP_MS * BURST as i64;

            let seed_event = if next < plan.len() && plan[next].0 == m {
                let q = plan[next].1;
                let project = PROJECTS[q % PROJECTS.len()];
                let (topic, template) = DECISIONS[(q / PROJECTS.len()) % DECISIONS.len()];
                let who = PEOPLE[q % PEOPLE.len()];
                let round = q / (PROJECTS.len() * DECISIONS.len());
                let topic = if round == 0 {
                    topic.to_string()
                } else {
                    format!("{topic}{round}")
                };
                let mut ev = decision_event(base_ts, project, who, &topic, template);
                if round > 0 {
                    ev.content = format!("{} (revision {})", ev.content, round);
                }
                query_specs.push((project.to_string(), topic, ev.fact_id.clone()));
                next += 1;
                ev
            } else {
                chatter_event(rng, base_ts, m)
            };

            for j in 0..BURST {
                if out.len() >= events {
                    break;
                }
                let mut copy = seed_event.clone();
                copy.ts = base_ts + (j as i64) * BURST_STEP_MS;
                if j > 0 && !rng.chance(BURST_VERBATIM_SHARE) {
                    // A re-read of the same window that shifted slightly. Same fact, new bytes —
                    // `content_hash` will not collapse it, so it becomes a second row.
                    copy.content = format!("{} [re-read {}]", seed_event.content, j);
                }
                // `fact_id` is inherited: every capture in the burst carries the same fact. That
                // is the whole point — redundancy is repeated *facts*, not repeated bytes.
                out.push(copy);
            }
        }

        // Keep `--events N` literally true.
        while out.len() < events {
            let ts = (out.len() as i64 + 1) * TS_STEP_MS;
            let index = out.len();
            out.push(chatter_event(rng, ts, index));
        }
        out.truncate(events);
        out.sort_by_key(|e| e.ts);

        let queries = query_specs
            .into_iter()
            .map(|(project, topic, fact_id)| BenchQuery {
                ask: format!("what did we decide about the {project} {topic}"),
                expected: vec![fact_id],
                superseded: Vec::new(),
            })
            .collect();

        GeneratedWorkload {
            name: self.name(),
            events: out,
            queries,
        }
    }
}

/// Topics the abstention query set asks about, which nothing in the corpus ever decides.
///
/// Deliberately the same *shape* as the real topics in [`DECISIONS`], so the question is
/// well-formed and its terms are all present in the corpus. Only the answer is missing.
const UNANSWERED_TOPICS: &[&str] = &[
    "compliance audit",
    "hardware budget",
    "support rotation",
    "trademark filing",
    "office lease",
    "insurance renewal",
];

/// Abstention corpus: the questions have no answer anywhere in the log.
///
/// LongMemEval's abstention category, and the cheapest unmeasured failure in the system. The
/// retrieval path is FTS5 over a trigram index, ORing the query's terms — it returns whatever
/// matches any of them and has no notion of "nothing here is relevant". So every query is
/// expected to come back with confident, irrelevant evidence.
///
/// Nothing here is a recall measurement: `expected` is empty on purpose, which routes these
/// queries to [`crate::metrics::QualityAccumulator`]'s abstention counters instead of its recall
/// denominator. The number this workload produces is `false_evidence_rate` — the share of
/// unanswerable questions that still got rows back, which is the share of the time a downstream
/// agent would be handed evidence for an answer that does not exist.
#[derive(Debug, Default, Clone, Copy)]
pub struct AbstentionWorkload;

impl Workload for AbstentionWorkload {
    fn name(&self) -> &'static str {
        "abstention"
    }

    fn generate(&self, rng: &mut Rng, events: usize, queries: usize) -> GeneratedWorkload {
        // Chatter only. Every term the questions use is present; none of the answers are.
        let out: Vec<BenchEvent> = (0..events)
            .map(|i| chatter_event(rng, (i as i64 + 1) * TS_STEP_MS, i))
            .collect();

        let queries = (0..queries)
            .map(|q| {
                let project = PROJECTS[q % PROJECTS.len()];
                let topic = UNANSWERED_TOPICS[(q / PROJECTS.len()) % UNANSWERED_TOPICS.len()];
                BenchQuery {
                    ask: format!("what did we decide about the {project} {topic}"),
                    // No correct answer and no superseded answer: this question is unanswerable,
                    // which is a third state the metrics have to keep separate from a miss.
                    expected: Vec::new(),
                    superseded: Vec::new(),
                }
            })
            .collect();

        GeneratedWorkload {
            name: self.name(),
            events: out,
            queries,
        }
    }
}

/// The two halves of a multi-session answer. Each entry is (topic, first session, second session),
/// split so that neither half answers the question alone.
const SESSION_PAIRS: &[(&str, &str, &str)] = &[
    (
        "launch",
        "{p} launch is held until legal signs off on the terms",
        "{p} legal review is booked for the last week of the month",
    ),
    (
        "vendor",
        "{p} picked the Dublin vendor over the Lisbon one",
        "the Dublin vendor quoted nine thousand for {p} onboarding",
    ),
    (
        "staffing",
        "{who} moves onto {p} once the migration closes",
        "the {p} migration closes at the end of the quarter",
    ),
    (
        "rollout",
        "{p} rolls out to internal users before external ones",
        "internal rollout for {p} needs the audit log finished first",
    ),
];

/// Multi-session corpus: the answer is split across two sessions and must be combined.
///
/// LongMemEval's largest category (133 of 500 questions). Each question's evidence is two facts
/// stated far apart in the log and in different applications, so neither is a complete answer.
///
/// This is the workload that makes [`crate::metrics::QualitySummary::coverage_at_5`] mean
/// something. Recall asks whether *an* expected fact came back, which a single half satisfies;
/// coverage asks whether *every* expected fact came back, which is what actually has to happen for
/// the question to be answerable. The gap between the two is the measurement.
#[derive(Debug, Default, Clone, Copy)]
pub struct MultiSessionWorkload;

impl Workload for MultiSessionWorkload {
    fn name(&self) -> &'static str {
        "multi-session"
    }

    fn generate(&self, rng: &mut Rng, events: usize, queries: usize) -> GeneratedWorkload {
        let tracked = queries.min(PROJECTS.len() * SESSION_PAIRS.len()).max(1);

        // The two halves go in different thirds of the corpus, so they are genuinely separate
        // sessions rather than two lines of one conversation.
        let mut planned: Vec<(usize, usize, usize)> = Vec::with_capacity(tracked * 2);
        for t in 0..tracked {
            for half in 0..2usize {
                let band_start = if half == 0 { 0 } else { 2 * events / 3 };
                let band_len = (events / 3).max(1);
                let offset = t * band_len / tracked;
                planned.push(((band_start + offset).min(events.saturating_sub(1)), t, half));
            }
        }
        planned.sort_unstable();

        let mut out: Vec<BenchEvent> = Vec::with_capacity(events);
        let mut evidence: Vec<Vec<String>> = vec![Vec::new(); tracked];
        let mut cursor = 0usize;

        for i in 0..events {
            let ts = (i as i64 + 1) * TS_STEP_MS;
            // `<=`, not `==`. Two planted positions can land on the same index when the corpus is
            // small relative to the query count; with an equality test the second one would never
            // match a later index either, stranding the cursor and silently dropping every
            // remaining piece of evidence. Draining lets a collided event slide to the next slot.
            if cursor < planned.len() && planned[cursor].0 <= i {
                let (_, t, half) = planned[cursor];
                let project = PROJECTS[t % PROJECTS.len()];
                let (topic, first, second) =
                    SESSION_PAIRS[(t / PROJECTS.len()) % SESSION_PAIRS.len()];
                let who = PEOPLE[t % PEOPLE.len()];
                let template = if half == 0 { first } else { second };
                let fact_id = format!("session-{project}-{topic}-{half}");
                let (bundle, app) = if half == 0 {
                    ("com.apple.mail", "Mail")
                } else {
                    ("com.tinyspeck.slackmacgap", "Slack")
                };
                out.push(BenchEvent {
                    ts,
                    source: "capture".to_string(),
                    kind: "text".to_string(),
                    app_bundle_id: Some(bundle.to_string()),
                    window_title: Some(format!("{app} - {project} {topic}")),
                    content: fill(template, project, who),
                    dwell_ms: 0,
                    fact_id: fact_id.clone(),
                });
                evidence[t].push(fact_id);
                cursor += 1;
            } else {
                out.push(chatter_event(rng, ts, i));
            }
        }

        while out.len() < events {
            let ts = (out.len() as i64 + 1) * TS_STEP_MS;
            let index = out.len();
            out.push(chatter_event(rng, ts, index));
        }
        out.truncate(events);

        let queries = evidence
            .into_iter()
            .enumerate()
            .filter_map(|(t, facts)| {
                // A question with only one half planted is not a multi-session question; drop it
                // rather than let it inflate coverage with a trivially satisfiable case.
                if facts.len() < 2 {
                    return None;
                }
                let project = PROJECTS[t % PROJECTS.len()];
                let (topic, _, _) = SESSION_PAIRS[(t / PROJECTS.len()) % SESSION_PAIRS.len()];
                Some(BenchQuery {
                    ask: format!("what is the state of the {project} {topic}"),
                    expected: facts,
                    superseded: Vec::new(),
                })
            })
            .collect();

        GeneratedWorkload {
            name: self.name(),
            events: out,
            queries,
        }
    }
}

/// How many times the habitual choice is stated, against [`PREFERENCE_MINORITY`] for the others.
const PREFERENCE_MAJORITY: usize = 7;
/// How many times each non-habitual choice is stated.
const PREFERENCE_MINORITY: usize = 2;

/// The habit each preference question asks about: (topic, usual value, two exceptions).
const HABITS: &[(&str, &str, &str, &str)] = &[
    ("review", "in the morning", "late at night", "after lunch"),
    ("deploy", "on Tuesdays", "on Fridays", "over the weekend"),
    ("standup", "over video", "in the office", "by written update"),
    ("notes", "in Markdown", "in a spreadsheet", "on paper"),
];

/// Preference corpus: the answer is the habit, not the most recent statement.
///
/// LongMemEval's single-session-preference category, and the state tables' reason for existing —
/// "what do I usually do" is a question about a distribution, not about an event.
///
/// Each person states a habit [`PREFERENCE_MAJORITY`] times and each exception
/// [`PREFERENCE_MINORITY`] times. The habitual value is `expected`; the exceptions are
/// `superseded`, because retrieving one of them is a real and specific failure — answering "late
/// at night" to "when do you usually review" is wrong in exactly the way the temporal workload's
/// stale answers are wrong, and it reuses the existing stale-outranked metric to say so.
///
/// Nothing in the retrieval path counts how often a fact was asserted: FTS5 ranks by bm25 over one
/// row's text, so a minority statement and a majority statement look identical to it. This
/// workload measures the size of that gap.
#[derive(Debug, Default, Clone, Copy)]
pub struct PreferenceWorkload;

impl Workload for PreferenceWorkload {
    fn name(&self) -> &'static str {
        "preference"
    }

    fn generate(&self, rng: &mut Rng, events: usize, queries: usize) -> GeneratedWorkload {
        let tracked = queries.min(PEOPLE.len() * HABITS.len()).max(1);
        let per_person = PREFERENCE_MAJORITY + 2 * PREFERENCE_MINORITY;

        // Statement positions, spread through the corpus so a habit is not one contiguous block.
        let mut planned: Vec<(usize, usize, usize)> = Vec::with_capacity(tracked * per_person);
        for t in 0..tracked {
            for k in 0..per_person {
                let band_len = (events / per_person).max(1);
                let band_start = k * band_len;
                let offset = t * band_len / tracked;
                planned.push(((band_start + offset).min(events.saturating_sub(1)), t, k));
            }
        }
        planned.sort_unstable();

        let mut out: Vec<BenchEvent> = Vec::with_capacity(events);
        let mut majority_fact: Vec<Option<String>> = vec![None; tracked];
        let mut minority_facts: Vec<Vec<String>> = vec![Vec::new(); tracked];
        let mut cursor = 0usize;

        for i in 0..events {
            let ts = (i as i64 + 1) * TS_STEP_MS;
            // `<=` for the same reason as the multi-session generator: a collided position slides
            // to the next slot instead of stranding the cursor.
            if cursor < planned.len() && planned[cursor].0 <= i {
                let (_, t, k) = planned[cursor];
                let who = PEOPLE[t % PEOPLE.len()];
                let (topic, usual, rare_a, rare_b) = HABITS[(t / PEOPLE.len()) % HABITS.len()];
                // The first PREFERENCE_MAJORITY statements are the habit; the rest are exceptions.
                let value = if k < PREFERENCE_MAJORITY {
                    usual
                } else if k < PREFERENCE_MAJORITY + PREFERENCE_MINORITY {
                    rare_a
                } else {
                    rare_b
                };
                // Fact identity is the value: stating the same habit seven times is one fact
                // asserted seven times, exactly as in the temporal workload.
                let slug = value.replace(' ', "-").to_lowercase();
                let fact_id = format!("habit-{who}-{topic}-{slug}");
                out.push(BenchEvent {
                    ts,
                    source: "capture".to_string(),
                    kind: "text".to_string(),
                    app_bundle_id: Some("com.tinyspeck.slackmacgap".to_string()),
                    window_title: Some(format!("Slack - {who}")),
                    // The occurrence index keeps repeats from collapsing on `content_hash`; the
                    // question is whether ranking notices frequency, not whether dedup does.
                    content: format!("{who} did the {topic} {value} again [{k}]"),
                    dwell_ms: 0,
                    fact_id: fact_id.clone(),
                });
                if k < PREFERENCE_MAJORITY {
                    majority_fact[t] = Some(fact_id);
                } else if !minority_facts[t].contains(&fact_id) {
                    minority_facts[t].push(fact_id);
                }
                cursor += 1;
            } else {
                out.push(chatter_event(rng, ts, i));
            }
        }

        while out.len() < events {
            let ts = (out.len() as i64 + 1) * TS_STEP_MS;
            let index = out.len();
            out.push(chatter_event(rng, ts, index));
        }
        out.truncate(events);

        let queries = (0..tracked)
            .filter_map(|t| {
                let expected = majority_fact[t].clone()?;
                let who = PEOPLE[t % PEOPLE.len()];
                let (topic, _, _, _) = HABITS[(t / PEOPLE.len()) % HABITS.len()];
                Some(BenchQuery {
                    ask: format!("when does {who} usually do the {topic}"),
                    expected: vec![expected],
                    superseded: minority_facts[t].clone(),
                })
            })
            .collect();

        GeneratedWorkload {
            name: self.name(),
            events: out,
            queries,
        }
    }
}

/// Distinct multi-session questions this vocabulary can generate.
pub fn multi_session_query_capacity() -> usize {
    PROJECTS.len() * SESSION_PAIRS.len()
}

/// Distinct preference questions this vocabulary can generate.
pub fn preference_query_capacity() -> usize {
    PEOPLE.len() * HABITS.len()
}

/// Smallest corpus that lets `workload` plant a complete query set for `queries` questions.
///
/// Each generator spreads its planted events across evenly-spaced bands so that no two land on the
/// same position. That spacing needs room: below the minimum, positions collide, planted events
/// are silently dropped, and the run reports a quality number for a corpus that never contained
/// the evidence. Validation rejects those configurations rather than measuring them (issue #221 is
/// the same failure in the temporal generator).
pub fn minimum_events(workload: &str, queries: usize) -> usize {
    let queries = queries.max(1);
    match workload {
        "temporal" => minimum_temporal_events(queries),
        // Every question needs a whole burst of its own.
        "redundant" => queries * BURST,
        // Two halves, planted in the first and last third of the corpus.
        "multi-session" => queries.min(multi_session_query_capacity()).max(1) * 3,
        // One band per statement, and each person states the habit plus both exceptions.
        "preference" => {
            queries.min(preference_query_capacity()).max(1)
                * (PREFERENCE_MAJORITY + 2 * PREFERENCE_MINORITY)
        }
        _ => 1,
    }
}

/// Resolve a `--workload` name. Returns `None` for an unknown name so the CLI can list the valid
/// ones rather than silently running the default.
pub fn by_name(name: &str) -> Option<Box<dyn Workload>> {
    match name {
        "clean" => Some(Box::new(CleanWorkload)),
        "duplicate" => Some(Box::new(DuplicateWorkload)),
        "temporal" => Some(Box::new(TemporalWorkload)),
        "redundant" => Some(Box::new(RedundantWorkload)),
        "abstention" => Some(Box::new(AbstentionWorkload)),
        "multi-session" => Some(Box::new(MultiSessionWorkload)),
        "preference" => Some(Box::new(PreferenceWorkload)),
        _ => None,
    }
}

/// Every workload name, for `--help` and for the tests that sweep all of them.
pub const ALL: &[&str] = &[
    "clean",
    "duplicate",
    "temporal",
    "redundant",
    "abstention",
    "multi-session",
    "preference",
];
