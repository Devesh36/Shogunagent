//! Which credential the Agent lane runs on (issue #110), shared by the desktop and the API faces
//! (invariant 6).
//!
//! The choice lives in `llm.json` next to `memory.db` (non-secret: a provider id, a model id and
//! the subscription opt-in; every key stays in the Keychain, invariant 7). The desktop owns the
//! Settings screen and runs the lane; MCP / REST / CLI read and change the same file through
//! `agent.source.get` / `agent.source.set`.
//!
//! The write is not one level:
//! - Selecting an API-key provider, changing the model, or withdrawing the subscription opt-in
//!   only narrows where prompts go. Those apply directly (L1).
//! - Selecting a subscription CLI (`claude-code`, `codex`, `gemini-cli`) routes every later chat
//!   and draft prompt through a vendor's consumer plan, whose data handling is not the API path's.
//!   The desktop asks for an explicit opt-in before that, and an AI caller cannot give it on the
//!   user's behalf: the request is queued for the user's in-app approval (L3), and the approval
//!   card carries the same disclosure. [`apply_direct`] refuses it, so a face that forgot to route
//!   it to the queue fails closed.
//! - `openai_store` (issue #390, owner decision 2026-09-25 on #664): whether OpenAI may keep the
//!   Agent lane's responses on its side, which `previous_response_id` continuation needs. Default
//!   off. Turning it **off** only shortens what a third party keeps, so it is direct (L1). Turning
//!   it **on** is refused on the API ([`Invalid::StoreOnlyInApp`]), not queued: the user turns it
//!   on at the toggle in Settings, which carries the retention disclosure. The same direction as
//!   the Mail policy's loosening moves (`connections.composio.set`, #82). Restating a setting
//!   already in force is a no-op either way.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{json, Value};

use crate::llm::subscription::{BridgedDelegate, Delegate, DelegateState};

/// The settings file, in the desktop's app-data directory.
pub const SETTINGS_FILE: &str = "llm.json";
/// Overrides where the API faces look for [`SETTINGS_FILE`].
pub const SETTINGS_ENV: &str = "SHOGUN_LLM_SETTINGS";

/// API-key providers the Agent lane can run on. The Batch lane is untouched by this choice: it
/// stays on the Select KK lane (invariant 5).
pub const BYOK_PROVIDERS: [&str; 4] = ["anthropic", "openrouter", "openai", "gemini"];

/// The Agent lane's source, as stored in `llm.json`. The desktop reads and writes this exact
/// shape; keep field names stable (an install's file is years old).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LlmSettings {
    pub provider: String,
    /// Model id in the provider's own naming. Empty = the provider's default. Ignored for
    /// subscription delegates: the vendor CLI picks the model its plan allows.
    pub model: String,
    /// The user has read the subscription-route disclosure and opted in (issue #110). A delegate
    /// never runs without it: a defaulted-on disclosure is not consent.
    #[serde(default)]
    pub subscription_consent: bool,
    /// The user let OpenAI store the Agent lane's responses ("Let OpenAI store responses",
    /// issue #390; owner decision 2026-09-25 on #664). Default off, and a file written before the
    /// setting existed reads as off. Only an OpenAI selection uses it
    /// ([`Self::stores_openai_responses`]); it is kept while another provider is selected so
    /// switching back does not silently change the user's answer.
    #[serde(default)]
    pub openai_store: bool,
}

/// The source a Mac with no saved choice runs on: SHOGUN, the company-funded live gateway
/// (#1101 D1, owner decision 2026-10-21: the fixed default, with no picker in the app). A saved
/// API-key provider or subscription delegate is an explicit per-lane override and is kept.
pub const DEFAULT_PROVIDER: &str = "shogun";

impl Default for LlmSettings {
    fn default() -> Self {
        Self {
            provider: DEFAULT_PROVIDER.into(),
            model: String::new(),
            subscription_consent: false,
            openai_store: false,
        }
    }
}

impl LlmSettings {
    /// Whether the Agent lane asks the provider to store responses: only on OpenAI, and only
    /// after the user opted in. What `OpenAiResponsesConfig::for_agent` sends as `store`.
    pub fn stores_openai_responses(&self) -> bool {
        self.provider == "openai" && self.openai_store
    }
}

/// The tool-capable Agent lane a chat turn runs on (issues #81 / #390 / #396 / #484). Every one
/// runs on the user's own credential — an API key, or their own plan through Claude Code or
/// Codex (invariant 5); none is ever the Batch / Select KK lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatToolLane {
    /// Anthropic key: the Messages API tool loop (`llm::tool_turns`).
    AnthropicMessages,
    /// Company live gateway; availability is checked by the desktop licence gate.
    HostedResponses,
    /// OpenAI key: the Responses API tool loop (`llm::openai_responses::tool_turns`), always
    /// with `store: false` ([`CHAT_TOOL_LANE_STORES_RESPONSES`]).
    OpenAiResponses,
    /// OpenRouter or Gemini key: the `chat/completions` function-calling tool loop
    /// (`llm::openai_compat::tool_turns`, owner decision 2026-10-08 on #396). Stateless, like
    /// the OpenAI lane: each turn replays the history it needs.
    ChatCompletions,
    /// A subscription delegate, after the user's opt-in (issue #484): Claude Code or Codex. The
    /// CLI runs with SHOGUN's chat bridge as its only MCP server (`llm::delegate_tools`), and
    /// every call it makes goes through the same tool loop and gate as the API-key lanes. The
    /// Gemini CLI has no bridged form (`BridgedDelegate`), so it is never a tool lane.
    DelegateBridge(BridgedDelegate),
}

/// Whether the chat tool lane asks OpenAI to store responses. Pinned off (owner decision on
/// #390): no provider-side conversation storage, so every tool turn resends the history it needs
/// (stateless) and no response id is kept between chat turns. The "Let OpenAI store responses"
/// setting does not widen this lane.
pub const CHAT_TOOL_LANE_STORES_RESPONSES: bool = false;

/// Which lane may carry connected tools for this chat, or `None` when chat answers without them.
/// The one rule behind both the desktop tool loop and its "Connected tools" line, so the line
/// never promises tools the next answer will not use.
///
/// - `anthropic` with a saved key → [`ChatToolLane::AnthropicMessages`];
/// - `openai` with a saved key → [`ChatToolLane::OpenAiResponses`] (function tools live on the
///   Responses API only — [`crate::llm::openai_responses::prefers_responses_api`]);
/// - `openrouter` or `gemini` with a saved key → [`ChatToolLane::ChatCompletions`] (function
///   calling on their OpenAI-compatible `chat/completions`; owner decision 2026-10-08, #396);
/// - `claude-code` or `codex` with the subscription opt-in → [`ChatToolLane::DelegateBridge`]
///   (issue #484; no key is involved, so `has_byok_key` does not matter). Without the opt-in no
///   delegate runs at all;
/// - `gemini-cli` (its MCP servers can only come from a settings file SHOGUN cannot write
///   without widening the run — see `BridgedDelegate`), an unknown id, or no key → `None`: those
///   lanes answer without tools.
///
/// The desktop adds one fact of its own for the bridge: that the `shogun-mcp` it starts the CLI
/// with ships next to the app (otherwise the chat runs without tools, and the line says so).
pub fn chat_tool_lane(settings: &LlmSettings, has_byok_key: bool) -> Option<ChatToolLane> {
    if settings.provider == "shogun" { return Some(ChatToolLane::HostedResponses); }
    if let Some(delegate) = Delegate::parse(&settings.provider) {
        if !settings.subscription_consent {
            return None;
        }
        return BridgedDelegate::of(delegate).map(ChatToolLane::DelegateBridge);
    }
    if !has_byok_key {
        return None;
    }
    match settings.provider.as_str() {
        "anthropic" => Some(ChatToolLane::AnthropicMessages),
        p if crate::llm::openai_responses::prefers_responses_api(p, true) => {
            Some(ChatToolLane::OpenAiResponses)
        }
        "openrouter" | "gemini" => Some(ChatToolLane::ChatCompletions),
        _ => None,
    }
}

