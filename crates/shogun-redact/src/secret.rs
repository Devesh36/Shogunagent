//! The [`Secret`] type: a string that never renders in logs.
//!
//! Defined here (dependency-free) rather than in `shogun-core::llm` so crates below the core —
//! `shogun-integrations`' token store (E5-T5) — hold credentials in the same type the HTTP egress
//! reads them from. `shogun_core::llm::Secret` re-exports it, so every existing path still names
//! one type. `.expose(` stays confined to the files `scripts/check-secret-exposure.py` allows.

use std::fmt;

/// A secret string that never appears in logs. `Debug` and `Display` render a fixed redaction;
/// the real value is only reachable via [`Secret::expose`], which callers must go out of their
/// way to use (and never log).
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    /// The raw secret. The single choke point for reading it — never pass the result to a log,
    /// a record, or telemetry (NFR-SEC-01/02).
    pub fn expose(&self) -> &str {
        &self.0
    }
    /// Last-4 for a UI echo (NFR-SEC-02: BYOK read-back shows only the last four chars).
    pub fn last4(&self) -> String {
        let n = self.0.chars().count();
        self.0.chars().skip(n.saturating_sub(4)).collect()
    }
    /// Whether the secret is empty (an unset credential), without exposing it.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl PartialEq for Secret {
    /// Constant-time over equal lengths, so comparing a stored credential with a candidate does
    /// not leak a prefix through timing. Lengths are not hidden.
    fn eq(&self, other: &Self) -> bool {
        let (a, b) = (self.0.as_bytes(), other.0.as_bytes());
        if a.len() != b.len() {
            return false;
        }
        a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }
}

impl Eq for Secret {}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***redacted***)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***redacted***")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_redacted_and_exposes_only_on_request() {
        let s = Secret::new("xoxb-123456");
        assert_eq!(format!("{s:?}"), "Secret(***redacted***)");
        assert_eq!(format!("{s}"), "***redacted***");
        assert_eq!(s.expose(), "xoxb-123456");
        assert_eq!(s.last4(), "3456");
        assert!(!s.is_empty());
    }

    #[test]
    fn equality_compares_values() {
        assert_eq!(Secret::new("abc"), Secret::new("abc"));
        assert_ne!(Secret::new("abc"), Secret::new("abd"));
        assert_ne!(Secret::new("abc"), Secret::new("abcd"));
    }
}
