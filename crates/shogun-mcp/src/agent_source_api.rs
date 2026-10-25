//! `agent.source.set` over the API faces (issue #110, invariant 6).
//!
//! The Agent lane runs either on an API key the user pasted (BYOK) or on a subscription they
//! already pay for, through the vendor's own CLI. The desktop only runs a subscription after an
//! explicit opt-in to a three-point disclosure, and what a write does depends on that:
//! - Selecting an API-key provider, changing the model, or withdrawing the opt-in only narrows
//!   where prompts go. That stays a direct write at **L1** on every face.
//! - `openai_store: false` (stop letting OpenAI store responses) only shortens what a third party
//!   keeps, so it is direct (**L1**) too. `openai_store: true` is refused, never queued: the user
//!   turns it on at the Settings toggle that carries the retention disclosure (issue #390, owner
//!   decision 2026-09-25 on #664), the same way `connections.composio.set` refuses loosening.
//!   The refusal comes from the backend's classify and write, so there is nothing to route here.
//! - Selecting a subscription CLI that is not already the consented source is the opt-in itself.
//!   An MCP / REST / CLI caller can only *request* it: it lands in the shared approval queue as
//!   [`SendAction::UseSubscriptionAgent`](shogun_agents::permission::SendAction::UseSubscriptionAgent),
//!   the caller gets the same pending envelope as an `actions.execute` send, and `llm.json` is
//!   untouched until the user confirms the card (which carries the disclosure) in the app. The
//!   caller must still pass `subscription_consent: true`, but that is its own claim, so it can
//!   never stand in for the confirmation.
//!
//! The backend owns the settings, so it classifies ([`MemoryBackend::agent_source_change`]). It
//! also refuses a subscription selection on its direct write path (`shogun-core`'s
//! `agent_source::apply_direct`), so a face that forgot to route one here fails closed instead of
//! granting consent. The desktop Settings screen does not come through here: the user is the
//! actor there.

use shogun_agents::approval::{subscription_agent_request, ApprovalOrigin, ApprovalQueue};

use crate::backend::{AgentSubscriptionRequest, MemoryBackend};
use crate::rest::ApprovalSurface;

/// The approval a body needs, if any: `Some` only when the backend says it selects a subscription
/// the user has not opted into. `None` for a direct change and for anything the backend cannot
/// classify; both go to the backend's write, which applies direct changes and refuses the rest.
pub fn loosening<B: MemoryBackend + ?Sized>(
    backend: &B,
    body: Option<&str>,
) -> Option<AgentSubscriptionRequest> {
    backend.agent_source_change(body?).ok().flatten()
}

/// Queue a subscription selection for the user's confirmation, with the envelope of
/// `writing_voice_api::request_enable`. Nothing on disk changes here.
pub fn request_subscription(
    request: &AgentSubscriptionRequest,
    now_ms: i64,
    approvals: &mut ApprovalQueue,
    origin: ApprovalOrigin,
    surface: ApprovalSurface,
) -> (u16, String) {
    let (action, preview) = subscription_agent_request(&request.delegate, &request.label, &request.plan);
    crate::rest::enqueue(action, preview, now_ms, approvals, origin, surface)
}

