//! Translation and reply drafts asked for over the Memory API (issue #279, invariant 6).
//!
//! The notch panel translates a selection and drafts a reply; MCP / REST / CLI can ask for
//! the same two things (`translate.text`, `translate.reply_draft`). This module is the one
//! function both kinds of request run through once the desktop has claimed them: the same
//! prompt builders, the same output gates and the same strict draft parse the panel uses
//! ([`crate::translate`], [`crate::reply_draft`]), so an AI caller cannot get a translation the
//! panel would have refused, or a "draft" with the model's preamble in it.
//!
//! **Lane (invariant 5).** A request from an AI client is agent work: it runs on the user's own
//! credential (subscription delegation or an API key) through whatever [`AgentClient`] the
//! desktop builds — the same lane the panel's Quick Translate runs on since #782, and never the
//! company-funded Select KK key. The caller of [`run`] supplies the client, so this module cannot
//! reach any key at all.
//!
//! **It cannot send (FR-TL-14, invariant 4).** The output is JSON with text in it. There is no
//! transport, no `SendAction`, and no path to one; inserting and sending stay with the user.
//!
//! **No screen.** The panel's reply draft reads the live screen block (#296); an API request does
//! not. The caller hands over the thread it wants answered, and nothing it did not send is added.
//!
//! Pure: no I/O, no clock, no Keychain. Tested on Linux with a scripted client.

use serde_json::{json, Value};

use crate::llm::{AgentClient, LlmError};
use crate::reply_draft::{
    parse_reply_draft, reply_draft_system_prompt, reply_draft_user_block, resolve_reply_language,
    validate_thread, ReplyDraftOptions, ReplyDraftRejection, ReplyLanguageSource, MAX_THREAD_CHARS,
};
use crate::translate::{
    quick_translate_system_prompt, resolve_target, translate_in_chunks, validate_input, Formality,
    Language, LanguagePair, TargetSource, TranslateOptions, TranslateRejection,
    MAX_QUICK_TRANSLATE_CHARS,
};
use crate::translate_prefs::{formality_from_tag, formality_tag, TranslatePrefs};

/// The job kinds, as the queue's `kind` column spells them.
pub const KIND_TRANSLATE: &str = "translate";
pub const KIND_REPLY_DRAFT: &str = "reply_draft";

/// A tone instruction longer than this is a prompt, not an adjustment (same cap as the panel).
pub const MAX_ADJUSTMENT_CHARS: usize = 200;

/// One request, validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobRequest {
    /// Translate `text` into `target`, or into the other side of the pair when `target` is absent.
    Translate {
        text: String,
        target: Option<Language>,
        formality: Option<Formality>,
        app: Option<String>,
    },
    /// Draft a reply to `incoming`, in its own language unless `reply_in` says otherwise, with a
    /// back-translation into the user's language.
    ReplyDraft {
        incoming: String,
        reply_in: Option<Language>,
        adjustment: Option<String>,
        formality: Option<Formality>,
        app: Option<String>,
    },
}

/// Why a request body was refused before a row was written. Stable wire codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestError {
    InvalidBody,
    MissingText,
    TextTooLong,
    UnknownLanguage,
    UnknownFormality,
    InvalidApp,
}

impl RequestError {
    pub fn code(self) -> &'static str {
        match self {
            RequestError::InvalidBody => "invalid_request",
            RequestError::MissingText => "missing_text",
            RequestError::TextTooLong => "text_too_long",
            RequestError::UnknownLanguage => "unknown_language",
            RequestError::UnknownFormality => "unknown_formality",
            RequestError::InvalidApp => "invalid_app",
        }
    }

    /// One sentence for the caller. No request text in it.
    pub fn message(self) -> String {
        match self {
            RequestError::InvalidBody => "the request must be a JSON object".to_string(),
            RequestError::MissingText => "nothing to translate or reply to".to_string(),
            RequestError::TextTooLong => format!(
                "too long: translate up to {MAX_QUICK_TRANSLATE_CHARS} characters, reply to a thread of up to {MAX_THREAD_CHARS}"
            ),
            RequestError::UnknownLanguage => "language must be en or ja".to_string(),
            RequestError::UnknownFormality => {
                "formality must be one of match, casual, polite or formal".to_string()
            }
            RequestError::InvalidApp => "app must be a bundle id".to_string(),
        }
    }
}

