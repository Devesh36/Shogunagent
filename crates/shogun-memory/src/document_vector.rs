//! Rebuildable Warm semantic index over clean distilled memory documents.

use rusqlite::{params, Connection};

use crate::embed::{EmbedError, Embedder};
use crate::search::MemoryQueryFocus;

const SCHEMA_SQL: &str = include_str!("migrations/V23__search_document_vectors.sql");

#[derive(Debug, thiserror::Error)]
pub enum DocumentVectorError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("embed: {0}")]
    Embed(#[from] EmbedError),
}

pub fn ensure_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(SCHEMA_SQL)?;
    crate::embedding_space::ensure_schema(conn)
}

pub fn pending(
    conn: &Connection,
    limit: usize,
) -> Result<Vec<(i64, String, String, String)>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT d.id,d.title,d.body,d.content_digest
           FROM search_documents d
           LEFT JOIN search_document_embeddings e
             ON e.document_id=d.id AND e.content_digest=d.content_digest
           LEFT JOIN search_document_vec v ON v.rowid=d.id
          LEFT JOIN vector_models m ON m.vec_table='search_document_vec' AND m.row_id=d.id
          WHERE d.is_dirty=0
            AND (e.document_id IS NULL OR v.rowid IS NULL OR m.model_id IS NULL
                 OR m.model_id IS NOT (SELECT model_id FROM embedding_space WHERE id=1))
            AND NOT EXISTS (
                SELECT 1 FROM search_document_invalidations i
                 WHERE i.source_table=d.source_table AND i.source_id=d.source_id)
          ORDER BY d.id LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit.min(100) as i64], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
    })?;
    rows.collect()
}

/// Store a document vector without naming its model ([`crate::embedding_space::UNSPECIFIED_MODEL`],
/// which becomes the active space only if the store has none). The embed job uses [`upsert_in`].
pub fn upsert(
    conn: &Connection,
    document_id: i64,
    content_digest: &str,
    embedding: &[f32],
    now_ms: i64,
) -> Result<(), rusqlite::Error> {
    let model = crate::embedding_space::UNSPECIFIED_MODEL;
    upsert_in(conn, document_id, content_digest, model, embedding, now_ms)?;
    crate::embedding_space::activate_if_unset(conn, model, embedding.len())
}

/// Store the vector `model_id` produced for a document, recording the model.
pub fn upsert_in(
    conn: &Connection,
    document_id: i64,
    content_digest: &str,
    model_id: &str,
    embedding: &[f32],
    now_ms: i64,
) -> Result<(), rusqlite::Error> {
    crate::embedding_space::record(conn, crate::embedding_space::DOCUMENT_VEC, document_id, model_id)?;
    conn.execute("DELETE FROM search_document_vec WHERE rowid=?1", [document_id])?;
    conn.execute(
        "INSERT INTO search_document_vec(rowid,embedding) VALUES(?1,?2)",
        params![document_id, to_blob(embedding)],
    )?;
    conn.execute(
        "INSERT INTO search_document_embeddings(document_id,content_digest,embedded_at_ms)
         VALUES(?1,?2,?3)
         ON CONFLICT(document_id) DO UPDATE SET
           content_digest=excluded.content_digest,embedded_at_ms=excluded.embedded_at_ms",
        params![document_id, content_digest, now_ms],
    )?;
    Ok(())
}

pub fn embed_batch(
    conn: &mut Connection,
    embedder: &dyn Embedder,
    batch: usize,
    now_ms: i64,
) -> Result<usize, DocumentVectorError> {
    ensure_schema(conn)?;
    crate::embedding_space::activate(conn, embedder.model_id(), embedder.dim(), now_ms)?;
    let rows = pending(conn, batch)?;
    if rows.is_empty() {
        return Ok(0);
    }
    let passages = rows
        .iter()
        .map(|(_, title, body, _)| format!("{title}\n{body}"))
        .collect::<Vec<_>>();
    let inputs = passages.iter().map(String::as_str).collect::<Vec<_>>();
    let embeddings = embedder.embed_passages(&inputs)?;
    if embeddings.len() != rows.len() {
        return Err(EmbedError::Inference("passage count mismatch".to_owned()).into());
    }
    if let Some(bad) = embeddings.iter().find(|v| v.len() != embedder.dim()) {
        return Err(EmbedError::Dim { expected: embedder.dim(), got: bad.len() }.into());
    }
    let tx = conn.transaction()?;
    for ((document_id, _, _, digest), embedding) in rows.iter().zip(&embeddings) {
        let current: bool = tx.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM search_documents d
                 WHERE d.id=?1 AND d.content_digest=?2 AND d.is_dirty=0
                   AND NOT EXISTS (
                     SELECT 1 FROM search_document_invalidations i
                      WHERE i.source_table=d.source_table AND i.source_id=d.source_id))",
            params![document_id, digest],
            |row| row.get(0),
        )?;
        if current {
            upsert_in(&tx, *document_id, digest, embedder.model_id(), embedding, now_ms)?;
        }
    }
    tx.commit()?;
    Ok(rows.len())
}

pub fn embed_all_pending(
    conn: &mut Connection,
    embedder: &dyn Embedder,
    batch: usize,
    now_ms: i64,
) -> Result<usize, DocumentVectorError> {
    let mut total = 0;
    loop {
        let embedded = embed_batch(conn, embedder, batch, now_ms)?;
        if embedded == 0 {
            return Ok(total);
        }
        total += embedded;
    }
}

