//! Shared user-memory operations, owned by the Rust database core.
use super::Db;
use serde_json::{json, Value};
use shogun_mcp::memory_notes_api::NoteChange;
use shogun_memory::memory_notes;
impl Db {
    pub fn memory_notes_list(&self, id: Option<i64>) -> Result<Value, String> {
        self.with_conn("memory_notes.list", |c| match id {
            Some(id) => Ok::<_, shogun_memory::MemoryError>(
                json!({"note":memory_notes::get(c,id)?,"history":memory_notes::history(c,id)?}),
            ),
            None => Ok(json!({"notes":memory_notes::list(c)?})),
        })
        .map_err(|_| "memory_unavailable".to_string())
    }
    /// Newest user notes still remembered (short ones, a small total budget), for
    /// `memory.get_context`'s `note:` lines.
    pub fn recent_note_texts(&self) -> Vec<String> {
        self.with_conn("memory_notes.recent", |c| memory_notes::recent_context(c, 4096))
            .map(|notes| notes.into_iter().map(|n| n.text).filter(|t| !super::auto_memory::reads_as_reply(t)).collect())
            .unwrap_or_default()
    }
    pub fn memory_notes_change(&self, change: NoteChange) -> Result<Value, String> {
        let now = self.now_ms();
        self.with_conn_mut("memory_notes.change", |c| match change {
            NoteChange::Save { text } => {
                memory_notes::change(c, "save", None, None, Some(&text), now)
            }
            NoteChange::Update { id, revision, text } => {
                memory_notes::change(c, "update", Some(id), Some(revision), Some(&text), now)
            }
            NoteChange::Forget { id, revision } => {
                memory_notes::change(c, "forget", Some(id), Some(revision), None, now)
            }
            NoteChange::Undo { id, revision } => {
                memory_notes::change(c, "undo", Some(id), Some(revision), None, now)
            }
        })
        .map_err(|_| "memory_unavailable".to_string())
    }
}
