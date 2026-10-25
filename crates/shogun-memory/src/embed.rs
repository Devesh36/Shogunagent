//! Local embeddings (FR-MEM-21, ADR-001) — the model-agnostic core.
//!
//! This module defines the [`Embedder`] abstraction the search layer depends on, the e5
//! input-format helpers (e5 models require `query:` / `passage:` prefixes), a pure cosine
//! similarity, and a deterministic [`MockEmbedder`] for tests.
//!
//! **The default build has no embedder.** Search is lexical (FTS5) unless the server is built with
//! the `embed-local` feature (E24) *and* the operator has fetched a model with
//! `shogun models fetch` — nothing downloads a model at run time. The ONNX implementation of this
//! trait is `embed_onnx` (features `onnx` / `embed-local`), driven by a [`ModelSpec`]; the
//! default is [`NOMIC_EMBED_TEXT_V15`] (see its docs for why). Every vector is recorded with the
//! [`Embedder::model_id`] that produced it, and a store only ever compares vectors of one model
//! ([`crate::embedding_space`]).

/// e5-small output dimensionality (ADR-001 / the chosen model).
pub const E5_SMALL_DIM: usize = 384;

/// The width of every stored vector: the `vec0` tables (V2 `event_vec`, V23 `search_document_vec`)
/// are declared `float[384]`. A model with a wider native output stores its Matryoshka prefix of
/// this length (see [`ModelSpec::stored_dim`]).
pub const STORED_DIM: usize = E5_SMALL_DIM;

/// How token vectors become one text vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pooling {
    /// Mean over the attention mask (padding excluded).
    Mean,
}

/// One file of a model, pinned by content hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelFile {
    /// Path inside the model repository at [`ModelSpec::revision`].
    pub remote: &'static str,
    /// File name on disk under the model's directory.
    pub local: &'static str,
    /// Lowercase hex SHA-256 the download must match.
    pub sha256: &'static str,
}

/// Everything that decides what vector a text gets: weights, tokenizer, prefixes, pooling, the
/// stored width. Two specs with different [`ModelSpec::space_id`]s never share a vector space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelSpec {
    /// The model's name (`nomic-embed-text-v1.5`).
    pub name: &'static str,
    /// The vector-space id recorded per stored row: name and stored width.
    pub space_id: &'static str,
    /// The source repository (Hugging Face `owner/name`) and the pinned commit fetched from it.
    pub repo: &'static str,
    pub revision: &'static str,
    /// SPDX licence of the weights.
    pub license: &'static str,
    /// What `shogun models fetch` downloads (empty: not fetchable, place the files by hand).
    pub files: &'static [ModelFile],
    /// The model's own output width.
    pub native_dim: usize,
    /// The stored width ([`STORED_DIM`]): a Matryoshka prefix when narrower than `native_dim`.
    pub stored_dim: usize,
    /// Task prefixes the model was trained with. Leaving them off degrades retrieval silently.
    pub query_prefix: &'static str,
    pub passage_prefix: &'static str,
    pub pooling: Pooling,
    /// Layer-normalise the pooled vector before taking the Matryoshka prefix (nomic v1.5).
    pub layer_norm_before_truncate: bool,
    /// Longest input in tokens; longer text is truncated.
    pub max_tokens: usize,
    /// The vector lane's relevance cutoff for this model: the largest L2 distance between a
    /// query vector and a stored (unit) vector that still counts as a semantic match. Beyond it
    /// a neighbour is not evidence at all, and below it the lane's quality scales linearly to 0
    /// at the cutoff. For unit vectors `d = sqrt(2 − 2·cos)`, so `1.0` is cosine ≥ 0.5. Every
    /// model places unrelated text at its own typical distance, so the cutoff is a property of
    /// the model, calibrated on the memory-bench abstention suite
    /// (`docs/benchmarks/README.md`, "The vector lane").
    pub max_l2_distance: f32,
}

