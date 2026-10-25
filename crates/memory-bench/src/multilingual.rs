//! The multilingual retrieval suite (E43): one fixed corpus written in six languages and the
//! questions that interrogate it — in the fact's own language, paraphrased, from another language,
//! as short CJK words, as unsegmented CJK sentences, and with no answer at all.
//!
//! ```text
//! FACTS + FILLER  → one in-memory store (the real write path, `event_log::insert_or_touch`)
//!                 → embedded with the run's one embedder, when there is one (`--embedder`)
//! CASES           → search_federated, the daemon's path, k = 10, abstaining below its floor
//!                 → R@5, MRR and false-answerable per query language and per kind
//! ```
//!
//! **Why fixed text and not a generator.** Cross-lingual retrieval is about meaning, and a seeded
//! generator cannot write a German question whose answer is a Japanese sentence. Every fact here
//! is one invented sentence with two topics (cost + contract, launch + bug …), every topic pair
//! appears in exactly one fact, and every question names the facts that answer it. The abstention
//! questions ask about a topic pair nothing in the corpus states, about a project the corpus does
//! name, so they are well-formed and share words with the store.
//!
//! **What each kind measures.**
//!
//! - `lexical`: the question shares the fact's words, same language. The lexical lanes' job.
//! - `paraphrase`: same language, synonyms instead of the fact's words (the project name may
//!   stay). Only a vector lane can be expected to bridge it.
//! - `cross_lingual`: asked in one language about a fact written in another. Only the project
//!   name is shared, and the relevance floor rightly refuses a row that matches one word of four,
//!   so the lexical number here is the floor working, not a bug; the vector lane is the only path.
//! - `cjk_short`: two- and three-character Japanese and Chinese words (会議, 发票, 請求書). The
//!   trigram index cannot match two characters; the V81 gram index does (`search/terms.rs`).
//! - `cjk_sentence`: a CJK question without particles or spaces (发票费用是多少). The lexical side
//!   treats an unbroken run of Han characters as one compound that matches only when every one of
//!   its bigrams is in a row, so a whole sentence is one term that no stored row contains.
//! - `abstention`: no answer exists. Scored as false-answerable: the share that still returned a
//!   row, i.e. handed a reader evidence for an answer that does not exist.
//!
//! **One embedder for the whole run**, exactly as the harness pins it: the report names it, and a
//! number from one model is never compared with a number from another. With `test` the vector lane
//! is the deterministic [`shogun_memory::embed::TestEmbedder`], whose concept axes carry every
//! suite language's words: it proves the cross-lingual plumbing in CI, it says nothing about a
//! real model's quality. A real model runs the same cases with `--features embed-local --embedder
//! multilingual-e5-small` on a machine that has it.

use std::collections::{BTreeMap, HashMap};

use serde::Serialize;

use crate::backend::{BackendError, MemoryBackend, SearchPath, ShogunBackend};
use crate::embedding::{BenchEmbedder, EmbeddingRef};
use crate::metrics::QualityAccumulator;
use crate::workload::BenchEvent;

/// Results requested per question; R@5 and MRR are read from these.
pub const K: usize = 10;

/// The language a fact is written in, or a question is asked in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Lang {
    En,
    Ja,
    Es,
    De,
    Fr,
    Zh,
}

impl Lang {
    pub const ALL: [Lang; 6] = [Lang::En, Lang::Ja, Lang::Es, Lang::De, Lang::Fr, Lang::Zh];

    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Ja => "ja",
            Lang::Es => "es",
            Lang::De => "de",
            Lang::Fr => "fr",
            Lang::Zh => "zh",
        }
    }
}

/// What a question tests (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Lexical,
    Paraphrase,
    CrossLingual,
    CjkShort,
    CjkSentence,
    Abstention,
}

impl Kind {
    pub const ALL: [Kind; 6] =
        [Kind::Lexical, Kind::Paraphrase, Kind::CrossLingual, Kind::CjkShort, Kind::CjkSentence, Kind::Abstention];

    pub fn name(self) -> &'static str {
        match self {
            Kind::Lexical => "lexical",
            Kind::Paraphrase => "paraphrase",
            Kind::CrossLingual => "cross_lingual",
            Kind::CjkShort => "cjk_short",
            Kind::CjkSentence => "cjk_sentence",
            Kind::Abstention => "abstention",
        }
    }
}

/// One stored sentence that answers something.
#[derive(Debug, Clone, Copy)]
pub struct Fact {
    pub id: &'static str,
    pub lang: Lang,
    pub text: &'static str,
}

