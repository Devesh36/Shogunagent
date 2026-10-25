//! The multilingual retrieval suite (E43), run the way CI can: lexical (no embedder) and with the
//! deterministic test embedder. Print the tables with
//! `cargo test -p memory-bench --test multilingual_eval -- --nocapture`; the same cases run
//! against a real model with `memory-bench multilingual --embedder <model>` (built with
//! `--features embed-local`, `docs/benchmarks/README.md`, "Multilingual retrieval (E43)").
//!
//! The asserted values are the measured ones, to three decimals. The suite is fixed text with no
//! seed, so a run is exact rather than a sample: a change that moves any number is a change to
//! multilingual retrieval, and updates this file and the table in `docs/benchmarks/README.md`
//! together (raise them when retrieval gets better).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use memory_bench::embedding::BenchEmbedder;
use memory_bench::multilingual::{self, Kind, Lang, MultilingualConfig, MultilingualReport};

fn run(embedder: Option<BenchEmbedder>) -> MultilingualReport {
    let report = multilingual::run(&MultilingualConfig { embedder, vector_cutoff: None }).unwrap();
    // log-hygiene-ok: fixed eval fixtures and their scores, no captured text
    eprintln!("{}", multilingual::render(&report));
    report
}

fn close(got: Option<f64>, want: f64, what: &str) {
    let got = got.unwrap_or_else(|| panic!("{what}: not measured"));
    assert!((got - want).abs() < 5e-4, "{what}: measured {got:.3}, recorded {want:.3}");
}

/// (language, R@5, false-answerable)
type LanguageRow = (Lang, f64, f64);
/// (kind, R@5) for the answerable kinds
type KindRow = (Kind, f64);

fn check(report: &MultilingualReport, languages: &[LanguageRow], kinds: &[KindRow], abstention: f64) {
    for &(lang, recall, false_answerable) in languages {
        let row = report.language(lang).unwrap();
        close(row.recall_at_5, recall, &format!("{} R@5", lang.code()));
        close(row.false_answerable, false_answerable, &format!("{} false-answerable", lang.code()));
    }
    for &(kind, recall) in kinds {
        close(report.kind(kind).unwrap().recall_at_5, recall, &format!("{} R@5", kind.name()));
    }
    close(report.kind(Kind::Abstention).unwrap().false_answerable, abstention, "abstention false-answerable");
}

/// The lexical baseline: no embedder, what every build without `embed-local` (and every store
/// whose model is not installed) answers.
#[test]
fn multilingual_suite_lexical_baseline() {
    let report = run(None);
    assert!(report.embedding.is_none());
    check(
        &report,
        &[
            (Lang::En, 0.375, 0.0),
            (Lang::Ja, 0.933, 1.0),
            (Lang::Es, 0.429, 0.0),
            (Lang::De, 0.571, 0.0),
            (Lang::Fr, 0.429, 0.0),
            (Lang::Zh, 0.857, 1.0),
        ],
        &[
            // Every same-language question with the fact's words, and every 2-3 character CJK
            // word (the gram index), is found at rank 1.
            (Kind::Lexical, 1.0),
            (Kind::CjkShort, 1.0),
            (Kind::Paraphrase, 0.429),
            // Only the project name is shared; the relevance floor refuses a one-word-of-four row.
            (Kind::CrossLingual, 0.333),
            // An unbroken CJK sentence is one compound term no row contains.
            (Kind::CjkSentence, 0.0),
        ],
        0.333,
    );
}

/// The vector lane on, with the deterministic test embedder (concept axes that span the six
/// languages). It proves the cross-lingual path end to end; it is not a model's quality.
#[test]
fn multilingual_suite_with_the_test_embedder() {
    let report = run(Some(BenchEmbedder::test()));
    assert_eq!(report.embedding.as_ref().unwrap().model, shogun_memory::embed::TestEmbedder::SPACE_ID);
    check(
        &report,
        &[
            (Lang::En, 1.0, 0.0),
            (Lang::Ja, 1.0, 1.0),
            (Lang::Es, 1.0, 0.0),
            (Lang::De, 0.857, 0.0),
            (Lang::Fr, 0.857, 0.0),
            (Lang::Zh, 0.929, 1.0),
        ],
        &[
            (Kind::Lexical, 1.0),
            (Kind::CjkShort, 1.0),
            (Kind::Paraphrase, 1.0),
            (Kind::CrossLingual, 0.889),
            (Kind::CjkSentence, 0.667),
        ],
        0.333,
    );
}

/// What the vector lane must never do, whatever the model: lose an answer the lexical lanes found,
/// or turn an abstention into evidence. The CJK abstentions it does not fix come from the lexical
/// floor (a question of two concepts, one of them the project name, passes at half coverage).
#[test]
fn the_vector_lane_adds_answers_and_no_false_evidence() {
    let lexical = run(None);
    let vector = run(Some(BenchEmbedder::test()));
    for lang in Lang::ALL {
        let (l, v) = (lexical.language(lang).unwrap(), vector.language(lang).unwrap());
        assert!(v.recall_at_5.unwrap() >= l.recall_at_5.unwrap(), "{} R@5 fell", lang.code());
        assert!(v.false_answerable.unwrap() <= l.false_answerable.unwrap(), "{} false-answerable rose", lang.code());
    }
    for (l, v) in lexical.cases.iter().zip(&vector.cases) {
        assert_eq!(l.ask, v.ask);
        if matches!(l.rank, Some(rank) if rank <= 5) {
            assert!(matches!(v.rank, Some(rank) if rank <= 5), "lost with the vector lane on: {}", l.ask);
        }
        if l.kind == Kind::Abstention && l.returned == 0 {
            assert_eq!(v.returned, 0, "the vector lane answered an unanswerable question: {}", l.ask);
        }
    }
}