impl ModelSpec {
    /// The `.onnx` file on disk (the first file whose name ends in `.onnx`).
    pub fn onnx_file(&self) -> Option<&'static str> {
        self.files.iter().map(|f| f.local).find(|name| name.ends_with(".onnx"))
    }

    /// Whether this build pins the model's bytes: a 40-hex commit and a 64-hex SHA-256 for every
    /// file. `shogun models fetch` downloads only a pinned model and `shogun models verify` checks
    /// only against real pins; a spec whose pins are [`UNPINNED`] placeholders is refused by both
    /// (E43: the multilingual model's pins could not be read offline, so the operator records
    /// them once, `docs/embedding-model-setup.md`).
    pub fn is_pinned(&self) -> bool {
        let hex = |value: &str, len: usize| value.len() == len && value.chars().all(|c| c.is_ascii_hexdigit());
        hex(self.revision, 40) && !self.files.is_empty() && self.files.iter().all(|file| hex(file.sha256, 64))
    }
}

/// The placeholder for a revision or SHA-256 this build does not know. A spec carrying it is not
/// [`ModelSpec::is_pinned`]: nothing downloads it, nothing verifies against it.
pub const UNPINNED: &str = "";

/// The vector lane's cutoff for a space that is no known model (the hashing [`MockEmbedder`] of
/// unit tests, an `unspecified` space): the value the lane was first tuned with, for e5.
pub const DEFAULT_MAX_L2_DISTANCE: f32 = 1.0;

/// `nomic-embed-text-v1.5`'s cutoff: **0.70**, i.e. cosine ≥ 0.755 between the `search_query:`
/// vector and the `search_document:` vector (both the layer-normalised, truncated, re-normalised
/// 384 prefix). Conservative on purpose, favouring abstention:
///
/// - At the e5 value (1.0, cosine ≥ 0.5) every unanswerable question of the memory-bench
///   abstention suite got a vector neighbour back as evidence (false evidence 1.000, 600 events,
///   seed 42): nomic places unrelated text of the same domain at cosine 0.5–0.7 to a query.
/// - The real model cannot be downloaded in CI, so this value was not measured against it here.
///   It sits above that band, where nomic's own retrieval pairs (a query and the passage that
///   answers it) usually land; a paraphrase it drops is still found by the lexical lanes, while a
///   wrong neighbour returned as evidence is the failure the abstention suite exists to catch.
/// - Recalibrate on the owner's machine with the fetched model and keep the largest cutoff at
///   which `abstention_false_evidence` stays 0.000 while temporal recall@1 and multi-session
///   coverage@5 hold: `memory-bench harness --embedder nomic-embed-text-v1.5 --seeds 3 --events
///   20000 --out /tmp/nomic.json` (build with `--features embed-local`, `SHOGUN_MODELS_DIR` and
///   `ORT_DYLIB_PATH` set), once per candidate value (`docs/benchmarks/README.md`).
pub const NOMIC_MAX_L2_DISTANCE: f32 = 0.70;

/// [`TestEmbedder`]'s cutoff (**0.58**, cosine ≥ 0.83), calibrated on the memory-bench abstention
/// suite with the test embedder itself, CI's regression guard for the per-model cutoff. Swept on
/// `--path federated --embedder test --vector-cutoff F`, 500 questions: false evidence is 0.168 at
/// every cutoff from 1.0 down to 0.66 (a question and a chatter row sharing one concept axis sit
/// at about 0.62–0.66), still non-zero at 0.60–0.64 on some seeds and corpus sizes, and 0.000 at
/// 0.58 and 0.55 on seeds 42–46 at 2,000 and 20,000 events, with temporal recall@1 and
/// multi-session coverage@5 at 1.000. 0.58 is the largest of those; a single-concept paraphrase
/// ("what did it cost" → "The fee is 12k", about 0.53) is still found
/// (`docs/benchmarks/README.md`, "The vector lane").
pub const TEST_EMBEDDER_MAX_L2_DISTANCE: f32 = 0.58;

/// The vector lane's relevance cutoff for the vector space `model_id` (a [`ModelSpec::space_id`],
/// a model name, or [`TestEmbedder::SPACE_ID`]); [`DEFAULT_MAX_L2_DISTANCE`] for anything else.
pub fn max_l2_distance_for(model_id: &str) -> f32 {
    if model_id == TestEmbedder::SPACE_ID {
        return TEST_EMBEDDER_MAX_L2_DISTANCE;
    }
    model_spec(model_id).map_or(DEFAULT_MAX_L2_DISTANCE, |spec| spec.max_l2_distance)
}

