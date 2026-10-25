//! `shogun-api --multi` (E2-T2): one REST server, many principals. The bearer names the principal
//! through a [`PrincipalResolver`]; the [`PrincipalStore`] opens that principal's file; every
//! write on the request records the resolver's actor; every read is world-only unless the request
//! asks for scratch. The single-principal composition in `bin/shogun_api.rs` is untouched.

use std::sync::{Arc, Mutex};

use shogun_agents::approval::ApprovalQueue;
use shogun_mcp::backend::{MemoryBackend, ReadItem, ReadParams};
use shogun_mcp::memory_api::{Tool, TokenRegistry};
use shogun_mcp::server::{AppState, BackendResolver, ResolvedBackend};

use crate::daemon::Clock;

use super::{PrincipalId, PrincipalResolver, PrincipalStore, TeamRegistry};

/// The resolver the server calls per request: token → principal → open store → backend.
pub struct MultiBackends {
    store: Arc<PrincipalStore>,
    resolver: Arc<dyn PrincipalResolver>,
    teams: Arc<TeamRegistry>,
}

fn unavailable(error: &super::TenancyError) -> (u16, String) {
    (
        503,
        serde_json::json!({
            "error": "memory_unavailable",
            "message": error.to_string(),
            "next_action": "retry_after",
        })
        .to_string(),
    )
}

impl MultiBackends {
    /// The teams come from `<root>/teams.json` (an unreadable file leaves no teams: the safe
    /// direction, no cross-principal read).
    pub fn new(store: Arc<PrincipalStore>, resolver: Arc<dyn PrincipalResolver>) -> Self {
        let teams = TeamRegistry::load(store.root()).unwrap_or_else(|_| TeamRegistry::in_memory());
        Self::with_teams(store, resolver, Arc::new(teams))
    }

    pub fn with_teams(store: Arc<PrincipalStore>, resolver: Arc<dyn PrincipalResolver>, teams: Arc<TeamRegistry>) -> Self {
        Self { store, resolver, teams }
    }

    pub fn store(&self) -> &Arc<PrincipalStore> {
        &self.store
    }

    pub fn teams_registry(&self) -> &Arc<TeamRegistry> {
        &self.teams
    }
}