/// Whether live meeting translation may run on this Agent-lane source (owner decision 2026-10-03,
/// #782): **API-key providers only**.
///
/// Live translation sends one request per final caption line, up to three at a time. A
/// subscription delegate starts a vendor CLI process per request and answers in seconds to tens
/// of seconds, so captions would age out of their queue faster than they are translated, and
/// hundreds of runs an hour would spend the user's own plan window — the allowance their
/// interactive assistant shares (FR-AG-06a). So a delegate is refused before any process starts,
/// and the meeting says why. Live summary (one run per 45 s) and Quick Translate (one per press)
/// still run on a delegate.
///
/// Anything that is not a delegate is allowed here; whether a key is actually present is
/// `build_agent`'s answer, as on every other Agent-lane surface.
pub fn live_translation_allowed(provider: &str) -> bool {
    Delegate::parse(provider).is_none()
}

/// A provider id is valid if it is an API-key provider or a subscription delegate.
pub fn is_known_provider(provider: &str) -> bool {
    provider == "shogun" || BYOK_PROVIDERS.contains(&provider) || Delegate::parse(provider).is_some()
}

/// The Keychain account an API-key provider's key lives under (one per provider, so switching
/// never overwrites another key). `None` for a delegate: SHOGUN holds no subscription credential.
pub fn byok_keychain_account(provider: &str) -> Option<&'static str> {
    Some(match provider {
        "anthropic" => "anthropic-byok",
        "openrouter" => "openrouter-byok",
        "openai" => "openai-byok",
        "gemini" => "gemini-byok",
        _ => return None,
    })
}

/// Heuristics for "this is a credential, not a model id". Deliberately eager: a refused model
/// name costs a retry, a stored key costs the key.
pub fn looks_like_secret(value: &str) -> bool {
    let v = value.trim();
    v.len() >= 32
        || v.starts_with("sk-")
        || v.starts_with("AQ.")
        || v.starts_with("AIza")
        || v.starts_with("sk_")
}

/// Whether a writer may store `model` for `provider` over `current` (issue #389): a model the
/// provider lists, empty (its recommended model), or the value already on file for the same
/// provider — restating it is not a new choice, so an unrelated change (the OpenAI store toggle)
/// never fails on a model an older build wrote. A delegate's model is ignored (its CLI picks).
/// The one rule for the Settings screen and `agent.source.set`.
pub fn model_change_allowed(current: &LlmSettings, provider: &str, model: &str) -> bool {
    (provider == "shogun" && model.is_empty())
        || Delegate::parse(provider).is_some()
        || crate::model_catalog::is_selectable(provider, model)
        || (provider == current.provider && model.trim() == current.model.trim())
}

/// Whether `model` may be stored: empty (the provider default), or a short id in the model-id
/// alphabet that does not look like a key. Refusing here keeps a key pasted into the wrong field
/// out of `llm.json` (invariant 7).
pub fn plausible_model(model: &str) -> bool {
    let m = model.trim();
    m.is_empty()
        || (m.len() <= 64
            && !looks_like_secret(m)
            && m
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '/' | ':' | '_')))
}

/// `llm.json` for a daemon whose DB is at `db_path`: [`SETTINGS_ENV`] when set, otherwise beside
/// the DB (the desktop keeps both in its app-data directory).
pub fn resolve_path(db_path: &str) -> PathBuf {
    std::env::var(SETTINGS_ENV)
        .ok()
        .filter(|p| !p.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(db_path)
                .parent()
                .map_or_else(|| PathBuf::from(SETTINGS_FILE), |dir| dir.join(SETTINGS_FILE))
        })
}

/// What the desktop runs on: the saved settings, or the default when the file is missing,
/// unreadable, or names a provider this build does not know (the desktop's own startup rule).
pub fn load_effective(path: &Path) -> LlmSettings {
    load_strict(path).unwrap_or_default()
}

/// The saved settings for a writer. A missing file is the default. An unreadable or corrupt file
/// is an error, never silently replaced. What it returns is checked against the file's seal
/// (audit 2026-10-20 M6, `crate::settings_seal`): a source chosen outside the app is not
/// followed unless it is the default, and the subscription opt-in and OpenAI response storage
/// can only be withdrawn there.
pub fn load_strict(path: &Path) -> Result<LlmSettings, String> {
    let file = match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str::<LlmSettings>(&text)
            .ok()
            .filter(|s| is_known_provider(&s.provider))
            .ok_or_else(|| "agent source settings are unreadable".to_string())?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => LlmSettings::default(),
        Err(_) => return Err("agent source settings are unreadable".to_string()),
    };
    Ok(crate::settings_seal::trusted(path, file, crate::settings_seal::now_ms()))
}

/// Serializes writers in one process (the desktop's Settings command and a confirmed approval).
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Persist `settings` atomically (temp file + rename), so a reader in another process never sees
/// half a file.
pub fn save(path: &Path, settings: &LlmSettings) -> Result<(), String> {
    let _guard = WRITE_LOCK
        .lock()
        .map_err(|_| "agent source settings lock poisoned".to_string())?;
    write_atomic(path, settings)
}

fn write_atomic(path: &Path, settings: &LlmSettings) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("save failed: {e}"))?;
    }
    let json = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    std::fs::write(&tmp, json).map_err(|e| format!("save failed: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("save failed: {e}")
    })?;
    // The app's choice, a confirmed approval, or a narrowing API write: seal it (M6).
    crate::settings_seal::seal(path, settings);
    Ok(())
}

/// Why an `agent.source.set` body is refused. Every face answers 400 / invalid params.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invalid {
    /// Not a JSON object of the known fields, or nothing to change.
    BadRequest,
    /// `source` is not an API-key provider or a subscription delegate.
    UnknownSource,
    /// `model` is not a plausible model id (or looks like a key).
    ImplausibleModel,
    /// `model` is plausible but not one the selected API-key provider offers in this build
    /// (`model_catalog`, issue #389). The list is curated: `agent.source.get` returns it.
    /// OpenRouter alone also takes a well-formed free-form id (owner decision 2026-09-26).
    UnknownModel,
    /// Selecting a subscription without `subscription_consent: true` (the caller's attestation
    /// that the user saw the disclosure; necessary, never sufficient).
    ConsentRequired,
    /// `subscription_consent: true` without selecting a subscription. The opt-in is given by
    /// approving a subscription selection in the app, never on its own.
    ConsentNeedsSubscription,
    /// `openai_store: true` while it is off (module docs: only the Settings toggle turns it on).
    StoreOnlyInApp,
}

