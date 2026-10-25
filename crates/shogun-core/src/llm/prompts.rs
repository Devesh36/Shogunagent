//! The prompt registry (FR-HN-02, Issue #313 / #103): every prompt the product sends has a
//! stable identity — an id and a hand-incremented version — recorded on each `llm_usage` row.
//!
//! Why: without identity, no metric row can be attributed to the prompt wording that produced
//! it, which makes regression detection, A/B comparison and prompt optimisation guesswork.
//! The registry is deliberately *not* a template engine — prompts stay hand-built where they
//! are today — it is only the name tag every call must wear.
//!
//! Rules:
//! - **One constant per product prompt.** A new LLM call site starts by adding its tag here
//!   (the `ALL` test makes an unregistered id impossible to sneak in via a collision).
//! - **Bump `version` when the prompt's wording or structure changes** in a way that could move
//!   quality or token counts. The version is the unit eval results and usage rows key on.
//! - Ids are snake_case ASCII, stable forever once shipped (rows referencing them live in the
//!   user's DB for years — CLAUDE.md migration rule applies in spirit).

/// How hard the model should think for a prompt (issue #388). A *policy* the registry owns per
/// tag, not a provider parameter: each backend maps it onto its own documented knob, and a
/// backend without one ignores it.
///
/// Today only the Codex delegate consumes it (`-c model_reasoning_effort="…"`, see
/// `subscription::DelegateOptions`). Only the three levels reasoning models commonly advertise
/// are representable — vendor extras (`xhigh`, …) depend on the model, and asking a model for a
/// level it lacks would fail the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Effort {
    Low,
    Medium,
    High,
}

impl Effort {
    /// The lowercase wire word (`low` / `medium` / `high`).
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
        }
    }
}

/// A prompt's identity: registered id + hand-incremented version, plus the static reasoning
/// effort policy for it. `Copy`, cheap to thread everywhere a completion happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptTag {
    pub id: &'static str,
    pub version: u32,
    /// Reasoning effort this prompt should run at, where the backend can express it (issue
    /// #388). `None` leaves the backend on its own default — SHOGUN does not guess.
    ///
    /// Policy: latency-bound edits at the caret (inline draft, Scribe) are `Low`; chat is
    /// `Medium`. The handoff brief for an external agent (epic #387) is to be `High`; no tag
    /// exists for it yet, so register it with `.with_effort(Effort::High)` when it lands.
    pub effort: Option<Effort>,
}

impl PromptTag {
    const fn new(id: &'static str, version: u32) -> Self {
        Self { id, version, effort: None }
    }

    /// The same tag with a reasoning-effort policy.
    const fn with_effort(self, effort: Effort) -> Self {
        Self { id: self.id, version: self.version, effort: Some(effort) }
    }
}