/// **The default local model (E24): `nomic-embed-text-v1.5`, quantized ONNX.**
///
/// Chosen over `Qwen3-Embedding-0.6B` on the two criteria the plan names, licence and size. Both
/// are Apache-2.0. nomic v1.5 is a 137M-parameter BERT encoder: the int8 ONNX export is 137 MB
/// and runs through the existing mean-pooling path. Qwen3-Embedding-0.6B is 600M parameters
/// (about 1.2 GB at fp16, more as fp32 ONNX), a decoder that needs last-token pooling, an
/// instruction-formatted query and a community ONNX export. nomic v1.5 is a Matryoshka model, so
/// its 384-wide prefix (layer-normalised, then truncated, then L2-normalised, as its model card
/// prescribes) fits the existing `float[384]` tables with no new vector table. Its weakness is
/// that it is English-first; the Japanese-heavy golden eval is lexical and does not depend on it,
/// and a multilingual model is a new [`ModelSpec`] (a new space, re-embedded, never mixed).
pub const NOMIC_EMBED_TEXT_V15: ModelSpec = ModelSpec {
    name: "nomic-embed-text-v1.5",
    space_id: "nomic-embed-text-v1.5@384",
    repo: "nomic-ai/nomic-embed-text-v1.5",
    revision: "e9b6763023c676ca8431644204f50c2b100d9aab",
    license: "Apache-2.0",
    files: &[
        ModelFile {
            remote: "onnx/model_quantized.onnx",
            local: "model.onnx",
            sha256: "b4342336debaea79de872370664b0aaeb67dea4605513d00ee236ea871a81f27",
        },
        ModelFile {
            remote: "tokenizer.json",
            local: "tokenizer.json",
            sha256: "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66",
        },
    ],
    native_dim: 768,
    stored_dim: STORED_DIM,
    query_prefix: "search_query: ",
    passage_prefix: "search_document: ",
    pooling: Pooling::Mean,
    layer_norm_before_truncate: true,
    max_tokens: 512,
    max_l2_distance: NOMIC_MAX_L2_DISTANCE,
};

/// `multilingual-e5-small`'s cutoff: **0.63**, i.e. cosine ≥ 0.80 between the `query:` vector and
/// the `passage:` vector (both mean-pooled over the attention mask and L2-normalised, 384 native).
/// Conservative on purpose, favouring abstention, like nomic's:
///
/// - e5 was trained with an InfoNCE temperature of 0.01, which packs its cosine scores into a
///   narrow high band: its model card says scores sit around 0.7–1.0 even for unrelated text.
///   The 1.0 this spec carried before E43 (cosine ≥ 0.5, the value ShogunAI's vector lane was
///   first tuned with) therefore admits nearly every row of a store as a neighbour: no abstention.
/// - The one real-model measurement in this repository (ShogunAI, Apple Silicon, 2026-07-25,
///   `embed_onnx` tests) put an answering passage at cosine 0.796 and an on-topic non-answer at
///   0.830 for the same question. Unrelated text of the same domain lands below that band, so the
///   cutoff goes at its lower edge: 0.80. That measured answer sits just outside, which is what
///   "conservative" costs: a dropped same-language paraphrase is still reachable lexically, while
///   a wrong neighbour handed over as evidence is the failure the abstention suites exist to
///   catch. The cost falls on cross-lingual questions, which only the vector lane can answer.
/// - The real model cannot be fetched here (no network to model hubs), so the value is not
///   measured. Recalibrate on a machine with the model, on the multilingual suite and the
///   abstention suite together: keep the largest cutoff at which `false_answerable` (every
///   language) and `abstention_false_evidence` stay 0.000 (`docs/benchmarks/README.md`,
///   "Multilingual retrieval (E43)").
pub const MULTILINGUAL_E5_MAX_L2_DISTANCE: f32 = 0.63;

