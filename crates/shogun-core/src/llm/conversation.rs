//! A chat turn as the role-capable providers want it: **system** (ours, stable, cacheable),
//! the **conversation so far** as real `user` / `assistant` turns, and the **new user message**
//! with the volatile context (screen, transcript, evidence) attached to it.
//!
//! Concatenating all of that into one string sent as a single `user` message would mean no
//! `system` field, no prompt caching (the stable prefix changes position every turn when the
//! screen block precedes the memory), and the model reading the thread as a blob of `User:` /
//! `SHOGUN:` lines rather than turns: the whole prompt at full input price every turn and an
//! instruction boundary that is only a convention.
//!
//! The shape here is what makes Anthropic's prompt cache work: the request is a monotonic prefix
//! — `system` first, then the history in order, then the one message that changes — with cache
//! breakpoints on the system block and on the last history turn. A provider without roles (the
//! subscription CLIs) gets [`ChatRequest::render_flat`], which lays the same content out in the
//! same order so the two lanes cannot describe one conversation two different ways.

use crate::chat_prompt::{self, HistoryTurn, Role};

/// One earlier turn, as a role plus its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatTurn {
    pub role: Role,
    pub text: String,
    pub images: Vec<super::chat_image::ChatImage>,
}

impl ChatTurn {
    pub fn anthropic_content(&self) -> serde_json::Value {
        if self.images.is_empty() { return serde_json::json!(self.text); }
        let mut blocks: Vec<_> = self.images.iter().map(|image| image.anthropic_block()).collect();
        blocks.push(serde_json::json!({"type":"text", "text":self.text}));
        serde_json::json!(blocks)
    }
    pub fn compat_content(&self) -> serde_json::Value {
        if self.images.is_empty() { return serde_json::json!(self.text); }
        let mut blocks = vec![serde_json::json!({"type":"text", "text":self.text})];
        blocks.extend(self.images.iter().map(|image| image.compat_block()));
        serde_json::json!(blocks)
    }
    pub fn responses_item(&self) -> serde_json::Value {
        let role = if self.role == Role::User { "user" } else { "assistant" };
        let mut item = super::openai_responses::message_item(role, &self.text);
        if !self.images.is_empty() {
            let mut content = vec![serde_json::json!({"type":"input_text", "text":self.text})];
            content.extend(self.images.iter().map(|image| image.responses_block()));
            item["content"] = serde_json::json!(content);
        }
        item
    }
}

impl From<&HistoryTurn> for ChatTurn {
    fn from(t: &HistoryTurn) -> Self {
        // The "looking at" label stays with the user's turn, as text: the model still needs it
        // to resolve "compare what I showed you in Slack with this", and it is the only thing
        // that survives from a screen block that was replaced turns ago.
        let text = match t.screen_label.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(label) if t.role == Role::User => {
                format!(
                    "[looking at {}] {}",
                    crate::screen_context::screen_prompt_text(label),
                    t.text.trim()
                )
            }
            _ => t.text.trim().to_string(),
        };
        Self { role: t.role, text, images: Vec::new() }
    }
}

/// A chat turn ready to send.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChatRequest {
    /// Ours: the fixed instructions and the user's standing directives. Stable across turns —
    /// this is the cacheable prefix, so nothing volatile may be put here.
    pub system: String,
    /// Quasi-static context-cache facts (FR-HN-05 layer 2). Sorted, no clock, no screen.
    /// Empty when there is nothing to remember. Cached separately from [`Self::system`] so a
    /// focus change can miss this block without busting the standing instructions.
    pub stable_context: String,
    /// The conversation so far, oldest first, already trimmed to budget (see
    /// [`chat_prompt::trim_history_stable`]). Kept as [`HistoryTurn`]s so the flat rendering
    /// can reuse the one history renderer; [`Self::normalized_turns`] converts for the API.
    pub turns: Vec<HistoryTurn>,
    /// Volatile, untrusted context for this turn (live screen, transcript, evidence),
    /// already rendered with its fences. Empty when there is none. Facts do **not** belong
    /// here — they ride in [`Self::stable_context`].
    pub context: String,
    /// The user's new message.
    pub message: String,
    pub images: Vec<super::chat_image::ImageTurn>,
}

impl ChatRequest {
    /// The content of the final `user` message: this turn's context, then the question.
    pub fn user_content(&self) -> String {
        let ctx = self.context.trim_end();
        if ctx.is_empty() {
            self.message.clone()
        } else {
            format!("{ctx}\n\nUser: {}", self.message)
        }
    }