fn optional_str<'a>(value: &'a Value, key: &str) -> Result<Option<&'a str>, RequestError> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.trim())),
        Some(_) => Err(RequestError::InvalidBody),
    }
}

fn language(value: &Value, key: &str) -> Result<Option<Language>, RequestError> {
    optional_str(value, key)?
        .map(|tag| Language::from_tag(tag).ok_or(RequestError::UnknownLanguage))
        .transpose()
}

fn formality(value: &Value) -> Result<Option<Formality>, RequestError> {
    optional_str(value, "formality")?
        .map(|tag| formality_from_tag(tag).ok_or(RequestError::UnknownFormality))
        .transpose()
}

fn app(value: &Value) -> Result<Option<String>, RequestError> {
    match optional_str(value, "app") {
        Ok(Some(id)) if id.len() <= 255 && !id.chars().any(char::is_whitespace) => {
            Ok(Some(id.to_string()))
        }
        Ok(Some(_)) | Err(_) => Err(RequestError::InvalidApp),
        Ok(None) => Ok(None),
    }
}

impl JobRequest {
    /// The queue's `kind` for this request.
    pub fn kind(&self) -> &'static str {
        match self {
            JobRequest::Translate { .. } => KIND_TRANSLATE,
            JobRequest::ReplyDraft { .. } => KIND_REPLY_DRAFT,
        }
    }

    /// Parse and validate a caller's body. Everything that can be refused without a model is
    /// refused here, where it is asked, so a bad request never becomes a row.
    ///
    /// `translate`: `{"text", "target"?: "en"|"ja", "formality"?, "app"?}`.
    /// `reply_draft`: `{"incoming", "reply_in"?: "en"|"ja", "adjustment"?, "formality"?, "app"?}`.
    pub fn parse(kind: &str, body: &str) -> Result<JobRequest, RequestError> {
        let value: Value = serde_json::from_str(body).map_err(|_| RequestError::InvalidBody)?;
        if !value.is_object() {
            return Err(RequestError::InvalidBody);
        }
        let formality = formality(&value)?;
        let app = app(&value)?;
        match kind {
            KIND_TRANSLATE => {
                let text = optional_str(&value, "text")?.ok_or(RequestError::MissingText)?;
                match validate_input(text) {
                    Ok(()) => {}
                    Err(TranslateRejection::Empty) => return Err(RequestError::MissingText),
                    Err(TranslateRejection::TooLong { .. }) => {
                        return Err(RequestError::TextTooLong)
                    }
                }
                Ok(JobRequest::Translate {
                    text: text.to_string(),
                    target: language(&value, "target")?,
                    formality,
                    app,
                })
            }
            KIND_REPLY_DRAFT => {
                let incoming =
                    optional_str(&value, "incoming")?.ok_or(RequestError::MissingText)?;
                match validate_thread(incoming) {
                    Ok(()) => {}
                    Err(ReplyDraftRejection::NothingToReplyTo) => {
                        return Err(RequestError::MissingText)
                    }
                    Err(ReplyDraftRejection::TooLong { .. }) => {
                        return Err(RequestError::TextTooLong)
                    }
                }
                let adjustment = optional_str(&value, "adjustment")?
                    .map(|a| a.chars().take(MAX_ADJUSTMENT_CHARS).collect::<String>());
                Ok(JobRequest::ReplyDraft {
                    incoming: incoming.to_string(),
                    reply_in: language(&value, "reply_in")?,
                    adjustment,
                    formality,
                    app,
                })
            }
            _ => Err(RequestError::InvalidBody),
        }
    }

    /// The stored form (the queue's `request` column). Round-trips through [`Self::parse`].
    pub fn to_json(&self) -> String {
        let tag = |l: &Option<Language>| l.map(Language::tag);
        let reg = |f: &Option<Formality>| f.map(formality_tag);
        match self {
            JobRequest::Translate {
                text,
                target,
                formality,
                app,
            } => json!({
                "text": text, "target": tag(target), "formality": reg(formality), "app": app,
            }),
            JobRequest::ReplyDraft {
                incoming,
                reply_in,
                adjustment,
                formality,
                app,
            } => json!({
                "incoming": incoming, "reply_in": tag(reply_in), "adjustment": adjustment,
                "formality": reg(formality), "app": app,
            }),
        }
        .to_string()
    }

    /// Rewrite every free-text field (the masking hook: the queue stores what this returns).
    pub fn map_text(self, mut f: impl FnMut(&str) -> String) -> JobRequest {
        match self {
            JobRequest::Translate {
                text,
                target,
                formality,
                app,
            } => JobRequest::Translate {
                text: f(&text),
                target,
                formality,
                app,
            },
            JobRequest::ReplyDraft {
                incoming,
                reply_in,
                adjustment,
                formality,
                app,
            } => JobRequest::ReplyDraft {
                incoming: f(&incoming),
                reply_in,
                adjustment: adjustment.map(|a| f(&a)),
                formality,
                app,
            },
        }
    }

    fn register(&self, prefs: &TranslatePrefs) -> Formality {
        let (explicit, app) = match self {
            JobRequest::Translate { formality, app, .. }
            | JobRequest::ReplyDraft { formality, app, .. } => (formality, app),
        };
        // An explicit register wins; else the one remembered for the named app, else the
        // default — the same resolution the panel's chips feed (FR-TL-07).
        explicit.unwrap_or_else(|| prefs.formality_for(app.as_deref()))
    }
}

