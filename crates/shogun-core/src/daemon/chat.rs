//! Persisted notch chat: threads, their rolling message tails, and the active selection.

use super::*;

impl Db {
    /// Load every persisted notch chat without collapsing a storage failure into an empty list.
    pub fn chat_threads(&self) -> MemoryResult<shogun_memory::chat_thread::ChatStore> {
        self.with_conn("chat.load", shogun_memory::chat_thread::load)
    }

    /// The active conversation's last `n` messages, oldest first (the chat prompt's history).
    pub fn chat_active_tail(
        &self,
        n: usize,
    ) -> MemoryResult<Option<shogun_memory::chat_thread::ActiveTail>> {
        self.with_conn("chat.active_tail", |conn| shogun_memory::chat_thread::active_tail(conn, n))
    }

    /// Start and select a new empty conversation.
    pub fn chat_thread_create(&self) -> MemoryResult<shogun_memory::chat_thread::ChatStore> {
        let now_ms = self.now_ms();
        self.with_conn_mut("chat.create", |conn| {
            shogun_memory::chat_thread::create(conn, now_ms)
        })
    }

    /// Select one persisted conversation.
    pub fn chat_thread_select(
        &self,
        thread_id: i64,
    ) -> MemoryResult<shogun_memory::chat_thread::ChatStore> {
        self.with_conn_mut("chat.select", |conn| {
            shogun_memory::chat_thread::select(conn, thread_id)
        })
    }

    /// Append one committed message. Each thread owns its own bounded tail.
    pub fn chat_message_append(
        &self,
        thread_id: i64,
        message: &shogun_memory::chat_thread::ChatMessage,
    ) -> MemoryResult<()> {
        let now_ms = self.now_ms();
        self.with_conn_mut("chat.append", |conn| {
            shogun_memory::chat_thread::append(conn, thread_id, message, now_ms)
        })
    }

    /// Append and return the durable message key for the RAM-only image sidecar.
    pub fn chat_message_append_position(
        &self, thread_id: i64, message: &shogun_memory::chat_thread::ChatMessage,
    ) -> MemoryResult<i64> {
        let now_ms = self.now_ms();
        self.with_conn_mut("chat.append", |conn| {
            shogun_memory::chat_thread::append_position(conn, thread_id, message, now_ms)
        })
    }

    /// Delete one conversation and return the remaining canonical store.
    pub fn chat_thread_delete(
        &self,
        thread_id: i64,
    ) -> MemoryResult<shogun_memory::chat_thread::ChatStore> {
        self.with_conn_mut("chat.delete", |conn| {
            shogun_memory::chat_thread::delete(conn, thread_id)
        })
    }

    /// One-time import for the pre-Rust webview transcript. Existing DB content always wins.
    pub fn chat_thread_import_legacy(
        &self,
        messages: &[shogun_memory::chat_thread::ChatMessage],
    ) -> MemoryResult<shogun_memory::chat_thread::ChatStore> {
        let now_ms = self.now_ms();
        self.with_conn_mut("chat.import_legacy", |conn| {
            shogun_memory::chat_thread::import_legacy(conn, messages, now_ms)
        })
    }
}
