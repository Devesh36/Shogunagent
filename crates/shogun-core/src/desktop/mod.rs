//! The ShogunAI Mac app's glue for the standalone faces (feature `desktop`, E1-T7).
//!
//! Everything here reaches something only the Mac app has — the login Keychain, the code
//! signature of the process that started `shogun-mcp` — and every `cfg(target_os = "macos")`
//! that the server build must not carry sits in this module or in another `desktop`-gated one
//! (`scripts/check-desktop-gate.py`). Each function also builds off macOS with the feature on,
//! so Linux CI proves the Mac app can keep consuming this crate; there it answers exactly what
//! the server build answers (no Keychain, no signatures).
//!
//! - `keychain` — the Keychain items the daemons read and narrow: the memory DB key, the
//!   Memory API connections, the BYOK presence flags, the connector account labels and the
//!   settings-seal marker.
//! - `tokens` — the Memory API token list the two binaries load at start-up.
//! - `launcher` — the code-signature check on the program that started `shogun-mcp` (#979).

#[cfg(feature = "db")]
pub mod keychain;
#[cfg(any(feature = "db", feature = "net"))]
pub mod launcher;
#[cfg(any(feature = "db", feature = "net"))]
pub mod tokens;
