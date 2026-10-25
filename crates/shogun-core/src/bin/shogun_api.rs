//! `shogun-api` — the runnable Memory API server (§6.11). Composes the daemon's DB, the DB-backed
//! Memory API backend, the shared approval queue, and the localhost REST listener into one process.
//!
//! It is the composition root: every piece was unit-tested in its own crate; here they are wired.
//! Run it with:
//!
//! ```text
//! SHOGUN_DB_PATH="$HOME/Library/Application Support/com.syogun.shogunai/memory.db" \
//!   SHOGUN_API_TOKEN=dev-token cargo run -p shogun-core --features daemon-server --bin shogun-api
//! curl -s 127.0.0.1:7464/v1/status
//! curl -s -H "Authorization: Bearer dev-token" 127.0.0.1:7464/v1/state/commitments
//! ```
//!
//! It never creates or migrates the store (#800): only the desktop app does, after its pre-migration
//! snapshot. A missing (or still-plaintext) store exits at start-up with `memory_update_pending`,
//! so point `SHOGUN_DB_PATH` at a `memory.db` the app has opened. A store at another schema version
//! answers each memory call with `memory_update_pending` / `memory_newer_than_app` and recovers
//! without a restart once the versions match.
//!
//! The key comes from `SHOGUN_DB_KEY` (hex), else the app's Keychain entry (the Mac app's build,
//! feature `desktop`, on macOS).
//!
//! Config via env: `SHOGUN_DB_PATH` (default `./shogun.db`, which does not exist unless you put
//! the app's store there, so the default alone always exits), `SHOGUN_API_TOKEN` (issues one client
//! token; without it every call is 401), `SHOGUN_API_PORT` (default 7464, ephemeral fallback if
//! busy). The port actually bound is written to `api.port` next to the DB for `shogun` to find.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use shogun_agents::approval::ApprovalQueue;
use shogun_core::daemon::{Clock, DaemonDb, Db};
use shogun_core::db_backend::DbBackend;
use shogun_core::metrics::{
    render_snapshots_json_with_shared_service, SanitizerCounters, SloRegistry,
};
use shogun_mcp::live_tokens::{revision_path, LiveTokens};
use shogun_mcp::mcp::McpServer;
use shogun_mcp::memory_api::TokenRegistry;
use shogun_mcp::memory_api_settings;
use shogun_mcp::startup_report;
use shogun_mcp::server::{bind_local, serve_on, AppState, MetricsSource, DEFAULT_PORT};

/// The live SLO metrics source served at `GET /v1/metrics` (NFR-SLO-00). Wraps the shared
/// registry the runtime records into; here it starts empty, so every SLO reads as unmeasured until
/// the notch runtime populates it (silence ≠ success, spec §4.5). The D-6 lesson counters
/// (active lessons, feedback in the last 7 days) come from the DB; an unreadable DB renders
/// `lessons.measured:false` in the same convention. H1 harness counters (compression + tool loop)
/// ride next to them: unmeasured until an assemble or loop has run; never prompt text.
struct RegistryMetrics {
    registry: Arc<Mutex<SloRegistry>>,
    db: Db,
}

impl MetricsSource for RegistryMetrics {
    fn snapshot_json(&self) -> String {
        let lessons = self.db.lesson_counters();
        let harness = self.db.harness_counters();
        let patterns = self.db.pattern_counters();
        // Issue #29 §4-6: shared-service round trips, read from the DB the desktop records into.
        let shared = self.db.shared_service_metrics();
        let snap = shogun_memory::sanitize::snapshot();
        let sanitizer = SanitizerCounters {
            events_stripped: snap.events_stripped,
            chars_removed: snap.chars_removed,
            instruction_shaped_dropped: snap.instruction_shaped_dropped,
        };
        self.registry
            .lock()
            .map(|r| {
                render_snapshots_json_with_shared_service(
                    &r.snapshot_all(),
                    lessons,
                    harness,
                    sanitizer,
                    patterns,
                    shared.as_deref(),
                )
            })
            .unwrap_or_else(|_| {
                r#"{"metrics":[],"lessons":{"measured":false},"harness":{"measured":false},"sanitizer":{"events_stripped":0,"chars_removed":0,"instruction_shaped_dropped":0},"patterns":{"measured":false},"shared_service":{"measured":false}}"#
                    .to_string()
            })
    }
}

