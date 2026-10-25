//! `capture.set_paused` over the API faces (issue #897).
//!
//! What the write does depends on the pause in force, not on the tool:
//! - Pausing capture, or keeping a pause at least as long, narrows what SHOGUN reads from the
//!   screen. It stays a direct write at **L1** on every face.
//! - `{"paused": false}` while capture is paused starts SHOGUN reading the screen again after the
//!   user stopped it. That is **L3**: an MCP / REST / CLI caller can only *request* it. The
//!   request lands in the shared approval queue as
//!   [`SendAction::UnpauseCapture`](shogun_agents::permission::SendAction::UnpauseCapture), the
//!   caller gets the same pending envelope as an `actions.execute` send, and the pause stays until
//!   the user confirms in the app. The request carries the pause it was made against; the
//!   desktop ends only that pause, and a confirm after the pause changed fails closed.
//! - Bringing a timed pause's resume time forward, or giving an open-ended pause one, also starts
//!   capture sooner. There is no request for that: the backend refuses it, and the user changes
//!   the pause in the app.
//!
//! The backend owns the pause, so it classifies ([`MemoryBackend::capture_unpause_requested`]).
//! It also refuses, on its direct write path, anything that would start capture sooner
//! (`shogun-core`'s `DbBackend`), so a face that forgot to route a resume here fails closed. The
//! desktop's own pause switch does not come through here: the user is the actor there.

use shogun_agents::approval::{capture_unpause_request, ApprovalOrigin, ApprovalQueue};
use shogun_agents::permission::PausedCapture;

use crate::backend::MemoryBackend;
use crate::rest::ApprovalSurface;

/// The pause this body would end, when it ends the user's pause and so waits for their approval.
/// `None` for a pause, for a resume while capture already runs, and for anything the backend
/// cannot classify; those go to the backend's write, which applies what narrows capture and
/// refuses the rest.
pub fn unpause_request<B: MemoryBackend + ?Sized>(
    backend: &B,
    body: Option<&str>,
) -> Option<PausedCapture> {
    body.and_then(|body| backend.capture_unpause_requested(body).ok().flatten())
}

/// Queue the resume of `pause` for the user's confirmation, with the envelope of
/// `writing_voice_api::request_enable`. Nothing on disk changes here.
pub fn request_unpause(
    pause: PausedCapture,
    now_ms: i64,
    approvals: &mut ApprovalQueue,
    origin: ApprovalOrigin,
    surface: ApprovalSurface,
) -> (u16, String) {
    let (action, preview) = capture_unpause_request(pause);
    crate::rest::enqueue(action, preview, now_ms, approvals, origin, surface)
}