/// One question and the facts that answer it (none for an abstention case).
#[derive(Debug, Clone, Copy)]
pub struct Case {
    pub lang: Lang,
    pub kind: Kind,
    pub ask: &'static str,
    pub expected: &'static [&'static str],
}

/// The answers. Each carries two topics, and no topic pair repeats; the project names are
/// invented and appear in one fact each.
pub const FACTS: &[Fact] = &[
    Fact { id: "en-kestrel", lang: Lang::En, text: "Kestrel contract renewal: the fee is fixed at 12k per year." },
    Fact { id: "en-heron", lang: Lang::En, text: "Heron launch slips a week because of the login crash." },
    Fact { id: "en-osprey", lang: Lang::En, text: "Osprey hiring: candidate shortlist due by Friday." },
    Fact { id: "ja-falcon", lang: Lang::Ja, text: "大阪出張の前に、Falcon チームと定例の会議を水曜に入れた。" },
    Fact { id: "ja-wren", lang: Lang::Ja, text: "Wren の請求書の支払い期限は今月末。" },
    Fact { id: "ja-lark", lang: Lang::Ja, text: "Lark の採用面接は夏季休暇のあとに再開する。" },
    Fact { id: "es-condor", lang: Lang::Es, text: "El presupuesto del lanzamiento de Condor sube a 40.000 euros." },
    Fact { id: "es-petrel", lang: Lang::Es, text: "El plazo para firmar el contrato con Petrel vence el lunes." },
    Fact { id: "es-ibis", lang: Lang::Es, text: "La reunión sobre el fallo del servidor de Ibis será el jueves." },
    Fact { id: "de-puffin", lang: Lang::De, text: "Die Kosten der Dienstreise von Puffin nach Berlin liegen bei 2.300 Euro." },
    Fact { id: "de-swift", lang: Lang::De, text: "Die Rechnung für Swift enthält einen Fehler bei der Mehrwertsteuer." },
    Fact { id: "de-robin", lang: Lang::De, text: "Wegen Urlaub fällt die Sitzung von Robin am Montag aus." },
    Fact { id: "fr-pelican", lang: Lang::Fr, text: "Le coût du recrutement pour l'équipe Pelican dépasse le budget prévu." },
    Fact { id: "fr-gannet", lang: Lang::Fr, text: "Le contrat de Gannet doit être signé avant le lancement de la version 2." },
    Fact { id: "fr-tern", lang: Lang::Fr, text: "La date limite pour réserver le déplacement de Tern à Lyon est vendredi." },
    Fact { id: "zh-merlin", lang: Lang::Zh, text: "Merlin 项目的发票费用是八万元。" },
    Fact { id: "zh-kite", lang: Lang::Zh, text: "招聘 Kite 团队前端工程师的会议定在周二。" },
    Fact { id: "zh-plover", lang: Lang::Zh, text: "出差期间 Plover 的服务器出现故障。" },
];

/// Everyday chatter in every language: rows that share function words with the questions and
/// answer none of them.
pub const FILLER: &[(Lang, &str)] = &[
    (Lang::En, "Standup notes: nothing blocking today."),
    (Lang::En, "Lunch options near the office on Thursday."),
    (Lang::En, "The office wifi keeps dropping since the upgrade."),
    (Lang::En, "Reminder to water the plants on the third floor."),
    (Lang::En, "The weekly newsletter draft is in the shared folder."),
    (Lang::Ja, "今日のランチは駅前のカレー屋に行った。"),
    (Lang::Ja, "オフィスのWi-Fiがまた切れている。"),
    (Lang::Ja, "共有フォルダに週報の下書きを置いた。"),
    (Lang::Ja, "三階の観葉植物に水をあげること。"),
    (Lang::Ja, "明日は在宅勤務の予定です。"),
    (Lang::Es, "Hoy comemos en el restaurante de la esquina."),
    (Lang::Es, "La wifi de la oficina vuelve a cortarse."),
    (Lang::Es, "El boletín semanal está en la carpeta compartida."),
    (Lang::Es, "Mañana trabajo desde casa."),
    (Lang::Es, "Hay que regar las plantas del tercer piso."),
    (Lang::De, "Heute gibt es Mittagessen in der Kantine."),
    (Lang::De, "Das WLAN im Büro bricht wieder ab."),
    (Lang::De, "Der Wochenbericht liegt im geteilten Ordner."),
    (Lang::De, "Morgen arbeite ich von zu Hause."),
    (Lang::De, "Die Pflanzen im dritten Stock brauchen Wasser."),
    (Lang::Fr, "On déjeune au restaurant du coin aujourd'hui."),
    (Lang::Fr, "Le wifi du bureau coupe encore."),
    (Lang::Fr, "La lettre hebdomadaire est dans le dossier partagé."),
    (Lang::Fr, "Demain je travaille depuis la maison."),
    (Lang::Fr, "Il faut arroser les plantes du troisième étage."),
    (Lang::Zh, "今天中午去楼下吃饭。"),
    (Lang::Zh, "办公室的无线网络又断了。"),
    (Lang::Zh, "周报草稿放在共享文件夹里。"),
    (Lang::Zh, "明天在家办公。"),
    (Lang::Zh, "三楼的植物需要浇水。"),
];

