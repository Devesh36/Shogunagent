//! `shogun-mcp` — the MCP server face (§6.11), a stdio JSON-RPC loop. An AI client (spawned by the
//! user) speaks the Model Context Protocol over stdin/stdout; each request runs against the same
//! DB-backed Memory API backend the REST/CLI faces use (invariant 6).
//!
//! Config via env: `SHOGUN_DB_PATH` (default `./shogun.db`); `SHOGUN_MCP_TOOLSETS` (comma list of
//! tool groups, or `all`; unset = all) narrows what `tools/list` shows. The entry SHOGUN writes
//! into an app passes `--client <id>` (issue #963): the session is that app's connection, made in
//! Settings ("Add to …") and kept in the Keychain next to the token verifiers, and no token is
//! involved. An entry without it is the older form: `SHOGUN_API_TOKEN` must match an issued token,
//! checked once at start-up. An entry with neither is refused, even before any app is connected
//! (audit 2026-10-20 M5: no process trust). The session then runs
//! under that token's scope (issue #400): a `read` token lists and calls read tools only. Levels
//! still apply: an external send routes to the approval queue.
//!
//! The same check runs again on every `tools/list` / `tools/call` against the live token list
//! (`shogun_mcp::live_tokens`): a token revoked in Settings, or with "Remove from …", is refused
//! from its next call, without restarting the client.
//!
//! A connection the app made on its own runs without meeting notes and screen recall until the
//! user allows them in Settings (owner decision 2026-10-20, `shogun_mcp::source_policy`): the
//! session re-reads that setting with its scope on every call.
//!
//! An app whose code signature SHOGUN expects (issue #979, `shogun_mcp::launcher_check`) must also
//! be the program that started this process, checked once at start-up. No app is listed yet, so
//! no connection is refused for it today.
//!
//! `--chat-bridge` (issue #484) is a different mode altogether: the one MCP server SHOGUN starts a
//! delegated chat run with. It relays that run's tool calls to the desktop over a per-run socket
//! (`shogun_mcp::chat_bridge`) and opens nothing else.
//!
//! Run (as an MCP server the client launches):
//! ```text
//! SHOGUN_DB_PATH="$HOME/Library/Application Support/com.syogun.shogunai/memory.db" \
//!   cargo run -p shogun-core --features db --bin shogun-mcp
//! ```
//!
//! Like `shogun-api`, it never creates or migrates the store (#800); that binary's header spells
//! out the `memory_update_pending` / `memory_newer_than_app` behaviour.
//!
//! Built with `net` too (the copy the app bundles: `--features db,net`), a session that passes the
//! gate sends one anonymous `memory_api_mounted` event carrying only its scope, unless the user
//! turned analytics off (`shogun_core::analytics::mount`, issue #400).

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use shogun_core::daemon::DaemonDb;
use shogun_core::db_backend::DbBackend;
// The env names the generated client entries write (issue #400): one definition, so the docs
// snippets cannot name a variable this binary does not read.
use shogun_mcp::client_config::{API_TOKEN_ENV, DB_PATH_ENV};
use shogun_mcp::mcp::{serve, McpServer};
use shogun_mcp::live_tokens::{revision_path, LiveTokens};
use shogun_mcp::memory_api::{TokenRegistry, TokenScope};
use shogun_mcp::memory_api_settings;
use shogun_mcp::startup_report;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn visual_recall_settings_path(db_path: &str) -> Option<std::path::PathBuf> {
    std::env::var("SHOGUN_VISUAL_RECALL_SETTINGS")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::path::Path::new(db_path)
                .parent()
                .map(|p| p.join("visual_recall.json"))
        })
}

/// The token list: the app's Keychain item in the Mac app's build (feature `desktop`, read
/// uncached — `shogun_core::desktop::tokens`, issue #744), else one client token from
/// `SHOGUN_API_TOKEN`.
fn load_token_registry() -> Result<TokenRegistry, String> {
    #[cfg(feature = "desktop")]
    {
        shogun_core::desktop::tokens::load_token_registry()
    }
    #[cfg(not(feature = "desktop"))]
    {
        let mut tokens = TokenRegistry::new();
        if let Ok(token) = std::env::var(API_TOKEN_ENV) {
            if !token.is_empty() {
                tokens.issue(token);
            }
        }
        Ok(tokens)
    }
}

/// The name every stderr line starts with, so a client's server log says whose line it is.
const FACE: &str = "shogun-mcp";

