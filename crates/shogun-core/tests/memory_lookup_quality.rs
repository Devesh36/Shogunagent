#![cfg(feature = "db")]
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use shogun_core::daemon::Db;
use shogun_memory::embed::{EmbedError, Embedder, E5_SMALL_DIM};
use shogun_memory::event_log::NewEvent;
use shogun_memory::state::{NewProject, ProjectStatus, Provenance};

struct LookupEvalEmbedder;

impl LookupEvalEmbedder {
    fn vector(text: &str) -> Vec<f32> {
        let lower = text.to_lowercase();
        let concepts = [
            ["vendor", "renewal", "supplier", "contract", "extension", "accepted"].as_slice(),
            ["designer", "start", "dana", "onboarding", "begins", "march"].as_slice(),
            ["production", "down", "outage", "interruption", "malformed", "configuration"].as_slice(),
            ["promise", "acme", "obligation", "deliver", "export", "timeline"].as_slice(),
            ["release", "launch", "go-live", "twentieth", "localization"].as_slice(),
        ];
        let mut vector = vec![0.0; E5_SMALL_DIM];
        let index = concepts
            .iter()
            .position(|words| words.iter().any(|word| lower.contains(word)))
            .unwrap_or(32);
        vector[index] = 1.0;
        vector
    }
}

impl Embedder for LookupEvalEmbedder {
    fn dim(&self) -> usize {
        E5_SMALL_DIM
    }

    fn embed_passages(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|text| Self::vector(text)).collect())
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let mut vector = Self::vector(text);
        if vector[32] == 1.0 {
            vector[32] = 0.0;
            vector[33] = 1.0;
        }
        Ok(vector)
    }
}

#[test]
fn natural_memory_lookup_improves_recall_and_abstains_on_unknown_topic() {
    let db = Db::open_in_memory(Arc::new(|| 10_000)).unwrap();
    let cases = [
        ("Was the vendor renewal approved?", "supplier contract extension accepted"),
        ("When does the new designer start?", "Dana onboarding begins March 3"),
        ("Why did production go down?", "service interruption caused by malformed configuration deployment"),
        ("What did we promise Acme?", "customer obligation: deliver export timeline Friday"),
        ("When is the release?", "go-live shifted to the twentieth due localization delay"),
    ];
    for (index, (_, answer)) in cases.iter().enumerate() {
        let hash = format!("lookup-quality-evidence-{index}");
        let event_id = db
            .capture(&NewEvent {
                ts: index as i64 + 1,
                source: "capture",
                kind: "text",
                app_bundle_id: None,
                window_title: Some("Evidence"),
                content: "generic canonical supporting row",
                content_hash: &hash,
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            })
            .unwrap()
            .0;
        db.insert_project(
            &NewProject {
                name: "Distilled memory",
                status: ProjectStatus::Active,
                summary: Some(answer),
                participants_json: None,
                sources_json: None,
                confidence: 0.9,
                now: 100 + index as i64,
            },
            &[Provenance::new(event_id)],
        )
        .unwrap();
    }

    let lexical_hits = cases
        .iter()
        .filter(|(query, answer)| {
            db.assemble_context(query, 4, 240)
                .facts
                .iter()
                .any(|fact| fact.contains(answer))
        })
        .count();

    let db = db.with_embedder(Arc::new(LookupEvalEmbedder));
    assert!(db.embed_pending(100) >= cases.len());
    let hybrid_hits = cases
        .iter()
        .filter(|(query, answer)| {
            db.assemble_context(query, 4, 240)
                .facts
                .first()
                .is_some_and(|fact| fact.contains(answer))
        })
        .count();
    let unknown = db.assemble_context("Where is my scuba certificate?", 4, 240);

    println!(
        "memory lookup quality: lexical recall@4={lexical_hits}/{}; hybrid rank1={hybrid_hits}/{}; unknown facts={} evidence={}",
        cases.len(),
        cases.len(),
        unknown.facts.len(),
        unknown.evidence.len(),
    );
    assert!(hybrid_hits > lexical_hits, "semantic lookup must improve natural-question recall");
    assert_eq!(hybrid_hits, cases.len());
    assert!(unknown.facts.is_empty() && unknown.evidence.is_empty(), "weak evidence must abstain");
}

#[test]
fn captured_orchid_launch_fact_reaches_chat_context() {
    let db = Db::open_in_memory(Arc::new(|| 10_000)).unwrap();
    db.capture(&NewEvent {
        ts: 9_000,
        source: "capture",
        kind: "text",
        app_bundle_id: Some("com.apple.TextEdit"),
        window_title: Some("Untitled"),
        content: "Project Orchid is owned by Maya Chen. The launch date is October 17. Do not ship Orchid without legal approval.",
        content_hash: "orchid-capture-fixture",
        dwell_ms: 2_000,
        display_id: None,
        window_bounds: None,
    })
    .unwrap();

    let context = db.assemble_context("When does Orchid launch?", 6, 600);

    assert!(
        context
            .evidence
            .iter()
            .any(|item| item.excerpt.contains("October 17")),
        "captured fact must reach the chat prompt: {context:?}"
    );
}
