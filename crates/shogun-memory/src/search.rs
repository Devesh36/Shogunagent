//! Local hybrid search (FR-MEM-20, NFR-SLO-04).
//!
//! Public façade for focused search modules. Existing callers keep using
//! `shogun_memory::search::*`; parsing, filtering, ranking, retrieval, and tests stay isolated.

mod excerpt;
mod federated;
mod filtering;
mod grams;
mod lexical;
mod model;
mod query;
mod ranking;
mod retrieval;
mod terms;

pub use excerpt::{excerpt, snippet, SNIPPET_CHARS};
pub use federated::{
    event_places, hit_event_ids, search_federated, CanonicalEvidenceId, ComponentRanks, EventPlace, FederatedHit,
    FederatedSearchOptions, FederatedSearchResult, FederatedSource, FederatedSourceId,
    FederatedSourceKind, MatchQualitySummary, RelevanceFloor, RelevanceFloorReason, TierMetadata,
    MIN_FACT_CONFIDENCE, RELEVANCE_FLOOR_COVERAGE,
};
pub use filtering::{
    evidence_source_label, query_asks_about_screen, query_asks_for_latest, query_time_window,
    query_wants_visual_recall, visual_recall_window, LocalDayBounds,
};
pub use grams::{maintain_gram_index, GramIndexReport};
pub use model::SearchHit;
pub use query::{
    fts_query, lexical_terms, plan_memory_query, MemoryQueryFocus, MemoryQueryPlan,
    MemorySourceFamily, QueryAnchor, QueryAnchorKind, SourceConstraint,
};
pub use ranking::{apply_recency, reciprocal_rank_fusion};
pub use retrieval::{
    cold_partition_count_in_range, fts_search, fts_search_all_history_bounded,
    fts_search_fallback_bounded, fts_search_range, fts_search_since, fts_search_source,
    fuse_and_hydrate, hydrate, search, search_cold_partitions,
    search_exact_candidates, search_hybrid, search_hybrid_since, search_hybrid_with_options,
    search_meetings, search_meetings_in_range, search_warm_first, warm_knn_range, ColdScan,
    ColdScanStats, DeepSearchResult,
    MeetingSearchHit, SearchCandidates, SearchDepth, SearchOptions, SearchRange, COLD_CUTOFF_MS,
    DEFAULT_MAX_COLD_PARTITIONS, WARM_WINDOW_MS,
};

#[cfg(test)]
pub(crate) use query::MAX_FTS_TERMS;

/// Write path hook (V81): index one event's two-character grams. Best effort — see `grams.rs`.
pub(crate) fn index_event_grams(
    conn: &rusqlite::Connection,
    event_id: i64,
    window_title: Option<&str>,
    content: &str,
) -> Result<(), rusqlite::Error> {
    grams::index_event(conn, event_id, window_title, content)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod federated_tests;
