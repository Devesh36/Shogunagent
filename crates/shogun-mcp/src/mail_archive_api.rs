//! `mail.triage.archive` / `mail.triage.unarchive` over the API faces (issue #779).
//!
//! Archiving a Gmail thread is a reversible change to the user's own mailbox (L2, FR-TG-21). In
//! the desktop's Mail pane the user's own click is the one tap. An MCP / REST / CLI caller is not
//! the user: a client steered by text in an email it read could archive the whole Inbox. So the
//! same L2 needs the user's tap here too (FR-AG-04 / invariant 6: an AI call gets the same
//! approval as the UI):
//! - When the thread's label carries the user's **standing approval** (`auto_archive`, granted
//!   in Settings, FR-TG-21), the user has already said yes for that label. The request is
//!   recorded directly (`archive_status = pending`) and the desktop's write-back tick runs it.
//!   The caller sees `{"accepted":true,"pending":true,"level":"L2"}`.
//! - Otherwise the request lands in the shared approval queue as
//!   [`SendAction::ArchiveMailThread`](shogun_agents::permission::SendAction::ArchiveMailThread)
//!   and the caller gets the pending envelope with an `approval_id` (`"level":"L2"`; poll
//!   `actions.status`). Nothing is recorded until the user confirms in the app; the desktop then
//!   records it (re-checking the thread) and the tick runs it.
//! - An unarchive always waits for the user: no standing approval covers putting mail back.
//!   Withdrawing an archive that has not run yet changes nothing on Gmail and stays direct.
//!
//! The backend owns the triage rows, so it classifies ([`MemoryBackend::mail_archive_change`]).
//! Its direct write path (`shogun-core`'s `DbBackend`) refuses anything that needs approval, so a
//! face that forgot to route a request here fails closed instead of changing Gmail.

use shogun_agents::approval::{mail_archive_request, ApprovalOrigin, ApprovalQueue};
use shogun_agents::permission::Level;

use crate::backend::MemoryBackend;
use crate::memory_api::Tool;
use crate::rest::ApprovalSurface;

/// One API caller's archive or unarchive request that needs the user's confirm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailArchiveRequest {
    /// The triage key (`gmail:<thread id>`).
    pub thread_key: String,
    /// `true` for `mail.triage.unarchive` (put the thread back in the Inbox).
    pub unarchive: bool,
}

/// The two Gmail archive writes whose path depends on the thread and its label.
pub fn is_archive_write(tool: Tool) -> bool {
    matches!(tool, Tool::MailTriageArchive | Tool::MailTriageUnarchive)
}

/// The approval a body needs, if any: `Some` only when the backend says the request would change
/// Gmail without the user's approval. `None` for every other tool, for a request the backend may
/// record directly, and for anything it cannot classify (an unknown thread, a bad body); those go
/// to the backend's write, which answers with its own reason and refuses anything that needs
/// approval.
pub fn needs_approval<B: MemoryBackend + ?Sized>(
    backend: &B,
    tool: Tool,
    body: Option<&str>,
) -> Option<MailArchiveRequest> {
    if !is_archive_write(tool) {
        return None;
    }
    backend.mail_archive_change(tool, body?).ok().flatten()
}

/// Queue a request for the user's one-tap confirm. Returns the HTTP status and JSON body; the
/// MCP face returns the same JSON as its text result. 202 +
/// `{"pending":true,"approval_id":…,"level":"L2"}` on success. Nothing is recorded here.
pub fn request(
    request: MailArchiveRequest,
    now_ms: i64,
    approvals: &mut ApprovalQueue,
    origin: ApprovalOrigin,
    surface: ApprovalSurface,
) -> (u16, String) {
    let (action, preview) = mail_archive_request(&request.thread_key, request.unarchive);
    crate::rest::enqueue_at(action, preview, now_ms, approvals, origin, surface, Level::L2)
}