    /// The `https://` links the user wrote in this thread: their new message and their earlier
    /// turns, never SHOGUN's answers, the screen, or other context. A model-chosen web read may
    /// fetch these without asking (#775); a link that arrived any other way could be an
    /// instruction planted in a page or an email.
    pub fn user_links(&self) -> Vec<String> {
        let mut text = String::new();
        for turn in self.turns.iter().filter(|t| t.role == Role::User) {
            text.push_str(&turn.text);
            text.push('\n');
        }
        text.push_str(&self.message);
        crate::web_fetch::https_links(&text)
    }

    /// Everything that is not ours, for the traceability digest: the history (the user's words
    /// and SHOGUN's earlier answers) and this turn's context + question. The system half is
    /// SHOGUN's instructions, which AR-11 does not account for.
    pub fn untrusted_text(&self) -> String {
        let mut s = String::new();
        s.push_str(&self.stable_context);
        for t in &self.turns {
            s.push_str(&t.text);
            s.push('\n');
        }
        s.push_str(&self.user_content());
        for image in &self.images { s.push_str(&image.image.data_url); }
        s
    }

    /// The turns as the Messages API accepts them: first turn is `user`, roles alternate, and
    /// the final `user` turn is the new message. Consecutive same-role turns are joined (a
    /// stored thread can carry two user messages in a row after a failed answer), a leading
    /// assistant turn is dropped, and a trailing user turn is folded into the final message.
    pub fn normalized_turns(&self) -> Vec<ChatTurn> {
        let mut out: Vec<ChatTurn> = Vec::with_capacity(self.turns.len() + 1);
        for (index, original) in self.turns.iter().enumerate() {
            let mut t = ChatTurn::from(original);
            if t.role == Role::User { t.images = self.images.iter().filter(|image| image.index == index).map(|image| image.image.clone()).collect(); }
            if t.text.trim().is_empty() {
                continue;
            }
            if out.is_empty() && t.role == Role::Assistant {
                continue;
            }
            match out.last_mut() {
                Some(last) if last.role == t.role => {
                    last.text.push_str("\n\n");
                    last.text.push_str(t.text.trim());
                    last.images.extend(t.images);
                }
                _ => out.push(t),
            }
        }
        let user = self.user_content();
        match out.last_mut() {
            Some(last) if last.role == Role::User => {
                last.text.push_str("\n\n");
                last.text.push_str(&user);
            }
            _ => out.push(ChatTurn {
                role: Role::User,
                text: user,
                images: Vec::new(),
            }),
        }
        if let Some(last) = out.last_mut() {
            last.images.extend(self.images.iter().filter(|image| image.index == self.turns.len()).map(|image| image.image.clone()));
        }
        out
    }

    /// The same content as one flat prompt, for a lane that has no roles (a vendor CLI on
    /// stdin) — system, this turn's context, the thread so far, then the question. The order
    /// is the one the flat prompt always had, so the CLI lane and the API lane say the same
    /// thing in the same order.
    pub fn render_flat(&self) -> String {
        let mut p = String::new();
        let system = self.system.trim();
        if !system.is_empty() {
            p.push_str(system);
            p.push('\n');
        }
        let ctx = self.context.trim_end();
        if !ctx.is_empty() {
            p.push_str(ctx);
            p.push('\n');
        }
        // Facts after the live blocks so the CLI lane still obeys FR-CH-04 (screen outranks
        // memory). The Anthropic lane puts this same string in a cached system block instead.
        let facts = self.stable_context.trim_end();
        if !facts.is_empty() {
            p.push_str(&super::prompt_cache::untrusted_stable_context(facts));
            p.push('\n');
        }
        // Already trimmed, so the renderer's own budget is irrelevant here.
        if let Some(block) = chat_prompt::render_history_block(&self.turns, usize::MAX) {
            p.push_str(&block);
        }
        p.push_str("\nUser: ");
        p.push_str(&self.message);
        p.push_str("\nSHOGUN:");
        p
    }
}

/// The retry policy for a streamed chat turn: one retry, only for failures that happened before
/// any text reached the user, only for the failure classes a second attempt can fix.
pub const STREAM_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(750);

