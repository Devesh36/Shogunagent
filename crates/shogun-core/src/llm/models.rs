//! The model ids SHOGUN chooses itself (as opposed to the one the user types for BYOK chat).
//!
//! One place, undated aliases: a dated id (`claude-haiku-4-5-20251001`) copied into each lane
//! would break all of them at once, in as many files, when the snapshot is retired. The alias
//! tracks the current snapshot on the provider side.

/// The fast, cheap tier for background text work on the development-only direct route: recap
/// minutes and mail triage classification.
pub const FAST_TEXT_MODEL: &str = "claude-haiku-4-5";