/// **The multilingual local model (E43): `multilingual-e5-small`**, the fp32 ONNX export in the
/// model's own repository (`onnx/model.onnx`, about 470 MB: the export ShogunAI ran through this
/// same ONNX path on device). MIT licence. A 118M-parameter XLM-RoBERTa encoder (12 layers, 384
/// hidden) trained on about 100 languages, among them every language of the multilingual suite
/// (Japanese, Chinese, Spanish, German, French, English).
///
/// - **Dims:** 384 native, so it fills the `float[384]` tables with no truncation and no new
///   vector table (no migration).
/// - **Pooling:** mean over the attention mask, then L2-normalise (no layer norm, no Matryoshka).
/// - **Prefixes:** `query: ` for questions and `passage: ` for stored text; the model degrades
///   silently without them.
/// - **Tokenizer:** XLM-R SentencePiece (`tokenizer.json`), which segments CJK without spaces.
///   Some exports take `token_type_ids` and some do not; the embedder asks the graph.
/// - **Max input:** 512 tokens.
///
/// Chosen over `Qwen3-Embedding-0.6B` (Apache-2.0, stronger on multilingual MTEB) because that
/// model does not run under the existing ONNX path: it is a decoder that needs last-token pooling
/// and an `Instruct: …` query template, its ONNX exports are community conversions with extra
/// inputs (`position_ids`, past key values), and at 600M parameters it is about 1.2 GB at fp16
/// against e5-small's 470 MB fp32. Over `multilingual-e5-base` because its 768 dims are not a
/// Matryoshka space, so a 384 prefix is not a valid embedding and a 768 table would need a
/// migration. Over nomic v1.5 for this role because nomic is English-first.
///
/// **Not pinned in this build.** The commit and the files' SHA-256s could not be read offline, so
/// they are [`UNPINNED`] and `shogun models fetch --model multilingual-e5-small` refuses until the
/// operator records them (`docs/embedding-model-setup.md`); files placed by hand still load.
pub const MULTILINGUAL_E5_SMALL: ModelSpec = ModelSpec {
    name: "multilingual-e5-small",
    space_id: "multilingual-e5-small@384",
    repo: "intfloat/multilingual-e5-small",
    // Pin: the 40-hex commit of intfloat/multilingual-e5-small the SHA-256s below were taken at.
    revision: UNPINNED,
    license: "MIT",
    files: &[
        ModelFile {
            remote: "onnx/model.onnx",
            local: "model.onnx",
            // Pin: `sha256sum` of onnx/model.onnx at `revision`.
            sha256: UNPINNED,
        },
        ModelFile {
            remote: "tokenizer.json",
            local: "tokenizer.json",
            // Pin: `sha256sum` of tokenizer.json at `revision`.
            sha256: UNPINNED,
        },
    ],
    native_dim: E5_SMALL_DIM,
    stored_dim: STORED_DIM,
    query_prefix: "query: ",
    passage_prefix: "passage: ",
    pooling: Pooling::Mean,
    layer_norm_before_truncate: false,
    max_tokens: 512,
    max_l2_distance: MULTILINGUAL_E5_MAX_L2_DISTANCE,
};

/// The default model and every model this build knows. The multilingual model is opt-in
/// (`SHOGUN_EMBED_MODEL=multilingual-e5-small`) until the owner's calibration run (E43).
pub const DEFAULT_MODEL: &ModelSpec = &NOMIC_EMBED_TEXT_V15;
pub const MODELS: &[&ModelSpec] = &[&NOMIC_EMBED_TEXT_V15, &MULTILINGUAL_E5_SMALL];

/// Look a model up by its name or space id.
pub fn model_spec(name: &str) -> Option<&'static ModelSpec> {
    MODELS.iter().copied().find(|spec| spec.name == name || spec.space_id == name)
}

/// Errors from embedding.
#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("model not loaded")]
    NotLoaded,
    #[error("inference failed: {0}")]
    Inference(String),
    #[error("dimension mismatch: expected {expected}, got {got}")]
    Dim { expected: usize, got: usize },
}