const fn case(lang: Lang, kind: Kind, ask: &'static str, expected: &'static [&'static str]) -> Case {
    Case { lang, kind, ask, expected }
}

/// Every question of the suite.
pub const CASES: &[Case] = &[
    // lexical: the fact's own words, the fact's own language
    case(Lang::En, Kind::Lexical, "Kestrel contract renewal fee", &["en-kestrel"]),
    case(Lang::En, Kind::Lexical, "Heron launch login crash", &["en-heron"]),
    case(Lang::En, Kind::Lexical, "Osprey candidate shortlist", &["en-osprey"]),
    case(Lang::Ja, Kind::Lexical, "Falcon との定例会議", &["ja-falcon"]),
    case(Lang::Ja, Kind::Lexical, "Wren の請求書の支払い期限", &["ja-wren"]),
    case(Lang::Ja, Kind::Lexical, "Lark の採用面接", &["ja-lark"]),
    case(Lang::Es, Kind::Lexical, "presupuesto del lanzamiento de Condor", &["es-condor"]),
    case(Lang::Es, Kind::Lexical, "plazo del contrato con Petrel", &["es-petrel"]),
    case(Lang::Es, Kind::Lexical, "reunión sobre el fallo del servidor de Ibis", &["es-ibis"]),
    case(Lang::De, Kind::Lexical, "Kosten der Dienstreise von Puffin", &["de-puffin"]),
    case(Lang::De, Kind::Lexical, "Fehler in der Rechnung für Swift", &["de-swift"]),
    case(Lang::De, Kind::Lexical, "Sitzung von Robin am Montag", &["de-robin"]),
    case(Lang::Fr, Kind::Lexical, "coût du recrutement Pelican", &["fr-pelican"]),
    case(Lang::Fr, Kind::Lexical, "contrat de Gannet avant le lancement", &["fr-gannet"]),
    case(Lang::Fr, Kind::Lexical, "date limite déplacement Tern", &["fr-tern"]),
    case(Lang::Zh, Kind::Lexical, "Merlin 发票费用", &["zh-merlin"]),
    case(Lang::Zh, Kind::Lexical, "Kite 招聘会议", &["zh-kite"]),
    case(Lang::Zh, Kind::Lexical, "Plover 服务器故障", &["zh-plover"]),
    // paraphrase: same language, synonyms instead of the fact's words
    case(Lang::En, Kind::Paraphrase, "how much does the Kestrel agreement cost", &["en-kestrel"]),
    case(Lang::En, Kind::Paraphrase, "is the Heron release delayed by an outage", &["en-heron"]),
    case(Lang::Ja, Kind::Paraphrase, "Wren の支払の締め切り", &["ja-wren"]),
    case(Lang::Es, Kind::Paraphrase, "¿Qué día son las reuniones por el error de Ibis?", &["es-ibis"]),
    case(Lang::De, Kind::Paraphrase, "Was kostet die Reise von Puffin?", &["de-puffin"]),
    case(Lang::Fr, Kind::Paraphrase, "Quel est le prix de l'embauche chez Pelican ?", &["fr-pelican"]),
    case(Lang::Zh, Kind::Paraphrase, "Kite 团队的候选人什么时候开会", &["zh-kite"]),
    // cross-lingual: asked in one language about a fact written in another
    case(Lang::En, Kind::CrossLingual, "When is the Wren invoice payment due?", &["ja-wren"]),
    case(Lang::En, Kind::CrossLingual, "What is the Condor launch budget?", &["es-condor"]),
    case(Lang::En, Kind::CrossLingual, "Did Plover have a crash during the trip?", &["zh-plover"]),
    case(Lang::Ja, Kind::CrossLingual, "Kestrel の契約更新の費用は?", &["en-kestrel"]),
    case(Lang::Ja, Kind::CrossLingual, "Robin の会議は休暇で中止?", &["de-robin"]),
    case(Lang::Ja, Kind::CrossLingual, "Tern の出張の締切はいつ?", &["fr-tern"]),
    case(Lang::Es, Kind::CrossLingual, "¿Cuánto cuesta el viaje de Puffin a Berlín?", &["de-puffin"]),
    case(Lang::Es, Kind::CrossLingual, "¿Cuándo es la reunión de contratación de Kite?", &["zh-kite"]),
    case(Lang::Es, Kind::CrossLingual, "¿La contratación de Lark sigue después de las vacaciones?", &["ja-lark"]),
    case(Lang::De, Kind::CrossLingual, "Verschiebt der Absturz die Markteinführung von Heron?", &["en-heron"]),
    case(Lang::De, Kind::CrossLingual, "Was kostet die Einstellung bei Pelican?", &["fr-pelican"]),
    case(Lang::De, Kind::CrossLingual, "Wann ist die Frist für den Vertrag mit Petrel?", &["es-petrel"]),
    case(Lang::Fr, Kind::CrossLingual, "Quel est le prix de la facture de Merlin ?", &["zh-merlin"]),
    case(Lang::Fr, Kind::CrossLingual, "La réunion avec Falcon a lieu avant le déplacement à Osaka ?", &["ja-falcon"]),
    case(Lang::Fr, Kind::CrossLingual, "Y a-t-il une erreur dans la facture de Swift ?", &["de-swift"]),
    case(Lang::Zh, Kind::CrossLingual, "Osprey 招聘候选人的截止日期", &["en-osprey"]),
    case(Lang::Zh, Kind::CrossLingual, "Ibis 服务器故障的会议是哪天", &["es-ibis"]),
    case(Lang::Zh, Kind::CrossLingual, "Gannet 的合同要在发布前签吗", &["fr-gannet"]),
    // cjk_short: two- and three-character words (the gram index and the trigram index)
    case(Lang::Ja, Kind::CjkShort, "定例", &["ja-falcon"]),
    case(Lang::Ja, Kind::CjkShort, "出張", &["ja-falcon"]),
    case(Lang::Ja, Kind::CjkShort, "請求", &["ja-wren"]),
    case(Lang::Ja, Kind::CjkShort, "期限", &["ja-wren"]),
    case(Lang::Ja, Kind::CjkShort, "採用", &["ja-lark"]),
    case(Lang::Ja, Kind::CjkShort, "休暇", &["ja-lark"]),
    case(Lang::Ja, Kind::CjkShort, "請求書", &["ja-wren"]),
    case(Lang::Zh, Kind::CjkShort, "发票", &["zh-merlin"]),
    case(Lang::Zh, Kind::CjkShort, "招聘", &["zh-kite"]),
    case(Lang::Zh, Kind::CjkShort, "故障", &["zh-plover"]),
    case(Lang::Zh, Kind::CjkShort, "出差", &["zh-plover"]),
    case(Lang::Zh, Kind::CjkShort, "服务器", &["zh-plover"]),
    // cjk_sentence: no particles, no spaces
    case(Lang::Zh, Kind::CjkSentence, "发票费用是多少", &["zh-merlin"]),
    case(Lang::Zh, Kind::CjkSentence, "服务器出现故障了吗", &["zh-plover"]),
    case(Lang::Ja, Kind::CjkSentence, "請求書支払期限", &["ja-wren"]),
    // abstention: a topic pair nothing states, about a project the corpus names
    case(Lang::En, Kind::Abstention, "What is the vacation budget for the Kestrel team?", &[]),
    case(Lang::En, Kind::Abstention, "Which contract covers the Heron travel?", &[]),
    case(Lang::Ja, Kind::Abstention, "Heron の会議の費用はいくら?", &[]),
    case(Lang::Ja, Kind::Abstention, "Wren のリリースの期限は?", &[]),
    case(Lang::Es, Kind::Abstention, "¿Cuándo es el viaje de vacaciones de Condor?", &[]),
    case(Lang::Es, Kind::Abstention, "¿Cuánto cuesta la reunión de Petrel?", &[]),
    case(Lang::De, Kind::Abstention, "Was kostet die Sitzung mit Puffin?", &[]),
    case(Lang::De, Kind::Abstention, "Gibt es einen Fehler im Vertrag von Robin?", &[]),
    case(Lang::Fr, Kind::Abstention, "Quand sont les vacances de l'équipe Gannet pour le lancement ?", &[]),
    case(Lang::Fr, Kind::Abstention, "Combien coûte la réunion de Tern ?", &[]),
    case(Lang::Zh, Kind::Abstention, "Merlin 团队的休假预算是多少", &[]),
    case(Lang::Zh, Kind::Abstention, "Plover 的合同有错误吗", &[]),
];

/// One run of the suite.
#[derive(Debug, Clone, Default)]
pub struct MultilingualConfig {
    /// The one embedder of the run, or `None` for the lexical baseline.
    pub embedder: Option<BenchEmbedder>,
    /// The vector lane's cutoff override (`--vector-cutoff`, calibration runs).
    pub vector_cutoff: Option<f32>,
}

/// One table row: a language, a kind, or everything.
#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub label: String,
    /// Questions with an answer.
    pub answerable: usize,
    pub recall_at_5: Option<f64>,
    pub mrr: Option<f64>,
    /// Questions without one.
    pub abstention: u64,
    /// Share of those that still returned a row.
    pub false_answerable: Option<f64>,
}