/// Whether a streamed-turn failure is worth exactly one more attempt. Credentials, quota and
/// malformed responses are not: a second identical request gets the same answer, and the user
/// needs to see the real one.
pub fn retryable_before_first_token(e: &super::LlmError) -> bool {
    super::retry::is_retryable(e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::LlmError;

    fn turn(role: Role, text: &str) -> HistoryTurn {
        HistoryTurn {
            role,
            text: text.into(),
            screen_label: None,
        }
    }

    #[test]
    fn normalized_turns_alternate_and_end_with_the_new_message() {
        let req = ChatRequest {
            system: "sys".into(),
            turns: vec![
                turn(Role::Assistant, "stray leading answer"),
                turn(Role::User, "first"),
                turn(Role::User, "second (after a failed answer)"),
                turn(Role::Assistant, "reply"),
                turn(Role::User, ""),
            ],
            context: "<screen>x</screen>".into(),
            message: "now?".into(),
            ..Default::default()
        };
        let got = req.normalized_turns();
        let roles: Vec<Role> = got.iter().map(|t| t.role).collect();
        assert_eq!(roles, vec![Role::User, Role::Assistant, Role::User]);
        assert_eq!(got[0].text, "first\n\nsecond (after a failed answer)");
        assert!(
            got[2].text.starts_with("<screen>x</screen>\n\nUser: now?"),
            "{}",
            got[2].text
        );
    }

    #[test]
    fn user_links_come_only_from_what_the_user_wrote() {
        let req = ChatRequest {
            stable_context: "Known site: https://facts.example/".into(),
            turns: vec![
                turn(Role::User, "read https://first.example/post please"),
                turn(Role::Assistant, "Done. See also https://answer.example/"),
            ],
            context: "<screen>https://screen.example/?d=secret</screen>".into(),
            message: "and compare with https://second.example/page.".into(),
            ..Default::default()
        };
        assert_eq!(
            req.user_links(),
            vec!["https://first.example/post".to_string(), "https://second.example/page".to_string()]
        );
    }

    #[test]
    fn a_trailing_user_turn_folds_into_the_new_message() {
        let req = ChatRequest {
            turns: vec![turn(Role::User, "unanswered")],
            message: "again".into(),
            ..Default::default()
        };
        let got = req.normalized_turns();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].role, Role::User);
        assert_eq!(got[0].text, "unanswered\n\nagain");
    }

    #[test]
    fn flat_rendering_keeps_the_historical_order() {
        let req = ChatRequest {
            system: "SYS".into(),
            turns: vec![turn(Role::User, "q1"), turn(Role::Assistant, "a1")],
            context: "CTX".into(),
            message: "q2".into(),
            ..Default::default()
        };
        let flat = req.render_flat();
        let idx = |s: &str| {
            flat.find(s)
                .unwrap_or_else(|| panic!("missing {s} in {flat}"))
        };
        assert!(idx("SYS") < idx("CTX"));
        assert!(idx("CTX") < idx("User: q1"));
        assert!(idx("SHOGUN: a1") < idx("\nUser: q2\nSHOGUN:"));
    }

    #[test]
    fn facts_follow_the_live_screen_in_the_flat_render() {
        let req = ChatRequest {
            system: "SYS".into(),
            stable_context: "What you remember about their work:\n- ship Orchid\n".into(),
            context: "<screen>invoice</screen>".into(),
            message: "this?".into(),
            ..Default::default()
        };
        let flat = req.render_flat();
        let screen = flat.find("<screen>invoice</screen>").unwrap();
        let facts = flat.find("ship Orchid").unwrap();
        let question = flat.find("User: this?").unwrap();
        assert!(screen < facts && facts < question);
        assert!(
            !req.user_content().contains("ship Orchid"),
            "facts must not ride in the variable user turn"
        );
    }

    #[test]
    fn the_screen_label_travels_with_the_user_turn_only() {
        let user = HistoryTurn {
            role: Role::User,
            text: "look".into(),
            screen_label: Some("Slack — #launch".into()),
        };
        let bot = HistoryTurn {
            role: Role::Assistant,
            text: "ok".into(),
            screen_label: Some("ignored".into()),
        };
        assert_eq!(
            ChatTurn::from(&user).text,
            "[looking at Slack — #launch] look"
        );
        assert_eq!(ChatTurn::from(&bot).text, "ok");
    }

    #[test]
    fn only_transient_failures_are_retried() {
        assert!(retryable_before_first_token(&LlmError::RateLimited(
            429,
            String::new()
        )));
        assert!(retryable_before_first_token(&LlmError::Provider(
            "messages HTTP 529".into()
        )));
        assert!(retryable_before_first_token(&LlmError::Provider(
            "messages HTTP 503".into()
        )));
        assert!(!retryable_before_first_token(&LlmError::Provider(
            "messages HTTP 400".into()
        )));
        assert!(!retryable_before_first_token(&LlmError::Unauthorized(
            401,
            String::new()
        )));
        assert!(!retryable_before_first_token(&LlmError::QuotaExhausted(
            "x".into()
        )));
        assert!(!retryable_before_first_token(&LlmError::Parse("x".into())));
        assert!(!retryable_before_first_token(&LlmError::PolicyStopped));
    }
}