/// What the desktop knows that the request does not: the pair in force, the remembered
/// registers, and the voice dictionary's terms (FR-TL-09).
#[derive(Debug, Clone, Default)]
pub struct JobSettings {
    pub pair: LanguagePair,
    pub prefs: TranslatePrefs,
    pub glossary: Vec<String>,
}

/// Why a claimed request produced nothing. [`Self::code`] is what the queue stores and the caller
/// sees; provider text never leaves this type.
#[derive(Debug)]
pub enum JobFailure {
    /// The request no longer validates (it was stored by an older build, or tampered with).
    Rejected(RequestError),
    /// The model answered with something that is not a translation / not a two-part draft.
    UnusableOutput,
    /// The provider call failed.
    Llm(LlmError),
}

impl JobFailure {
    pub fn code(&self) -> &'static str {
        match self {
            JobFailure::Rejected(e) => e.code(),
            JobFailure::UnusableOutput => "unusable_output",
            JobFailure::Llm(LlmError::Unauthorized(..)) => "key_rejected",
            JobFailure::Llm(LlmError::RateLimited(..)) => "rate_limited",
            JobFailure::Llm(LlmError::NeedsUpdate(..)) => "needs_update",
            JobFailure::Llm(_) => "provider_error",
        }
    }
}

fn target_source(source: TargetSource) -> &'static str {
    match source {
        TargetSource::Explicit => "explicit",
        TargetSource::Detected => "detected",
        TargetSource::PairDefault => "pair_default",
    }
}

fn reply_source(source: ReplyLanguageSource) -> &'static str {
    match source {
        ReplyLanguageSource::Explicit => "explicit",
        ReplyLanguageSource::Incoming => "incoming",
        ReplyLanguageSource::PairDefault => "pair_default",
    }
}