/// What one question did.
#[derive(Debug, Clone, Serialize)]
pub struct CaseOutcome {
    pub lang: Lang,
    pub kind: Kind,
    pub ask: &'static str,
    /// 1-indexed rank of the first answering row; `None` when none came back (or, for an
    /// abstention case, always).
    pub rank: Option<usize>,
    /// Rows returned.
    pub returned: usize,
}

/// The whole result. Serialized as-is by `memory-bench multilingual --out`.
#[derive(Debug, Clone, Serialize)]
pub struct MultilingualReport {
    pub suite: &'static str,
    pub search_path: SearchPath,
    pub k: usize,
    pub embedding: Option<EmbeddingRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vector_cutoff: Option<f32>,
    pub facts: usize,
    pub filler: usize,
    pub overall: Row,
    pub by_language: Vec<Row>,
    pub by_kind: Vec<Row>,
    pub cases: Vec<CaseOutcome>,
}

impl MultilingualReport {
    pub fn language(&self, lang: Lang) -> Option<&Row> {
        self.by_language.iter().find(|row| row.label == lang.code())
    }

    pub fn kind(&self, kind: Kind) -> Option<&Row> {
        self.by_kind.iter().find(|row| row.label == kind.name())
    }
}

fn row(label: &str, acc: &QualityAccumulator) -> Row {
    let summary = acc.summary(K);
    Row {
        label: label.to_string(),
        answerable: summary.queries,
        recall_at_5: summary.recall_at_5,
        mrr: summary.mrr,
        abstention: summary.abstention_queries,
        false_answerable: summary.false_evidence_rate,
    }
}

