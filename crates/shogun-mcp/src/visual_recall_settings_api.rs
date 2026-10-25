//! `visual_recall.set_enabled` / `visual_recall.set_retention` over the API faces (issue #782).
//!
//! Visual recall keeps encrypted images of the screen on this Mac for a finite retention. What a
//! write does depends on the settings in force, not on the tool:
//! - Turning it **off**, enabling it while it is already on, and an equal or **shorter** retention
//!   keep no more of the screen than before. They stay direct writes at **L1** on every face.
//!   (Turning it off deletes every automatically captured frame at once.)
//! - Turning it **on** while it is Off, or keeping saved screens **longer** than the retention in
//!   force, starts or lengthens keeping images of the user's screen. That is **L3**: an MCP / REST
//!   / CLI caller can only *request* it. The request lands in the shared approval queue as
//!   [`SendAction::WidenVisualRecall`](shogun_agents::permission::SendAction::WidenVisualRecall),
//!   the caller gets the same pending envelope as an `actions.execute` send, and
//!   `visual_recall.json` is untouched until the user confirms in the app.
//!
//! The backend owns the settings, so it classifies ([`MemoryBackend::visual_recall_change`]). It
//! also refuses any widening on its direct write path (`shogun-core`'s `DbBackend`), so a face
//! that forgot to route one here fails closed instead of turning capture on. On confirm the
//! desktop applies exactly the requested change and never a longer retention
//! (`Db::apply_confirmed_setting`). The desktop Settings screen does not come through here: the
//! user is the actor there.

use shogun_agents::approval::{visual_recall_request, ApprovalOrigin, ApprovalQueue};
use shogun_agents::permission::VisualRecallWidening;

use crate::backend::MemoryBackend;
use crate::memory_api::Tool;
use crate::rest::ApprovalSurface;

/// The two Visual recall settings writes whose effect depends on the settings in force.
pub fn is_settings_write(tool: Tool) -> bool {
    matches!(tool, Tool::VisualRecallSetEnabled | Tool::VisualRecallSetRetention)
}

/// The approval a body needs, if any: `Some` only when the backend says the write would turn
/// Visual recall on or keep saved screens longer. `None` for every other tool, for a write that
/// keeps no more of the screen, and for anything the backend cannot classify; those go to the
/// backend's write, which applies narrowing changes and refuses the rest.
pub fn loosening<B: MemoryBackend + ?Sized>(
    backend: &B,
    tool: Tool,
    body: Option<&str>,
) -> Option<VisualRecallWidening> {
    if !is_settings_write(tool) {
        return None;
    }
    backend
        .visual_recall_change(tool, body?)
        .ok()
        .flatten()
        .filter(VisualRecallWidening::widens)
}

/// Queue a widening for the user's confirmation, with the envelope of
/// `writing_voice_api::request_enable`. Nothing on disk changes here.
pub fn request_change(
    change: VisualRecallWidening,
    now_ms: i64,
    approvals: &mut ApprovalQueue,
    origin: ApprovalOrigin,
    surface: ApprovalSurface,
) -> (u16, String) {
    let (action, preview) = visual_recall_request(change);
    crate::rest::enqueue(action, preview, now_ms, approvals, origin, surface)
}

