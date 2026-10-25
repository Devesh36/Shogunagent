//! Why a headless face (the bundled `shogun-mcp` / `shogun-api`) refuses memory calls (#800).
//!
//! Only the desktop app upgrades the memory store, because only it takes the verified
//! pre-migration snapshot first. A headless face opens the store without migrating and serves
//! memory only while the store's schema is exactly the one this binary was built for; otherwise
//! every memory call gets one of these stable refusals (MCP tool error / REST 503 / CLI message).
//! The faces re-check on the next call, so the app upgrading the store needs no restart here.

use crate::memory_api::Tool;

/// Shown when the store is older than this binary (or not initialised yet).
pub const UPDATE_PENDING_MESSAGE: &str =
    "ShogunAI needs to finish updating your memory. Open the ShogunAI app once, then try again.";
/// Shown when a newer SHOGUN already upgraded the store past this binary: usually this process
/// was started before the app updated itself (the binary on disk is already new), so restarting
/// the connection is the fix; otherwise the app itself was downgraded.
pub const NEWER_THAN_APP_MESSAGE: &str = "Your memory was upgraded by a newer ShogunAI. Restart this \
app's ShogunAI connection (or update ShogunAI) to continue.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryUnavailable {
    /// `memory_update_pending`: the app has not migrated the store to this version yet.
    UpdatePending,
    /// `memory_newer_than_app`: the store is ahead of this binary (a process started before the
    /// app updated, or a downgrade).
    NewerThanApp,
}

impl MemoryUnavailable {
    /// The refusal for a store at `version` when this binary expects `latest`; `None` = serve.
    pub fn for_schema(version: Option<u32>, latest: u32) -> Option<Self> {
        match version {
            Some(v) if v == latest => None,
            Some(v) if v > latest => Some(Self::NewerThanApp),
            _ => Some(Self::UpdatePending),
        }
    }

    /// The stable machine code.
    pub fn code(self) -> &'static str {
        match self {
            Self::UpdatePending => "memory_update_pending",
            Self::NewerThanApp => "memory_newer_than_app",
        }
    }

    /// The user-facing sentence.
    pub fn message(self) -> &'static str {
        match self {
            Self::UpdatePending => UPDATE_PENDING_MESSAGE,
            Self::NewerThanApp => NEWER_THAN_APP_MESSAGE,
        }
    }

    /// `"<code>: <message>"`: the start-up line and the MCP error message.
    pub fn line(self) -> String {
        format!("{}: {}", self.code(), self.message())
    }

    /// The REST 503 body: `{"error":<code>,"message":<sentence>}`.
    pub fn rest_body(self) -> String {
        serde_json::json!({ "error": self.code(), "message": self.message() }).to_string()
    }
}

/// Does this tool need the memory store, i.e. is it refused while the store is unavailable? One
/// predicate for both faces so they cannot drift: REST gates the Read / Write / Action routes
/// (every tool endpoint) and leaves approval polling (`/v1/actions/status/{id}`) open; MCP gates
/// the same tools and leaves `actions.status` open. Polling reads the approval queue, not memory,
/// so an L3 request submitted before the store went pending can still be followed to its outcome.
pub fn tool_needs_store(tool: Tool) -> bool {
    tool != Tool::ActionsStatus
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_exact_version_serves() {
        assert_eq!(MemoryUnavailable::for_schema(Some(67), 67), None);
        assert_eq!(MemoryUnavailable::for_schema(Some(66), 67), Some(MemoryUnavailable::UpdatePending));
        assert_eq!(MemoryUnavailable::for_schema(None, 67), Some(MemoryUnavailable::UpdatePending));
        assert_eq!(MemoryUnavailable::for_schema(Some(68), 67), Some(MemoryUnavailable::NewerThanApp));
    }

    #[test]
    fn only_approval_polling_skips_the_store() {
        assert!(!tool_needs_store(Tool::ActionsStatus));
        assert!(tool_needs_store(Tool::ActionsExecute));
        assert!(tool_needs_store(Tool::MemoryAppendNote));
    }
}
