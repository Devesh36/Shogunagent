//! SHOGUN first-layer MCP integration (§6.9). v1 milestone: the per-service **permission scope
//! table** (§6.9.2), which is the source of truth for what each connected service may do and at
//! which permission level.
//!
//! The load-bearing acceptance rule (§6.9): *an operation not in the table is denied by the
//! permission table*, and every external send is L3 (invariant 4). Both are encoded in
//! [`scope`] and enforced by tests over the tables — no service adapter re-derives the policy.
//!
//! Platform-independent so the policy is exhaustively Linux-testable; the MCP client transport and
//! OAuth-to-Keychain live in the desktop adapter.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

/// Re-export of the plan entitlement vocabulary (issue #97) so downstream crates that reach the
/// Memory API through shogun-mcp alone (e.g. shogun-core's `db` feature) can name it without a
/// direct shogun-agents dependency.
pub use shogun_agents::entitlement;

pub mod activity_api;
/// The frozen core tool set: agent-facing descriptions, generated annotations, cost classes
/// (E4-T3 / T4 / T5).
pub mod core_tools;
/// `memory.ingest` / `memory.recall`: one parser and envelope for every face (E4-T3).
pub mod ingest_api;
/// `memory.fs` (E6-T2): the memory tool's file operations over `/memories`.
pub mod memfs_tool;
/// The stateless Streamable HTTP transport, as pure request → reply logic (E4-T1).
pub mod mcp_http;
/// MRTR and URL-mode elicitation: how L2 / L3 look on the MCP face (E4-T2).
pub mod mrtr;
pub mod ai_search_api;
pub mod api_port;
pub mod approval_store;
pub mod reply_thread;
pub mod backend;
pub mod forms_api;
pub mod clarify_api;
pub mod steer_api;
pub mod client_config;
pub mod client_discovery;
// One-click "Add to …" (issue #400): plan, preview and apply SHOGUN's entry in another app's MCP
// config — SHOGUN's entry only, never a token, applied in-app only behind a confirmation dialog.
pub mod client_install;
pub mod composio;
pub mod composio_policy;
pub mod connection;
pub mod connections_api;
pub mod desktop_heartbeat;
pub mod meeting_recap_status;
pub mod onboarding_state_api;
pub mod account_status_api;
pub mod dictionary_autoadd_api;
pub mod dispatch;
pub mod export_folders;
pub mod gmail_policy;
// `launcher_check` is the pure policy table (which app must have started `shogun-mcp`); the
// token listing names each connection's policy, so every build carries it. Reading the process
// tree's signatures is the Mac app's (`shogun_core::desktop::launcher`).
pub mod launcher_check;
/// The Mac app's data folder (feature `desktop`, E1-T7): where `plan_source` finds the app's
/// files.
#[cfg(feature = "desktop")]
pub mod desktop_paths;
pub mod live_tokens;
pub mod mcp;
pub mod memory_api;
pub mod memory_notes_api;
pub mod memory_api_settings;
pub mod memory_import_api;
pub mod memory_readiness;
pub mod meeting_microphone_api;
pub mod meeting_recap_prefs_api;
pub mod meeting_record_now_api;
pub mod meeting_system_audio_api;
pub mod plan_source;
pub mod translate_api;
pub mod translate_register_api;
pub mod trial_relay;
pub mod ui_language_api;
pub mod update_status_api;
pub mod rate_limit;
pub mod rest;
pub mod scope;
pub mod search_index_api;
pub mod startup_report;
#[cfg(feature = "server")]
pub mod server;
pub mod service_gate;
mod settings_file;
pub mod settings_seal_hook;
pub mod slack;
pub mod source_policy;
pub mod support_reports;
pub mod sync;
pub mod token_clipboard;
pub mod tool_catalog;
#[cfg(unix)]
pub mod chat_bridge;
pub mod tool_loop;
pub mod toolsets;
pub mod visual_recall_api;
pub mod voice_dictionary_api;
pub mod voice_microphone_api;
pub mod writing_voice_api;
pub mod capture_exclusions_api;
pub mod capture_pause_api;
pub mod visual_recall_settings_api;
pub mod mail_archive_api;
pub mod agent_source_api;
pub mod web_consent;

pub mod calendar_reminder_api;