/// Exit unless the API is on and the process names a connected app or presents a valid token.
/// Returns the scope this session starts under and the live token list every later call is
/// re-checked against (issue #400). Every refusal names the file it checked and the specific
/// cause (issue #744); the decisions and the exit status (1) are unchanged.
fn gate_or_exit(
    db_path: &str,
    db_env_set: bool,
    client: Option<&str>,
) -> (TokenScope, Arc<LiveTokens>, bool) {
    let settings_path = memory_api_settings::resolve_settings_path(db_path);
    if memory_api_settings::require_enabled(&settings_path).is_err() {
        let db = startup_report::DbPathFacts::of(
            std::path::Path::new(db_path),
            db_env_set,
            shogun_mcp::plan_source::app_data_dir().as_deref(),
        );
        let refusal = startup_report::settings_refusal(
            FACE,
            startup_report::settings_state(&settings_path),
            &settings_path,
            &db,
            startup_report::home_dir().as_deref(),
        );
        eprintln!("{refusal}");
        std::process::exit(1);
    }
    let live = Arc::new(LiveTokens::watching(revision_path(&settings_path), load_token_registry));
    let tokens = live.current();
    if tokens.is_unavailable() {
        let why = live.last_error().unwrap_or_default();
        eprintln!("{}", startup_report::tokens_unavailable(FACE, &why));
        std::process::exit(1);
    }
    // An entry that names its app (issue #963) is that app's connection and nothing else: no
    // token, no process trust.
    if let Some(client) = client {
        return match tokens.authenticate_client(client) {
            Some(scope) => {
                check_launcher_or_exit(client);
                (scope, live, true)
            }
            None => {
                eprintln!("{}", startup_report::connection_refusal(FACE, client));
                std::process::exit(1);
            }
        };
    }
    // Audit 2026-10-20 M5: an entry that names no app must present an issued token, whatever
    // the list holds. There is no process trust, so an empty list (nothing connected yet, or
    // after "Delete everything") refuses here rather than granting full access.
    let token_env = std::env::var(API_TOKEN_ENV).ok();
    let token_shape = startup_report::classify_token_env(token_env.as_deref());
    match tokens.authenticate_process(token_env.as_deref()) {
        // Only ever admitted by a token now (issue #744's flag stays true).
        Some(scope) => (scope, live, true),
        None => {
            eprintln!("{}", startup_report::token_refusal(FACE, token_shape));
            std::process::exit(1);
        }
    }
}

/// Issue #979: for an app whose code signature SHOGUN expects
/// (`shogun_mcp::launcher_check::EXPECTED_SIGNERS`), exit unless that app started this process.
/// Every other app is not checked and nothing is read, so this only ever takes access away. The
/// table is empty until each app is confirmed on a Mac, so today it never refuses. The check is
/// the Mac app's (feature `desktop`, `shogun_core::desktop::launcher`); the server build has no
/// signatures to read and nothing to refuse.
fn check_launcher_or_exit(client: &str) {
    #[cfg(feature = "desktop")]
    if let Some(refusal) = shogun_core::desktop::launcher::refusal(client) {
        eprintln!("{}", startup_report::launcher_refusal(FACE, client, refusal));
        std::process::exit(1);
    }
    #[cfg(not(feature = "desktop"))]
    let _ = client;
}

/// The PostHog key embedded at build time, like the desktop's (`SHOGUN_POSTHOG_KEY` in release CI).
/// Unset: the count is a no-op.
#[cfg(feature = "net")]
const BUILT_IN_POSTHOG_KEY: Option<&str> = option_env!("SHOGUN_POSTHOG_KEY");

/// Count this mount (issue #400): the session's scope and nothing else, only with the user's
/// analytics on (the desktop's `analytics.json`), on a background thread.
#[cfg(feature = "net")]
fn count_mount(scope: TokenScope) {
    let runtime_key = std::env::var("SHOGUN_POSTHOG_KEY").ok();
    let key = shogun_core::analytics::resolve_api_key(runtime_key.as_deref(), BUILT_IN_POSTHOG_KEY);
    // Debug-only, like every other env override in the tree: a release binary must not let an
    // environment variable redirect its analytics egress.
    #[cfg(debug_assertions)]
    let host = std::env::var("SHOGUN_POSTHOG_HOST")
        .unwrap_or_else(|_| shogun_core::analytics::DEFAULT_HOST.to_string());
    #[cfg(not(debug_assertions))]
    let host = shogun_core::analytics::DEFAULT_HOST.to_string();
    let analytics_json = shogun_mcp::plan_source::analytics_json_path()
        .and_then(|path| std::fs::read_to_string(path).ok());
    shogun_core::analytics::mount::send_in_background(scope.as_str(), analytics_json, key, &host);
}

