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

use super::{PrincipalResolver, PrincipalStore};

/// The resolver the server calls per request: token → principal → open store → backend.
pub struct MultiBackends {
    store: Arc<PrincipalStore>,
    resolver: Arc<dyn PrincipalResolver>,
}

impl MultiBackends {
    pub fn new(store: Arc<PrincipalStore>, resolver: Arc<dyn PrincipalResolver>) -> Self {
        Self { store, resolver }
    }

    pub fn store(&self) -> &Arc<PrincipalStore> {
        &self.store
    }
}

impl BackendResolver for MultiBackends {
    fn resolve(&self, token: &str) -> Result<ResolvedBackend, (u16, String)> {
        let who = self
            .resolver
            .resolve(token)
            .ok_or_else(|| (401, r#"{"error":"unauthorized"}"#.to_string()))?;
        let handle = self.store.open(&who.principal).map_err(|error| {
            (
                503,
                serde_json::json!({
                    "error": "memory_unavailable",
                    "message": error.to_string(),
                    "next_action": "retry_after",
                })
                .to_string(),
            )
        })?;
        Ok(ResolvedBackend { backend: handle.memory_backend(), actor: who.actor })
    }
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
    let approvals = Arc::new(Mutex::new(ApprovalQueue::new()));
    AppState::new(tokens, Arc::new(NoPrincipalBackend), approvals, clock)
        .with_entitlements(Arc::new(|| {
            shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0)
        }))
        .with_backend_resolver(Arc::new(MultiBackends::new(store, resolver)))
}
