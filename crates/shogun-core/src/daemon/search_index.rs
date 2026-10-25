//! The search-index compaction job's DB calls (#714). Each is one short use of the shared
//! connection: the job holds the lock for one bounded step at a time and releases it between steps,
//! so capture and search wait at most one step.

use shogun_memory::fts_compact::{self, DbPages, Move, StepOutcome, TableState};

use super::*;

impl Db {
    /// Explicit bounded canonical-text inventory; never called by status polling.
    pub fn storage_preview(&self) -> MemoryResult<shogun_memory::storage_preview::StoragePreview> {
        self.with_conn("storage.preview", shogun_memory::storage_preview::preview)
    }
    /// One table's index shape and the job's next move for it (counts only).
    pub fn fts_compact_plan(&self, table: &str) -> MemoryResult<TableState> {
        self.with_conn("search_index.plan", |conn| fts_compact::plan(conn, table))
    }

    /// One bounded merge step: its own transaction.
    pub fn fts_compact_step(&self, table: &str, next: Move, pages: u32) -> MemoryResult<StepOutcome> {
        self.with_conn("search_index.step", |conn| fts_compact::merge_step(conn, table, next, pages))
    }

    /// Page accounting for the database file (four pragmas).
    pub fn db_pages(&self) -> MemoryResult<DbPages> {
        self.with_conn("search_index.pages", fts_compact::db_pages)
    }

    /// Hand up to `max_pages` free pages back to the filesystem (incremental-vacuum databases).
    pub fn release_free_pages(&self, max_pages: i64) -> MemoryResult<i64> {
        self.with_conn("search_index.release", |conn| {
            fts_compact::release_free_pages(conn, max_pages)
        })
    }
}
