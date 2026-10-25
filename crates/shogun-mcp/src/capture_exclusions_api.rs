//! `capture_exclusions.set` over the API faces (issue #657).
//!
//! The tool replaces the user's editable exclusion lists, and what that does depends on the lists
//! in force, not on the tool:
//! - A request whose lists are supersets of the current ones only **adds** exclusions. That
//!   narrows capture, so it stays a direct write at **L1** on every face.
//! - A request that drops even one app or website would start capturing something the user chose
//!   to keep out. That is **L3**: an MCP / REST / CLI caller can only *request* it. The request
//!   lands in the shared approval queue as
//!   [`SendAction::ResumeCapture`](shogun_agents::permission::SendAction::ResumeCapture), the
//!   caller gets the same pending envelope as an `actions.execute` send, and the policy file is
//!   untouched until the user confirms in the app. A request that both adds and removes is
//!   queued whole and applied atomically on approval.
//!
//! The backend owns the lists, so it classifies ([`MemoryBackend::capture_exclusions_change`]).
//! It also refuses any removal on its direct write path (`shogun-core`'s `DbBackend`), so a face
//! that forgot to route a removal here fails closed instead of widening capture. On confirm the
//! desktop re-checks the change against the lists as they are then (`Db::apply_confirmed_setting`).
//! The desktop Settings screen does not come through here: the user is the actor there.

use shogun_agents::approval::{capture_resume_request, ApprovalOrigin, ApprovalQueue};
use shogun_agents::permission::CaptureExclusionChange;

use crate::backend::MemoryBackend;
use crate::rest::ApprovalSurface;

/// The approval a body needs, if any: `Some` only when the backend says the replacement would
/// stop excluding something. `None` for an addition-only request and for anything the backend
/// cannot classify; both go to the backend's write, which applies additions and refuses the rest.
pub fn loosening<B: MemoryBackend + ?Sized>(
    backend: &B,
    body: Option<&str>,
) -> Option<CaptureExclusionChange> {
    backend
        .capture_exclusions_change(body?)
        .ok()
        .flatten()
        .filter(CaptureExclusionChange::resumes_capture)
}

/// Queue a removal for the user's confirmation, with the envelope of
/// `writing_voice_api::request_enable`. Nothing on disk changes here.
pub fn request_change(
    change: CaptureExclusionChange,
    now_ms: i64,
    approvals: &mut ApprovalQueue,
    origin: ApprovalOrigin,
    surface: ApprovalSurface,
) -> (u16, String) {
    let (action, preview) = capture_resume_request(change);
    crate::rest::enqueue(action, preview, now_ms, approvals, origin, surface)
}