/// Run one request on `agent` and return the result JSON.
///
/// The system half is ours (the prompt builders); the user half is the caller's text, which the
/// prompts already treat as content, never as instructions (#123).
pub fn run<A: AgentClient + ?Sized>(
    request: &JobRequest,
    settings: &JobSettings,
    agent: &A,
) -> Result<Value, JobFailure> {
    let register = request.register(&settings.prefs);
    match request {
        JobRequest::Translate { text, target, .. } => {
            validate_input(text).map_err(|e| {
                JobFailure::Rejected(match e {
                    TranslateRejection::Empty => RequestError::MissingText,
                    TranslateRejection::TooLong { .. } => RequestError::TextTooLong,
                })
            })?;
            let resolved = resolve_target(text, settings.pair, *target);
            let opts = TranslateOptions {
                formality: register,
                glossary: settings.glossary.clone(),
            };
            let system = quick_translate_system_prompt(resolved.target, &opts);
            // A long text goes in parts; FR-TL-05: every part passes the panel's own gate —
            // preamble, quote-wrapping, refusals.
            let clean = translate_in_chunks(text, |part| agent.complete_split(&system, part))
                .map_err(JobFailure::Llm)?
                .ok_or(JobFailure::UnusableOutput)?;
            Ok(json!({
                "kind": KIND_TRANSLATE,
                "text": clean,
                "target": resolved.target.tag(),
                "detected_source": resolved.detected_source.map(Language::tag),
                "target_source": target_source(resolved.source),
                "formality": formality_tag(register),
            }))
        }
        JobRequest::ReplyDraft {
            incoming,
            reply_in,
            adjustment,
            ..
        } => {
            validate_thread(incoming).map_err(|e| {
                JobFailure::Rejected(match e {
                    ReplyDraftRejection::NothingToReplyTo => RequestError::MissingText,
                    ReplyDraftRejection::TooLong { .. } => RequestError::TextTooLong,
                })
            })?;
            let language = resolve_reply_language(incoming, settings.pair, *reply_in);
            let opts = ReplyDraftOptions {
                formality: register,
                glossary: settings.glossary.clone(),
                adjustment: adjustment.clone(),
            };
            let system = reply_draft_system_prompt(language, &opts);
            // No screen block: an API caller's draft rests only on the thread it handed over.
            let user = reply_draft_user_block(incoming, None);
            let raw = agent
                .complete_split(&system, &user)
                .map_err(JobFailure::Llm)?;
            // Strict (FR-TL-12): both sections or nothing — never the raw answer as a "draft".
            let draft =
                parse_reply_draft(&raw, language).map_err(|_| JobFailure::UnusableOutput)?;
            let placeholders = draft.placeholders();
            let redundant = draft.back_translation_is_redundant();
            Ok(json!({
                "kind": KIND_REPLY_DRAFT,
                "draft": draft.draft,
                "back_translation": draft.back_translation,
                "reply_in": language.reply_in.tag(),
                "back_into": language.back_into.tag(),
                "language_source": reply_source(language.source),
                "formality": formality_tag(register),
                // FR-TL-13 (3): the caller must fill these before anyone sends the draft.
                "placeholders": placeholders,
                "back_translation_redundant": redundant,
                // FR-TL-14: nothing was sent, and nothing here can send.
                "sent": false,
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// A scripted Agent-lane client: records the two halves and answers with `reply`.
    struct Scripted {
        reply: Result<String, LlmError>,
        seen: Mutex<Vec<(String, String)>>,
    }

    impl Scripted {
        fn ok(reply: &str) -> Self {
            Scripted {
                reply: Ok(reply.to_string()),
                seen: Mutex::new(Vec::new()),
            }
        }
        fn halves(&self) -> (String, String) {
            self.seen.lock().unwrap()[0].clone()
        }
    }

    impl AgentClient for Scripted {
        fn complete(&self, prompt: &str) -> Result<String, LlmError> {
            self.complete_split("", prompt)
        }
        fn complete_split(&self, system: &str, user: &str) -> Result<String, LlmError> {
            self.seen
                .lock()
                .unwrap()
                .push((system.to_string(), user.to_string()));
            match &self.reply {
                Ok(text) => Ok(text.clone()),
                Err(LlmError::Unauthorized(s, m)) => Err(LlmError::Unauthorized(*s, m.clone())),
                Err(_) => Err(LlmError::Provider("down".into())),
            }
        }
    }

    fn ja_user() -> JobSettings {
        JobSettings {
            pair: LanguagePair::from_locale("ja_JP"),
            ..JobSettings::default()
        }
    }

    #[test]
    fn a_translation_runs_the_panels_prompt_and_gate() {
        let req = JobRequest::parse(
            KIND_TRANSLATE,
            r#"{"text":"Could you send the invoice by Friday?"}"#,
        )
        .unwrap();
        let agent = Scripted::ok("\"金曜日までに請求書を送っていただけますか？\"");
        let out = run(&req, &ja_user(), &agent).unwrap();
        // The quote-wrapping the panel strips is stripped here too.
        assert_eq!(out["text"], "金曜日までに請求書を送っていただけますか？");
        assert_eq!(out["target"], "ja");
        assert_eq!(out["detected_source"], "en");
        assert_eq!(out["target_source"], "detected");
        let (system, user) = agent.halves();
        assert!(
            system.starts_with("Translate the user's text into Japanese."),
            "{system}"
        );
        assert_eq!(
            user, "Could you send the invoice by Friday?",
            "the text rides the untrusted half"
        );
    }

    #[test]
    fn an_explicit_target_register_and_the_glossary_reach_the_prompt() {
        let req = JobRequest::parse(
            KIND_TRANSLATE,
            r#"{"text":"了解です","target":"en","formality":"formal"}"#,
        )
        .unwrap();
        let settings = JobSettings {
            glossary: vec!["SHOGUN".into()],
            ..ja_user()
        };
        let agent = Scripted::ok("Understood.");
        let out = run(&req, &settings, &agent).unwrap();
        assert_eq!(
            (out["target"].as_str(), out["target_source"].as_str()),
            (Some("en"), Some("explicit"))
        );
        assert_eq!(out["formality"], "formal");
        let (system, _) = agent.halves();
        assert!(system.contains("formal business register"), "{system}");
        assert!(system.contains("SHOGUN"), "{system}");
    }

    #[test]
    fn a_remembered_app_register_applies_when_the_caller_names_the_app() {
        let mut prefs = TranslatePrefs::default();
        prefs.remember(Some("com.tinyspeck.slackmacgap"), Formality::Casual);
        let settings = JobSettings { prefs, ..ja_user() };
        let req = JobRequest::parse(
            KIND_TRANSLATE,
            r#"{"text":"Thanks, see you tomorrow","app":"com.tinyspeck.slackmacgap"}"#,
        )
        .unwrap();
        let out = run(&req, &settings, &Scripted::ok("ありがとう、また明日")).unwrap();
        assert_eq!(out["formality"], "casual");
    }

    #[test]
    fn a_refusal_is_not_handed_back_as_a_translation() {
        let req = JobRequest::parse(KIND_TRANSLATE, r#"{"text":"hello there, friend"}"#).unwrap();
        let err = run(
            &req,
            &ja_user(),
            &Scripted::ok("As an AI, I cannot help with that."),
        )
        .unwrap_err();
        assert_eq!(err.code(), "unusable_output");
    }

    #[test]
    fn a_reply_is_drafted_in_their_language_with_a_back_translation_and_never_sent() {
        let req = JobRequest::parse(
            KIND_REPLY_DRAFT,
            r#"{"incoming":"Hi Taro, can we move the review to Thursday?","adjustment":"shorter"}"#,
        )
        .unwrap();
        let raw = "###DRAFT###\nHi Anna, Thursday works for me.\n###BACK###\nアンナさん、木曜日で大丈夫です。";
        let agent = Scripted::ok(raw);
        let out = match run(&req, &ja_user(), &agent) {
            Ok(out) => out,
            Err(e) => {
                // The marker spelling belongs to reply_draft; if it changes, this test must too.
                panic!("parse failed ({}); markers are reply_draft's", e.code())
            }
        };
        assert_eq!(out["reply_in"], "en");
        assert_eq!(out["back_into"], "ja");
        assert_eq!(out["language_source"], "incoming");
        assert_eq!(out["sent"], false);
        assert!(out["draft"].as_str().unwrap().contains("Thursday works"));
        let (system, user) = agent.halves();
        assert!(
            system.contains("shorter"),
            "the adjustment is ours: {system}"
        );
        assert!(user.contains("move the review to Thursday"));
        assert!(
            !user.contains("<screen"),
            "an API draft never reads the screen: {user}"
        );
    }

    #[test]
    fn a_draft_without_both_sections_is_a_failure_not_a_draft() {
        let req =
            JobRequest::parse(KIND_REPLY_DRAFT, r#"{"incoming":"Can we talk tomorrow?"}"#).unwrap();
        let err = run(
            &req,
            &ja_user(),
            &Scripted::ok("Sure! Here's a draft: Yes, tomorrow is fine."),
        )
        .unwrap_err();
        assert_eq!(err.code(), "unusable_output");
    }

    #[test]
    fn provider_failures_become_codes_without_provider_text() {
        let req = JobRequest::parse(KIND_TRANSLATE, r#"{"text":"hello"}"#).unwrap();
        let agent = Scripted {
            reply: Err(LlmError::Unauthorized(401, "invalid key".into())),
            seen: Mutex::new(Vec::new()),
        };
        assert_eq!(
            run(&req, &ja_user(), &agent).unwrap_err().code(),
            "key_rejected"
        );
        let down = Scripted {
            reply: Err(LlmError::Provider("x".into())),
            seen: Mutex::new(Vec::new()),
        };
        assert_eq!(
            run(&req, &ja_user(), &down).unwrap_err().code(),
            "provider_error"
        );
        // A subscription CLI too old for SHOGUN's flags is its own code, not a provider error.
        assert_eq!(
            JobFailure::Llm(LlmError::NeedsUpdate("update it".into())).code(),
            "needs_update"
        );
    }

    #[test]
    fn bad_bodies_are_refused_before_a_row_exists() {
        let cases = [
            (KIND_TRANSLATE, "nonsense", "invalid_request"),
            (KIND_TRANSLATE, "[1]", "invalid_request"),
            (KIND_TRANSLATE, "{}", "missing_text"),
            (KIND_TRANSLATE, r#"{"text":"  ...  "}"#, "missing_text"),
            (
                KIND_TRANSLATE,
                r#"{"text":"hi","target":"fr"}"#,
                "unknown_language",
            ),
            (
                KIND_TRANSLATE,
                r#"{"text":"hi","formality":"shouty"}"#,
                "unknown_formality",
            ),
            (
                KIND_TRANSLATE,
                r#"{"text":"hi","app":"has space"}"#,
                "invalid_app",
            ),
            (KIND_TRANSLATE, r#"{"text":7}"#, "invalid_request"),
            (KIND_REPLY_DRAFT, r#"{"text":"hi"}"#, "missing_text"),
            (
                KIND_REPLY_DRAFT,
                r#"{"incoming":"hi","reply_in":"de"}"#,
                "unknown_language",
            ),
            ("summarise", r#"{"text":"hi"}"#, "invalid_request"),
        ];
        for (kind, body, code) in cases {
            assert_eq!(
                JobRequest::parse(kind, body).unwrap_err().code(),
                code,
                "{kind} {body}"
            );
        }
        let long = "a".repeat(MAX_QUICK_TRANSLATE_CHARS + 1);
        let body = json!({ "text": long }).to_string();
        assert_eq!(
            JobRequest::parse(KIND_TRANSLATE, &body).unwrap_err().code(),
            "text_too_long"
        );
        // A thread is accepted up to its own cap.
        let body = json!({ "incoming": "b".repeat(MAX_THREAD_CHARS) }).to_string();
        assert!(JobRequest::parse(KIND_REPLY_DRAFT, &body).is_ok());
    }

    #[test]
    fn the_stored_form_round_trips_and_masking_touches_only_text() {
        let req = JobRequest::parse(
            KIND_REPLY_DRAFT,
            r#"{"incoming":"ping","reply_in":"ja","adjustment":"warmer","formality":"polite","app":"com.apple.mail"}"#,
        )
        .unwrap();
        assert_eq!(JobRequest::parse(req.kind(), &req.to_json()).unwrap(), req);
        let masked = req.map_text(|t| t.to_uppercase());
        let JobRequest::ReplyDraft {
            incoming,
            adjustment,
            app,
            ..
        } = &masked
        else {
            panic!("kind changed")
        };
        assert_eq!(
            (incoming.as_str(), adjustment.as_deref()),
            ("PING", Some("WARMER"))
        );
        assert_eq!(app.as_deref(), Some("com.apple.mail"), "ids are not text");
    }
}