/// A backend that holds the Agent lane settings in memory and behaves like the real one for this
/// tool: it classifies against its state, and its write refuses a subscription selection. Shared
/// by the face tests.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Mutex;

    use crate::backend::{AgentSubscriptionRequest, MemoryBackend, ReadItem, ReadParams, WriteResult};
    use crate::memory_api::Tool;

    /// `(provider, subscription_consent)`, plus the OpenAI store answer.
    pub(crate) struct FakeAgentSource {
        pub(crate) state: Mutex<(String, bool)>,
        pub(crate) openai_store: Mutex<bool>,
        pub(crate) writes: Mutex<usize>,
    }

    impl FakeAgentSource {
        pub(crate) fn with(provider: &str, consent: bool) -> Self {
            Self {
                state: Mutex::new((provider.to_string(), consent)),
                openai_store: Mutex::new(false),
                writes: Mutex::new(0),
            }
        }

        pub(crate) fn storing(self) -> Self {
            *self.openai_store.lock().unwrap() = true;
            self
        }

        fn delegate(id: &str) -> Option<(&'static str, &'static str)> {
            match id {
                "claude-code" => Some(("Claude Code", "Claude Pro / Max")),
                "codex" => Some(("Codex", "ChatGPT Plus / Pro")),
                _ => None,
            }
        }

        /// Mirrors `shogun_core::agent_source::plan` for the cases the face tests exercise.
        fn classify(&self, body: &str) -> Result<Option<AgentSubscriptionRequest>, String> {
            let value: serde_json::Value =
                serde_json::from_str(body).map_err(|_| "invalid_agent_source_request".to_string())?;
            let (current, consented) = self.state.lock().unwrap().clone();
            if value["openai_store"] == serde_json::json!(true) && !*self.openai_store.lock().unwrap() {
                return Err("openai_store_only_in_app".into());
            }
            let source = value["source"].as_str().unwrap_or(&current).to_string();
            let consent = value["subscription_consent"].as_bool();
            let Some((label, plan)) = Self::delegate(&source) else {
                return if consent == Some(true) {
                    Err("consent_needs_subscription_source".into())
                } else {
                    Ok(None)
                };
            };
            if consent == Some(false) || (source == current && consented) {
                return Ok(None);
            }
            if consent != Some(true) {
                return Err("consent_required".into());
            }
            Ok(Some(AgentSubscriptionRequest {
                delegate: source,
                label: label.into(),
                plan: plan.into(),
            }))
        }
    }

    impl MemoryBackend for FakeAgentSource {
        fn read(&self, _tool: Tool, _params: &ReadParams) -> Vec<ReadItem> {
            Vec::new()
        }

        fn read_structured(&self, tool: Tool, _params: &ReadParams) -> Option<String> {
            (tool == Tool::AgentSourceGet).then(|| {
                let (provider, consent) = self.state.lock().unwrap().clone();
                let store = *self.openai_store.lock().unwrap();
                serde_json::json!({ "selected": provider, "subscription_consent": consent, "openai_store": store })
                    .to_string()
            })
        }

        fn agent_source_change(&self, body: &str) -> Result<Option<AgentSubscriptionRequest>, String> {
            self.classify(body)
        }

        fn write(&self, tool: Tool, body: &str) -> WriteResult {
            assert_eq!(tool, Tool::AgentSourceSet);
            if self.classify(body).map_err(|e| format!("invalid agent source request: {e}"))?.is_some() {
                return Err("invalid agent source request: using a subscription needs approval".into());
            }
            let value: serde_json::Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
            let mut state = self.state.lock().unwrap();
            if let Some(source) = value["source"].as_str() {
                state.0 = source.to_string();
            }
            if let Some(consent) = value["subscription_consent"].as_bool() {
                state.1 = consent;
            }
            if let Some(store) = value["openai_store"].as_bool() {
                *self.openai_store.lock().unwrap() = store;
            }
            *self.writes.lock().unwrap() += 1;
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::FakeAgentSource;
    use super::*;
    use shogun_agents::approval::{ApprovalId, ConfirmIntent, Decision, KeyKind};
    use shogun_agents::permission::SendAction;

    #[test]
    fn only_an_unconsented_subscription_needs_approval() {
        let backend = FakeAgentSource::with("anthropic", false);
        assert_eq!(loosening(&backend, Some(r#"{"source":"openai"}"#)), None, "an API key is direct");
        assert_eq!(loosening(&backend, Some(r#"{"subscription_consent":false}"#)), None);
        let request = loosening(&backend, Some(r#"{"source":"claude-code","subscription_consent":true}"#)).unwrap();
        assert_eq!(request.delegate, "claude-code");
        // Unclassifiable (no attestation, not JSON, no backend): left to the backend's write,
        // which refuses a subscription on its own.
        assert_eq!(loosening(&backend, Some(r#"{"source":"claude-code"}"#)), None);
        assert_eq!(loosening(&backend, Some("not json")), None);
        assert_eq!(loosening(&backend, None), None);
        assert_eq!(
            loosening(&crate::backend::StubBackend, Some(r#"{"source":"claude-code","subscription_consent":true}"#)),
            None
        );
    }

    #[test]
    fn a_subscription_is_queued_pending_and_only_the_user_confirms_it() {
        let request = AgentSubscriptionRequest {
            delegate: "codex".into(),
            label: "Codex".into(),
            plan: "ChatGPT Plus / Pro".into(),
        };
        let mut queue = ApprovalQueue::new();
        let (status, body) =
            request_subscription(&request, 10, &mut queue, ApprovalOrigin::Api, ApprovalSurface::Present);
        assert_eq!(status, 202, "{body}");
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["pending"], true);
        assert_eq!(value["level"], "L3");
        assert_eq!(value["origin"], "api");
        let id = ApprovalId(value["approval_id"].as_u64().unwrap());
        let action = SendAction::UseSubscriptionAgent { delegate: "codex".into() };
        assert_eq!(queue.action(id), Some(&action));
        let preview = queue.preview(id).unwrap();
        assert!(preview.full_body.contains("ChatGPT Plus / Pro"), "{}", preview.full_body);
        assert_eq!(preview.key_kind, KeyKind::AiProvider);
        assert_eq!(queue.confirm(id, ConfirmIntent::EnterKey, 11), Decision::RequiresDedicatedButton);
        assert!(matches!(
            queue.confirm(id, ConfirmIntent::DedicatedButton, 12),
            Decision::Confirmed(confirmed) if confirmed.action == action
        ));
    }

    #[test]
    fn a_headless_process_refuses_rather_than_strands_the_request() {
        let request = AgentSubscriptionRequest {
            delegate: "claude-code".into(),
            label: "Claude Code".into(),
            plan: "Claude Pro / Max".into(),
        };
        let mut queue = ApprovalQueue::new();
        let (status, body) =
            request_subscription(&request, 1, &mut queue, ApprovalOrigin::Api, ApprovalSurface::Absent);
        assert_eq!(status, 501);
        assert!(body.contains("no_approval_surface"));
        assert_eq!(queue.pending_len(), 0);
    }
}
