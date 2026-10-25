//! How we connect to SHOGUN.
//!
//! Everything the runner does to memory goes through [`MemoryBackend`]. That indirection is the
//! whole reason this crate is shaped the way it is: a later experiment (selective update,
//! consolidation, a retention policy) implements this trait a second time, and the workload, the
//! metrics and the report stay byte-for-byte the same. If an intervention needed changes to the
//! evaluator, the two runs would no longer be measuring the same thing.
//!
//! [`ShogunBackend`] is the baseline implementation and it deliberately owns no memory logic of
//! its own. It calls [`shogun_memory::event_log::insert_or_touch`] and
//! [`shogun_memory::search::search_hybrid_with_options`] — the real product paths — so the numbers
//! describe SHOGUN, not a reimplementation of SHOGUN that happens to live in a benchmark.

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use shogun_memory::search::{SearchDepth, SearchOptions};
use shogun_memory::{event_log, search};

use crate::workload::BenchEvent;

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("memory: {0}")]
    Memory(#[from] shogun_memory::MemoryError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("search: {0}")]
    Search(#[from] shogun_memory::search_documents::SearchDocumentError),
}

/// Which retrieval path a run measures.
///
/// This is not a tuning knob — the two are different code paths with different fusion, and a
/// number from one does not describe the other. The daemon (`shogun-core/src/daemon/search.rs`)
/// uses [`SearchPath::Federated`], so measuring only [`SearchPath::Hybrid`] would report on a path
/// no user query reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchPath {
    /// `search_hybrid_with_options`: plain reciprocal rank fusion over the lexical, Warm-vector
    /// and Cold lanes, with `1/(k+rank)` discarding bm25 magnitude entirely.
    ///
    /// The default, because every baseline in `BASELINE.md` was measured here and changing what
    /// the default means would silently break every historical comparison.
    #[default]
    Hybrid,
    /// `search_federated`: what the daemon actually calls. Fuses the same lanes but weights each
    /// contribution by normalised bm25 strength and lexical coverage, so magnitude survives
    /// fusion, and it reaches derived, meeting and imported sources as well as raw events.
    Federated,
}

impl SearchPath {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "hybrid" => Some(Self::Hybrid),
            "federated" => Some(Self::Federated),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hybrid => "hybrid",
            Self::Federated => "federated",
        }
    }
}

/// What one write actually did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteOutcome {
    /// The row the event now lives in — a new row, or the existing row that absorbed it.
    pub event_id: i64,
    /// `true` when the backend recognised this as a repeat and touched an existing row instead of
    /// appending. This is the measurement, not an assumption: the bench never decides for itself
    /// what counted as a duplicate.
    pub deduplicated: bool,
}

/// How big the store is, measured two ways because they answer different questions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StorageSize {
    /// `page_count * page_size` — the logical size SQLite accounts for. Defined for in-memory
    /// databases too, so it is the number that is always comparable across runs.
    pub logical_bytes: u64,
    /// Bytes actually on disk, including the `-wal` and `-shm` sidecars. `None` for an in-memory
    /// database. Larger than `logical_bytes` mid-run, because the WAL has not been checkpointed.
    pub file_bytes: Option<u64>,
}

/// The seam every experiment plugs into.
pub trait MemoryBackend {
    /// Recorded in the report so a result names the thing it measured.
    fn name(&self) -> &'static str;

    /// Ingest one event.
    fn write(&mut self, ev: &BenchEvent) -> Result<WriteOutcome, BackendError>;

    /// Retrieve up to `k` event ids, best first.
    fn search(&self, query: &str, k: usize) -> Result<Vec<i64>, BackendError>;

    /// Rows durably held. Compared against writes submitted to get write amplification.
    fn count(&self) -> Result<i64, BackendError>;

    fn size(&self) -> Result<StorageSize, BackendError>;

    /// Open a bulk-load transaction. Default is a no-op, so a backend with no notion of batching
    /// still satisfies the trait and the runner needs no special case.
    fn begin_batch(&mut self) -> Result<(), BackendError> {
        Ok(())
    }

    fn commit_batch(&mut self) -> Result<(), BackendError> {
        Ok(())
    }
}

/// The current memory layer, unmodified.
pub struct ShogunBackend {
    /// Recency weight passed to [`shogun_memory::search::SearchOptions`]. `0.0` reproduces the
    /// shipped ranking exactly, so the A/B is one field wide.
    pub recency_weight: f64,
    /// Which retrieval path [`MemoryBackend::search`] exercises.
    pub search_path: SearchPath,
    /// The clock value handed to the retrieval path.
    ///
    /// Passed in rather than read so a run is reproducible. The runner sets it past the newest
    /// event in the corpus: the federated path resolves its Warm window relative to "now", and
    /// leaving it at zero would place a synthetic corpus entirely in the future.
    pub now_ms: i64,
    conn: Connection,
    path: Option<PathBuf>,
}