/// Build the metrics source for the API process (empty SLO registry + DB-backed lesson counters).
fn metrics_source(db: Db) -> Arc<dyn MetricsSource> {
    Arc::new(RegistryMetrics {
        registry: Arc::new(Mutex::new(SloRegistry::with_core_overlay())),
        db,
    })
}

/// A real wall-clock in unix ms (never panics; 0 before the epoch).
fn wall_clock() -> Clock {
    Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0)
    })
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

fn db_backend(db: Db) -> DbBackend {
    // `dream.status` reads the same last-outcome record the desktop's driver writes (#285).
    let db = db.with_dream_outcome_path(shogun_mcp::plan_source::dream_outcome_json_path());
    let db_path = std::env::var("SHOGUN_DB_PATH").unwrap_or_else(|_| "./shogun.db".to_string());
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
    backend
        .with_memory_api_settings_path(memory_api_settings::resolve_settings_path(&db_path))
        // `api.connections.restrict` (owner decision 2026-10-20): withhold meeting notes and
        // screen recall from one connected app. Only narrows, so a token may do it.
        .with_connection_restrictor(shogun_core::db_backend::keychain_connection_restrictor(
            memory_api_settings::resolve_settings_path(&db_path),
        ))
        // The Agent lane's source (issue #110): the desktop's llm.json, presence-only probes.
        .with_agent_source(shogun_core::db_backend::AgentSourceConfig::for_daemon(&db_path))
        // Snapshot retention (issue #647): the desktop's backup.json and backups/.
        .with_backup_retention(shogun_core::db_backend::BackupRetentionConfig::for_daemon(&db_path))
        // The Connections screen (issue #82): the desktop's published states, composio.json and
        // the disconnect requests it applies. No Keychain access from here.
        .with_connections(shogun_mcp::connections_api::ConnectionsPaths::for_db(&db_path))
        // Mid-run steering (issue #395): the store the desktop's runner reads at each turn.
        .with_steer_store(shogun_mcp::steer_api::resolve_path(&db_path))
        // "Compact search index" (#714 step 3): requests for the desktop's job, its status.
        .with_search_index(shogun_core::db_backend::SearchIndexConfig::for_daemon(&db_path))
        // `dream.status` actions (#285): plan and licence-token presence, re-read per answer.
        .with_dream_action_facts(shogun_core::db_backend::dream_action_facts_for_daemon)
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
        if let Ok(token) = std::env::var("SHOGUN_API_TOKEN") {
            if !token.is_empty() {
                tokens.issue(token);
            }
        }
        Ok(tokens)
    }
}

/// The name every stderr line starts with.
const FACE: &str = "shogun-api";

/// The principal root when this process runs multi-principal (E2-T2): `--multi [<root>]` on the
/// command line (the root then defaults to `SHOGUN_MULTI_ROOT`, else `./principals`), or
/// `SHOGUN_MULTI_ROOT` alone. `None` is the single-principal server, unchanged.
fn multi_root(args: impl IntoIterator<Item = String>) -> Option<std::path::PathBuf> {
    let mut args = args.into_iter();
    let env_root = std::env::var_os("SHOGUN_MULTI_ROOT")
        .filter(|root| !root.is_empty())
        .map(std::path::PathBuf::from);
    while let Some(arg) = args.next() {
        if arg == "--multi" {
            return args
                .next()
                .filter(|next| !next.starts_with("--"))
                .map(std::path::PathBuf::from)
                .or(env_root)
                .or_else(|| Some(std::path::PathBuf::from("./principals")));
        }
    }
    env_root
}

/// Exit unless the Memory API is on, naming the settings file checked and why it failed
/// (issue #744). The decision and the exit status (1) are unchanged.
fn gate_or_exit(db_path: &str, db_env_set: bool) {
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
}

