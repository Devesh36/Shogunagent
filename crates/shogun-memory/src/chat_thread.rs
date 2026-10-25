//! Rust-owned notch chat conversations (PR #303 TODO 4).

use rusqlite::{Connection, OptionalExtension, params};

/// Each conversation keeps a bounded tail. Retention is per thread, never global.
pub const KEEP_LAST: usize = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    pub role: String,
    pub text: String,
    pub citations: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatThread {
    pub id: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub messages: Vec<ChatMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatStore {
    pub active_thread_id: Option<i64>,
    pub threads: Vec<ChatThread>,
}

/// One message of the active thread's tail, with its position — the stable key the chat
/// prompt anchors its history window on (see `chat_prompt::anchored_window`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailMessage {
    pub pos: i64,
    pub message: ChatMessage,
}

/// The active thread's tail: which thread, and its last `n` messages oldest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveTail {
    pub thread_id: i64,
    pub messages: Vec<TailMessage>,
}

/// The last `n` messages of the active conversation, oldest first — one bounded query.
/// `None` when no conversation is active.
///
/// The chat prompt needs exactly this, not [`load`]: that reads **every** thread and every
/// message on each turn to then keep forty rows of one thread, which grows with the life of the
/// install and sits on the press path of every question.
pub fn active_tail(conn: &Connection, n: usize) -> Result<Option<ActiveTail>, rusqlite::Error> {
    let active: Option<i64> = conn
        .query_row("SELECT active_thread_id FROM chat_state WHERE id = 1", [], |row| row.get(0))
        .optional()?
        .flatten();
    let Some(thread_id) = active else {
        return Ok(None);
    };
    let mut stmt = conn.prepare(
        "SELECT pos, role, text, citations
           FROM chat_messages
          WHERE thread_id = ?1
          ORDER BY pos DESC
          LIMIT ?2",
    )?;
    let mut messages = stmt
        .query_map(params![thread_id, n as i64], |row| {
            Ok(TailMessage {
                pos: row.get(0)?,
                message: ChatMessage { role: row.get(1)?, text: row.get(2)?, citations: row.get(3)? },
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    messages.reverse();
    Ok(Some(ActiveTail { thread_id, messages }))
}

/// Load every conversation, newest first, with messages oldest first.
pub fn load(conn: &Connection) -> Result<ChatStore, rusqlite::Error> {
    let active_thread_id = conn
        .query_row(
            "SELECT active_thread_id FROM chat_state WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let mut thread_stmt = conn.prepare(
        "SELECT id, created_at_ms, updated_at_ms
           FROM chat_threads
          ORDER BY updated_at_ms DESC, id DESC",
    )?;
    let metadata = thread_stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(thread_stmt);

    let mut message_stmt = conn.prepare(
        "SELECT role, text, citations
           FROM chat_messages
          WHERE thread_id = ?1
          ORDER BY pos",
    )?;
    let mut threads = Vec::with_capacity(metadata.len());
    for (id, created_at_ms, updated_at_ms) in metadata {
        let messages = message_stmt
            .query_map([id], |row| {
                Ok(ChatMessage {
                    role: row.get(0)?,
                    text: row.get(1)?,
                    citations: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        threads.push(ChatThread {
            id,
            created_at_ms,
            updated_at_ms,
            messages,
        });
    }
    Ok(ChatStore {
        active_thread_id,
        threads,
    })
}

/// Start and select a new empty conversation.
pub fn create(conn: &mut Connection, now_ms: i64) -> Result<ChatStore, rusqlite::Error> {
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO chat_threads (created_at_ms, updated_at_ms) VALUES (?1, ?1)",
        [now_ms],
    )?;
    let id = tx.last_insert_rowid();
    tx.execute(
        "UPDATE chat_state SET active_thread_id = ?1 WHERE id = 1",
        [id],
    )?;
    let store = load(&tx)?;
    tx.commit()?;
    Ok(store)
}

/// Select an existing conversation. Missing IDs fail instead of creating phantom state.
pub fn select(conn: &mut Connection, thread_id: i64) -> Result<ChatStore, rusqlite::Error> {
    let tx = conn.transaction()?;
    let changed = tx.execute(
        "UPDATE chat_state
            SET active_thread_id = ?1
          WHERE id = 1 AND EXISTS (SELECT 1 FROM chat_threads WHERE id = ?1)",
        [thread_id],
    )?;
    if changed != 1 {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    }
    let store = load(&tx)?;
    tx.commit()?;
    Ok(store)
}

/// Append one committed message and trim only this conversation's oldest tail.
pub fn append(conn: &mut Connection, thread_id: i64, message: &ChatMessage, now_ms: i64) -> Result<(), rusqlite::Error> {
    append_position(conn, thread_id, message, now_ms).map(|_| ())
}

/// Return the committed position so ephemeral attachments cannot bind to a concurrent append.
pub fn append_position(
    conn: &mut Connection,
    thread_id: i64,
    message: &ChatMessage,
    now_ms: i64,
) -> Result<i64, rusqlite::Error> {
    let tx = conn.transaction()?;
    let next_pos: i64 = tx.query_row(
        "SELECT COALESCE(MAX(pos) + 1, 0) FROM chat_messages WHERE thread_id = ?1",
        [thread_id],
        |row| row.get(0),
    )?;
    // `ts` = now_ms: stamped here rather than trusted from the webview, so the windowed
    // privacy delete (`maintenance::delete_since`) can reach the transcript.
    tx.execute(
        "INSERT INTO chat_messages (thread_id, pos, role, text, citations, ts)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            thread_id,
            next_pos,
            message.role,
            message.text,
            message.citations,
            now_ms
        ],
    )?;
    tx.execute(
        "DELETE FROM chat_messages
          WHERE thread_id = ?1
            AND pos NOT IN (
                SELECT pos FROM chat_messages
                 WHERE thread_id = ?1
                 ORDER BY pos DESC
                 LIMIT ?2
            )",
        params![thread_id, KEEP_LAST as i64],
    )?;
    let changed = tx.execute(
        "UPDATE chat_threads SET updated_at_ms = ?2 WHERE id = ?1",
        params![thread_id, now_ms],
    )?;
    if changed != 1 {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    }
    tx.commit()?;
    Ok(next_pos)
}

/// Delete exactly one conversation. Foreign-key cascade removes only its messages.
pub fn delete(conn: &mut Connection, thread_id: i64) -> Result<ChatStore, rusqlite::Error> {
    let tx = conn.transaction()?;
    let changed = tx.execute("DELETE FROM chat_threads WHERE id = ?1", [thread_id])?;
    if changed != 1 {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    }
    let store = load(&tx)?;
    tx.commit()?;
    Ok(store)
}

/// The only roles the schema accepts. A row carrying anything else cannot be stored, and one
/// such row must not cost the user the rest of the transcript.
fn is_storable(message: &ChatMessage) -> bool {
    matches!(message.role.as_str(), "me" | "shogun")
}

/// Import a pre-Rust `localStorage` transcript once. Existing persisted messages always win.
///
/// The webview JSON is unvalidated user-writable storage, so rows the schema would reject are
/// SKIPPED rather than aborting the import: a single bad row would otherwise fail every launch's
/// import (the caller keeps `shogun.msgs` until this succeeds), leaving chat disabled forever.
pub fn import_legacy(
    conn: &mut Connection,
    messages: &[ChatMessage],
    now_ms: i64,
) -> Result<ChatStore, rusqlite::Error> {
    let messages: Vec<&ChatMessage> = messages.iter().filter(|m| is_storable(m)).collect();
    let tx = conn.transaction()?;
    let existing: i64 = tx.query_row("SELECT count(*) FROM chat_messages", [], |row| row.get(0))?;
    if existing == 0 && !messages.is_empty() {
        tx.execute(
            "INSERT INTO chat_threads (created_at_ms, updated_at_ms) VALUES (?1, ?1)",
            [now_ms],
        )?;
        let thread_id = tx.last_insert_rowid();
        tx.execute(
            "UPDATE chat_state SET active_thread_id = ?1 WHERE id = 1",
            [thread_id],
        )?;
        let skip = messages.len().saturating_sub(KEEP_LAST);
        // Legacy rows carry no timestamp; stamping the import time over-includes them in a later
        // windowed delete, which errs on the privacy-safe side.
        let mut stmt = tx.prepare(
            "INSERT INTO chat_messages (thread_id, pos, role, text, citations, ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for (pos, message) in messages.into_iter().skip(skip).enumerate() {
            stmt.execute(params![
                thread_id,
                pos as i64,
                message.role,
                message.text,
                message.citations,
                now_ms
            ])?;
        }
        drop(stmt);
    }
    let store = load(&tx)?;
    tx.commit()?;
    Ok(store)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, text: &str) -> ChatMessage {
        ChatMessage {
            role: role.into(),
            text: text.into(),
            citations: None,
        }
    }

    /// The windowed privacy delete reaches the transcript (#303): a message
    /// appended inside the window goes with the events it quoted; older rows stay, and a thread
    /// emptied by the delete is pruned rather than left as a ghost.
    #[test]
    fn delete_since_reaches_the_thread() {
        let mut conn = crate::open_in_memory().expect("migrated database");
        let old = create(&mut conn, 1_000).expect("old thread").active_thread_id.unwrap();
        append(&mut conn, old, &msg("me", "yesterday"), 1_000).expect("old message");
        let recent = create(&mut conn, 5_000).expect("recent thread").active_thread_id.unwrap();
        append(&mut conn, recent, &msg("me", "just now"), 5_000).expect("recent message");

        crate::maintenance::delete_since(&mut conn, 3_000).expect("delete_since");

        let store = load(&conn).expect("load");
        assert_eq!(store.threads.len(), 1, "emptied thread is pruned");
        assert_eq!(store.threads[0].id, old);
        assert_eq!(store.threads[0].messages[0].text, "yesterday");
        assert_eq!(store.active_thread_id, None, "FK cleared the dangling active id");
    }

    #[test]
    fn deleting_one_thread_preserves_other_threads() {
        let mut conn = crate::open_in_memory().expect("migrated database");
        let first = create(&mut conn, 10)
            .expect("first thread")
            .active_thread_id
            .unwrap();
        append(&mut conn, first, &msg("me", "first"), 11).expect("first message");
        let second = create(&mut conn, 20)
            .expect("second thread")
            .active_thread_id
            .unwrap();
        append(&mut conn, second, &msg("me", "second"), 21).expect("second message");

        let store = delete(&mut conn, second).expect("delete second only");

        assert_eq!(store.active_thread_id, None);
        assert_eq!(store.threads.len(), 1);
        assert_eq!(store.threads[0].id, first);
        assert_eq!(store.threads[0].messages[0].text, "first");
    }

    #[test]
    fn append_keeps_a_tail_per_thread() {
        let mut conn = crate::open_in_memory().expect("migrated database");
        let first = create(&mut conn, 10)
            .expect("first thread")
            .active_thread_id
            .unwrap();
        for i in 0..KEEP_LAST + 7 {
            append(
                &mut conn,
                first,
                &msg("me", &format!("m{i}")),
                i as i64 + 11,
            )
            .expect("append");
        }
        let second = create(&mut conn, 100)
            .expect("second thread")
            .active_thread_id
            .unwrap();
        append(&mut conn, second, &msg("me", "other"), 101).expect("other thread");

        let store = load(&conn).expect("load");
        let retained = store
            .threads
            .iter()
            .find(|thread| thread.id == first)
            .unwrap();
        assert_eq!(retained.messages.len(), KEEP_LAST);
        assert_eq!(retained.messages[0].text, "m7");
        assert_eq!(
            store
                .threads
                .iter()
                .find(|thread| thread.id == second)
                .unwrap()
                .messages
                .len(),
            1
        );
    }

    /// A row the schema cannot store (an unknown role in hand-editable webview JSON) is skipped,
    /// not fatal: aborting the import would fail again on every launch — the caller keeps
    /// `shogun.msgs` until it succeeds — and leave chat disabled for good.
    #[test]
    fn legacy_import_skips_rows_the_schema_rejects() {
        let mut conn = crate::open_in_memory().expect("migrated database");

        let store = import_legacy(
            &mut conn,
            &[
                msg("me", "kept"),
                msg("assistant", "from some other build"),
                msg("shogun", "also kept"),
            ],
            10,
        )
        .expect("import survives the unusable row");

        let messages = &store.threads[0].messages;
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text, "kept");
        assert_eq!(messages[1].text, "also kept");
    }

    /// Nothing storable means nothing imported — and no empty thread left behind for it.
    #[test]
    fn legacy_import_of_only_unusable_rows_creates_no_thread() {
        let mut conn = crate::open_in_memory().expect("migrated database");

        let store = import_legacy(&mut conn, &[msg("assistant", "unusable")], 10).expect("import");

        assert!(store.threads.is_empty());
        assert_eq!(store.active_thread_id, None);
    }

    #[test]
    fn legacy_import_is_idempotent_and_never_overwrites_persisted_chat() {
        let mut conn = crate::open_in_memory().expect("migrated database");
        let imported = import_legacy(&mut conn, &[msg("me", "legacy")], 10).expect("import");
        let imported_id = imported.active_thread_id.unwrap();

        let store = import_legacy(&mut conn, &[msg("me", "stale copy")], 20).expect("retry");

        assert_eq!(store.threads.len(), 1);
        assert_eq!(store.threads[0].id, imported_id);
        assert_eq!(store.threads[0].messages[0].text, "legacy");
    }

    #[test]
    fn selecting_a_missing_thread_fails_without_changing_active_thread() {
        let mut conn = crate::open_in_memory().expect("migrated database");
        let active = create(&mut conn, 10).expect("thread").active_thread_id;

        assert!(select(&mut conn, 999).is_err());
        assert_eq!(load(&conn).expect("load").active_thread_id, active);
    }

    #[test]
    fn active_tail_reads_only_the_active_thread_newest_n_oldest_first() {
        let mut conn = crate::open_in_memory().unwrap();
        let first = create(&mut conn, 1).unwrap().active_thread_id.unwrap();
        for i in 0..5 {
            append(&mut conn, first, &msg("me", &format!("old {i}")), 10 + i).unwrap();
        }
        let second = create(&mut conn, 100).unwrap().active_thread_id.unwrap();
        assert_ne!(first, second);
        for i in 0..5 {
            append(&mut conn, second, &msg(if i % 2 == 0 { "me" } else { "shogun" }, &format!("new {i}")), 200 + i).unwrap();
        }
        let tail = active_tail(&conn, 3).unwrap().unwrap();
        assert_eq!(tail.thread_id, second);
        let texts: Vec<&str> = tail.messages.iter().map(|m| m.message.text.as_str()).collect();
        assert_eq!(texts, vec!["new 2", "new 3", "new 4"]);
        let pos: Vec<i64> = tail.messages.iter().map(|m| m.pos).collect();
        assert!(pos.windows(2).all(|w| w[0] < w[1]), "positions ascend with the messages: {pos:?}");
        assert!(active_tail(&conn, 100).unwrap().unwrap().messages.iter().all(|m| m.message.text.starts_with("new")));
    }
}
