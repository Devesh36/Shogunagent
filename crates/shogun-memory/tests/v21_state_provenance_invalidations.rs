use std::{
    error::Error,
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::params;

fn current_unix_ms() -> Result<i64, Box<dyn Error>> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}

#[test]
fn state_provenance_update_and_delete_record_owner_and_wall_clock_invalidation(
) -> Result<(), Box<dyn Error>> {
    let conn = shogun_memory::open_in_memory()?;
    conn.execute(
        "INSERT INTO event_log
           (ts, source, kind, content, content_hash, last_seen_at)
         VALUES (1, 'capture', 'text', 'evidence', 'v21-state-provenance', 1)",
        [],
    )?;
    let event_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO people
           (display_name, confidence, created_at, updated_at, last_evidence_at,
            base_confidence)
         VALUES ('Alice', 0.8, 20, 20, 20, 0.8)",
        [],
    )?;
    let first_person = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO people
           (display_name, confidence, created_at, updated_at, last_evidence_at,
            base_confidence)
         VALUES ('Bob', 0.8, 41, 41, 41, 0.8)",
        [],
    )?;
    let second_person = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO state_provenance (state_table, state_id, event_id, weight)
         VALUES ('people', ?1, ?2, 1.0)",
        params![first_person, event_id],
    )?;
    conn.execute("DELETE FROM search_document_invalidations", [])?;

    let before_update = current_unix_ms()?;
    conn.execute(
        "UPDATE state_provenance SET state_id = ?1
          WHERE state_table = 'people' AND state_id = ?2",
        params![second_person, first_person],
    )?;

    let update_invalidations: Vec<(String, i64, i64)> = conn
        .prepare(
            "SELECT invalidation_kind, required_owner_updated_after_ms, invalidated_at_ms
               FROM search_document_invalidations
              WHERE source_table = 'people' AND source_id IN (?1, ?2)
              ORDER BY source_id",
        )?
        .query_map(
            params![first_person.to_string(), second_person.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(
        update_invalidations
            .iter()
            .map(|(kind, required, _)| (kind.as_str(), *required))
            .collect::<Vec<_>>(),
        vec![("evidence_changed", 20), ("evidence_changed", 41)]
    );
    let after_update = current_unix_ms()?;
    assert!(update_invalidations
        .iter()
        .all(|(_, _, invalidated_at_ms)| {
            *invalidated_at_ms >= before_update - 1_000 && *invalidated_at_ms <= after_update
        }));

    let before_delete = current_unix_ms()?;
    conn.execute(
        "DELETE FROM state_provenance WHERE state_table = 'people' AND state_id = ?1",
        [second_person],
    )?;

    let delete_invalidation: (String, i64, i64) = conn.query_row(
        "SELECT invalidation_kind, required_owner_updated_after_ms, invalidated_at_ms
               FROM search_document_invalidations
              WHERE source_table = 'people' AND source_id = ?1",
        [second_person.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    assert_eq!(
        (delete_invalidation.0.as_str(), delete_invalidation.1),
        ("evidence_deleted", 41)
    );
    let after_delete = current_unix_ms()?;
    assert!(
        delete_invalidation.2 >= before_delete - 1_000 && delete_invalidation.2 <= after_delete
    );
    Ok(())
}