/// A backend that holds triage rows in memory and behaves like the real one for these tools: it
/// classifies against each thread's state and label, and its write refuses anything that needs
/// approval. Shared by the face tests.
#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::MailArchiveRequest;
    use crate::backend::{MemoryBackend, ReadItem, ReadParams, WriteResult};
    use crate::memory_api::Tool;

    /// A thread: whether it is archived on Gmail, whether a request is recorded, and whether its
    /// label carries the standing `auto_archive` approval.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct Thread {
        pub(crate) archived: bool,
        pub(crate) requested: bool,
        pub(crate) standing: bool,
    }

    pub(crate) struct FakeMail {
        pub(crate) threads: Mutex<HashMap<String, Thread>>,
        pub(crate) writes: Mutex<usize>,
    }

    impl FakeMail {
        pub(crate) fn with(threads: &[(&str, Thread)]) -> Self {
            Self {
                threads: Mutex::new(threads.iter().map(|(k, t)| (k.to_string(), *t)).collect()),
                writes: Mutex::new(0),
            }
        }

        pub(crate) fn thread(&self, key: &str) -> Option<Thread> {
            self.threads.lock().unwrap().get(key).copied()
        }

        fn key(body: &str) -> Result<String, String> {
            let value: serde_json::Value =
                serde_json::from_str(body).map_err(|_| "invalid mail request".to_string())?;
            value["thread_key"]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| "expected thread_key".to_string())
        }
    }

    impl MemoryBackend for FakeMail {
        fn read(&self, _tool: Tool, _params: &ReadParams) -> Vec<ReadItem> {
            Vec::new()
        }

        fn mail_archive_change(
            &self,
            tool: Tool,
            body: &str,
        ) -> Result<Option<MailArchiveRequest>, String> {
            let key = Self::key(body)?;
            let thread = self.thread(&key).ok_or_else(|| "thread not found".to_string())?;
            let unarchive = tool == Tool::MailTriageUnarchive;
            if !unarchive && thread.archived {
                return Err("invalid archive request: thread is already archived".to_string());
            }
            if unarchive && !thread.archived && !thread.requested {
                return Err("invalid unarchive request: thread is not archived".to_string());
            }
            let direct = thread.requested || (!unarchive && thread.standing);
            Ok((!direct).then_some(MailArchiveRequest { thread_key: key, unarchive }))
        }

        fn write(&self, tool: Tool, body: &str) -> WriteResult {
            if self.mail_archive_change(tool, body)?.is_some() {
                return Err("invalid archive request: needs the user's approval in ShogunAI".into());
            }
            let key = Self::key(body)?;
            if let Some(thread) = self.threads.lock().unwrap().get_mut(&key) {
                thread.requested = tool == Tool::MailTriageArchive;
            }
            *self.writes.lock().unwrap() += 1;
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{FakeMail, Thread};
    use super::*;
    use shogun_agents::approval::{ApprovalId, ConfirmIntent, Decision};
    use shogun_agents::permission::SendAction;

    const INBOX: Thread = Thread { archived: false, requested: false, standing: false };
    const STANDING: Thread = Thread { archived: false, requested: false, standing: true };
    const ARCHIVED: Thread = Thread { archived: true, requested: false, standing: true };

    fn body(key: &str) -> String {
        format!(r#"{{"thread_key":"{key}"}}"#)
    }

    #[test]
    fn only_a_gmail_change_without_a_standing_approval_needs_the_user() {
        let mail = FakeMail::with(&[("gmail:a", INBOX), ("gmail:s", STANDING), ("gmail:z", ARCHIVED)]);
        assert_eq!(
            needs_approval(&mail, Tool::MailTriageArchive, Some(&body("gmail:a"))),
            Some(MailArchiveRequest { thread_key: "gmail:a".into(), unarchive: false })
        );
        // The label's standing approval is the user's yes for that label.
        assert_eq!(needs_approval(&mail, Tool::MailTriageArchive, Some(&body("gmail:s"))), None);
        // No standing approval covers putting mail back, even on that label.
        assert_eq!(
            needs_approval(&mail, Tool::MailTriageUnarchive, Some(&body("gmail:z"))),
            Some(MailArchiveRequest { thread_key: "gmail:z".into(), unarchive: true })
        );
        // Unclassifiable (unknown thread, already archived, bad body): left to the write, which
        // answers with its own reason.
        assert_eq!(needs_approval(&mail, Tool::MailTriageArchive, Some(&body("gmail:nope"))), None);
        assert_eq!(needs_approval(&mail, Tool::MailTriageArchive, Some(&body("gmail:z"))), None);
        assert_eq!(needs_approval(&mail, Tool::MailTriageArchive, Some("{}")), None);
        assert_eq!(needs_approval(&mail, Tool::MailTriageArchive, None), None);
        assert_eq!(
            needs_approval(&crate::backend::StubBackend, Tool::MailTriageArchive, Some(&body("gmail:a"))),
            None
        );
        // Every other tool is never classified here.
        assert_eq!(needs_approval(&mail, Tool::MailTriageSet, Some(&body("gmail:a"))), None);
    }

    #[test]
    fn a_request_is_queued_pending_at_l2_and_only_the_user_confirms_it() {
        let mut queue = ApprovalQueue::new();
        let wanted = MailArchiveRequest { thread_key: "gmail:a".into(), unarchive: false };
        let (status, reply) =
            request(wanted, 10, &mut queue, ApprovalOrigin::Mcp, ApprovalSurface::Present);
        assert_eq!(status, 202, "{reply}");
        let value: serde_json::Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(value["pending"], true);
        assert_eq!(value["level"], "L2");
        assert_eq!(value["origin"], "mcp");
        let id = ApprovalId(value["approval_id"].as_u64().unwrap());
        let expected = SendAction::ArchiveMailThread { thread_key: "gmail:a".into(), unarchive: false };
        assert_eq!(queue.action(id), Some(&expected));
        assert_eq!(queue.confirm(id, ConfirmIntent::EnterKey, 11), Decision::RequiresDedicatedButton);
        assert!(matches!(
            queue.confirm(id, ConfirmIntent::DedicatedButton, 12),
            Decision::Confirmed(confirmed) if confirmed.action == expected
        ));
    }

    #[test]
    fn a_headless_process_refuses_rather_than_strands_the_request() {
        let mut queue = ApprovalQueue::new();
        let wanted = MailArchiveRequest { thread_key: "gmail:a".into(), unarchive: true };
        let (status, reply) =
            request(wanted, 1, &mut queue, ApprovalOrigin::Api, ApprovalSurface::Absent);
        assert_eq!(status, 501);
        assert!(reply.contains("no_approval_surface"), "{reply}");
        assert_eq!(queue.pending_len(), 0);
    }

    #[test]
    fn a_key_that_cannot_name_a_gmail_thread_is_refused_not_queued() {
        let mut queue = ApprovalQueue::new();
        let wanted = MailArchiveRequest { thread_key: "slack:C1".into(), unarchive: false };
        let (status, reply) =
            request(wanted, 1, &mut queue, ApprovalOrigin::Api, ApprovalSurface::Present);
        assert_eq!(status, 400, "{reply}");
        assert_eq!(queue.pending_len(), 0);
    }
}