/// Produces embedding vectors. Implemented by the bundled ONNX model in production and by
/// [`MockEmbedder`] in tests. `Send + Sync` so the async embed job can hold one across tasks.
pub trait Embedder: Send + Sync {
    fn dim(&self) -> usize;
    /// The vector space this embedder writes and queries ([`ModelSpec::space_id`]). Recorded with
    /// every stored vector; search only compares vectors of the store's active model. Production
    /// embedders must override it; the default is the shared "unspecified" space test doubles use.
    fn model_id(&self) -> &str {
        crate::embedding_space::UNSPECIFIED_MODEL
    }
    /// Embed documents to store (e5 `passage:` role). One vector per input, each of length
    /// [`Embedder::dim`].
    fn embed_passages(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError>;
    /// Embed a search query (e5 `query:` role).
    fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError>;
}

/// e5 input formatting: the model is trained with these role prefixes and degrades without
/// them. The ONNX embedder applies these before tokenizing; exposed so the contract is explicit.
pub fn e5_query(text: &str) -> String {
    format!("query: {text}")
}

pub fn e5_passage(text: &str) -> String {
    format!("passage: {text}")
}

/// Cosine similarity of two equal-length vectors, in [-1, 1]. Returns 0.0 for a zero vector or
/// a length mismatch (both are "no signal" rather than an error on the hot search path).
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// L2-normalize a vector in place (unit length). A zero vector is left unchanged.
pub fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// A deterministic, model-free embedder for tests: hashes token bytes into a fixed-dim vector
/// and L2-normalizes it. Similar strings get similar vectors (shared tokens), so it exercises
/// the store + cosine-ranking path meaningfully without the real model. NOT for production.
#[derive(Debug, Clone)]
pub struct MockEmbedder {
    dim: usize,
}

impl MockEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }

    fn embed_one(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0.0f32; self.dim];
        // Bag-of-words hashing: each whitespace token bumps one dimension. Case-insensitive.
        for tok in text.to_lowercase().split_whitespace() {
            let mut h: u64 = 1469598103934665603; // FNV-1a offset basis
            for b in tok.bytes() {
                h ^= b as u64;
                h = h.wrapping_mul(1099511628211);
            }
            let idx = (h % self.dim as u64) as usize;
            v[idx] += 1.0;
        }
        l2_normalize(&mut v);
        v
    }
}

impl Embedder for MockEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn embed_passages(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|t| self.embed_one(&e5_passage(t))).collect())
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        Ok(self.embed_one(&e5_query(text)))
    }
}

/// A deterministic, model-free embedder with a **meaning** signal, for the tests that must show
/// the vector lane recovering a paraphrase the lexical lane cannot (E24). A small fixed table maps
/// synonyms to one shared dimension each — "cost", "price", "fee", 費用, 料金 all land on the same
/// axis — and every other word is hashed, at a lower weight, into the remaining dimensions. Since
/// E43 each axis also carries its Spanish, German, French and Chinese words (precio, Kosten, coût,
/// 费用 …), so a question in one language lands next to an answer written in another: the
/// cross-lingual half of the multilingual suite runs in CI with no model. It has its own space id,
/// so it never shares a store's space with a real model or with [`MockEmbedder`]. NOT for
/// production.
#[derive(Debug, Clone, Default)]
pub struct TestEmbedder;