/// Ingest the corpus, embed it when the run has an embedder, ask every question.
pub fn run(config: &MultilingualConfig) -> Result<MultilingualReport, BackendError> {
    const STEP_MS: i64 = 60_000;
    let mut backend = ShogunBackend::in_memory()?;
    backend.search_path = SearchPath::Federated;
    backend.embedder = config.embedder.clone();
    backend.vector_cutoff = config.vector_cutoff;

    let rows = FACTS
        .iter()
        .map(|fact| (fact.id.to_string(), fact.text))
        .chain(FILLER.iter().enumerate().map(|(i, (lang, text))| (format!("filler-{}-{i}", lang.code()), *text)));
    let mut fact_of: HashMap<i64, String> = HashMap::new();
    let mut last_ts = 0;
    backend.begin_batch()?;
    for (i, (id, text)) in rows.enumerate() {
        last_ts = (i as i64 + 1) * STEP_MS;
        let event = BenchEvent {
            ts: last_ts,
            source: "capture".to_string(),
            kind: "text".to_string(),
            app_bundle_id: None,
            window_title: None,
            content: text.to_string(),
            dwell_ms: 0,
            fact_id: id.clone(),
        };
        let written = backend.write(&event)?;
        fact_of.insert(written.event_id, id);
    }
    backend.commit_batch()?;
    backend.now_ms = last_ts + 1;
    backend.embed_corpus()?;

    let mut overall = QualityAccumulator::new();
    let mut by_language: BTreeMap<Lang, QualityAccumulator> = BTreeMap::new();
    let mut by_kind: BTreeMap<Kind, QualityAccumulator> = BTreeMap::new();
    let mut cases = Vec::with_capacity(CASES.len());
    for case in CASES {
        let returned = backend.search(case.ask, K)?;
        let expected: Vec<String> = case.expected.iter().map(|id| (*id).to_string()).collect();
        for acc in [
            &mut overall,
            by_language.entry(case.lang).or_default(),
            by_kind.entry(case.kind).or_default(),
        ] {
            acc.record(&returned, &fact_of, &expected, &[]);
        }
        let rank = returned
            .iter()
            .position(|id| fact_of.get(id).is_some_and(|fact| expected.contains(fact)))
            .map(|index| index + 1);
        cases.push(CaseOutcome { lang: case.lang, kind: case.kind, ask: case.ask, rank, returned: returned.len() });
    }

    Ok(MultilingualReport {
        suite: "multilingual",
        search_path: SearchPath::Federated,
        k: K,
        embedding: config.embedder.as_ref().map(BenchEmbedder::model_ref),
        vector_cutoff: config.vector_cutoff,
        facts: FACTS.len(),
        filler: FILLER.len(),
        overall: row("overall", &overall),
        by_language: Lang::ALL
            .iter()
            .filter_map(|lang| by_language.get(lang).map(|acc| row(lang.code(), acc)))
            .collect(),
        by_kind: Kind::ALL
            .iter()
            .filter_map(|kind| by_kind.get(kind).map(|acc| row(kind.name(), acc)))
            .collect(),
        cases,
    })
}