/// ⌥-tap inline draft at the caret (`shogun_core::inline::build_split_prompt`).
pub const INLINE_DRAFT: PromptTag = PromptTag::new("inline_draft", 4).with_effort(Effort::Low);
/// Notch chat (assembled in the desktop shell today — DEBT-003b tracks moving it into core).
///
/// Separate from [`MEETING_CHAT`] (FR-CH-17): a meeting question and a notch question spend the
/// same way but send different things, and a ledger that calls both "chat" cannot show a user
/// which turn put their transcript on the wire.
// Policy for the Codex delegate (medium). The desktop BYOK lanes override on the wire (`chat_anthropic_config`: low).
pub const INLINE_CHAT: PromptTag = PromptTag::new("inline_chat", 5).with_effort(Effort::Medium);
/// Chat asked from the meeting overlay, where the live transcript rides along (FR-CH-15/17).
pub const MEETING_CHAT: PromptTag = PromptTag::new("meeting_chat", 4).with_effort(Effort::Medium);
/// Scribe instructed rewrite (`build_scribe_edit_split_prompt`).
pub const SCRIBE_EDIT: PromptTag = PromptTag::new("scribe_edit", 2).with_effort(Effort::Low);
/// Reply Drafter / L3 send drafting (`approvals.rs`).
pub const REPLY_DRAFT: PromptTag = PromptTag::new("reply_draft", 1);
/// Meeting minutes / Recap (`shogun_core::meeting::minutes`).
pub const MEETING_MINUTES: PromptTag = PromptTag::new("meeting_minutes", 2);
/// Dream Cycle consolidation / classification (`dreamcycle::jobs::consolidation_prompt`).
pub const DREAM_CONSOLIDATION: PromptTag = PromptTag::new("dream_consolidation", 1);
/// "Which stored people / projects are the same?" (Batch lane, `dreamcycle::entities`).
pub const ENTITY_DEDUPE: PromptTag = PromptTag::new("entity_dedupe", 1);
/// The loop consolidator's short list of commitments / open loops (Batch lane, `dreamcycle::loops`).
/// v2 (E49): describes the "Outcomes noticed in later messages" section and re-dating by re-list.
pub const LOOPS_CONSOLIDATE: PromptTag = PromptTag::new("loops_consolidate", 2);
/// Person / project page rewrite (Batch lane, `dreamcycle::pages`).
pub const ENTITY_PAGE: PromptTag = PromptTag::new("entity_page", 1);
/// Morning Brief generation (Batch lane, `ModelClass::Brief` — wiring tracked by #316).
pub const MORNING_BRIEF: PromptTag = PromptTag::new("morning_brief", 1);
/// Evening Wrap lines (Batch lane, `ModelClass::Brief`, `daemon::wrap_generation`).
pub const EVENING_WRAP: PromptTag = PromptTag::new("evening_wrap", 1);
/// Weekly source-backed About You identity profile, by section (authorized Batch lane).
pub const ABOUT_PROFILE: PromptTag = PromptTag::new("about_profile", 3);
/// Model-written completed-day activity summary (authorized Batch lane).
pub const DAILY_ACTIVITY_SUMMARY: PromptTag = PromptTag::new("daily_activity_summary", 1);
/// Which words a user fixed after a dictation are names/terms (relay `/v1/review`, `dictionary_learn`).
pub const DICTIONARY_TERMS: PromptTag = PromptTag::new("dictionary_terms", 1);
/// Live meeting summary (Agent lane, the user's own credential, since #782; a budget-model lane).
pub const MEETING_LIVE_SUMMARY: PromptTag = PromptTag::new("meeting_live_summary", 2);
/// Live meeting translation (Agent lane, the user's own credential, since #782; a budget-model lane).
pub const MEETING_LIVE_TRANSLATE: PromptTag = PromptTag::new("meeting_live_translate", 1);
/// Voice dictation cleanup (Groq edit model, `voice_editor`).
pub const VOICE_EDIT: PromptTag = PromptTag::new("voice_edit", 1);
/// Mail triage Stage B classification (issue #319, Batch / Select-KK lane).
pub const MAIL_TRIAGE: PromptTag = PromptTag::new("mail_triage", 1);
/// Summary of a fetched page / post / video transcript for chat (issue #555, Agent lane only —
/// `web_fetch::prompt`).
pub const WEB_SUMMARY: PromptTag = PromptTag::new("web_summary", 1);
/// A user-authored skill run (issue #251, Agent lane). The skill body is the user's own text.
pub const USER_SKILL: PromptTag = PromptTag::new("user_skill", 1);
/// Drafting a skill file from a finished one-off chat request (issue #251 "save as skill").
pub const SKILL_DRAFT: PromptTag = PromptTag::new("skill_draft", 1);
/// A translation on the Agent lane with the user's own credential (issue #279, invariant 5): the
/// Memory API's `translate.text` and the notch panel's Quick Translate (#782). The prompt
/// and its output gate are the same on both (`translate::quick_translate_system_prompt`).
pub const AGENT_TRANSLATE: PromptTag = PromptTag::new("agent_translate", 1);
/// AI memory search, step 1: the question rewritten into local FTS queries (issue #940, Agent
/// lane only — `ai_search::expansion_system_prompt`). Latency-bound: keyword results are already
/// on screen while it runs.
pub const MEMORY_SEARCH_EXPAND: PromptTag = PromptTag::new("memory_search_expand", 1).with_effort(Effort::Low);
/// AI memory search, step 2: the bounded keyword candidates reranked against the question
/// (issue #940, Agent lane only — `ai_search::rerank_system_prompt`).
pub const MEMORY_SEARCH_RERANK: PromptTag = PromptTag::new("memory_search_rerank", 2).with_effort(Effort::Low);
/// AI memory search, step 3 on the desktop: a short streamed answer over the reranked excerpts
/// (Agent lane only — `ai_search::summary_system_prompt`).
pub const MEMORY_SEARCH_SUMMARY: PromptTag = PromptTag::new("memory_search_summary", 1).with_effort(Effort::Low);

/// Hourly style-note mining over the user's own edited rewrites (#1066, Agent lane only —
/// `rewrite_mining`). One call per run, and none when nothing new was edited.
pub const STYLE_NOTE_MINE: PromptTag = PromptTag::new("style_note_mine", 1);
/// Miniature chat-memory extraction for delegates without tool calls.
pub const CHAT_MEMORY: PromptTag = PromptTag::new("chat_memory", 2).with_effort(Effort::Low);

