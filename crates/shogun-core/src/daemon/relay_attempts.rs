//! Content-free relay attempts for callers without an existing domain job ledger.
use super::*;

impl Db {
    /// Reuse an unfinished attempt; start a new identity only after confirmed terminal work.
    pub fn relay_attempt(&self, fingerprint: &str) -> Result<(String, Option<String>), String> {
        self.with_conn("relay_attempt.prepare", |conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute("INSERT INTO relay_attempts(fingerprint,request_id) VALUES (?1,?2) ON CONFLICT(fingerprint) DO UPDATE SET request_id=lower(hex(randomblob(16))), provider_id=NULL, finished=0 WHERE finished=1", rusqlite::params![fingerprint, format!("payload-{fingerprint}")])?;
            let attempt = tx.query_row("SELECT request_id,provider_id FROM relay_attempts WHERE fingerprint=?1", [fingerprint], |r| Ok((r.get(0)?, r.get(1)?)))?;
            tx.commit()?;
            Ok::<_, rusqlite::Error>(attempt)
        }).map_err(|_| "relay attempt unavailable".to_string())
    }

    /// Compare the attempt identity so a late worker cannot update its replacement.
    pub fn relay_attempt_bind(
        &self,
        fingerprint: &str,
        request_id: &str,
        provider_id: &str,
    ) -> Result<(), String> {
        self.with_conn("relay_attempt.bind", |conn| conn.execute("UPDATE relay_attempts SET provider_id=?3 WHERE fingerprint=?1 AND request_id=?2 AND finished=0", rusqlite::params![fingerprint, request_id, provider_id]))
            .map(|_| ()).map_err(|_| "relay attempt binding unavailable".to_string())
    }

    pub fn relay_attempt_finish(&self, fingerprint: &str, request_id: &str) -> Result<(), String> {
        self.with_conn("relay_attempt.finish", |conn| {
            conn.execute(
                "UPDATE relay_attempts SET finished=1 WHERE fingerprint=?1 AND request_id=?2",
                rusqlite::params![fingerprint, request_id],
            )
        })
        .map(|_| ())
        .map_err(|_| "relay attempt completion unavailable".to_string())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::Arc;

    #[test]
    fn persisted_unknown_attempt_survives_restart_and_terminal_retry_fences_old_worker() {
        let path =
            std::env::temp_dir().join(format!("shogun-relay-attempt-{}.db", std::process::id()));
        let db = Db::open(&path, Arc::new(|| 100)).unwrap();
        let first = db.relay_attempt("same-payload").unwrap();
        let mail_items = vec![("thread".into(), "thread".into())];
        let mail_id = db
            .mail_batch_claim_attempt("mail-fingerprint", &mail_items, 0, 10)
            .unwrap()
            .unwrap();
        drop(db);
        let db = Db::open(&path, Arc::new(|| 100)).unwrap();
        assert_eq!(first, db.relay_attempt("same-payload").unwrap());
        assert_eq!(db.mail_batch_unapplied()[0].0, format!("request:{mail_id}"));
        assert!(db
            .mail_batch_claim_attempt("mail-fingerprint", &mail_items, 0, 10)
            .unwrap()
            .is_none());
        db.relay_attempt_bind("same-payload", &first.0, "original-job")
            .unwrap();
        assert_eq!(
            db.relay_attempt("same-payload").unwrap().1.as_deref(),
            Some("original-job")
        );
        db.relay_attempt_finish("same-payload", &first.0).unwrap();
        let second = db.relay_attempt("same-payload").unwrap();
        assert_ne!(first.0, second.0);
        assert!(second.1.is_none());
        db.relay_attempt_bind("same-payload", &first.0, "late-old-job")
            .unwrap();
        db.relay_attempt_finish("same-payload", &first.0).unwrap();
        assert_eq!(db.relay_attempt("same-payload").unwrap(), second);
        drop(db);
        std::fs::remove_file(path).unwrap();
    }
}