/// A backend that holds the Visual recall settings in memory and behaves like the real one for
/// these tools: it classifies against its settings and its write refuses any widening. Shared by
/// the face tests.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Mutex;

    use shogun_agents::permission::VisualRecallWidening;

    use crate::backend::{MemoryBackend, ReadItem, ReadParams, WriteResult};
    use crate::memory_api::Tool;

    pub(crate) struct FakeVisualRecall {
        pub(crate) enabled: Mutex<bool>,
        pub(crate) days: Mutex<u32>,
        pub(crate) writes: Mutex<usize>,
    }

    impl FakeVisualRecall {
        pub(crate) fn with(enabled: bool, days: u32) -> Self {
            Self { enabled: Mutex::new(enabled), days: Mutex::new(days), writes: Mutex::new(0) }
        }

        /// `(enabled, days)` the body asks for; `None` for the half it leaves alone.
        fn request(tool: Tool, body: &str) -> Result<(Option<bool>, Option<u32>), String> {
            let value: serde_json::Value =
                serde_json::from_str(body).map_err(|_| "invalid visual recall request".to_string())?;
            match tool {
                Tool::VisualRecallSetEnabled => value["enabled"]
                    .as_bool()
                    .map(|enabled| (Some(enabled), None))
                    .ok_or_else(|| "expected {\"enabled\":bool}".to_string()),
                Tool::VisualRecallSetRetention => value["days"]
                    .as_u64()
                    .and_then(|days| u32::try_from(days).ok())
                    .filter(|days| (1..=3650).contains(days))
                    .map(|days| (None, Some(days)))
                    .ok_or_else(|| "expected retention days between 1 and 3650".to_string()),
                _ => Err("not a visual recall settings write".to_string()),
            }
        }
    }

    impl MemoryBackend for FakeVisualRecall {
        fn read(&self, _tool: Tool, _params: &ReadParams) -> Vec<ReadItem> {
            Vec::new()
        }

        fn visual_recall_change(
            &self,
            tool: Tool,
            body: &str,
        ) -> Result<Option<VisualRecallWidening>, String> {
            let (enable, days) = Self::request(tool, body)?;
            let enabled = *self.enabled.lock().unwrap();
            let current = *self.days.lock().unwrap();
            let change = VisualRecallWidening {
                enable: enable == Some(true) && !enabled,
                retention_days: days.filter(|days| *days > current),
                previous_retention_days: current,
            };
            Ok(change.widens().then_some(change))
        }

        fn write(&self, tool: Tool, body: &str) -> WriteResult {
            if self.visual_recall_change(tool, body)?.is_some() {
                return Err("invalid visual recall request: widening needs approval".into());
            }
            match Self::request(tool, body)? {
                (Some(enabled), _) => *self.enabled.lock().unwrap() = enabled,
                (_, Some(days)) => *self.days.lock().unwrap() = days,
                _ => {}
            }
            *self.writes.lock().unwrap() += 1;
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::FakeVisualRecall;
    use super::*;
    use shogun_agents::approval::{ApprovalId, ConfirmIntent, Decision};
    use shogun_agents::permission::SendAction;

    #[test]
    fn only_turning_on_or_lengthening_needs_approval() {
        let off = FakeVisualRecall::with(false, 3);
        let enable = loosening(&off, Tool::VisualRecallSetEnabled, Some(r#"{"enabled":true}"#));
        assert_eq!(
            enable,
            Some(VisualRecallWidening { enable: true, retention_days: None, previous_retention_days: 3 })
        );
        assert_eq!(loosening(&off, Tool::VisualRecallSetEnabled, Some(r#"{"enabled":false}"#)), None);
        let on = FakeVisualRecall::with(true, 3);
        assert_eq!(
            loosening(&on, Tool::VisualRecallSetEnabled, Some(r#"{"enabled":true}"#)),
            None,
            "already on: nothing widens"
        );
        let longer = loosening(&on, Tool::VisualRecallSetRetention, Some(r#"{"days":3650}"#));
        assert_eq!(
            longer,
            Some(VisualRecallWidening { enable: false, retention_days: Some(3650), previous_retention_days: 3 })
        );
        assert_eq!(loosening(&on, Tool::VisualRecallSetRetention, Some(r#"{"days":3}"#)), None, "equal");
        assert_eq!(loosening(&on, Tool::VisualRecallSetRetention, Some(r#"{"days":1}"#)), None, "shorter");
        // Unclassifiable: left to the backend's write, which refuses a widening itself.
        assert_eq!(loosening(&off, Tool::VisualRecallSetEnabled, Some("not json")), None);
        assert_eq!(loosening(&off, Tool::VisualRecallSetEnabled, None), None);
        assert_eq!(
            loosening(&crate::backend::StubBackend, Tool::VisualRecallSetEnabled, Some(r#"{"enabled":true}"#)),
            None
        );
        // Every other tool is never classified here.
        assert_eq!(loosening(&off, Tool::VisualRecallDeleteFrame, Some(r#"{"enabled":true}"#)), None);
    }

    #[test]
    fn a_widening_is_queued_pending_and_only_the_user_confirms_it() {
        let change = VisualRecallWidening { enable: true, retention_days: None, previous_retention_days: 3 };
        let mut queue = ApprovalQueue::new();
        let (status, body) =
            request_change(change.clone(), 10, &mut queue, ApprovalOrigin::Mcp, ApprovalSurface::Present);
        assert_eq!(status, 202, "{body}");
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["pending"], true);
        assert_eq!(value["level"], "L3");
        assert_eq!(value["origin"], "mcp");
        let id = ApprovalId(value["approval_id"].as_u64().unwrap());
        assert_eq!(queue.action(id), Some(&SendAction::WidenVisualRecall { change: change.clone() }));
        assert_eq!(queue.confirm(id, ConfirmIntent::EnterKey, 11), Decision::RequiresDedicatedButton);
        assert!(matches!(
            queue.confirm(id, ConfirmIntent::DedicatedButton, 12),
            Decision::Confirmed(confirmed) if confirmed.action == SendAction::WidenVisualRecall { change }
        ));
    }

    #[test]
    fn a_headless_process_refuses_rather_than_strands_the_request() {
        let change = VisualRecallWidening { enable: false, retention_days: Some(30), previous_retention_days: 3 };
        let mut queue = ApprovalQueue::new();
        let (status, body) =
            request_change(change, 1, &mut queue, ApprovalOrigin::Api, ApprovalSurface::Absent);
        assert_eq!(status, 501);
        assert!(body.contains("no_approval_surface"));
        assert_eq!(queue.pending_len(), 0);
    }
}