impl Invalid {
    pub fn code(self) -> &'static str {
        match self {
            Invalid::BadRequest => "invalid_agent_source_request",
            Invalid::UnknownSource => "unknown_agent_source",
            Invalid::ImplausibleModel => "invalid_model",
            Invalid::UnknownModel => "unknown_model",
            Invalid::ConsentRequired => "consent_required",
            Invalid::ConsentNeedsSubscription => "consent_needs_subscription_source",
            Invalid::StoreOnlyInApp => "openai_store_only_in_app",
        }
    }
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    source: Option<String>,
    model: Option<String>,
    subscription_consent: Option<bool>,
    openai_store: Option<bool>,
}

/// What a valid body does against the settings in force.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Applies directly (L1): the settings it writes.
    Direct(LlmSettings),
    /// Selects a subscription the user has not opted into for this source: L3, the user's
    /// in-app approval is the opt-in.
    NeedsApproval(Delegate),
}

/// Classify `body` against `current` without writing anything.
///
/// `openai_store` rides along with any direct change: `false` applies, `true` applies only when
/// it is already on (a restatement), and turning it on is [`Invalid::StoreOnlyInApp`]. It cannot
/// ride along with a subscription selection that waits for approval
/// ([`Invalid::BadRequest`]): the approval writes the selection only, so the store change would
/// be silently lost. Send it on its own.
pub fn plan(current: &LlmSettings, body: &str) -> Result<Plan, Invalid> {
    let req: Request = serde_json::from_str(body).map_err(|_| Invalid::BadRequest)?;
    if req.source.is_none()
        && req.model.is_none()
        && req.subscription_consent.is_none()
        && req.openai_store.is_none()
    {
        return Err(Invalid::BadRequest);
    }
    if req.openai_store == Some(true) && !current.openai_store {
        return Err(Invalid::StoreOnlyInApp);
    }
    let openai_store = req.openai_store.unwrap_or(current.openai_store);
    let source = match req.source.as_deref().map(str::trim) {
        Some(id) if !is_known_provider(id) => return Err(Invalid::UnknownSource),
        Some(id) => id.to_string(),
        None => current.provider.clone(),
    };
    if let Some(model) = &req.model {
        // A free-form OpenRouter id (owner decision 2026-09-26) has its own, longer format check
        // (`model_catalog::custom_model_ok`); anything else must look like a model id, not a key.
        if !plausible_model(model) && !crate::model_catalog::is_selectable(&source, model) {
            return Err(Invalid::ImplausibleModel);
        }
    }
    let same_provider = source == current.provider;
    let model = req
        .model
        .as_deref()
        .map(|m| m.trim().to_string())
        .unwrap_or_else(|| if same_provider { current.model.clone() } else { String::new() });
    match Delegate::parse(&source) {
        None => {
            // Issue #389: an API-key provider runs one of its listed models (empty = the
            // recommended one). A stored one carried over is left alone (it resolves to the
            // default with a note on read).
            if req.model.is_some() && !model_change_allowed(current, &source, &model) {
                return Err(Invalid::UnknownModel);
            }
            // An API-key provider. The opt-in can be withdrawn here, never granted.
            if req.subscription_consent == Some(true) {
                return Err(Invalid::ConsentNeedsSubscription);
            }
            Ok(Plan::Direct(LlmSettings {
                provider: source,
                model,
                subscription_consent: req.subscription_consent.unwrap_or(current.subscription_consent),
                openai_store,
            }))
        }
        Some(delegate) => {
            if req.subscription_consent == Some(false) {
                // Selecting it with the opt-in withdrawn: the delegate never runs (the desktop
                // refuses it without consent), so this only narrows egress.
                return Ok(Plan::Direct(LlmSettings {
                    provider: source,
                    model,
                    subscription_consent: false,
                    openai_store,
                }));
            }
            if same_provider && current.subscription_consent {
                // Already the consented source: nothing new leaves.
                return Ok(Plan::Direct(LlmSettings {
                    provider: source,
                    model,
                    subscription_consent: true,
                    openai_store,
                }));
            }
            // A new route through a vendor's consumer plan. The consent on file (if any) was
            // given for another vendor, or not at all, so the user approves this one.
            if req.subscription_consent != Some(true) {
                return Err(Invalid::ConsentRequired);
            }
            if req.openai_store.is_some() {
                return Err(Invalid::BadRequest);
            }
            Ok(Plan::NeedsApproval(delegate))
        }
    }
}

/// Classify `body` against the file on disk (the API faces' routing decision). `Ok(None)`: it
/// applies directly. `Ok(Some(delegate))`: it waits for the user's approval.
pub fn classify(path: &Path, body: &str) -> Result<Option<Delegate>, String> {
    let current = load_strict(path)?;
    match plan(&current, body) {
        Ok(Plan::Direct(_)) => Ok(None),
        Ok(Plan::NeedsApproval(delegate)) => Ok(Some(delegate)),
        Err(invalid) => Err(invalid.code().to_string()),
    }
}

/// Why [`apply_direct`] refused a subscription selection. Starts with "invalid " so the REST face
/// answers 400: the caller asked for something it may not do on its own.
pub const SUBSCRIPTION_NOT_APPROVED: &str = "invalid agent source request: using a subscription \
needs approval in the ShogunAI app";

/// Apply an `agent.source.set` from a caller that is not the user at the Settings screen. The
/// file is re-read under the write lock, and a body that would select an unconsented subscription
/// is refused, so a face that skipped the approval queue cannot grant the opt-in.
pub fn apply_direct(path: &Path, body: &str) -> Result<LlmSettings, String> {
    let _guard = WRITE_LOCK
        .lock()
        .map_err(|_| "agent source settings lock poisoned".to_string())?;
    let current = load_strict(path)?;
    match plan(&current, body) {
        Ok(Plan::Direct(next)) => {
            write_atomic(path, &next)?;
            Ok(next)
        }
        Ok(Plan::NeedsApproval(_)) => Err(SUBSCRIPTION_NOT_APPROVED.to_string()),
        Err(invalid) => Err(refusal(invalid)),
    }
}

/// The refusal text for an invalid body. Starts with "invalid " so the REST face answers 400;
/// the store refusal also says where the user turns it on.
pub fn refusal(invalid: Invalid) -> String {
    match invalid {
        Invalid::StoreOnlyInApp => format!(
            "invalid agent source request: {}: the user turns on OpenAI response storage in ShogunAI \
             Settings > Privacy, where its retention disclosure is",
            invalid.code()
        ),
        other => format!("invalid agent source request: {}", other.code()),
    }
}

