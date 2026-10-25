//! Which embedding model a store's vectors belong to (E24, V102).
//!
//! Two vectors are only comparable when the same model produced them. A store therefore has one
//! **active space** (`embedding_space`: model id + dimension) and records, per vector row, the
//! model that wrote it (`vector_models`). The rules, enforced here and in the queries that read
//! vectors:
//!
//! - **Search reads the active space only.** Every KNN (`vector`, `document_vector`) and the Cold
//!   scan take their candidate set from [`ACTIVE_EVENT_ROWS`] / [`ACTIVE_DOCUMENT_ROWS`] /
//!   [`ACTIVE_COLD_ROWS`]: a row of another model — or of none, written before V102 — is never
//!   scored against a query.
//! - **The embed job writes the active space and re-embeds the rest.** [`activate`] switches the
//!   space to the job's model; rows of any other model then read as pending and are re-embedded
//!   in place, never compared. Nothing is deleted on a switch.
//! - **A query is only embedded with the active model.** The caller checks [`matches`] before it
//!   hands a query vector to search (`shogun-core`'s `Db::query_embedding`), so a model that has
//!   not finished building its space contributes nothing rather than nonsense.
//!
//! With no embedder ever attached there is no active space and every vector query is empty:
//! search is lexical, exactly as before E24.

use rusqlite::{params, Connection, OptionalExtension};

const SCHEMA_SQL: &str = include_str!("migrations/V102__embedding_spaces.sql");

/// The model id of a vector written without naming its model (tests, direct
/// [`crate::vector::upsert`] callers). It is a space like any other: a real model never shares it.
pub const UNSPECIFIED_MODEL: &str = "unspecified";

/// `vec_table` values in `vector_models`.
pub const EVENT_VEC: &str = "event_vec";
pub const DOCUMENT_VEC: &str = "search_document_vec";
pub const COLD: &str = "cold_embeddings";

/// Candidate-set subqueries: the row ids of each vector table that belong to the active space.
pub const ACTIVE_EVENT_ROWS: &str = "SELECT row_id FROM vector_models
     WHERE vec_table = 'event_vec'
       AND model_id = (SELECT model_id FROM embedding_space WHERE id = 1)";
pub const ACTIVE_DOCUMENT_ROWS: &str = "SELECT row_id FROM vector_models
     WHERE vec_table = 'search_document_vec'
       AND model_id = (SELECT model_id FROM embedding_space WHERE id = 1)";
pub const ACTIVE_COLD_ROWS: &str = "SELECT row_id FROM vector_models
     WHERE vec_table = 'cold_embeddings'
       AND model_id = (SELECT model_id FROM embedding_space WHERE id = 1)";

/// The store's active space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Space {
    pub model_id: String,
    pub dim: usize,
}

/// Re-create the V102 objects if a projection rebuild dropped the triggers on `search_documents`.
pub fn ensure_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(SCHEMA_SQL)
}

/// The active space, if any embedder has ever written to this store.
pub fn active(conn: &Connection) -> Result<Option<Space>, rusqlite::Error> {
    conn.query_row("SELECT model_id, dim FROM embedding_space WHERE id = 1", [], |row| {
        Ok(Space { model_id: row.get(0)?, dim: row.get::<_, i64>(1)?.max(0) as usize })
    })
    .optional()
}

/// Whether a query embedded by `model_id` may search this store: only when it is the active model.
pub fn matches(conn: &Connection, model_id: &str) -> Result<bool, rusqlite::Error> {
    Ok(active(conn)?.is_some_and(|space| space.model_id == model_id))
}

/// Make `model_id` the active space. Returns `true` when the space changed (a first activation or
/// a model switch): the rows of the previous model stay, unread, until the embed job re-embeds
/// them.
pub fn activate(conn: &Connection, model_id: &str, dim: usize, now_ms: i64) -> Result<bool, rusqlite::Error> {
    if active(conn)?.is_some_and(|space| space.model_id == model_id && space.dim == dim) {
        return Ok(false);
    }
    conn.execute(
        "INSERT INTO embedding_space (id, model_id, dim, since_ms) VALUES (1, ?1, ?2, ?3)
         ON CONFLICT(id) DO UPDATE SET model_id = excluded.model_id, dim = excluded.dim,
                                       since_ms = excluded.since_ms",
        params![model_id, dim as i64, now_ms],
    )?;
    Ok(true)
}