/// Every registered prompt. New tags must be added here — the tests below turn a forgotten or
/// colliding registration into a failure instead of a silent mislabelled metric.
pub const ALL: &[PromptTag] = &[
    CHAT_MEMORY,
    INLINE_DRAFT,
    INLINE_CHAT,
    MEETING_CHAT,
    SCRIBE_EDIT,
    REPLY_DRAFT,
    MEETING_MINUTES,
    DREAM_CONSOLIDATION,
    ENTITY_DEDUPE,
    LOOPS_CONSOLIDATE,
    ENTITY_PAGE,
    MORNING_BRIEF,
    EVENING_WRAP,
    ABOUT_PROFILE,
    DAILY_ACTIVITY_SUMMARY,
    DICTIONARY_TERMS,
    MEETING_LIVE_SUMMARY,
    MEETING_LIVE_TRANSLATE,
    VOICE_EDIT,
    MAIL_TRIAGE,
    WEB_SUMMARY,
    USER_SKILL,
    SKILL_DRAFT,
    AGENT_TRANSLATE,
    MEMORY_SEARCH_EXPAND,
    MEMORY_SEARCH_RERANK,
    MEMORY_SEARCH_SUMMARY,
    STYLE_NOTE_MINE,
];

/// Digest of a prompt's *instruction half* (ours — never the untrusted/user half), for callers
/// that want to detect template drift across builds. Same xxh64 the traceability layer uses; a
/// digest, never the text.
pub fn instruction_digest(instruction_half: &str) -> String {
    super::traceability::digest(instruction_half)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for tag in ALL {
            assert!(seen.insert(tag.id), "duplicate prompt id: {}", tag.id);
        }
    }

    /// FR-HN-02(b): a forgotten registration fails here. `PromptTag::new` is private to this
    /// module, so every tag any call site can pass is a `PromptTag` const declared in this file;
    /// each one must also be in [`ALL`], or its usage rows would carry an id the registry (and
    /// everything that iterates it) does not know.
    #[test]
    fn every_declared_tag_is_registered() {
        let source = include_str!("prompts.rs");
        let registered: std::collections::HashSet<&str> = ALL.iter().map(|t| t.id).collect();
        let declared: Vec<&str> = source
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("pub const ") && l.contains(": PromptTag = PromptTag::new(\""))
            .filter_map(|l| l.split("PromptTag::new(\"").nth(1)?.split('"').next())
            .collect();
        assert_eq!(declared.len(), ALL.len(), "declared {declared:?}");
        for id in declared {
            assert!(registered.contains(id), "prompt `{id}` is declared but not in ALL");
        }
    }

    #[test]
    fn ids_are_stable_snake_case_ascii() {
        for tag in ALL {
            assert!(!tag.id.is_empty());
            assert!(
                tag.id.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "prompt id must be snake_case ascii: {}",
                tag.id
            );
            assert!(tag.version >= 1, "versions start at 1: {}", tag.id);
        }
    }

    #[test]
    fn effort_policy_matches_issue_388() {
        // Caret edits are latency-bound; chat gets the middle setting; every other prompt stays
        // on the backend's own default rather than a guess.
        assert_eq!(INLINE_DRAFT.effort, Some(Effort::Low));
        assert_eq!(SCRIBE_EDIT.effort, Some(Effort::Low));
        assert_eq!(INLINE_CHAT.effort, Some(Effort::Medium));
        assert_eq!(MEETING_CHAT.effort, Some(Effort::Medium));
        // AI memory search (#940) refines results already on screen: latency-bound, like a caret edit.
        assert_eq!(MEMORY_SEARCH_EXPAND.effort, Some(Effort::Low));
        assert_eq!(MEMORY_SEARCH_RERANK.effort, Some(Effort::Low));
        assert_eq!(MEMORY_SEARCH_SUMMARY.effort, Some(Effort::Low));
        assert_eq!(CHAT_MEMORY.effort, Some(Effort::Low));
        let reviewed = [
            CHAT_MEMORY,
            INLINE_DRAFT,
            SCRIBE_EDIT,
            INLINE_CHAT,
            MEETING_CHAT,
            MEMORY_SEARCH_EXPAND,
            MEMORY_SEARCH_RERANK,
            MEMORY_SEARCH_SUMMARY,
        ];
        for tag in ALL {
            if !reviewed.contains(tag) {
                assert_eq!(tag.effort, None, "{} has an unreviewed effort policy", tag.id);
            }
        }
        assert_eq!([Effort::Low, Effort::Medium, Effort::High].map(Effort::as_str), ["low", "medium", "high"]);
    }

    #[test]
    fn the_digest_changes_when_the_instruction_text_changes() {
        let a = instruction_digest("Draft only the body.");
        let b = instruction_digest("Draft only the body!");
        assert_ne!(a, b);
        assert_eq!(a, instruction_digest("Draft only the body."));
    }
}