/// A backend that holds the pause in memory and behaves like the real one for this tool: it
/// classifies against its pause and its write refuses anything that starts capture sooner.
/// Shared by the face tests.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Mutex;

    use crate::backend::{MemoryBackend, ReadItem, ReadParams, WriteResult};
    use crate::memory_api::Tool;

    /// `None` = capture running; `Some(None)` = paused until resumed; `Some(Some(ms))` = paused
    /// until `ms`. The clock is fixed at 0.
    pub(crate) struct FakePause {
        pub(crate) pause: Mutex<Option<Option<i64>>>,
        pub(crate) writes: Mutex<usize>,
    }

    impl FakePause {
        pub(crate) fn with(pause: Option<Option<i64>>) -> Self {
            Self {
                pause: Mutex::new(pause),
                writes: Mutex::new(0),
            }
        }

        fn request(body: &str) -> Result<Option<Option<i64>>, String> {
            let value: serde_json::Value = serde_json::from_str(body)
                .map_err(|_| "Invalid capture pause request.".to_string())?;
            match (value["paused"].as_bool(), value["resume_at_ms"].as_i64()) {
                (Some(false), None) => Ok(None),
                (Some(true), until) => Ok(Some(until)),
                _ => Err("Invalid capture pause request.".to_string()),
            }
        }

        /// When capture runs again: `Some(0)` now, `Some(ms)` at a resume time, `None` never.
        fn resumes_at(pause: Option<Option<i64>>) -> Option<i64> {
            match pause {
                None => Some(0),
                Some(until) => until,
            }
        }
    }

    impl MemoryBackend for FakePause {
        fn read(&self, _tool: Tool, _params: &ReadParams) -> Vec<ReadItem> {
            Vec::new()
        }

        fn capture_unpause_requested(
            &self,
            body: &str,
        ) -> Result<Option<shogun_agents::permission::PausedCapture>, String> {
            let resume = Self::request(body)?.is_none();
            Ok(self
                .pause
                .lock()
                .unwrap()
                .filter(|_| resume)
                .map(|resume_at_ms| shogun_agents::permission::PausedCapture { resume_at_ms }))
        }

        fn write(&self, tool: Tool, body: &str) -> WriteResult {
            assert_eq!(tool, Tool::CaptureSetPaused);
            let requested = Self::request(body)?;
            let current = *self.pause.lock().unwrap();
            let sooner = match (Self::resumes_at(current), Self::resumes_at(requested)) {
                (_, None) => false,
                (None, Some(_)) => true,
                (Some(now), Some(next)) => next < now,
            };
            if sooner {
                return Err(if requested.is_some() {
                    "invalid capture pause request: capture is already paused for longer; shorten or end the pause in the ShogunAI app"
                } else {
                    "invalid capture pause request: ending a pause needs approval in the ShogunAI app"
                }
                .into());
            }
            *self.pause.lock().unwrap() = requested;
            *self.writes.lock().unwrap() += 1;
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::FakePause;
    use super::*;
    use shogun_agents::approval::{ApprovalId, ConfirmIntent, Decision};
    use shogun_agents::permission::SendAction;

    #[test]
    fn only_ending_a_pause_needs_approval() {
        let paused = FakePause::with(Some(None));
        // The request carries the pause it ends.
        assert_eq!(
            unpause_request(&paused, Some(r#"{"paused":false}"#)),
            Some(PausedCapture { resume_at_ms: None })
        );
        assert_eq!(unpause_request(&paused, Some(r#"{"paused":true}"#)), None);
        let timed = FakePause::with(Some(Some(60_000)));
        assert_eq!(
            unpause_request(&timed, Some(r#"{"paused":false}"#)),
            Some(PausedCapture { resume_at_ms: Some(60_000) })
        );
        // Shortening is not a request: the backend's write refuses it.
        assert_eq!(
            unpause_request(&timed, Some(r#"{"paused":true,"resume_at_ms":30000}"#)),
            None
        );
        let running = FakePause::with(None);
        assert_eq!(
            unpause_request(&running, Some(r#"{"paused":false}"#)),
            None,
            "nothing to resume"
        );
        assert_eq!(unpause_request(&running, Some(r#"{"paused":true}"#)), None);
        // Unclassifiable: left to the backend's write, which refuses a resume itself.
        assert_eq!(unpause_request(&paused, Some("not json")), None);
        assert_eq!(unpause_request(&paused, None), None);
        assert_eq!(
            unpause_request(&crate::backend::StubBackend, Some(r#"{"paused":false}"#)),
            None
        );
    }

    #[test]
    fn a_resume_is_queued_pending_and_only_the_user_confirms_it() {
        let mut queue = ApprovalQueue::new();
        let pause = PausedCapture { resume_at_ms: Some(60_000) };
        let (status, body) = request_unpause(
            pause,
            10,
            &mut queue,
            ApprovalOrigin::Mcp,
            ApprovalSurface::Present,
        );
        assert_eq!(status, 202, "{body}");
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["pending"], true);
        assert_eq!(value["level"], "L3");
        assert_eq!(value["origin"], "mcp");
        let id = ApprovalId(value["approval_id"].as_u64().unwrap());
        assert_eq!(queue.action(id), Some(&SendAction::UnpauseCapture { pause }));
        assert_eq!(
            queue.confirm(id, ConfirmIntent::EnterKey, 11),
            Decision::RequiresDedicatedButton
        );
        assert!(matches!(
            queue.confirm(id, ConfirmIntent::DedicatedButton, 12),
            Decision::Confirmed(confirmed) if confirmed.action == SendAction::UnpauseCapture { pause }
        ));
    }

    #[test]
    fn a_headless_process_refuses_rather_than_strands_the_request() {
        let mut queue = ApprovalQueue::new();
        let (status, body) = request_unpause(
            PausedCapture::default(),
            1,
            &mut queue,
            ApprovalOrigin::Api,
            ApprovalSurface::Absent,
        );
        assert_eq!(status, 501);
        assert!(body.contains("no_approval_surface"));
        assert_eq!(queue.pending_len(), 0);
    }
}