pub fn knn(
    conn: &Connection,
    query: &[f32],
    limit: usize,
) -> Result<Vec<i64>, rusqlite::Error> {
    knn_filtered(conn, query, None, None, MemoryQueryFocus::Any, limit)
}

pub fn knn_filtered(
    conn: &Connection,
    query: &[f32],
    from_ms: Option<i64>,
    to_ms: Option<i64>,
    focus: MemoryQueryFocus,
    limit: usize,
) -> Result<Vec<i64>, rusqlite::Error> {
    Ok(knn_filtered_scored(conn, query, from_ms, to_ms, focus, limit)?
        .into_iter()
        .map(|hit| hit.id)
        .collect())
}

pub fn knn_filtered_scored(
    conn: &Connection,
    query: &[f32],
    from_ms: Option<i64>,
    to_ms: Option<i64>,
    focus: MemoryQueryFocus,
    limit: usize,
) -> Result<Vec<crate::vector::VectorHit>, rusqlite::Error> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    ensure_schema(conn)?;
    let mut stmt = conn.prepare(
        "SELECT rowid,distance FROM search_document_vec
          WHERE embedding MATCH ?1 AND k=?2
            AND rowid IN (
              SELECT d.id FROM search_documents d
              JOIN search_document_embeddings e
                ON e.document_id=d.id AND e.content_digest=d.content_digest
             WHERE d.is_dirty=0
               AND d.id IN (SELECT row_id FROM vector_models
                             WHERE vec_table='search_document_vec'
                               AND model_id=(SELECT model_id FROM embedding_space WHERE id=1))
               AND (?3 IS NULL OR d.period_end_ms IS NULL OR d.period_end_ms>?3)
               AND (?4 IS NULL OR d.period_start_ms IS NULL OR d.period_start_ms<?4)
               AND (
                 ?5=0
                 OR (?5=1 AND d.document_kind IN (?6,?7,?8))
                 OR (?5=2 AND d.document_kind IN (?9,?10))
                 OR (?5=3 AND d.document_kind=?11)
                 OR (?5=4 AND d.document_kind=?12)
                 OR (?5=5 AND d.document_kind=?13)
                 OR (?5=6 AND d.document_kind=?14)
                 OR (?5=7 AND d.document_kind=?15))
               AND NOT EXISTS (
                 SELECT 1 FROM search_document_invalidations i
                  WHERE i.source_table=d.source_table AND i.source_id=d.source_id))
          ORDER BY distance",
    )?;
    let rows = stmt.query_map(
        params![
            to_blob(query),
            limit.min(100) as i64,
            from_ms,
            to_ms,
            focus.code(),
            "session_summary",
            "meeting_recap",
            "meeting_note",
            "commitment",
            "open_loop",
            "brief",
            "period_summary",
            "person",
            "project",
            "thread_summary",
        ],
        |row| {
            Ok(crate::vector::VectorHit { id: row.get(0)?, distance: row.get(1)? })
        },
    )?;
    rows.collect()
}

fn to_blob(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|value| value.to_le_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::{MockEmbedder, E5_SMALL_DIM};
    use crate::event_log::{insert, NewEvent};
    use crate::state::{insert_project, NewProject, ProjectStatus, Provenance};

    #[test]
    fn migration_creates_rebuildable_document_vector_objects() {
        let conn = crate::open_in_memory().unwrap();
        let object_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master
                  WHERE name IN (?1,?2,?3,?4)",
                params![
                    "search_document_vec",
                    "search_document_embeddings",
                    "search_document_vectors_ad",
                    "search_document_vectors_au",
                ],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(object_count, 4);
        assert_eq!(
            crate::schema_version(&conn).unwrap(),
            Some(crate::LATEST_SCHEMA_VERSION)
        );
    }

    #[test]
    fn dirty_document_loses_vector_and_is_requeued_after_reprojection() {
        let mut conn = crate::open_in_memory().unwrap();
        let event_id = insert(
            &conn,
            &NewEvent {
                ts: 1,
                source: "capture",
                kind: "text",
                app_bundle_id: None,
                window_title: None,
                content: "canonical evidence",
                content_hash: "document-vector-evidence",
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap();
        insert_project(
            &mut conn,
            &NewProject {
                name: "Orchid",
                status: ProjectStatus::Active,
                summary: Some("semantic distilled memory"),
                participants_json: None,
                sources_json: None,
                confidence: 0.9,
                now: 2,
            },
            &[Provenance::new(event_id)],
        )
        .unwrap();
        let embedder = MockEmbedder::new(E5_SMALL_DIM);
        assert_eq!(embed_batch(&mut conn, &embedder, 10, 3).unwrap(), 1);
        let query = embedder.embed_query("semantic distilled memory").unwrap();
        assert_eq!(knn(&conn, &query, 10).unwrap().len(), 1);

        conn.execute(
            "UPDATE search_documents SET is_dirty=1,dirty_at_ms=4 WHERE source_table=?1",
            ["projects"],
        )
        .unwrap();

        assert!(knn(&conn, &query, 10).unwrap().is_empty());
        let metadata: i64 = conn
            .query_row("SELECT count(*) FROM search_document_embeddings", [], |row| row.get(0))
            .unwrap();
        assert_eq!(metadata, 0);
    }
}
