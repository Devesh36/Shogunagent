//! Durable, at-most-once crash recap attempts. Never changes transcript or note text.
use rusqlite::Connection;

/// Close all abandoned sessions; enqueue recap recovery only for meetings and calls.
pub fn close_abandoned(conn: &mut Connection, cutoff: i64, now: i64) -> rusqlite::Result<Vec<i64>> {
    let tx = conn.transaction()?;
    let ids: Vec<i64> = {
        let mut stmt = tx.prepare("SELECT id FROM sessions WHERE ended_at IS NULL AND started_at < ?1")?;
        let rows = stmt.query_map([cutoff], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for id in &ids {
        tx.execute("INSERT OR IGNORE INTO meeting_recovery(session_id) SELECT id FROM sessions WHERE id = ?1 AND kind IN ('meeting', 'call')", [id])?;
        tx.execute("UPDATE sessions SET ended_at = MAX(started_at, COALESCE((SELECT MAX(ts) FROM transcript_segments WHERE session_id = ?1), started_at), COALESCE((SELECT updated_at FROM session_notes WHERE session_id = ?1), started_at)), updated_at = ?2 WHERE id = ?1", rusqlite::params![id, now])?;
    }
    tx.commit()?;
    Ok(ids)
}

/// Claim before spawning paid work. A second launch never submits the same recovery again.
pub fn claim(conn: &mut Connection) -> rusqlite::Result<Vec<i64>> {
    let tx = conn.transaction()?;
    let ids = {
        let mut stmt = tx.prepare("SELECT r.session_id FROM meeting_recovery r JOIN sessions s ON s.id = r.session_id WHERE r.attempted = 0 AND s.ended_at IS NOT NULL AND NOT EXISTS (SELECT 1 FROM meeting_recaps m WHERE m.session_id = r.session_id)")?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for id in &ids {
        tx.execute("UPDATE meeting_recovery SET attempted = 1 WHERE session_id = ?1", [id])?;
    }
    tx.commit()?;
    Ok(ids)
}

/// The notch pulls after its listener mounts, so setup-time recovery cannot lose the notice.
pub fn notices(conn: &mut Connection) -> rusqlite::Result<Vec<Option<String>>> {
    let tx = conn.transaction()?;
    let titles = {
        let mut stmt = tx.prepare("SELECT s.title FROM meeting_recovery r JOIN sessions s ON s.id = r.session_id WHERE r.notified = 0 AND r.attempted = 1")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    tx.execute("UPDATE meeting_recovery SET notified = 1 WHERE attempted = 1", [])?;
    tx.commit()?;
    Ok(titles)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recovery_is_durable_and_claimed_only_once() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE sessions(id INTEGER PRIMARY KEY, started_at INTEGER, ended_at INTEGER, updated_at INTEGER, title TEXT, kind TEXT); CREATE TABLE session_notes(session_id INTEGER, updated_at INTEGER, body TEXT); CREATE TABLE transcript_segments(session_id INTEGER, ts INTEGER); CREATE TABLE meeting_recaps(session_id INTEGER); INSERT INTO sessions VALUES(1,100,NULL,100,'Planning','meeting'),(2,900,NULL,900,'Current','meeting'),(3,100,NULL,100,'Focus','focus'),(4,100,NULL,100,'Call','call'),(5,800,NULL,800,'At cutoff','focus'); INSERT INTO transcript_segments VALUES(1,450),(3,300); INSERT INTO session_notes VALUES(3,500,'Focus notes');").unwrap();
        conn.execute_batch(include_str!("migrations/V89__meeting_recovery.sql")).unwrap();
        assert_eq!(close_abandoned(&mut conn, 800, 1000).unwrap(), vec![1, 3, 4]);
        assert_eq!(claim(&mut conn).unwrap(), vec![1, 4]);
        assert!(close_abandoned(&mut conn, 800, 1100).unwrap().is_empty());
        assert!(claim(&mut conn).unwrap().is_empty());
        assert_eq!(notices(&mut conn).unwrap(), vec![Some("Planning".into()), Some("Call".into())]);
        assert!(notices(&mut conn).unwrap().is_empty());
        assert_eq!(conn.query_row("SELECT ended_at FROM sessions WHERE id=1", [], |r| r.get::<_, i64>(0)).unwrap(), 450);
        assert_eq!(conn.query_row("SELECT ended_at FROM sessions WHERE id=3", [], |r| r.get::<_, i64>(0)).unwrap(), 500);
        assert_eq!(conn.query_row("SELECT ended_at FROM sessions WHERE id=4", [], |r| r.get::<_, i64>(0)).unwrap(), 100);
        assert_eq!(conn.query_row("SELECT COUNT(*) FROM sessions WHERE id IN (2,5) AND ended_at IS NULL", [], |r| r.get::<_, i64>(0)).unwrap(), 2);
        assert_eq!(conn.query_row("SELECT COUNT(*) FROM meeting_recovery WHERE session_id=3", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    }
    #[test]
    fn existing_minutes_are_never_overwritten_and_notes_survive_recovery() {
        let mut conn = crate::open_in_memory().unwrap();
        let id = crate::session::open(&conn, &crate::session::NewSession {
            kind: "meeting", started_at: 100, title: Some("Interrupted"), app_bundle_id: None,
            calendar_occurrence_id: None, confidence: 0.9, provenance: "{}",
        }).unwrap();
        crate::session_notes::save(&conn, id, "User notes", 450).unwrap();
        conn.execute("INSERT INTO meeting_recaps(session_id, summary, decisions, next_actions, model, created_at) VALUES(?1, 'Keep existing minutes', '[]', '[]', 'test', 100)", [id]).unwrap();
        assert_eq!(close_abandoned(&mut conn, 800, 1000).unwrap(), vec![id]);
        assert!(claim(&mut conn).unwrap().is_empty());
        assert_eq!(crate::session_notes::get(&conn, id).unwrap().as_deref(), Some("User notes"));
        assert_eq!(crate::session::get(&conn, id).unwrap().unwrap().ended_at, Some(450));
    }

}
