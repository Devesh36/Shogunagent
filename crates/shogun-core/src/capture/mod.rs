//! Context-capture policy (spec §3.10 / §6.2).
//!
//! - [`walk_policy`] — the bounded accessibility-tree walk: BFS with depth/element/byte
//!   budgets, role filtering, total skip of `AXSecureTextField` subtrees, and a
//!   cancellation/timebox hook. Pure over an abstract `AxNode`; the macOS adapter implements
//!   `AxNode` over `AXUIElement`.
//! - [`noise`] — per-line cleanup the walk applies (issue #302): invisible characters, whitespace
//!   runs, symbol-only lines, clipped machine blobs. Measured by `crates/ax-noise-bench`.
//! - [`secret_field`] — which text fields count as secret from their placeholder / label /
//!   identifier (password, PIN, card number, CVC, one-time code, …; issue #283 OPEN-283-02).
//!   Every AX capture path treats a secret field exactly like `AXSecureTextField`.
//! - [`own_app`] — is this frontmost process SHOGUN itself? Capture / Idle / meeting must agree.
//! - [`focus_watch`] — when to walk after an app switch (≤300ms SLO, no blocking empty-tree retry).
//! - [`wake`] — let a focus-change notification cut the capture poller's sleep short (#355).
//! - [`accessibility_gate`] — the Accessibility degraded mode: when setup may finish without the
//!   permission, and when the degraded mode ends (FR-CAP-08 / FR-OB-04, #778).

pub mod accessibility_gate;
pub mod dedup;
pub mod control;
pub mod exclusion;
pub mod focus_watch;
pub mod noise;
pub mod own_app;
pub mod pipeline;
pub mod secret_field;
pub mod status;
pub mod visual_recall;
pub mod walk_policy;
pub mod wake;

pub use own_app::is_own_app;