#[cfg(not(feature = "net"))]
fn count_mount(_scope: TokenScope) {}

/// `--chat-bridge` (issue #484): the one MCP server a delegated chat run is started with. It relays
/// `tools/list` / `tools/call` to the desktop's per-run socket and nothing else — no store, no
/// Keychain, no Memory API grant — so it runs before (and instead of) everything below.
#[cfg(unix)]
fn chat_bridge_or_continue() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !shogun_mcp::chat_bridge::bridge_arg(args.iter().cloned()) {
        return;
    }
    // The bridge is not an app connection: an entry naming both is malformed, not either one.
    if args.iter().any(|a| a == "--client" || a.starts_with("--client=")) {
        eprintln!("{FACE}: --chat-bridge cannot be combined with --client");
        std::process::exit(1);
    }
    // The socket and nonce come from the env (Claude Code) or a `--chat-bridge-config` file in the
    // working directory (Codex); never from the command line.
    match shogun_mcp::chat_bridge::run_with_args(&args) {
        Ok(()) => std::process::exit(0),
        Err(why) => {
            eprintln!("{FACE}: {why}");
            std::process::exit(1);
        }
    }
}

#[cfg(not(unix))]
fn chat_bridge_or_continue() {}

fn main() -> std::io::Result<()> {
    chat_bridge_or_continue();
    // Audit 2026-10-20 M7: plan resolution reads the Keychain mirrors of the trial stamp and the
    // time high-water mark, so editing onboarding.json / billing.json cannot stretch the plan.
    // The Mac app's build only (feature `desktop`); the server has no Keychain to mirror.
    #[cfg(feature = "desktop")]
    shogun_core::plan_mirrors::install();
    let db_env = std::env::var(DB_PATH_ENV).ok();
    let db_env_set = db_env.is_some();
    let db_path = db_env.unwrap_or_else(|| "./shogun.db".to_string());
    // `--client <id>` (issue #963): the app this entry was written for.
    let client = match shogun_mcp::client_config::client_arg(std::env::args().skip(1)) {
        Ok(client) => client,
        Err(why) => {
            eprintln!("{FACE}: {why}");
            std::process::exit(1);
        }
    };
    let toolset_flag = match shogun_mcp::toolsets::toolset_arg(std::env::args().skip(1)) {
        Ok(flag) => flag,
        Err(why) => {
            eprintln!("{FACE}: {why}");
            std::process::exit(1);
        }
    };
    // Audit 2026-10-20 M6: check memory_api.json and the capture settings against the seals the
    // app writes, before the gate reads the first of them. This face never adopts an unsealed
    // file; the app does that on its first run after the upgrade.
    if let Some(key) = shogun_core::db_backend::BackupRetentionConfig::for_daemon(&db_path)
        .key
        .and_then(|load| load())
    {
        shogun_core::settings_seal::install_from_db_key(&key, false);
    }
    let (scope, live_tokens, admitted_by_token) =
        gate_or_exit(&db_path, db_env_set, client.as_deref());
    count_mount(scope);
    let clock: shogun_core::daemon::Clock = Arc::new(now_ms);
    // Never migrates (#800). The source wiring below mirrors `shogun-api`'s `main`; its comments
    // say what each source is for.
    let store = DaemonDb::open(&db_path, clock).unwrap_or_else(|e| {
        eprintln!("{FACE}: open db {db_path}: {e}");
        std::process::exit(1);
    });
    if let Some(why) = store.unavailable() {
        eprintln!("{FACE}: {}", why.line());
    }
    let db = store
        .db()
        .clone()
        .with_dream_outcome_path(shogun_mcp::plan_source::dream_outcome_json_path());
    let mut backend = DbBackend::new(db).with_forms_db_path(std::path::PathBuf::from(&db_path));
    if let Some(path) = shogun_core::user_config::default_path() {
        backend = backend.with_user_config_path(path);
    }
    if let Some(path) = visual_recall_settings_path(&db_path) {
        backend = backend.with_visual_recall_settings_path(path);
    }
    if let Some(path) = std::path::Path::new(&db_path).parent().map(|p| p.join("exclusions.json")) {
        backend = backend.with_capture_exclusions_path(path);
    }
    backend =
        backend.with_memory_api_settings_path(memory_api_settings::resolve_settings_path(&db_path));
    // `api.connections.restrict` (owner decision 2026-10-20): withholding meeting notes and
    // screen recall from a connection only narrows it, so this face may write that.
    backend = backend.with_connection_restrictor(shogun_core::db_backend::keychain_connection_restrictor(
        memory_api_settings::resolve_settings_path(&db_path),
    ));
    backend = backend
        .with_agent_source(shogun_core::db_backend::AgentSourceConfig::for_daemon(&db_path));
    backend = backend
        .with_backup_retention(shogun_core::db_backend::BackupRetentionConfig::for_daemon(&db_path));
    backend = backend.with_connections(shogun_mcp::connections_api::ConnectionsPaths::for_db(&db_path));
    backend = backend.with_steer_store(shogun_mcp::steer_api::resolve_path(&db_path));
    backend = backend
        .with_search_index(shogun_core::db_backend::SearchIndexConfig::for_daemon(&db_path))
        .with_dream_action_facts(shogun_core::db_backend::dream_action_facts_for_daemon);
    let plan_source = shogun_mcp::plan_source::FilePlanSource::from_env();
    // Issue #744: a locked plan still starts the server (tools/list answers), so say why once
    // here, and let every plan_required answer and the handshake carry the same reason.
    if let Some(lock) = plan_source.diagnose(u64::try_from(now_ms()).unwrap_or(0)).lock {
        eprintln!("{}", startup_report::plan_lock_note(FACE, lock));
    }
    let lock_source = plan_source.clone();
    // The process-wide L3 approval queue (B-3 / E-08): created once at the composition root and
    // injected — the MCP face never owns a private queue.
    let approvals = Arc::new(std::sync::Mutex::new(
        shogun_agents::approval::ApprovalQueue::new(),
    ));
    let approvals_path = shogun_mcp::approval_store::resolve_store_path(&db_path);
    let approval_seal = shogun_core::db_backend::BackupRetentionConfig::for_daemon(&db_path)
        .key
        .and_then(|load| load())
        .map(|key| shogun_mcp::approval_store::ApprovalSealKey::from_db_key(&key));
    if approval_seal.is_none() {
        eprintln!("{FACE}: memory key unavailable for the approval store; L3 requests from this face will be refused");
    }
    let approval_store = shogun_mcp::approval_store::ApprovalStore::new(approvals_path, approval_seal);
    let heartbeat_path = shogun_mcp::desktop_heartbeat::resolve_path(&db_path);
    let server = McpServer::new(backend, approvals, now_ms, move || {
        plan_source.resolve(u64::try_from(now_ms()).unwrap_or(0))
    })
    .with_plan_lock(move || lock_source.diagnose(u64::try_from(now_ms()).unwrap_or(0)).lock)
    // #800: every tool call re-checks the store's schema, so the app upgrading it needs no restart.
    .with_memory_gate(move || store.unavailable())
    .with_approval_store(approval_store)
    // 2026-10-20 cost audit: model-starting calls are limited per connection or token, in a
    // window shared with every other face through a file beside the memory DB.
    .with_llm_rate_limiter(Arc::new(shogun_mcp::rate_limit::LlmRateLimiter::shared(
        shogun_mcp::rate_limit::resolve_store_path(&db_path),
    )))
    .with_desktop_running_check(move || {
        shogun_mcp::desktop_heartbeat::fresh(
            &heartbeat_path,
            shogun_mcp::desktop_heartbeat::now_ms(),
        )
    })
    // Issue #410/#1066: `--toolset` > SHOGUN_MCP_TOOLSETS > core. Narrows tools/list only;
    // tools/call still answers every tool by name at its own level.
    .with_toolsets(shogun_mcp::toolsets::Toolsets::for_stdio(toolset_flag.as_deref()));
    // Issue #400: every call runs under the scope of the token the process was started with, a
    // `read` token lists and calls read tools only (the refusal is in `McpServer::tools_call`),
    // and the token is re-checked against the live list, so revoking it stops this session.
    // Issue #963: an entry that names its app runs under that app's connection instead, re-checked
    // the same way, so "Remove from …" stops it.
    let server = match client {
        Some(client) => server.with_live_client(live_tokens, client),
        None => server.with_live_session(
            live_tokens,
            std::env::var(API_TOKEN_ENV).ok(),
            admitted_by_token,
        ),
    };

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve(&server, stdin.lock(), stdout.lock())
}