/// A backend that holds exclusion lists in memory and behaves like the real one for this tool:
/// it classifies against its lists and its write refuses any removal. Shared by the face tests.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Mutex;

    use shogun_agents::permission::CaptureExclusionChange;

    use crate::backend::{MemoryBackend, ReadItem, ReadParams, WriteResult};
    use crate::memory_api::Tool;

    #[derive(Default)]
    pub(crate) struct FakeExclusions {
        pub(crate) apps: Mutex<Vec<String>>,
        pub(crate) websites: Mutex<Vec<String>>,
        pub(crate) writes: Mutex<usize>,
    }

    impl FakeExclusions {
        pub(crate) fn with(apps: &[&str], websites: &[&str]) -> Self {
            let owned = |items: &[&str]| items.iter().map(|item| item.to_string()).collect();
            Self {
                apps: Mutex::new(owned(apps)),
                websites: Mutex::new(owned(websites)),
                writes: Mutex::new(0),
            }
        }

        fn lists(body: &str) -> Result<(Vec<String>, Vec<String>), String> {
            let value: serde_json::Value =
                serde_json::from_str(body).map_err(|_| "invalid capture exclusions request".to_string())?;
            let list = |key: &str| {
                serde_json::from_value::<Vec<String>>(value[key].clone())
                    .map_err(|_| "invalid capture exclusions request".to_string())
            };
            Ok((list("apps")?, list("websites")?))
        }
    }

    impl MemoryBackend for FakeExclusions {
        fn read(&self, _tool: Tool, _params: &ReadParams) -> Vec<ReadItem> {
            Vec::new()
        }

        fn read_structured(&self, tool: Tool, _params: &ReadParams) -> Option<String> {
            (tool == Tool::CaptureExclusionsGet).then(|| {
                serde_json::json!({
                    "apps": *self.apps.lock().unwrap(),
                    "websites": *self.websites.lock().unwrap(),
                })
                .to_string()
            })
        }

        fn capture_exclusions_change(
            &self,
            body: &str,
        ) -> Result<Option<CaptureExclusionChange>, String> {
            let (apps, websites) = Self::lists(body)?;
            let dropped = |current: &Vec<String>, requested: &Vec<String>| -> Vec<String> {
                current.iter().filter(|item| !requested.contains(item)).cloned().collect()
            };
            let change = CaptureExclusionChange {
                resumed_apps: dropped(&self.apps.lock().unwrap(), &apps),
                resumed_websites: dropped(&self.websites.lock().unwrap(), &websites),
                apps,
                websites,
            };
            Ok(change.resumes_capture().then_some(change))
        }

        fn write(&self, tool: Tool, body: &str) -> WriteResult {
            assert_eq!(tool, Tool::CaptureExclusionsSet);
            if self.capture_exclusions_change(body)?.is_some() {
                return Err("invalid capture exclusions request: removal needs approval".into());
            }
            let (apps, websites) = Self::lists(body)?;
            *self.apps.lock().unwrap() = apps;
            *self.websites.lock().unwrap() = websites;
            *self.writes.lock().unwrap() += 1;
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{ReadItem, ReadParams};
    use crate::memory_api::Tool;
    use shogun_agents::approval::{ApprovalId, ConfirmIntent, Decision};
    use shogun_agents::permission::SendAction;

    /// Classifies against a fixed "current" list, like the real backend does against the file.
    struct Lists(Vec<String>);
    impl MemoryBackend for Lists {
        fn read(&self, _tool: Tool, _params: &ReadParams) -> Vec<ReadItem> {
            Vec::new()
        }
        fn capture_exclusions_change(
            &self,
            body: &str,
        ) -> Result<Option<CaptureExclusionChange>, String> {
            let value: serde_json::Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
            let apps: Vec<String> = serde_json::from_value(value["apps"].clone()).map_err(|e| e.to_string())?;
            let resumed: Vec<String> = self.0.iter().filter(|a| !apps.contains(a)).cloned().collect();
            Ok(Some(CaptureExclusionChange { apps, resumed_apps: resumed, ..Default::default() }))
        }
    }

    #[test]
    fn only_a_removal_needs_approval() {
        let backend = Lists(vec!["com.example.Secret".into()]);
        assert_eq!(
            loosening(&backend, Some(r#"{"apps":["com.example.Secret","com.example.New"],"websites":[]}"#)),
            None,
            "an addition is direct"
        );
        let change = loosening(&backend, Some(r#"{"apps":["com.example.New"],"websites":[]}"#)).unwrap();
        assert_eq!(change.resumed_apps, vec!["com.example.Secret".to_string()]);
        // Unclassifiable: left to the backend's write, which refuses removals itself.
        assert_eq!(loosening(&backend, Some("not json")), None);
        assert_eq!(loosening(&backend, None), None);
        assert_eq!(loosening(&crate::backend::StubBackend, Some(r#"{"apps":[],"websites":[]}"#)), None);
    }

    #[test]
    fn a_removal_is_queued_pending_and_only_the_user_confirms_it() {
        let change = CaptureExclusionChange {
            resumed_websites: vec!["private.example".into()],
            ..Default::default()
        };
        let mut queue = ApprovalQueue::new();
        let (status, body) =
            request_change(change.clone(), 10, &mut queue, ApprovalOrigin::Mcp, ApprovalSurface::Present);
        assert_eq!(status, 202, "{body}");
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["pending"], true);
        assert_eq!(value["level"], "L3");
        assert_eq!(value["origin"], "mcp");
        let id = ApprovalId(value["approval_id"].as_u64().unwrap());
        assert_eq!(queue.action(id), Some(&SendAction::ResumeCapture { change: change.clone() }));
        assert_eq!(queue.confirm(id, ConfirmIntent::EnterKey, 11), Decision::RequiresDedicatedButton);
        assert!(matches!(
            queue.confirm(id, ConfirmIntent::DedicatedButton, 12),
            Decision::Confirmed(confirmed) if confirmed.action == SendAction::ResumeCapture { change }
        ));
    }

    #[test]
    fn a_headless_process_refuses_rather_than_strands_the_request() {
        let change = CaptureExclusionChange {
            resumed_apps: vec!["com.example.Secret".into()],
            ..Default::default()
        };
        let mut queue = ApprovalQueue::new();
        let (status, body) =
            request_change(change, 1, &mut queue, ApprovalOrigin::Api, ApprovalSurface::Absent);
        assert_eq!(status, 501);
        assert!(body.contains("no_approval_surface"));
        assert_eq!(queue.pending_len(), 0);
    }
}
