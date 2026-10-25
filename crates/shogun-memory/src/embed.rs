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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

impl ModelSpec {
    /// The `.onnx` file on disk (the first file whose name ends in `.onnx`).
    pub fn onnx_file(&self) -> Option<&'static str> {
        self.files.iter().map(|f| f.local).find(|name| name.ends_with(".onnx"))
    }
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
};

/// The developer model ShogunAI used (`multilingual-e5-small`, MIT, 384 native). Not fetchable by
/// `shogun models fetch`: place `model.onnx` and `tokenizer.json` by hand
/// (`docs/shogunai-origin/embedding-model-setup.md`).
pub const MULTILINGUAL_E5_SMALL: ModelSpec = ModelSpec {
    name: "multilingual-e5-small",
    space_id: "multilingual-e5-small@384",
    repo: "intfloat/multilingual-e5-small",
    revision: "main",
    license: "MIT",
    files: &[],
    native_dim: E5_SMALL_DIM,
    stored_dim: STORED_DIM,
    query_prefix: "query: ",
    passage_prefix: "passage: ",
    pooling: Pooling::Mean,
    layer_norm_before_truncate: false,
    max_tokens: 512,
};

/// The default model and every model this build knows.
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
/// synonyms (English and Japanese) to one shared dimension each — "cost", "price", "fee", 費用,
/// 料金 all land on the same axis — and every other word is hashed, at a lower weight, into the
/// remaining dimensions. It has its own space id, so it never shares a store's space with a real
/// model or with [`MockEmbedder`]. NOT for production.
#[derive(Debug, Clone, Default)]
pub struct TestEmbedder;

/// Concept groups of [`TestEmbedder`]: each group is one dimension.
const TEST_CONCEPTS: &[&[&str]] = &[
    &["cost", "costs", "price", "prices", "pricing", "fee", "fees", "spend", "budget", "how much", "費用", "料金", "値段", "コスト", "金額", "予算"],
    &["meeting", "meetings", "sync", "standup", "call", "打ち合わせ", "打合せ", "会議", "ミーティング", "定例"],
    &["contract", "agreement", "renewal", "nda", "契約", "更新"],
    &["deadline", "due", "by friday", "by monday", "締め切り", "締切", "期限"],
    &["invoice", "bill", "billing", "payment", "pay", "請求", "支払い", "支払"],
    &["vacation", "holiday", "leave", "time off", "休暇", "休み"],
    &["bug", "crash", "error", "outage", "incident", "障害", "不具合", "エラー"],
    &["hire", "hiring", "recruit", "recruiting", "candidate", "採用", "候補者"],
    &["launch", "release", "ship", "rollout", "リリース", "公開"],
    &["travel", "flight", "trip", "hotel", "出張", "フライト"],
];
const TEST_CONCEPT_WEIGHT: f32 = 1.0;
const TEST_WORD_WEIGHT: f32 = 0.2;

impl TestEmbedder {
    pub const SPACE_ID: &'static str = "shogun-test/concept-v1@384";

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