/// Concept groups of [`TestEmbedder`]: each group is one dimension. ASCII keys without a space
/// match whole words; every other key (non-ASCII, or with a space) matches as a substring of the
/// lower-cased text, which is how CJK keys match inside unsegmented sentences.
const TEST_CONCEPTS: &[&[&str]] = &[
    &[
        "cost", "costs", "price", "prices", "pricing", "fee", "fees", "spend", "budget", "how much", "費用", "料金", "値段",
        "コスト", "金額", "予算", "precio", "coste", "costo", "cuesta", "cuánto", "presupuesto", "kosten", "kostet", "preis",
        "gebühr", "coût", "prix", "frais", "tarif", "费用", "价格", "多少钱", "预算",
    ],
    &[
        "meeting", "meetings", "sync", "standup", "call", "打ち合わせ", "打合せ", "会議", "ミーティング", "定例", "reunión",
        "reuniones", "sitzung", "besprechung", "réunion", "会议", "开会",
    ],
    &[
        "contract", "agreement", "renewal", "nda", "契約", "更新", "contrato", "renovación", "vertrag", "verlängerung",
        "contrat", "renouvellement", "合同", "续签", "续约",
    ],
    &[
        "deadline", "due", "by friday", "by monday", "締め切り", "締切", "期限", "plazo", "fecha límite", "frist",
        "abgabetermin", "échéance", "date limite", "截止",
    ],
    &[
        "invoice", "bill", "billing", "payment", "pay", "請求", "支払い", "支払", "factura", "pago", "rechnung", "zahlung",
        "facture", "paiement", "发票", "付款",
    ],
    &["vacation", "holiday", "leave", "time off", "休暇", "休み", "vacaciones", "urlaub", "congés", "vacances", "休假", "假期"],
    &[
        "bug", "crash", "error", "outage", "incident", "障害", "不具合", "エラー", "fallo", "fehler", "absturz", "panne",
        "erreur", "plantage", "故障", "错误", "崩溃",
    ],
    &[
        "hire", "hiring", "recruit", "recruiting", "candidate", "採用", "候補者", "contratación", "contratar", "einstellung",
        "bewerber", "recrutement", "embauche", "candidat", "招聘", "候选人",
    ],
    &[
        "launch", "release", "ship", "rollout", "リリース", "公開", "lanzamiento", "lanzar", "veröffentlichung",
        "markteinführung", "lancement", "发布", "上线",
    ],
    &["travel", "flight", "trip", "hotel", "出張", "フライト", "viaje", "vuelo", "reise", "dienstreise", "flug", "déplacement", "出差", "航班"],
];
const TEST_CONCEPT_WEIGHT: f32 = 1.0;
const TEST_WORD_WEIGHT: f32 = 0.2;

impl TestEmbedder {
    /// `v2` since E43 added the other languages' words to the axes: vectors of `v1` are not the
    /// same space.
    pub const SPACE_ID: &'static str = "shogun-test/concept-v2@384";

    pub fn new() -> Self {
        Self
    }

    fn embed_one(text: &str) -> Vec<f32> {
        let mut v = vec![0.0f32; STORED_DIM];
        let lower = text.to_lowercase();
        let words: Vec<&str> = lower
            .split(|c: char| !(c.is_alphanumeric()))
            .filter(|w| !w.is_empty())
            .collect();
        for (dim, group) in TEST_CONCEPTS.iter().enumerate() {
            let hit = group.iter().any(|key| {
                if key.is_ascii() && !key.contains(' ') {
                    words.iter().any(|w| w == key)
                } else {
                    lower.contains(key)
                }
            });
            if hit {
                v[dim] += TEST_CONCEPT_WEIGHT;
            }
        }
        let first_free = TEST_CONCEPTS.len();
        for word in &words {
            let mut h: u64 = 1469598103934665603;
            for b in word.bytes() {
                h ^= b as u64;
                h = h.wrapping_mul(1099511628211);
            }
            let idx = first_free + (h % (STORED_DIM - first_free) as u64) as usize;
            v[idx] += TEST_WORD_WEIGHT;
        }
        l2_normalize(&mut v);
        v
    }
}

impl Embedder for TestEmbedder {
    fn dim(&self) -> usize {
        STORED_DIM
    }

    fn model_id(&self) -> &str {
        Self::SPACE_ID
    }

