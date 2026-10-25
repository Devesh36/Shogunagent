//! The bench's vector lane (E24).
//!
//! Off by default: the published numbers are lexical and say so (`embedding: null`). With
//! `--embedder <name>` every arm of a run — every suite, every seed — uses **one** embedder
//! instance, so the embedding model is pinned across arms (MemDelta showed the embedding swap
//! alone moves accuracy by points, so arms with different models are not comparable). Each report
//! names the model it ran with, and the harness refuses to assemble arms that disagree.
//!
//! Names: `test` is the deterministic [`TestEmbedder`] (no model, CI-safe); with the
//! `embed-local` feature, `nomic-embed-text-v1.5` (or any [`shogun_memory::embed::MODELS`] name)
//! loads the fetched ONNX model from `SHOGUN_MODELS_DIR`.

use std::sync::Arc;

use serde::Serialize;
use shogun_memory::embed::{Embedder, TestEmbedder};

/// The model a run embedded with: the `embedding: {model, dim}` of `benchmarks.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EmbeddingRef {
    pub model: String,
    pub dim: usize,
}

/// One embedder shared by every arm.
#[derive(Clone)]
pub struct BenchEmbedder {
    embedder: Arc<dyn Embedder>,
}

impl std::fmt::Debug for BenchEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BenchEmbedder").field("model", &self.embedder.model_id()).finish()
    }
}

impl BenchEmbedder {
    pub fn new(embedder: Arc<dyn Embedder>) -> Self {
        Self { embedder }
    }

    /// The deterministic test embedder.
    pub fn test() -> Self {
        Self::new(Arc::new(TestEmbedder::new()))
    }

    /// Resolve `--embedder <name>`.
    pub fn parse(name: &str) -> Result<Self, String> {
        if name == "test" {
            return Ok(Self::test());
        }
        Self::load_model(name)
    }

    #[cfg(feature = "embed-local")]
    fn load_model(name: &str) -> Result<Self, String> {
        let spec = shogun_memory::embed::model_spec(name)
            .ok_or_else(|| format!("--embedder: no model named {name:?}"))?;
        let root = std::env::var_os("SHOGUN_MODELS_DIR")
            .map(std::path::PathBuf::from)
            .ok_or_else(|| "--embedder: set SHOGUN_MODELS_DIR to the directory `shogun models fetch` wrote".to_string())?;
        shogun_memory::embed_onnx::OnnxEmbedder::load_dir(spec, root.join(spec.name))
            .map(|embedder| Self::new(Arc::new(embedder)))
            .map_err(|error| format!("--embedder: {} failed to load: {error}", spec.name))
    }

    #[cfg(not(feature = "embed-local"))]
    fn load_model(name: &str) -> Result<Self, String> {
        Err(format!(
            "--embedder {name:?}: this build has only `test`; build memory-bench with --features embed-local for a real model"
        ))
    }

    pub fn embedder(&self) -> &dyn Embedder {
        self.embedder.as_ref()
    }

    pub fn model_ref(&self) -> EmbeddingRef {
        EmbeddingRef { model: self.embedder.model_id().to_string(), dim: self.embedder.dim() }
    }

    /// Whether two handles are the very same embedder instance (the pin the harness checks).
    pub fn same_instance(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.embedder, &other.embedder)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_always_available_and_named() {
        let e = BenchEmbedder::parse("test").unwrap();
        assert_eq!(e.model_ref(), EmbeddingRef { model: TestEmbedder::SPACE_ID.into(), dim: 384 });
        assert!(e.same_instance(&e.clone()));
        assert!(!e.same_instance(&BenchEmbedder::test()));
        assert!(BenchEmbedder::parse("no-such-model").is_err());
    }
}