impl BackendResolver for MultiBackends {
    fn resolve(&self, token: &str) -> Result<ResolvedBackend, (u16, String)> {
        let who = self
            .resolver
            .resolve(token)
            .ok_or_else(|| (401, r#"{"error":"unauthorized"}"#.to_string()))?;
        let handle = self.store.open(&who.principal).map_err(|e| unavailable(&e))?;
        Ok(ResolvedBackend {
            backend: handle.memory_backend(),
            actor: who.actor,
            principal: Some(who.principal.as_str().to_string()),
        })
    }

    fn backend_for_principal(&self, principal: &str) -> Result<Arc<dyn MemoryBackend>, (u16, String)> {
        let id = PrincipalId::parse(principal).map_err(|_| (404, r#"{"error":"principal_not_found"}"#.to_string()))?;
        if !self.store.exists(&id) {
            return Err((404, r#"{"error":"principal_not_found"}"#.to_string()));
        }
        Ok(self.store.open(&id).map_err(|e| unavailable(&e))?.memory_backend())
    }

    /// Team (E18): the caller's principal and `target` must share a team, and `target`'s own file
    /// must hold a `world_read` grant for the caller. Every refusal answers the same 403, so the
    /// route is no oracle on which principals exist.
    fn cross_read(&self, token: &str, target: &str) -> Result<ResolvedBackend, (u16, String)> {
        let refused = || (403, shogun_mcp::server::CROSS_READ_REFUSED.to_string());
        let who = self
            .resolver
            .resolve(token)
            .ok_or_else(|| (401, r#"{"error":"unauthorized"}"#.to_string()))?;
        let target = PrincipalId::parse(target).map_err(|_| refused())?;
        if !self.teams.teammates(&who.principal, &target) || !self.store.exists(&target) {
            return Err(refused());
        }
        let handle = self.store.open(&target).map_err(|e| unavailable(&e))?;
        let backend = handle.memory_backend();
        if !backend.principal_may_read(who.principal.as_str()) {
            return Err(refused());
        }
        Ok(ResolvedBackend { backend, actor: who.actor, principal: Some(target.as_str().to_string()) })
    }

    fn teams(&self) -> Option<serde_json::Value> {
        Some(self.teams.to_json())
    }

    /// E39: see [`BackendResolver::purge_principal`]. The purge log is
    /// `<root>/principal-purges.jsonl` (a principal id cannot contain `.`, so it never collides
    /// with a principal's directory): one line per purge with the principal id, what was removed,
    /// the time and the request id. Never a row of the store.
    fn purge_principal(&self, principal: &str, request_id: &str, now_ms: i64) -> Result<serde_json::Value, (u16, String)> {
        let id = PrincipalId::parse(principal).map_err(|_| {
            (400, serde_json::json!({ "error": "invalid_principal", "message": super::TenancyError::InvalidPrincipalId.to_string(), "next_action": "fix_input" }).to_string())
        })?;
        let report = match self.store.purge(&id) {
            Ok(report) => report,
            Err(super::TenancyError::Busy) => {
                return Err((
                    409,
                    serde_json::json!({ "error": "principal_busy", "message": super::TenancyError::Busy.to_string(), "next_action": "retry_after" }).to_string(),
                ))
            }
            Err(e) => return Err(unavailable(&e)),
        };
        let left_teams = self.teams.remove_member(&id).map_err(|e| unavailable(&e))?;
        let record = serde_json::json!({
            "ts_ms": now_ms,
            "principal": id.as_str(),
            "store_removed": report.store_removed,
            "key_removed": report.key_removed,
            "left_teams": left_teams,
            "request_id": request_id,
        });
        append_purge_log(self.store.root(), &record).map_err(|e| unavailable(&super::TenancyError::Io(e)))?;
        Ok(serde_json::json!({
            "principal": id.as_str(),
            "purged": report.store_removed || report.key_removed,
            "store_removed": report.store_removed,
            "key_removed": report.key_removed,
            "left_teams": left_teams,
        }))
    }

    fn set_team(&self, tenant: &str, team: &str, members: &[String]) -> Result<serde_json::Value, (u16, String)> {
        self.teams
            .set(tenant, team, members)
            .map(|t| {
                serde_json::json!({
                    "tenant": t.tenant,
                    "team": t.team,
                    "members": t.members.iter().map(PrincipalId::as_str).collect::<Vec<_>>(),
                })
            })
            .map_err(|e| {
                (
                    400,
                    serde_json::json!({ "error": "invalid_team", "message": e.to_string() }).to_string(),
                )
            })
    }
}

/// The operator's purge log under the multi-principal root (E39).
pub const PURGE_LOG: &str = "principal-purges.jsonl";

/// Append one purge record and sync it: the record outlives the store it describes.
fn append_purge_log(root: &std::path::Path, record: &serde_json::Value) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(root.join(PURGE_LOG))?;
    file.write_all(format!("{record}\n").as_bytes())?;
    file.sync_all()
}

/// The placeholder [`AppState::new`] takes in multi mode. Never consulted: every memory route
/// resolves its backend per request, and a route that could not is refused before any read.
struct NoPrincipalBackend;

impl MemoryBackend for NoPrincipalBackend {
    fn read(&self, _tool: Tool, _params: &ReadParams) -> Vec<ReadItem> {
        Vec::new()
    }
}

/// The token registry for multi mode: every token the resolver knows, full scope. Scope narrowing
/// and plan gates belong to `apps/api` (E3 / E9); the sidecar trusts what the gateway admits.
pub fn token_registry(resolver: &dyn PrincipalResolver) -> TokenRegistry {
    let mut tokens = TokenRegistry::new();
    for token in resolver.tokens() {
        tokens.issue(token);
    }
    tokens
}

/// The server state for `--multi`: per-request backends, a full-plan entitlement (the hosted
/// gates are the wallet and the caps, not a plan), the in-process approval queue, no desktop gates.
pub fn app_state(store: Arc<PrincipalStore>, resolver: Arc<dyn PrincipalResolver>, clock: Clock) -> AppState {
    let tokens = Arc::new(token_registry(resolver.as_ref()));
    // Approval ids are never reused across restarts: the in-process queue starts above every id
    // an earlier process could have handed out (a thousand per millisecond of uptime), so a
    // decision the principal's store persisted (V103) never names a later approval.
    let seed = u64::try_from((clock)()).unwrap_or(0).saturating_mul(1_000).max(1);
    let approvals = Arc::new(Mutex::new(ApprovalQueue::with_next_id(seed)));
    AppState::new(tokens, Arc::new(NoPrincipalBackend), approvals, clock)
        .with_entitlements(Arc::new(|| {
            shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0)
        }))
        .with_backend_resolver(Arc::new(MultiBackends::new(store, resolver)))
}