/// Apply a confirmed L3 approval to use `delegate_id` (the user pressed the dedicated button on a
/// card that carried the disclosure): select it and record the opt-in.
pub fn apply_confirmed_subscription(path: &Path, delegate_id: &str) -> Result<LlmSettings, String> {
    let delegate = Delegate::parse(delegate_id).ok_or_else(|| "unknown subscription delegate".to_string())?;
    let _guard = WRITE_LOCK
        .lock()
        .map_err(|_| "agent source settings lock poisoned".to_string())?;
    // Refuse to replace a file SHOGUN cannot read rather than overwrite it blind.
    let current = load_strict(path)?;
    let next = LlmSettings {
        provider: delegate.id().to_string(),
        model: String::new(),
        subscription_consent: true,
        // The approval is about the subscription only; the OpenAI answer on file stands.
        openai_store: current.openai_store,
    };
    write_atomic(path, &next)?;
    Ok(next)
}

/// Clear a retired default model from `llm.json` (issue #389), so the install follows today's
/// recommended model and future ones. `Ok(Some(old))` when the file changed,
/// `Ok(None)` when there was nothing to do (missing file, delegate, a model the user picked, or
/// an unknown id: that one is kept and shown with a note rather than silently discarded). A
/// settings-file change only; no database is touched. Called once at desktop startup.
pub fn migrate_retired_model(path: &Path) -> Result<Option<String>, String> {
    let _guard = WRITE_LOCK
        .lock()
        .map_err(|_| "agent source settings lock poisoned".to_string())?;
    if !path.exists() {
        return Ok(None);
    }
    let current = load_strict(path)?;
    let resolved = crate::model_catalog::resolve(&current.provider, &current.model);
    if resolved.choice != crate::model_catalog::Choice::Retired {
        return Ok(None);
    }
    let old = current.model.trim().to_string();
    write_atomic(path, &LlmSettings { model: String::new(), ..current })?;
    Ok(Some(old))
}

fn byok_label(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "Anthropic",
        "openrouter" => "OpenRouter",
        "openai" => "OpenAI",
        "gemini" => "Gemini",
        _ => "",
    }
}