    fn embed_passages(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|t| Self::embed_one(t)).collect())
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        Ok(Self::embed_one(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embedder_puts_synonyms_on_one_axis() {
        let e = TestEmbedder::new();
        let q = e.embed_query("what did it cost?").unwrap();
        let answer = e.embed_passages(&["The renewal fee lands at 12k"]).unwrap()[0].clone();
        let other = e.embed_passages(&["Standup notes: nothing blocking"]).unwrap()[0].clone();
        assert!(cosine_similarity(&q, &answer) > cosine_similarity(&q, &other));
        let ja = e.embed_passages(&["来月の料金は据え置き"]).unwrap()[0].clone();
        assert!(cosine_similarity(&q, &ja) > 0.3, "a Japanese synonym shares the axis");
        assert_eq!(e.model_id(), TestEmbedder::SPACE_ID);
        assert_ne!(e.model_id(), MockEmbedder::new(STORED_DIM).model_id());
    }

    #[test]
    fn the_default_model_is_licensed_pinned_and_fits_the_tables() {
        let spec = DEFAULT_MODEL;
        assert_eq!(spec.name, "nomic-embed-text-v1.5");
        assert!(["Apache-2.0", "MIT"].contains(&spec.license));
        assert_eq!(spec.stored_dim, STORED_DIM);
        assert!(spec.native_dim >= spec.stored_dim);
        assert!(spec.space_id.ends_with("@384"));
        assert_eq!(spec.onnx_file(), Some("model.onnx"));
        assert!(spec.files.iter().all(|f| f.sha256.len() == 64 && f.sha256.chars().all(|c| c.is_ascii_hexdigit())));
        assert_eq!(spec.revision.len(), 40, "a pinned commit, not a branch");
        assert_eq!(model_spec("nomic-embed-text-v1.5"), Some(&NOMIC_EMBED_TEXT_V15));
        assert_eq!(model_spec("multilingual-e5-small@384"), Some(&MULTILINGUAL_E5_SMALL));
        let mut spaces: Vec<_> = MODELS.iter().map(|m| m.space_id).collect();
        spaces.sort_unstable();
        spaces.dedup();
        assert_eq!(spaces.len(), MODELS.len(), "every model has its own space");
    }

    /// E43: the multilingual model is a full spec: fetchable files, its own space, its prefixes,
    /// 384 native (no truncation), and a cutoff of its own. Its pins are placeholders until the
    /// operator records them, and an unpinned spec is never treated as pinned.
    #[test]
    fn the_multilingual_model_is_a_complete_spec_that_waits_for_its_pins() {
        let spec = &MULTILINGUAL_E5_SMALL;
        assert_eq!(model_spec("multilingual-e5-small"), Some(spec));
        assert!(MODELS.iter().any(|m| std::ptr::eq(*m, spec)));
        assert_eq!(spec.license, "MIT");
        assert_eq!((spec.native_dim, spec.stored_dim), (STORED_DIM, STORED_DIM), "no Matryoshka cut");
        assert!(!spec.layer_norm_before_truncate);
        assert_eq!((spec.query_prefix, spec.passage_prefix), ("query: ", "passage: "));
        assert_eq!(spec.pooling, Pooling::Mean);
        assert_eq!(spec.onnx_file(), Some("model.onnx"));
        assert!(spec.files.iter().any(|f| f.local == "tokenizer.json"));
        assert!(!spec.is_pinned(), "placeholders are not pins");
        assert!(spec.files.iter().all(|f| f.sha256 == UNPINNED) && spec.revision == UNPINNED);
        assert!(DEFAULT_MODEL.is_pinned(), "the default model is pinned");
        assert!(!ModelSpec { revision: "main", ..NOMIC_EMBED_TEXT_V15 }.is_pinned(), "a branch is not a pin");
        assert!(!ModelSpec { files: &[], ..NOMIC_EMBED_TEXT_V15 }.is_pinned(), "nothing to pin");
        assert_ne!(spec.space_id, DEFAULT_MODEL.space_id);
    }

    /// The test embedder's concept axes span the multilingual suite's languages, so a question in
    /// one language lands next to its answer written in another, and an unrelated row does not.
    #[test]
    fn test_embedder_concepts_cross_languages() {
        let e = TestEmbedder::new();
        let l2 = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum::<f32>().sqrt();
        let answer = e.embed_passages(&["Kestrelとの契約更新の費用は月額12万円で確定"]).unwrap()[0].clone();
        for question in [
            "¿Cuánto cuesta la renovación del contrato?",
            "Was kostet die Verlängerung vom Vertrag?",
            "Quel est le coût du renouvellement du contrat ?",
            "续签合同的费用是多少",
            "how much is the contract renewal",
        ] {
            let q = e.embed_query(question).unwrap();
            assert!(l2(&q, &answer) <= TEST_EMBEDDER_MAX_L2_DISTANCE, "{question}: {}", l2(&q, &answer));
        }
        let unrelated = e.embed_query("¿Cuándo son las vacaciones del equipo?").unwrap();
        assert!(l2(&unrelated, &answer) > TEST_EMBEDDER_MAX_L2_DISTANCE);
    }

    #[test]
    fn the_vector_cutoff_is_a_property_of_the_model() {
        assert_eq!(max_l2_distance_for(NOMIC_EMBED_TEXT_V15.space_id), NOMIC_MAX_L2_DISTANCE);
        assert_eq!(max_l2_distance_for("nomic-embed-text-v1.5"), NOMIC_MAX_L2_DISTANCE);
        assert_eq!(max_l2_distance_for(MULTILINGUAL_E5_SMALL.space_id), MULTILINGUAL_E5_MAX_L2_DISTANCE);
        assert_eq!(max_l2_distance_for("multilingual-e5-small"), MULTILINGUAL_E5_MAX_L2_DISTANCE);
        assert_eq!(max_l2_distance_for(TestEmbedder::SPACE_ID), TEST_EMBEDDER_MAX_L2_DISTANCE);
        assert_eq!(max_l2_distance_for(crate::embedding_space::UNSPECIFIED_MODEL), DEFAULT_MAX_L2_DISTANCE);
        // e5 packs unrelated text into a high cosine band, so its cutoff is the strictest of the
        // real models, and stricter than the unknown-space default; every cutoff is a real distance.
        const { assert!(MULTILINGUAL_E5_MAX_L2_DISTANCE < NOMIC_MAX_L2_DISTANCE) };
        const { assert!(MULTILINGUAL_E5_MAX_L2_DISTANCE < DEFAULT_MAX_L2_DISTANCE) };
        for spec in MODELS {
            assert!(spec.max_l2_distance > 0.0 && spec.max_l2_distance <= 2.0, "{}", spec.name);
        }
        // The calibrated paraphrase is inside the test embedder's cutoff; an unrelated row is not.
        let e = TestEmbedder::new();
        let q = e.embed_query("what did it cost").unwrap();
        let near = e.embed_passages(&["The fee is 12k"]).unwrap()[0].clone();
        let far = e.embed_passages(&["Standup notes: nothing blocking"]).unwrap()[0].clone();
        let l2 = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum::<f32>().sqrt();
        assert!(l2(&q, &near) <= TEST_EMBEDDER_MAX_L2_DISTANCE, "{}", l2(&q, &near));
        assert!(l2(&q, &far) > TEST_EMBEDDER_MAX_L2_DISTANCE, "{}", l2(&q, &far));
    }

    #[test]
    fn cosine_of_identical_is_one() {
        let a = vec![1.0, 2.0, 3.0];
        assert!((cosine_similarity(&a, &a) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_of_orthogonal_is_zero() {
        assert_eq!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]), 0.0);
    }

    #[test]
    fn cosine_handles_zero_and_mismatch() {
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
        assert_eq!(cosine_similarity(&[1.0], &[1.0, 2.0]), 0.0);
    }

    #[test]
    fn l2_normalize_gives_unit_length() {
        let mut v = vec![3.0, 4.0];
        l2_normalize(&mut v);
        let len: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((len - 1.0).abs() < 1e-6);
    }

    #[test]
    fn e5_prefixes_are_applied() {
        assert_eq!(e5_query("hi"), "query: hi");
        assert_eq!(e5_passage("hi"), "passage: hi");
    }

    #[test]
    fn mock_embedder_dim_and_normalization() {
        let m = MockEmbedder::new(E5_SMALL_DIM);
        let v = m.embed_query("hello world").unwrap();
        assert_eq!(v.len(), E5_SMALL_DIM);
        let len: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((len - 1.0).abs() < 1e-5);
    }

    #[test]
    fn mock_similar_texts_rank_above_dissimilar() {
        let m = MockEmbedder::new(E5_SMALL_DIM);
        let q = m.embed_query("quarterly budget review").unwrap();
        let close = m.embed_passages(&["the quarterly budget review meeting"]).unwrap()[0].clone();
        let far = m.embed_passages(&["lunch plans for saturday"]).unwrap()[0].clone();
        assert!(
            cosine_similarity(&q, &close) > cosine_similarity(&q, &far),
            "shared tokens should rank closer"
        );
    }
}