fn opt(value: Option<f64>) -> String {
    value.map_or_else(|| "n/a".to_string(), |x| format!("{x:.3}"))
}

/// The console tables, followed by every answerable question that missed the top 5 and every
/// abstention question that returned rows.
pub fn render(report: &MultilingualReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "\nMULTILINGUAL RETRIEVAL (E43) — {} facts, {} filler rows, path {}, k {}, embedding {}{}\n\n",
        report.facts,
        report.filler,
        report.search_path.as_str(),
        report.k,
        report.embedding.as_ref().map_or("none (lexical)", |e| e.model.as_str()),
        report.vector_cutoff.map_or_else(String::new, |f| format!(", vector cutoff {f}")),
    ));
    let table = |out: &mut String, title: &str, rows: &[&Row]| {
        out.push_str(&format!(
            "{:<14} {:>4} {:>7} {:>7} {:>5} {:>16}\n",
            title, "n", "R@5", "MRR", "abst", "false-answerable"
        ));
        for row in rows {
            out.push_str(&format!(
                "{:<14} {:>4} {:>7} {:>7} {:>5} {:>16}\n",
                row.label,
                row.answerable,
                opt(row.recall_at_5),
                opt(row.mrr),
                row.abstention,
                opt(row.false_answerable),
            ));
        }
        out.push('\n');
    };
    let mut languages: Vec<&Row> = report.by_language.iter().collect();
    languages.push(&report.overall);
    table(&mut out, "language", &languages);
    let kinds: Vec<&Row> = report.by_kind.iter().collect();
    table(&mut out, "kind", &kinds);
    for case in &report.cases {
        let missed = match case.kind {
            Kind::Abstention => case.returned > 0,
            _ => !matches!(case.rank, Some(rank) if rank <= 5),
        };
        if missed {
            out.push_str(&format!(
                "  miss  {:<2} {:<13} rank {:<4} returned {:<2} {}\n",
                case.lang.code(),
                case.kind.name(),
                case.rank.map_or_else(|| "-".to_string(), |r| r.to_string()),
                case.returned,
                case.ask
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_is_asked_and_answered_and_every_expectation_names_a_fact() {
        for lang in Lang::ALL {
            assert!(FACTS.iter().any(|f| f.lang == lang), "{lang:?} has facts");
            for kind in [Kind::Lexical, Kind::CrossLingual, Kind::Abstention] {
                assert!(CASES.iter().any(|c| c.lang == lang && c.kind == kind), "{lang:?} {kind:?}");
            }
        }
        for case in CASES {
            assert_eq!(case.expected.is_empty(), case.kind == Kind::Abstention, "{}", case.ask);
            for id in case.expected {
                let fact = FACTS.iter().find(|f| f.id == *id).unwrap_or_else(|| panic!("{id}"));
                match case.kind {
                    Kind::CrossLingual => assert_ne!(fact.lang, case.lang, "{}", case.ask),
                    _ => assert_eq!(fact.lang, case.lang, "{}", case.ask),
                }
            }
        }
        let mut ids: Vec<&str> = FACTS.iter().map(|f| f.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), FACTS.len(), "fact ids are unique");
    }
}
