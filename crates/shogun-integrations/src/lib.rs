//! SHOGUN first-layer connector adapter — the effectful side of §6.9 that turns the pure scope
//! table ([`shogun_mcp`]) into real reads/writes against Google Workspace's **official** remote MCP
//! servers (Gmail / Calendar / Drive), direct user→Google with no third party in the data path.
//!
//! Layering (the pure/effect split used across the workspace):
//! - `endpoints` — Service → Google MCP URL + least-privilege OAuth scopes (pure data).
//! - `toolmap` — our scope-op name → Google MCP tool name (pure data).
//! - `result` — MCP `tools/call` reply → normalized `FetchedItem` (pure).
//! - `rpc` — the `McpRpc` / `TokenProvider` seams (pure traits).
//! - `transport` — `RemoteMcpTransport` implements `IntegrationTransport`, composing the above.
//! - `oauth_flow` — feature `live`: the interactive OAuth loopback flow (browser + loopback socket) over the `TokenExchange` seam. No HTTP client — the concrete reqwest impls live in shogun-core (the single traced egress, FR-TR-03).
//! - `token_store` — where a principal's credentials live (E5-T5): the [`TokenStore`] trait over
//!   principal × slot, holding [`Secret`]s; the file-sealed store for a local Linux server, the
//!   interface the hosted KMS-sealed Postgres store implements, and the Mac app's Keychain
//!   (`keychain`, feature `desktop`).
//!
//! Scope notes (product decision 2026-07-23):
//! - Google Docs / Sheets have no dedicated official MCP server — their content is read via Drive's `read_file_content`, and there is no first-layer write path for them.
//! - Gmail send is intentionally unreachable here (no `send` tool, no `gmail.send` scope) — sending is the second layer (Composio, §6.10); first-layer Gmail reads and drafts only.
//! - Policy is not decided in this crate — the daemon gates every op through `service_gate::authorize_op` before calling the transport.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod composio;
pub mod connect;
pub mod device_flow;
pub mod endpoints;
pub mod hosted_connect;
pub mod oauth;
pub mod provider_read;
pub mod read_sync;
pub mod result;
pub mod rpc;
pub mod runtime;
pub mod send_bridge;
pub mod slack_target;
pub mod token;
pub mod token_store;
pub mod toolmap;
pub mod transport;

// The Mac app's Keychain and code-signature readers (feature `desktop`; each file is macOS-only
// inside). The server keeps tokens in `token_store` instead (E5-T5).
#[cfg(feature = "desktop")]
pub mod keychain;
#[cfg(feature = "desktop")]
pub mod keychain_store;
#[cfg(feature = "desktop")]
pub mod code_signature;
pub mod keychain_fallback;
pub mod keychain_accounts;
#[cfg(feature = "live")]
pub mod oauth_flow;
#[cfg(all(feature = "live", feature = "desktop"))]
mod browser;

pub use composio::ComposioApi;
pub use connect::ConnectError;
pub use oauth::{AuthConfig, Pkce, TokenExchange, TokenSet};
pub use rpc::{McpRpc, StaticTokenProvider, TokenProvider};
pub use runtime::{
    persisted_consent_probe, publish_statuses, ConnUi, ConnectorRuntime, ConsentProbe, FetchKind, IngestCount,
    IngestSink, ServiceStatus, SyncReport,
};
pub use send_bridge::{args_for_send, route_send, SendRoute};
pub use token::{GrantStore, ManagedTokenProvider, MemoryTokenStore, TokenError, TokenManager};
pub use token_store::{
    CredentialSlot, FileSealedTokenStore, MemorySecretStore, PrincipalGrants, RemoteTokenStore, Secret,
    TokenStore, TokenStoreError,
};
pub use transport::{RemoteMcpTransport, WriteExecutor};