/// Activate `model_id` only when the store has no active space yet (a direct writer that names no
/// space joins the existing one's bookkeeping without switching it).
pub fn activate_if_unset(conn: &Connection, model_id: &str, dim: usize) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT OR IGNORE INTO embedding_space (id, model_id, dim, since_ms) VALUES (1, ?1, ?2, 0)",
        params![model_id, dim as i64],
    )?;
    Ok(())
}

/// Record that `model_id` wrote the vector `row_id` of `vec_table`.
pub fn record(conn: &Connection, vec_table: &str, row_id: i64, model_id: &str) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO vector_models (vec_table, row_id, model_id) VALUES (?1, ?2, ?3)
         ON CONFLICT(vec_table, row_id) DO UPDATE SET model_id = excluded.model_id",
        params![vec_table, row_id, model_id],
    )?;
    Ok(())
}

/// Forget the model of one vector row (its vector was removed).
pub fn forget(conn: &Connection, vec_table: &str, row_id: i64) -> Result<(), rusqlite::Error> {
    conn.execute(
        "DELETE FROM vector_models WHERE vec_table = ?1 AND row_id = ?2",
        params![vec_table, row_id],
    )?;
    Ok(())
}

/// The model that wrote one vector row, if recorded.
pub fn model_of(conn: &Connection, vec_table: &str, row_id: i64) -> Result<Option<String>, rusqlite::Error> {
    conn.query_row(
        "SELECT model_id FROM vector_models WHERE vec_table = ?1 AND row_id = ?2",
        params![vec_table, row_id],
        |row| row.get(0),
    )
    .optional()
}

/// Per-model row counts of one vector table, for diagnostics and the re-embed progress line.
pub fn counts(conn: &Connection, vec_table: &str) -> Result<Vec<(String, i64)>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT model_id, count(*) FROM vector_models WHERE vec_table = ?1 GROUP BY model_id ORDER BY model_id",
    )?;
    let rows = stmt.query_map([vec_table], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_store_starts_without_a_space_and_switches_explicitly() {
        let conn = crate::open_in_memory().unwrap();
        assert_eq!(active(&conn).unwrap(), None);
        assert!(!matches(&conn, "nomic-embed-text-v1.5@384").unwrap());

        assert!(activate(&conn, "a@384", 384, 10).unwrap());
        assert!(!activate(&conn, "a@384", 384, 11).unwrap(), "same model: no change");
        assert!(matches(&conn, "a@384").unwrap());
        assert!(!matches(&conn, "b@384").unwrap());

        activate_if_unset(&conn, "c@384", 384).unwrap();
        assert_eq!(active(&conn).unwrap().unwrap().model_id, "a@384", "an existing space is kept");

        assert!(activate(&conn, "b@384", 384, 12).unwrap());
        assert_eq!(active(&conn).unwrap(), Some(Space { model_id: "b@384".into(), dim: 384 }));
    }

    #[test]
    fn ledger_rows_follow_their_event() {
        let conn = crate::open_in_memory().unwrap();
        let id = crate::event_log::insert(
            &conn,
            &crate::event_log::NewEvent {
                ts: 1,
                source: "capture",
                kind: "text",
                app_bundle_id: None,
                window_title: None,
                content: "x",
                content_hash: "space-ledger",
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap();
        record(&conn, EVENT_VEC, id, "a@384").unwrap();
        record(&conn, EVENT_VEC, id, "b@384").unwrap();
        assert_eq!(model_of(&conn, EVENT_VEC, id).unwrap().as_deref(), Some("b@384"));
        assert_eq!(counts(&conn, EVENT_VEC).unwrap(), vec![("b@384".to_string(), 1)]);
        conn.execute("DELETE FROM event_log WHERE id = ?1", [id]).unwrap();
        assert_eq!(model_of(&conn, EVENT_VEC, id).unwrap(), None, "the trigger drops the ledger row");
    }

    #[test]
    fn ensure_schema_is_idempotent() {
        let conn = crate::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        ensure_schema(&conn).unwrap();
    }
}