impl ShogunBackend {
    /// Open a file-backed database. Prefer this over [`Self::in_memory`] for any run whose numbers
    /// are going to be quoted: an in-memory database has no WAL, no page cache eviction and no
    /// filesystem, so its write latency is not the latency the product experiences.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, BackendError> {
        let path = path.as_ref().to_path_buf();
        let conn = shogun_memory::open(&path)?;
        Ok(Self {
            conn,
            path: Some(path),
            recency_weight: 0.0,
            search_path: SearchPath::default(),
            now_ms: 0,
        })
    }

    /// In-memory database, for the CI smoke run where the point is that the pipeline works at all,
    /// not what it costs.
    pub fn in_memory() -> Result<Self, BackendError> {
        Ok(Self {
            conn: shogun_memory::open_in_memory()?,
            path: None,
            recency_weight: 0.0,
            search_path: SearchPath::default(),
            now_ms: 0,
        })
    }

    /// Escape hatch for measurements that need the connection directly (the tier and cold-scan
    /// accounting a later commit will add).
    pub fn conn(&self) -> &Connection {
        &self.conn
    }
}

impl MemoryBackend for ShogunBackend {
    fn name(&self) -> &'static str {
        match self.search_path {
            SearchPath::Hybrid => "shogun-memory (hybrid)",
            SearchPath::Federated => "shogun-memory (federated)",
        }
    }

    fn write(&mut self, ev: &BenchEvent) -> Result<WriteOutcome, BackendError> {
        // `content_hash` is left to the memory layer to compute. Passing our own would be passing
        // our own dedup contract, and the dedup contract is one of the things under measurement:
        // `event_log::content_hash` normalises the body first, and a bench that hashed the raw
        // string would report collapses the product would not make.
        let hash = event_log::content_hash(&ev.content);
        let new = event_log::NewEvent {
            ts: ev.ts,
            source: &ev.source,
            kind: &ev.kind,
            app_bundle_id: ev.app_bundle_id.as_deref(),
            window_title: ev.window_title.as_deref(),
            content: &ev.content,
            content_hash: &hash,
            dwell_ms: ev.dwell_ms,
            display_id: None,
            window_bounds: None,
        };
        let (event_id, deduplicated) = event_log::insert_or_touch(&self.conn, &new)?;
        Ok(WriteOutcome {
            event_id,
            deduplicated,
        })
    }

    fn search(&self, query: &str, k: usize) -> Result<Vec<i64>, BackendError> {
        // `query_embedding: None` means the semantic half contributes nothing and RRF fuses the
        // lexical list alone. That is not a shortcut — it is the configuration CI runs in, because
        // the ONNX embedder is behind an off-by-default feature and needs a model file on disk.
        // The report records `semantic: false` so a lexical number is never mistaken for a hybrid
        // one; `retrieval_eval.rs` established that the gap between them is real (recall@5 0.93 vs
        // 1.00), so the two must never be compared to each other.
        match self.search_path {
            SearchPath::Hybrid => {
                let opts = SearchOptions {
                    since_ts: None,
                    depth: SearchDepth::WarmOnly,
                    recency_epsilon: self.recency_weight,
                    ..Default::default()
                };
                let result = search::search_hybrid_with_options(
                    &self.conn,
                    query,
                    None,
                    self.now_ms,
                    &opts,
                    k,
                )?;
                Ok(result.hits.into_iter().map(|h| h.event_id).collect())
            }
            SearchPath::Federated => {
                // `raw_event_ranks` is the event lane before cross-source clustering. That is the
                // right comparison: the corpus contains only `event_log` rows, so the clustered
                // `hits` would be the same rows wearing a different shape, and taking them would
                // mix a clustering decision into a retrieval measurement.
                let opts = shogun_memory::search::FederatedSearchOptions {
                    depth: SearchDepth::WarmOnly,
                    ..Default::default()
                };
                let result = search::search_federated(
                    &self.conn,
                    query,
                    None,
                    self.now_ms,
                    &opts,
                    k,
                )?;
                Ok(result
                    .raw_event_ranks
                    .into_iter()
                    .take(k)
                    .map(|(id, _score)| id)
                    .collect())
            }
        }
    }

    fn count(&self) -> Result<i64, BackendError> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM event_log", [], |r| r.get(0))?)
    }

    fn size(&self) -> Result<StorageSize, BackendError> {
        let page_count: i64 = self.conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        let page_size: i64 = self.conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        let logical_bytes = (page_count.max(0) as u64).saturating_mul(page_size.max(0) as u64);

        let file_bytes = match &self.path {
            Some(p) => {
                let mut total = 0u64;
                for suffix in ["", "-wal", "-shm"] {
                    let candidate = if suffix.is_empty() {
                        p.clone()
                    } else {
                        let mut s = p.clone().into_os_string();
                        s.push(suffix);
                        PathBuf::from(s)
                    };
                    // A missing -wal/-shm is normal, not an error.
                    if let Ok(meta) = std::fs::metadata(&candidate) {
                        total = total.saturating_add(meta.len());
                    }
                }
                Some(total)
            }
            None => None,
        };
        Ok(StorageSize {
            logical_bytes,
            file_bytes,
        })
    }

    fn begin_batch(&mut self) -> Result<(), BackendError> {
        self.conn.execute_batch("BEGIN")?;
        Ok(())
    }

    fn commit_batch(&mut self) -> Result<(), BackendError> {
        self.conn.execute_batch("COMMIT")?;
        Ok(())
    }
}