/// The `agent.source.get` answer. `detected` is the `--version` probe of every delegate (`None`
/// when this process cannot run it: `installed` is then `null`). `key_present` answers whether a
/// provider's key is in the Keychain without reading it (`None` = unknown). `openai_store` is the
/// user's "Let OpenAI store responses" answer.
/// `state` is `DelegateState::tag` (`needs_update` = installed but older than `min_version`,
/// SHOGUN's own constant for the oldest release it can run, `null` when none is pinned).
/// Nothing here carries a key, a version string or any vendor text.
pub fn status_json(
    settings: &LlmSettings,
    detected: Option<&std::collections::BTreeMap<Delegate, DelegateState>>,
    key_present: &dyn Fn(&str) -> Option<bool>,
) -> Value {
    let selected_delegate = Delegate::parse(&settings.provider);
    let mut sources: Vec<Value> = Delegate::ALL
        .into_iter()
        .map(|d| {
            let state = detected.and_then(|m| m.get(&d));
            json!({
                "id": d.id(),
                "kind": "subscription",
                "label": d.label(),
                "plan": d.plan_label(),
                "binary": d.binary(),
                "installed": state.map(DelegateState::is_installed),
                "state": state.map(DelegateState::tag),
                "min_version": d.min_version(),
                "selected": selected_delegate == Some(d),
            })
        })
        .collect();
    sources.push(json!({"id": "shogun", "kind": "hosted", "label": "SHOGUN",
        "selected": settings.provider == "shogun", "gateway": "api.shogunaios.com",
        "downstream": "api.openai.com", "downstream_dictation_cleanup": "api.groq.com",
        "stores_content": false}));
    sources.extend(BYOK_PROVIDERS.iter().map(|p| {
        json!({
            "id": p,
            "kind": "api_key",
            "label": byok_label(p),
            "key_present": key_present(p),
            "selected": settings.provider == *p,
        })
    }));
    // The desktop's own "has something to authenticate with" rule: a key for an API provider, or
    // an installed and consented delegate. Not a sign-in check (that spends the user's quota and
    // stays the Settings "Test connection" button).
    let configured = match selected_delegate {
        Some(d) => detected
            .and_then(|m| m.get(&d))
            .map(|state| settings.subscription_consent && state.is_installed()),
        None if settings.provider == "shogun" => None, // the API face cannot attest live licence readiness
        None => key_present(&settings.provider),
    };
    // Issue #389: what actually runs, and how the stored model was read. `model_choice` is
    // `unknown` when the stored id is not offered for the provider (the default runs instead;
    // Settings shows the same note), `retired` for a former default (startup clears it), `custom`
    // for a free-form OpenRouter id (it runs as typed; `model_price` is null = unknown).
    let resolved = crate::model_catalog::resolve(&settings.provider, &settings.model);
    let model_price = crate::model_catalog::find(&settings.provider, &resolved.model)
        .map(|m| json!({ "input_usd_per_mtok": m.input_usd_per_mtok, "output_usd_per_mtok": m.output_usd_per_mtok }));
    json!({
        "selected": settings.provider,
        "selected_kind": if settings.provider == "shogun" { "hosted" } else if selected_delegate.is_some() { "subscription" } else { "api_key" },
        "model": settings.model,
        "model_effective": if settings.provider == "shogun" { "server_selected" } else { &resolved.model },
        "model_choice": if settings.provider == "shogun" { "server_selected" } else { resolved.choice.as_str() },
        "model_tier": if settings.provider == "shogun" { None } else { resolved.tier.map(crate::model_catalog::Tier::as_str) },
        "model_price": model_price,
        "models": crate::model_catalog::catalog_json(),
        // Issue #398: the most one deep-context run can cost on what actually runs — a USD range,
        // tokens only for a free-form model (`basis: unknown`), or the plan quota for a delegate.
        "deep_context": crate::deep_context::estimate_json(
            &crate::deep_context::estimate_at_cap(
                shogun_fusion::budget::DeepContextBudget::default(),
                &settings.provider,
                &settings.model,
            ),
            &resolved.model,
        ),
        "subscription_consent": settings.subscription_consent,
        "openai_store": settings.openai_store,
        "configured": configured,
        "sources": sources,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "shogun-llm-{}-{}.json",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn settings(provider: &str, consent: bool) -> LlmSettings {
        LlmSettings { provider: provider.into(), model: String::new(), subscription_consent: consent, openai_store: false }
    }

    #[test]
    fn live_translation_runs_on_api_keys_and_never_on_a_subscription_delegate() {
        for delegate in Delegate::ALL {
            assert!(
                !live_translation_allowed(delegate.id()),
                "{} must not run live translation",
                delegate.id()
            );
        }
        for provider in BYOK_PROVIDERS {
            assert!(live_translation_allowed(provider), "{provider} is an API-key provider");
        }
        // The ids the settings file actually stores for the three delegates.
        for id in ["claude-code", "codex", "gemini-cli"] {
            assert!(Delegate::parse(id).is_some(), "{id} is a delegate id");
            assert!(!live_translation_allowed(id));
        }
    }

    #[test]
    fn the_file_shape_is_the_desktop_one() {
        // The desktop has written `llm.json` in this shape since #110; an old file without the
        // consent field reads as "no consent".
        let s: LlmSettings = serde_json::from_str(r#"{"provider":"openai","model":"gpt-4o-mini"}"#).unwrap();
        assert_eq!(s, LlmSettings { provider: "openai".into(), model: "gpt-4o-mini".into(), subscription_consent: false, openai_store: false });
        let v: Value = serde_json::to_value(settings("claude-code", true)).unwrap();
        assert_eq!(v, json!({"provider":"claude-code","model":"","subscription_consent":true,"openai_store":false}));
    }

    #[test]
    fn openai_store_is_off_by_default_and_in_a_legacy_file() {
        assert!(!LlmSettings::default().openai_store);
        assert!(!LlmSettings::default().stores_openai_responses());
        // A file written before the setting existed (#110 / #665 shape) loads as off.
        let path = temp_path();
        std::fs::write(&path, r#"{"provider":"openai","model":"gpt-4o","subscription_consent":false}"#).unwrap();
        let legacy = load_strict(&path).unwrap();
        assert!(!legacy.openai_store);
        assert!(!legacy.stores_openai_responses());
        assert_eq!(load_effective(&path), legacy);
        save(&path, &LlmSettings { openai_store: true, ..legacy }).unwrap();
        assert!(load_strict(&path).unwrap().stores_openai_responses());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn only_an_openai_selection_stores_responses() {
        let mut s = LlmSettings { provider: "openai".into(), openai_store: true, ..LlmSettings::default() };
        assert!(s.stores_openai_responses());
        for other in ["anthropic", "openrouter", "gemini", "codex", "claude-code"] {
            s.provider = other.into();
            assert!(!s.stores_openai_responses(), "{other}");
        }
        s.provider = "openai".into();
        s.openai_store = false;
        assert!(!s.stores_openai_responses());
    }

    fn with_provider(provider: &str) -> LlmSettings {
        LlmSettings { provider: provider.into(), ..LlmSettings::default() }
    }

    #[test]
    fn chat_tools_run_on_an_api_key_and_not_on_the_gemini_cli() {
        assert_eq!(
            chat_tool_lane(&with_provider("anthropic"), true),
            Some(ChatToolLane::AnthropicMessages)
        );
        assert_eq!(
            chat_tool_lane(&with_provider("openai"), true),
            Some(ChatToolLane::OpenAiResponses)
        );
        // Owner decision 2026-10-08 (#396): OpenRouter and Gemini keys run the chat/completions
        // tool loop, whatever the subscription consent says.
        for provider in ["openrouter", "gemini"] {
            for consent in [false, true] {
                let s = LlmSettings { subscription_consent: consent, ..with_provider(provider) };
                assert_eq!(
                    chat_tool_lane(&s, true),
                    Some(ChatToolLane::ChatCompletions),
                    "{provider} consent={consent}"
                );
            }
        }
        // The Gemini CLI (consented or not; no bridged form, #484), Groq (not a chat provider)
        // and unknown ids answer without tools.
        for provider in ["gemini-cli", "groq", "", "acme"] {
            for consent in [false, true] {
                for key in [false, true] {
                    let s = LlmSettings { subscription_consent: consent, ..with_provider(provider) };
                    assert_eq!(chat_tool_lane(&s, key), None, "{provider} consent={consent}");
                }
            }
        }
    }

    #[test]
    fn claude_code_and_codex_get_the_bridge_only_after_the_opt_in() {
        // Issue #484. No key is read for a delegate, so the key flag changes nothing.
        for (provider, bridged) in [("claude-code", BridgedDelegate::ClaudeCode), ("codex", BridgedDelegate::Codex)] {
            for key in [false, true] {
                let consented = LlmSettings { subscription_consent: true, ..with_provider(provider) };
                assert_eq!(chat_tool_lane(&consented, key), Some(ChatToolLane::DelegateBridge(bridged)), "{provider}");
                let not_consented = with_provider(provider);
                assert_eq!(chat_tool_lane(&not_consented, key), None, "{provider}: no opt-in, no delegate run");
            }
        }
        // Every delegate is either bridged or tool-free; only the Gemini CLI is tool-free.
        for delegate in Delegate::ALL {
            let s = LlmSettings { subscription_consent: true, ..with_provider(delegate.id()) };
            let lane = chat_tool_lane(&s, true);
            match delegate {
                Delegate::GeminiCli => assert_eq!(lane, None),
                Delegate::ClaudeCode | Delegate::Codex => {
                    assert!(matches!(lane, Some(ChatToolLane::DelegateBridge(b)) if b.delegate() == delegate));
                }
            }
        }
        // The bridged lane is a delegate lane: it is never what an API-key provider gets.
        for provider in BYOK_PROVIDERS {
            let s = LlmSettings { subscription_consent: true, ..with_provider(provider) };
            assert!(!matches!(chat_tool_lane(&s, true), Some(ChatToolLane::DelegateBridge(_))), "{provider}");
        }
    }

    #[test]
    fn no_key_means_no_tool_lane() {
        for provider in ["anthropic", "openai", "openrouter", "gemini"] {
            assert_eq!(chat_tool_lane(&with_provider(provider), false), None, "{provider}");
        }
    }

    #[test]
    fn the_openai_tool_lane_never_stores_even_when_the_user_allowed_storage() {
        let s = LlmSettings { openai_store: true, ..with_provider("openai") };
        assert!(s.stores_openai_responses());
        assert_eq!(chat_tool_lane(&s, true), Some(ChatToolLane::OpenAiResponses));
        let compat = crate::llm::openai_compat::OpenAiCompatConfig::new("https://api.openai.com/v1", "m");
        let lane = crate::llm::openai_responses::OpenAiResponsesConfig::for_chat_tools(&compat);
        assert!(!lane.store);
        assert_eq!(lane.store, CHAT_TOOL_LANE_STORES_RESPONSES);
    }

    #[test]
    fn openai_store_off_is_direct_and_on_is_refused_on_the_api() {
        let on = LlmSettings { provider: "openai".into(), openai_store: true, ..LlmSettings::default() };
        let off = LlmSettings { openai_store: false, ..on.clone() };
        // Off: direct, alone or with other narrowing changes.
        assert_eq!(plan(&on, r#"{"openai_store":false}"#), Ok(Plan::Direct(off.clone())));
        assert_eq!(
            plan(&on, r#"{"source":"anthropic","openai_store":false}"#),
            Ok(Plan::Direct(LlmSettings { provider: "anthropic".into(), ..off.clone() }))
        );
        // On while off: refused, never queued.
        assert_eq!(plan(&off, r#"{"openai_store":true}"#), Err(Invalid::StoreOnlyInApp));
        assert_eq!(plan(&off, r#"{"source":"openai","openai_store":true}"#), Err(Invalid::StoreOnlyInApp));
        assert_eq!(Invalid::StoreOnlyInApp.code(), "openai_store_only_in_app");
        // Restating what is in force is a no-op either way.
        assert_eq!(plan(&on, r#"{"openai_store":true}"#), Ok(Plan::Direct(on.clone())));
        assert_eq!(plan(&off, r#"{"openai_store":false}"#), Ok(Plan::Direct(off.clone())));
        // Changes that do not name it keep the answer on file, across providers too.
        assert_eq!(
            plan(&on, r#"{"source":"gemini"}"#),
            Ok(Plan::Direct(LlmSettings { provider: "gemini".into(), ..on.clone() }))
        );
        assert_eq!(
            plan(&on, r#"{"source":"codex","subscription_consent":false}"#),
            Ok(Plan::Direct(LlmSettings { provider: "codex".into(), ..on.clone() }))
        );
        // It cannot ride along with a selection that waits for approval (it would be lost).
        assert_eq!(
            plan(&on, r#"{"source":"claude-code","subscription_consent":true,"openai_store":false}"#),
            Err(Invalid::BadRequest)
        );
        assert_eq!(plan(&off, r#"{"openai_store":"yes"}"#), Err(Invalid::BadRequest));
    }

    #[test]
    fn apply_direct_turns_store_off_but_never_on() {
        let path = temp_path();
        // Nothing on file: turning it on over the API is refused and writes nothing.
        let refused = apply_direct(&path, r#"{"openai_store":true}"#).unwrap_err();
        assert!(refused.starts_with("invalid "), "{refused}");
        assert!(refused.contains("openai_store_only_in_app"), "{refused}");
        assert!(!path.exists());
        // The user turned it on in Settings (the desktop's own `save`).
        save(&path, &LlmSettings { provider: "openai".into(), openai_store: true, ..LlmSettings::default() }).unwrap();
        assert_eq!(classify(&path, r#"{"openai_store":false}"#), Ok(None));
        let now = apply_direct(&path, r#"{"openai_store":false}"#).unwrap();
        assert!(!now.openai_store);
        assert!(!load_strict(&path).unwrap().openai_store);
        assert!(apply_direct(&path, r#"{"openai_store":true}"#).is_err());
        assert!(!load_strict(&path).unwrap().openai_store, "still off");
        // A confirmed subscription keeps the OpenAI answer on file.
        save(&path, &LlmSettings { provider: "openai".into(), openai_store: true, ..LlmSettings::default() }).unwrap();
        assert!(apply_confirmed_subscription(&path, "codex").unwrap().openai_store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn switching_to_an_api_key_or_withdrawing_consent_is_direct() {
        let current = settings("claude-code", true);
        assert_eq!(
            plan(&current, r#"{"source":"anthropic"}"#),
            Ok(Plan::Direct(LlmSettings { provider: "anthropic".into(), model: String::new(), subscription_consent: true, openai_store: false }))
        );
        assert_eq!(
            plan(&current, r#"{"subscription_consent":false}"#),
            Ok(Plan::Direct(settings("claude-code", false)))
        );
        assert_eq!(
            plan(&current, r#"{"source":"openai","model":"gpt-6-sol","subscription_consent":false}"#),
            Ok(Plan::Direct(LlmSettings { provider: "openai".into(), model: "gpt-6-sol".into(), subscription_consent: false, openai_store: false }))
        );
        // A model change on the same provider keeps everything else.
        let byok = LlmSettings { provider: "openai".into(), model: "gpt-6-sol".into(), subscription_consent: false, openai_store: false };
        assert_eq!(
            plan(&byok, r#"{"model":"gpt-6-luna"}"#),
            Ok(Plan::Direct(LlmSettings { model: "gpt-6-luna".into(), ..byok.clone() }))
        );
    }

    #[test]
    fn selecting_a_subscription_needs_approval_and_the_callers_attestation() {
        for current in [settings("anthropic", false), settings("anthropic", true), settings("codex", true), settings("claude-code", false)] {
            assert_eq!(
                plan(&current, r#"{"source":"claude-code","subscription_consent":true}"#),
                Ok(Plan::NeedsApproval(Delegate::ClaudeCode)),
                "{current:?}"
            );
            assert_eq!(plan(&current, r#"{"source":"claude-code"}"#), Err(Invalid::ConsentRequired));
        }
        // Enabling consent on the already-selected delegate is the same opt-in.
        assert_eq!(
            plan(&settings("gemini-cli", false), r#"{"subscription_consent":true}"#),
            Ok(Plan::NeedsApproval(Delegate::GeminiCli))
        );
        // Already the consented source: nothing new.
        assert_eq!(
            plan(&settings("claude-code", true), r#"{"source":"claude-code"}"#),
            Ok(Plan::Direct(settings("claude-code", true)))
        );
        // Selecting a delegate with the opt-in withdrawn never runs it: direct.
        assert_eq!(
            plan(&settings("anthropic", true), r#"{"source":"codex","subscription_consent":false}"#),
            Ok(Plan::Direct(settings("codex", false)))
        );
    }

    #[test]
    fn consent_is_never_granted_on_its_own_and_bad_bodies_are_refused() {
        let current = settings("anthropic", false);
        assert_eq!(plan(&current, r#"{"subscription_consent":true}"#), Err(Invalid::ConsentNeedsSubscription));
        assert_eq!(plan(&current, r#"{"source":"openai","subscription_consent":true}"#), Err(Invalid::ConsentNeedsSubscription));
        assert_eq!(plan(&current, r#"{"source":"mystery"}"#), Err(Invalid::UnknownSource));
        assert_eq!(plan(&current, "{}"), Err(Invalid::BadRequest));
        assert_eq!(plan(&current, "not json"), Err(Invalid::BadRequest));
        assert_eq!(plan(&current, r#"{"source":"anthropic","key":"x"}"#), Err(Invalid::BadRequest));
        // A key pasted where a model belongs never reaches the file.
        assert_eq!(plan(&current, r#"{"model":"sk-ant-api03-abcdef"}"#), Err(Invalid::ImplausibleModel));
        assert_eq!(plan(&current, r#"{"model":"a b"}"#), Err(Invalid::ImplausibleModel));
    }

    #[test]
    fn apply_direct_writes_narrowing_changes_and_refuses_a_subscription() {
        let path = temp_path();
        // Missing file = the default.
        assert_eq!(load_strict(&path), Ok(LlmSettings::default()));
        assert_eq!(classify(&path, r#"{"source":"openai"}"#), Ok(None));
        let saved = apply_direct(&path, r#"{"source":"openai","model":"gpt-6-sol"}"#).unwrap();
        assert_eq!(load_strict(&path), Ok(saved));

        assert_eq!(
            classify(&path, r#"{"source":"claude-code","subscription_consent":true}"#),
            Ok(Some(Delegate::ClaudeCode))
        );
        let before = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            apply_direct(&path, r#"{"source":"claude-code","subscription_consent":true}"#).err().as_deref(),
            Some(SUBSCRIPTION_NOT_APPROVED)
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before, "nothing changed");
        assert!(apply_direct(&path, r#"{"subscription_consent":true}"#).unwrap_err().starts_with("invalid "));

        // Only the confirmed approval selects it and records the opt-in.
        let applied = apply_confirmed_subscription(&path, "claude-code").unwrap();
        assert_eq!(applied, settings("claude-code", true));
        assert_eq!(load_effective(&path), settings("claude-code", true));
        assert!(apply_confirmed_subscription(&path, "anthropic").is_err(), "an API provider is not a delegate");

        // Withdrawing is direct again.
        assert_eq!(apply_direct(&path, r#"{"subscription_consent":false}"#), Ok(settings("claude-code", false)));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn shogun_is_the_fixed_default_and_a_saved_override_is_kept() {
        // Owner decision 2026-10-21 (#1101): no saved choice runs on the hosted lane.
        assert_eq!(LlmSettings::default().provider, DEFAULT_PROVIDER);
        assert_eq!(DEFAULT_PROVIDER, "shogun");
        let path = temp_path();
        assert_eq!(load_effective(&path).provider, "shogun");
        assert_eq!(chat_tool_lane(&load_effective(&path), false), Some(ChatToolLane::HostedResponses));
        // A saved BYOK or subscription choice is an explicit override: never replaced.
        let byok = apply_direct(&path, r#"{"source":"anthropic"}"#).unwrap();
        assert_eq!(load_effective(&path), byok);
        assert_eq!(load_effective(&path).provider, "anthropic");
        let delegate = apply_confirmed_subscription(&path, "codex").unwrap();
        assert_eq!(load_effective(&path), delegate);
        // And the API faces can go back to it directly.
        assert_eq!(apply_direct(&path, r#"{"source":"shogun"}"#).unwrap().provider, "shogun");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_corrupt_file_is_never_overwritten_by_an_api_write() {
        let path = temp_path();
        std::fs::write(&path, "{not json").unwrap();
        assert!(load_strict(&path).is_err());
        assert_eq!(load_effective(&path), LlmSettings::default(), "the desktop's startup rule");
        assert!(classify(&path, r#"{"source":"openai"}"#).is_err());
        assert!(apply_direct(&path, r#"{"source":"openai"}"#).is_err());
        assert!(apply_confirmed_subscription(&path, "codex").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{not json");
        // An unknown provider is as unreadable as broken JSON.
        std::fs::write(&path, r#"{"provider":"mystery","model":""}"#).unwrap();
        assert!(load_strict(&path).is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn the_status_carries_presence_only() {
        let mut detected = std::collections::BTreeMap::new();
        detected.insert(Delegate::ClaudeCode, DelegateState::Installed { version: "2.1.0 (Claude Code)".into() });
        detected.insert(Delegate::Codex, DelegateState::NotInstalled);
        detected.insert(Delegate::GeminiCli, DelegateState::NotInstalled);
        let keys = |p: &str| Some(p == "openai");
        let v = status_json(&settings("claude-code", true), Some(&detected), &keys);
        assert_eq!(v["selected"], "claude-code");
        assert_eq!(v["selected_kind"], "subscription");
        assert_eq!(v["subscription_consent"], true);
        assert_eq!(v["openai_store"], false);
        assert_eq!(v["configured"], true);
        let sources = v["sources"].as_array().unwrap();
        assert_eq!(sources.len(), 8);
        let claude = &sources[0];
        assert_eq!(claude["installed"], true);
        assert_eq!(claude["state"], "installed");
        assert_eq!(claude["selected"], true);
        assert_eq!(sources[1]["installed"], false);
        let openai = sources.iter().find(|s| s["id"] == "openai").unwrap();
        assert_eq!(openai["key_present"], true);
        assert_eq!(openai["kind"], "api_key");
        // No version text or anything key-shaped anywhere in the answer.
        let text = v.to_string();
        assert!(!text.contains("2.1.0"), "{text}");
        assert!(!text.contains("byok"), "{text}");

        // A CLI older than SHOGUN can run: its own tag and SHOGUN's minimum, never its version.
        detected.insert(
            Delegate::Codex,
            DelegateState::NeedsUpdate { version: "codex-cli 0.120.0".into(), minimum: Some("0.122.0") },
        );
        let v = status_json(&settings("codex", true), Some(&detected), &keys);
        let codex = &v["sources"][1];
        assert_eq!(codex["state"], "needs_update");
        assert_eq!(codex["installed"], true);
        assert_eq!(codex["min_version"], "0.122.0");
        assert_eq!(v["sources"][0]["min_version"], Value::Null);
        assert_eq!(v["sources"][2]["min_version"], "0.35.0");
        assert!(!v.to_string().contains("0.120.0"), "{v}");

        // Unknown probes are null, never a guess.
        let v = status_json(&settings("anthropic", false), None, &|_| None);
        assert_eq!(v["sources"][0]["installed"], Value::Null);
        assert_eq!(v["configured"], Value::Null);
        assert_eq!(v["selected_kind"], "api_key");

        // The OpenAI store answer is reported as saved.
        let on = LlmSettings { provider: "openai".into(), openai_store: true, ..LlmSettings::default() };
        assert_eq!(status_json(&on, None, &|_| Some(true))["openai_store"], true);
    }

    #[test]
    fn the_api_faces_find_the_file_beside_the_db() {
        // The env override is exercised by the binaries; the default is the DB's directory.
        if std::env::var(SETTINGS_ENV).is_err() {
            assert_eq!(resolve_path("/data/shogun/memory.db"), PathBuf::from("/data/shogun/llm.json"));
        }
        assert_eq!(byok_keychain_account("gemini"), Some("gemini-byok"));
        assert_eq!(byok_keychain_account("claude-code"), None);
    }

    #[test]
    fn a_model_outside_the_providers_list_is_refused_on_write() {
        // Issue #389: the picker is curated on every face. Empty is the recommended model.
        let current = settings("anthropic", false);
        assert_eq!(plan(&current, r#"{"model":"gpt-6-astra"}"#), Err(Invalid::UnknownModel));
        assert_eq!(plan(&current, r#"{"source":"openai","model":"gpt-4o-mini"}"#), Err(Invalid::UnknownModel));
        assert_eq!(Invalid::UnknownModel.code(), "unknown_model");
        assert_eq!(
            plan(&current, r#"{"source":"openai","model":"gpt-6-astra"}"#),
            Ok(Plan::Direct(LlmSettings { provider: "openai".into(), model: "gpt-6-astra".into(), ..current.clone() }))
        );
        assert_eq!(
            plan(&current, r#"{"model":""}"#),
            Ok(Plan::Direct(current.clone()))
        );
        // The key check still comes first: a pasted key is `invalid_model`, never echoed back.
        assert_eq!(plan(&current, r#"{"model":"sk-ant-api03-abcdef"}"#), Err(Invalid::ImplausibleModel));
        // A stored off-list model is not a reason to refuse an unrelated change, or a restatement
        // (the Settings screen sends the model it holds with every write).
        let odd = LlmSettings { provider: "openai".into(), model: "gpt-4o".into(), ..LlmSettings::default() };
        assert_eq!(
            plan(&odd, r#"{"openai_store":false}"#),
            Ok(Plan::Direct(odd.clone()))
        );
        assert_eq!(plan(&odd, r#"{"model":"gpt-4o"}"#), Ok(Plan::Direct(odd.clone())));
        assert!(model_change_allowed(&odd, "openai", "gpt-4o"));
        assert!(!model_change_allowed(&odd, "openai", "gpt-4.1"));
        assert!(!model_change_allowed(&odd, "gemini", "gpt-4o"), "same id, other provider");
        assert!(model_change_allowed(&odd, "openrouter", "gpt-4o"), "OpenRouter takes a free-form id");
        assert!(model_change_allowed(&odd, "codex", ""));
        // OpenRouter alone takes a well-formed free-form id (owner decision 2026-09-26), including
        // one longer than the plausible-model limit; a bad format is still refused.
        let long = "qwen/qwen3-235b-a22b-instruct-2507-long-context-preview-edition-x";
        assert!(long.len() > 64);
        for id in ["meta-llama/llama-4-maverick", long] {
            let body = format!(r#"{{"source":"openrouter","model":"{id}"}}"#);
            assert_eq!(
                plan(&current, &body),
                Ok(Plan::Direct(LlmSettings { provider: "openrouter".into(), model: id.into(), ..current.clone() })),
                "{id}"
            );
        }
        assert_eq!(plan(&current, r#"{"source":"openrouter","model":"two words"}"#), Err(Invalid::ImplausibleModel));
        assert_eq!(plan(&current, r#"{"source":"openrouter","model":"sk-or-v1-abcdef"}"#), Err(Invalid::ImplausibleModel));
        assert_eq!(plan(&current, r#"{"source":"openai","model":"meta-llama/llama-4"}"#), Err(Invalid::UnknownModel));
        assert_eq!(plan(&current, r#"{"source":"gemini","model":"meta-llama/llama-4"}"#), Err(Invalid::UnknownModel));
        // Refused writes leave the file alone.
        let path = temp_path();
        let refused = apply_direct(&path, r#"{"source":"gemini","model":"gemini-2.5-flash"}"#).unwrap_err();
        assert!(refused.starts_with("invalid ") && refused.contains("unknown_model"), "{refused}");
        assert!(!path.exists());
    }

    #[test]
    fn a_retired_default_is_cleared_from_the_file_and_nothing_else_is() {
        let path = temp_path();
        assert_eq!(migrate_retired_model(&path), Ok(None), "no file, nothing written");
        assert!(!path.exists());
        // A legacy install on the old OpenAI default, with the store answer on.
        std::fs::write(&path, r#"{"provider":"openai","model":"gpt-4o-mini","subscription_consent":false,"openai_store":true}"#).unwrap();
        assert_eq!(migrate_retired_model(&path), Ok(Some("gpt-4o-mini".into())));
        let now = load_strict(&path).unwrap();
        assert_eq!(now, LlmSettings { provider: "openai".into(), model: String::new(), subscription_consent: false, openai_store: true });
        assert_eq!(migrate_retired_model(&path), Ok(None), "idempotent");
        // A pick, an unknown id and a delegate are left exactly as they are.
        for body in [
            r#"{"provider":"openai","model":"gpt-6-astra","subscription_consent":false}"#,
            r#"{"provider":"openai","model":"gpt-4o","subscription_consent":false}"#,
            r#"{"provider":"codex","model":"","subscription_consent":true}"#,
        ] {
            std::fs::write(&path, body).unwrap();
            assert_eq!(migrate_retired_model(&path), Ok(None), "{body}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
        }
        // A corrupt file is an error, never overwritten.
        std::fs::write(&path, "{not json").unwrap();
        assert!(migrate_retired_model(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{not json");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn the_status_says_which_model_runs_and_why() {
        let keys = |_: &str| Some(true);
        let v = status_json(&LlmSettings { provider: "openai".into(), ..LlmSettings::default() }, None, &keys);
        assert_eq!(v["model"], "");
        assert_eq!(v["model_effective"], "gpt-6-sol");
        assert_eq!(v["model_choice"], "default");
        assert_eq!(v["model_tier"], "budget");
        assert_eq!(v["model_price"]["output_usd_per_mtok"], 10.0);
        // Issue #398: the deep-context ceiling on the model that runs (GPT-6 Sol, $2 / $10).
        assert_eq!(v["deep_context"]["cost"]["basis"], "price");
        assert_eq!(v["deep_context"]["cost"]["model"], "gpt-6-sol");
        assert_eq!(v["deep_context"]["cost"]["usd_high"], 1.32);
        assert_eq!(v["deep_context"]["cap_tokens"], 200_000);
        assert_eq!(v["models"]["providers"]["openai"]["budget"], "gpt-6-luna");
        assert_eq!(v["models"]["providers"]["openai"]["deep"], "gpt-6-astra");
        let astra = LlmSettings { provider: "openai".into(), model: "gpt-6-astra".into(), ..LlmSettings::default() };
        let v = status_json(&astra, None, &keys);
        assert_eq!((v["model_effective"].as_str(), v["model_choice"].as_str(), v["model_tier"].as_str()), (Some("gpt-6-astra"), Some("selected"), Some("deep")));
        // An unknown stored model: the default runs, and the answer says so.
        let odd = LlmSettings { provider: "anthropic".into(), model: "claude-sonnet-4-5".into(), ..LlmSettings::default() };
        let v = status_json(&odd, None, &keys);
        assert_eq!((v["model_effective"].as_str(), v["model_choice"].as_str()), (Some("claude-sonnet-5-5"), Some("unknown")));
        // A free-form OpenRouter id runs as typed, pricing unknown.
        let custom = LlmSettings { provider: "openrouter".into(), model: "meta-llama/llama-4-maverick".into(), ..LlmSettings::default() };
        let v = status_json(&custom, None, &keys);
        assert_eq!((v["model_effective"].as_str(), v["model_choice"].as_str()), (Some("meta-llama/llama-4-maverick"), Some("custom")));
        assert_eq!(v["model_tier"], Value::Null);
        assert_eq!(v["model_price"], Value::Null);
        // Issue #398: its deep-context estimate is tokens only, and asks first.
        assert_eq!(v["deep_context"]["cost"]["basis"], "unknown");
        assert_eq!(v["deep_context"]["needs_cost_confirm"], true);
        assert_eq!(v["deep_context"]["input_tokens"], 200_000);
        // A delegate names no model.
        let v = status_json(&settings("codex", true), None, &keys);
        assert_eq!(v["model_effective"], "");
        assert_eq!(v["model_tier"], Value::Null);
        assert_eq!(v["deep_context"]["cost"]["basis"], "plan_quota");
    }
    #[test]
    fn hosted_source_is_available_to_all_faces_without_a_key_or_client_model() {
        let old = LlmSettings::default();
        let Plan::Direct(hosted) = plan(&old, r#"{"source":"shogun"}"#).unwrap() else { panic!("direct source") };
        assert_eq!(hosted.provider, "shogun");
        assert_eq!(byok_keychain_account("shogun"), None);
        assert_eq!(chat_tool_lane(&hosted, false), Some(ChatToolLane::HostedResponses));
        assert!(!model_change_allowed(&old, "shogun", "gpt-client-choice"));
        let status = status_json(&hosted, None, &|p| { assert_ne!(p, "shogun"); None });
        assert_eq!(status["selected_kind"], "hosted");
        assert_eq!(status["model_effective"], "server_selected");
    }

}
