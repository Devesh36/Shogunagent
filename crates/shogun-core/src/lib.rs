//! SHOGUN core — pure, platform-independent behavioural logic.
//!
//! The macOS shell (`apps/desktop/src-tauri`) is a thin adapter: it sources OS events
//! (CGEventTap, NSWorkspace, AXUIElement, timers) and feeds them into these modules, then
//! applies the emitted effects. Keeping the logic here makes the notch behaviour (Q2 expand,
//! Q4 false-positive) and the capture walk policy unit-testable without a Mac.
//!
//! Core modules:
//! - [`notch`] — the notch UI: screen geometry / hit regions, hover judgement, the state
//!   machine, and the integrated engine that wires them together.
//! - [`capture`] — the context-capture policy: the bounded accessibility-tree walk.
//! - [`metrics`] — the always-on SLO histograms (NFR-SLO-00): fixed-bucket latency/percent
//!   tracking with pass/fail against each SLO budget.
//! - [`llm`] — the two model-access lanes (Batch / Agent) with compile-time key separation
//!   (invariant 5) and secret redaction.
//! - [`bus`] — the internal event bus (§5.3, AR-06/07): a non-blocking broadcast with
//!   backpressure-by-drop and a drop metric.
//! - [`dreamcycle`] — the nightly-batch orchestration logic (§6.7): run-condition gate, resumable
//!   job plan, and Batch-API failure escalation. Effects stay behind seams; the rules are pure.
//!
//! This crate makes no process-boundary assumptions (AR-03): it is a library the Tauri
//! backend hosts today and a future daemon could host unchanged.
//!
//! **Feature `desktop` (E1-T7).** The modules that exist only for the ShogunAI Mac app sit behind
//! `#[cfg(feature = "desktop")]`: the capture pipeline, the audio lane, the notch, inline / Scribe
//! drafting, form filling, the Gmail transport, the host-shell bridges and the Keychain glue
//! (`desktop`). The default build is the ShogunAgent server: it carries none of them and no
//! `cfg(target_os = "macos")` site (`scripts/check-desktop-gate.py`). Modules the data layer
//! shares with the Mac app — capture policy (`capture::{exclusion, control, status, visual_recall,
//! dedup, own_app}`), stored meeting notes (`meeting::{minutes, export, recap, templates, …}`),
//! translation jobs, the voice dictionary, writing-voice mining — stay in the default build
//! because the Memory API tools that serve them do.
//!
//! CLAUDE.md forbids `unwrap()`/`expect()` outside tests; test modules are exempted below.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

#[cfg(feature = "desktop")]
pub mod auto_update;
pub mod ai_search;
pub mod analytics;
pub mod backup_retention;
#[cfg(feature = "desktop")]
pub mod audio;
pub mod bus;
pub mod capture;
pub mod context;
/// The Mac app's glue for the standalone faces (feature `desktop`): the login Keychain reads the
/// daemons make, the launcher check. Every `cfg(target_os = "macos")` the server build must not
/// carry lives here or in another `desktop`-gated module.
#[cfg(feature = "desktop")]
pub mod desktop;
#[cfg(feature = "db")]
pub mod daemon;
pub mod daily_delivery;
#[cfg(feature = "db")]
pub mod db_backend;
#[cfg(feature = "db")]
pub mod tenancy;
/// The `/memories` virtual filesystem over the world model (E6).
#[cfg(feature = "db")]
pub mod memfs;
#[cfg(all(feature = "desktop", any(feature = "db", feature = "net")))]
pub mod plan_mirrors;
pub mod settings_seal;
#[cfg(feature = "db")]
pub mod db_sink;
#[cfg(feature = "db")]
pub mod search_index_compact;
pub mod dreamcycle;
// Issue #397: LocalAction::WriteFile — exports into user-chosen folders (L2, never overwrites).
#[cfg(feature = "db")]
pub mod file_export;
#[cfg(feature = "desktop")]
pub mod forms;
#[cfg(feature = "desktop")]
pub mod inline;
#[cfg(feature = "desktop")]
pub mod paste_verification;
pub mod inline_context;
pub mod when;
#[cfg(feature = "desktop")]
pub mod known_links;
#[cfg(feature = "desktop")]
pub mod left_option_hold;
#[cfg(feature = "db")]
pub mod rewrite_context;
#[cfg(feature = "db")]
pub mod rewrite_mining;
pub mod writing_recipient;
pub mod llm;
pub mod agent_source;
pub mod model_catalog;
pub mod deep_context;
pub mod log_redact;
pub mod memory_health;
#[cfg(feature = "net")]
pub mod composio_send;

#[cfg(all(feature = "desktop", feature = "net"))]
pub mod gmail_direct;
#[cfg(all(feature = "desktop", feature = "net"))]
pub mod gmail_local;
#[cfg(feature = "net")]
pub mod mcp_http;
#[cfg(feature = "net")]
pub mod mcp_oauth;
// E44: the connector read-sync's GET (Gmail, Calendar, Notion), the single egress.
#[cfg(feature = "net")]
pub mod provider_fetch;
#[cfg(feature = "net")]
pub mod github_device;
// The pinned static-asset download (no Mac code): the desktop's whisper weights and, since E24, the
// server's embedding model (`embed_models`). Server-neutral, so it rides `net` alone.
#[cfg(feature = "net")]
pub mod model_asset;
/// The local embedding model: where it lives, `shogun models fetch`, and loading it (E24).
#[cfg(feature = "db")]
pub mod embed_models;
/// The account API (#148 P2): browser sign-in over loopback + PKCE, the session's refresh, devices,
/// re-authentication and sign-out. Fail-closed `Route::Billing` rows, content-free.
#[cfg(feature = "net")]
pub mod account_client;
/// Licence verification against the Stripe-backed licence API (issue #8 / FR-BIL-08). Sends the
/// licence key + anonymous device id only; returns the signed token the Rust core verifies.
#[cfg(feature = "net")]
pub mod license_client;
#[cfg(feature = "net")]
pub mod support_client;
pub mod meeting;
#[cfg(feature = "net")]
pub mod composio_read;
pub mod gmail_shape;
#[cfg(all(feature = "desktop", feature = "db"))]
pub mod mail_actions;
#[cfg(feature = "db")]
pub mod mail_batch;
pub mod metrics;
#[cfg(feature = "desktop")]
pub mod notch;
#[cfg(feature = "desktop")]
pub mod panel_events;
pub mod chat_prompt;
pub mod chat_failure;
#[cfg(feature = "db")]
pub mod chat_grounding;
#[cfg(feature = "db")]
pub use daemon::auto_memory;
pub mod screen_context;
pub mod context_sufficiency;
#[cfg(feature = "exec")]
pub mod send_exec;
/// The executor behind a remote approval (E15 follow-up): runs an approved action through
/// `send_exec` once a human decided it.
#[cfg(feature = "exec")]
pub mod approval_exec;
#[cfg(feature = "exec")]
pub mod tool_runner;
#[cfg(feature = "desktop")]
pub mod self_install;
#[cfg(feature = "desktop")]
pub mod sound;
pub mod traceview;
pub mod reply_draft;
#[cfg(feature = "desktop")]
pub mod release_notes;
pub mod web_fetch;
pub mod translate;
pub mod translate_prefs;
pub mod translate_job;
pub mod voice_dictionary;
#[cfg(feature = "db")]
pub mod dictionary_learn;
pub mod user_config;