/// Multi-principal mode (ShogunAgent E2-T2): `--multi` on the command line, or `SHOGUN_MULTI_ROOT`
/// set. One process serves `<root>/<principal>/memory.db` for every principal the bearer map
/// names, opening each file on demand (`SHOGUN_MULTI_MAX_OPEN`, default 64) with the key from
/// `SHOGUN_DB_KEY_<PRINCIPAL>` or `SHOGUN_DB_KEY`. Tokens come from
/// `SHOGUN_PRINCIPAL_TOKENS="principal:token[:agent_id],…"` until `apps/api` issues agent keys
/// (E3). None of the desktop gates (memory_api.json, plan files, Keychain, heartbeat) apply.
async fn serve_multi(root: std::path::PathBuf) -> std::io::Result<()> {
    use shogun_core::tenancy::{EnvKeyProvider, PrincipalStore, TokenPrincipalMap};
    let keys = EnvKeyProvider::from_env().unwrap_or_else(|e| {
        eprintln!("{FACE}: {e}");
        std::process::exit(1);
    });
    let resolver = TokenPrincipalMap::from_env().unwrap_or_else(|e| {
        eprintln!("{FACE}: {e}");
        std::process::exit(1);
    });
    if resolver.is_empty() {
        eprintln!(
            "{FACE}: no principal tokens loaded ({}) — every memory call is 401 until one is set",
            TokenPrincipalMap::ENV_VAR
        );
    }
    let max_open = std::env::var("SHOGUN_MULTI_MAX_OPEN")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(64);
    let clock = wall_clock();
    let store = Arc::new(PrincipalStore::new(root.clone(), Arc::new(keys), clock.clone(), max_open));
    let state = shogun_core::tenancy::multi::app_state(store, Arc::new(resolver), clock);
    let port = std::env::var("SHOGUN_API_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT);
    let listener = bind_local(port).await?;
    let addr = listener.local_addr()?;
    println!(
        "shogun-memory-api listening on http://{addr}  (multi-principal root: {}, max open: {max_open})",
        root.display()
    );
    serve_on(listener, state.with_port(addr.port())).await
}

/// `shogun-api --dream-tick [--multi <root>]` (plan E5-T4): run the hourly Dream Cycle once for
/// every principal under the multi-principal root (`--multi <root>`, else `SHOGUN_MULTI_ROOT`,
/// else `./principals`) and exit. A principal with no new material since its cursor is not
/// reviewed and costs no model call. This build reviews with the on-device local rules (no model,
/// no egress) until a hosted Batch credential is wired per principal; the Batch-lane variant is
/// `tenancy::dream_tick::hourly_tick_with_lane`. Prints one line of counts, never content.
fn dream_tick(root: std::path::PathBuf) -> std::io::Result<()> {
    use shogun_core::tenancy::{dream_tick, EnvKeyProvider, PrincipalStore};
    let keys = EnvKeyProvider::from_env().unwrap_or_else(|e| {
        eprintln!("{FACE}: {e}");
        std::process::exit(1);
    });
    let clock = wall_clock();
    let now = (clock)();
    // One file open at a time: the tick visits each principal once. Same open policy as
    // `--multi` (the sidecar owns these files); only files already on disk are visited.
    let store = PrincipalStore::new(root, Arc::new(keys), clock, 1);
    let principals = dream_tick::principals_on_disk(&store);
    let report = dream_tick::hourly_tick_local(&store, &principals, now);
    println!("{FACE}: dream tick: {}", dream_tick::summary(&report));
    Ok(())
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    if std::env::args().skip(1).any(|arg| arg == "--dream-tick") {
        let root = multi_root(std::env::args().skip(1)).unwrap_or_else(|| std::path::PathBuf::from("./principals"));
        return dream_tick(root);
    }
    if let Some(root) = multi_root(std::env::args().skip(1)) {
        return serve_multi(root).await;
    }
    // Audit 2026-10-20 M7: plan resolution reads the Keychain mirrors of the trial stamp and the
    // time high-water mark, so editing onboarding.json / billing.json cannot stretch the plan.
    // The Mac app's build only (feature `desktop`); the server has no Keychain to mirror.
    #[cfg(feature = "desktop")]
    shogun_core::plan_mirrors::install();
    let db_env = std::env::var("SHOGUN_DB_PATH").ok();
    let db_env_set = db_env.is_some();
    let db_path = db_env.unwrap_or_else(|| "./shogun.db".to_string());
    // Audit 2026-10-20 M6: check memory_api.json and the capture settings against the seals the
    // app writes, before the gate reads the first of them. This face never adopts an unsealed
    // file; the app does that on its first run after the upgrade.
    if let Some(key) = shogun_core::db_backend::BackupRetentionConfig::for_daemon(&db_path)
        .key
        .and_then(|load| load())
    {
        shogun_core::settings_seal::install_from_db_key(&key, false);
    }
    gate_or_exit(&db_path, db_env_set);
    let port = std::env::var("SHOGUN_API_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT);

    let clock = wall_clock();
    // Never migrates (#800): only the desktop upgrades the store, after its verified snapshot.
    let store = DaemonDb::open(&db_path, clock.clone()).unwrap_or_else(|e| {
        eprintln!("{FACE}: open db {db_path}: {e}");
        std::process::exit(1);
    });
    if let Some(why) = store.unavailable() {
        eprintln!("{FACE}: {}", why.line());
    }
    let db = store.db().clone();
    let metrics = metrics_source(db.clone());
    let backend = Arc::new(db_backend(db));

    // Issue #400: the token list is live. A token revoked in Settings (or with "Remove from …")
    // is 401 on its next request, and a new one works at once, with no restart. The first read
    // still fails the start-up if the Keychain cannot be read, as before.
    let live_tokens = Arc::new(LiveTokens::watching(
        revision_path(&memory_api_settings::resolve_settings_path(&db_path)),
        load_token_registry,
    ));
    let tokens = live_tokens.current();
    if tokens.is_unavailable() {
        // The reason the one failed read gave (a Keychain status): no second read, no prompt.
        let why = live_tokens.last_error().unwrap_or_default();
        eprintln!("{}", startup_report::tokens_unavailable(FACE, &why));
        std::process::exit(1);
    }
    if tokens.is_empty() {
        eprintln!("{FACE}: no Memory API tokens loaded yet — tool calls are 401 until one is issued in Settings (only /v1/status is open)");
    }

    let approvals = Arc::new(Mutex::new(ApprovalQueue::new()));
    // Plan gate (issue #97): the trial stamp from the desktop app's onboarding.json and the signed
    // licence in billing.json (the SHOGUN_*_JSON path overrides are debug builds only).
    // Re-resolved on every request so a trial expiring while the server runs locks the next call.
    let plan_source = shogun_mcp::plan_source::FilePlanSource::from_env();
    let plan_clock = clock.clone();
    // Issue #744: say once why a locked plan will answer 403, and name it in every 403 body.
    if let Some(lock) = plan_source.diagnose(u64::try_from((clock)()).unwrap_or(0)).lock {
        eprintln!("{}", startup_report::plan_lock_note(FACE, lock));
    }
    let lock_source = plan_source.clone();
    let lock_clock = clock.clone();
    let approvals_path = shogun_mcp::approval_store::resolve_store_path(&db_path);
    // Pending L3 bodies are sealed under a key derived from the memory DB key: the
    // same key this process opened the store with, through the daemons' loader (`SHOGUN_DB_KEY`,
    // else the Keychain). Without it, this face can poll but refuses to queue a new L3 request.
    let approval_seal = shogun_core::db_backend::BackupRetentionConfig::for_daemon(&db_path)
        .key
        .and_then(|load| load())
        .map(|key| shogun_mcp::approval_store::ApprovalSealKey::from_db_key(&key));
    if approval_seal.is_none() {
        eprintln!("{FACE}: memory key unavailable for the approval store; L3 requests from this face will be refused");
    }
    let approval_store = shogun_mcp::approval_store::ApprovalStore::new(approvals_path, approval_seal);
    let heartbeat_path = shogun_mcp::desktop_heartbeat::resolve_path(&db_path);
    // 2026-10-20 cost audit: model-starting calls are limited per token, in a window shared
    // with the stdio faces through a file beside the memory DB. One limiter for both faces here.
    let llm_limiter = Arc::new(shogun_mcp::rate_limit::LlmRateLimiter::shared(
        shogun_mcp::rate_limit::resolve_store_path(&db_path),
    ));
    // E4-T1: the MCP face at `POST /mcp` (Streamable HTTP, stateless) over the same backend,
    // token list, approval queue, plan gate and store gate as REST. Every request is authenticated
    // by its own bearer against the live list (`handle_line_as`); there is no process session.
    // E15: one remote-approval configuration (base URL, signing key, ledger) for both faces, so an
    // approval queued over MCP and decided over REST is one record.
    let remote = shogun_mcp::remote_approvals::RemoteApprovals::from_env();
    let mcp = {
        let clock = clock.clone();
        let plan_source = plan_source.clone();
        let plan_clock = clock.clone();
        let lock_source = plan_source.clone();
        let lock_clock = clock.clone();
        let store = store.clone();
        let heartbeat_path = heartbeat_path.clone();
        McpServer::new(backend.clone(), approvals.clone(), move || (clock)(), move || {
            plan_source.resolve(u64::try_from((plan_clock)()).unwrap_or(0))
        })
        .with_live_session(live_tokens.clone(), None, true)
        .with_plan_lock(move || lock_source.diagnose(u64::try_from((lock_clock)()).unwrap_or(0)).lock)
        .with_memory_gate(move || store.unavailable())
        .with_approval_store(approval_store.clone())
        .with_remote_approvals(remote.clone())
        .with_llm_rate_limiter(llm_limiter.clone())
        .with_desktop_running_check(move || {
            shogun_mcp::desktop_heartbeat::fresh(
                &heartbeat_path,
                shogun_mcp::desktop_heartbeat::now_ms(),
            )
        })
    };
    let state = AppState::new(tokens, backend, approvals, clock)
        .with_live_tokens(live_tokens)
        .with_mcp(Arc::new(mcp))
        .with_metrics(metrics)
        .with_entitlements(Arc::new(move || {
            plan_source.resolve(u64::try_from((plan_clock)()).unwrap_or(0))
        }))
        .with_plan_lock(move || lock_source.diagnose(u64::try_from((lock_clock)()).unwrap_or(0)).lock)
        // #800: every memory request re-checks the store's schema; no restart after an upgrade.
        .with_memory_gate(move || store.unavailable())
        .with_approval_store(approval_store)
        .with_remote_approvals(remote)
        .with_llm_rate_limiter(llm_limiter)
        .with_desktop_running_check(move || {
            shogun_mcp::desktop_heartbeat::fresh(
                &heartbeat_path,
                shogun_mcp::desktop_heartbeat::now_ms(),
            )
        });

    let listener = bind_local(port).await?;
    let addr = listener.local_addr()?;
    if addr.port() != port {
        eprintln!(
            "{FACE}: port {port} is in use, so this server is on {} instead; `shogun api status` reports it.",
            addr.port()
        );
    }
    // Issue #776: record the port actually bound, so the CLI dials this server rather than
    // guessing 7464 and handing its token to whatever process holds that port.
    let port_file = shogun_mcp::api_port::port_file_path(std::path::Path::new(&db_path));
    if let Err(error) = shogun_mcp::api_port::write(&port_file, addr.port()) {
        eprintln!("{FACE}: could not record the port for `shogun` ({}); set SHOGUN_API_PORT={} for the CLI.", error.kind(), addr.port());
    }
    println!("shogun-memory-api listening on http://{addr}  (db: {db_path})");
    serve_on(listener, state.with_port(addr.port())).await
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Boot the real server on a background thread (own runtime) and return its port. Uses an
    /// in-memory DB seeded with nothing; the token `dev` is issued.
    fn boot_server() -> u16 {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let db = Db::open_in_memory(wall_clock()).unwrap();
                let metrics = metrics_source(db.clone());
                let backend = Arc::new(db_backend(db));
                let mut tokens = TokenRegistry::new();
                tokens.issue("dev");
                let approvals = Arc::new(Mutex::new(ApprovalQueue::new()));
                let state = AppState::new(Arc::new(tokens), backend, approvals, wall_clock())
                    .with_entitlements(Arc::new(|| {
                        shogun_agents::entitlement::entitlements(
                            shogun_agents::entitlement::Plan::Pro,
                            0,
                        )
                    }))
                    .with_metrics(metrics);
                let listener = bind_local(0).await.unwrap();
                let port = listener.local_addr().unwrap().port();
                tx.send(port).unwrap();
                let _ = serve_on(listener, state).await;
            });
        });
        let port = rx.recv().unwrap();
        // give the listener a moment to start accepting
        std::thread::sleep(Duration::from_millis(300));
        port
    }

    #[test]
    fn cli_client_drives_the_real_server_end_to_end() {
        use shogun_cli::http::request;
        let port = boot_server();

        // status is open
        let r = request(port, "GET", "/v1/status", None, None).unwrap();
        assert_eq!(r.status, 200);
        assert!(r.body.contains("shogun-memory-api"));

        // a tool call without a token is 401 (FR-API-03)
        let r = request(port, "GET", "/v1/state/people", None, None).unwrap();
        assert_eq!(r.status, 401);

        // write a note, then search it back — the full write→persist→read loop over the socket
        let r = request(
            port,
            "POST",
            "/v1/memory/notes",
            Some("dev"),
            Some("call Bob about the roadmap"),
        )
        .unwrap();
        assert_eq!(r.status, 202);
        assert!(r.body.contains("\"note_id\":"));

        let r = request(
            port,
            "GET",
            "/v1/memory/search?q=roadmap",
            Some("dev"),
            None,
        )
        .unwrap();
        assert_eq!(r.status, 200);
        assert!(
            r.body.contains("call Bob about the roadmap"),
            "search body: {}",
            r.body
        );

        // the in-product SLO snapshot is served, open like status (NFR-SLO-00); empty registry ⇒
        // every SLO reads unmeasured, never a false green (spec §4.5).
        let r = request(port, "GET", "/v1/metrics", None, None).unwrap();
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"metrics\":"), "metrics body: {}", r.body);
        assert!(r.body.contains("NFR-SLO-01"), "metrics body: {}", r.body);
        assert!(
            r.body.contains("\"measured\":false"),
            "unmeasured SLOs must not read as pass: {}",
            r.body
        );
        // the D-6 lesson counters ride on the same surface: a live DB reports real (zero) counts
        assert!(
            r.body.contains(
                r#""lessons":{"active_lessons":0,"feedback_events_last_7d":0,"edit_before_approve_last_7d":0,"approve_unchanged_last_7d":0,"lesson_hit_rate":{"measured":false},"measured":true}"#
            ),
            "lesson counters missing: {}",
            r.body
        );
        assert!(
            r.body.contains(r#""harness":{"measured":false}"#),
            "unmeasured harness must not read as zeros: {}",
            r.body
        );
        assert!(
            r.body.contains(r#""sanitizer":{"events_stripped":"#),
            "sanitizer counts missing: {}",
            r.body
        );

        // An external send never runs (FR-API-04, invariant 4). In THIS binary it is also never
        // accepted: standalone shogun-api has no confirm UI, so an enqueued L3 proposal would sit
        // until it expired and the caller would have been told "pending" for something that could
        // not be approved. Absent surface ⇒ 501, said out loud.
        let r = request(
            port,
            "POST",
            "/v1/actions/execute",
            Some("dev"),
            Some(r#"{"kind":"send_email","to":"a@b.com","subject":"s","body":"b"}"#),
        )
        .unwrap();
        assert_eq!(r.status, 501);
        assert!(r.body.contains("no_approval_surface"), "body: {}", r.body);
        assert!(
            !r.body.contains("\"pending\":true"),
            "must not read as accepted: {}",
            r.body
        );
    }
}
