//! The principal id: a slug that names a person's store under the root directory.

use std::fmt;
use std::str::FromStr;

use super::TenancyError;

/// `[a-z0-9][a-z0-9_-]{0,63}`: safe as a directory name on every file system, as an environment
/// variable suffix (upper-cased, `-` → `_`) and in a URL path, with no escaping anywhere.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PrincipalId(String);

impl PrincipalId {
    pub const MAX_LEN: usize = 64;

    /// Validate a slug. The input is not normalised: `Alice` is refused, not lower-cased, so the
    /// id a caller sends is the id on disk.
    pub fn parse(raw: &str) -> Result<Self, TenancyError> {
        let bytes = raw.as_bytes();
        let Some(first) = bytes.first() else {
            return Err(TenancyError::InvalidPrincipalId);
        };
        if bytes.len() > Self::MAX_LEN || !first.is_ascii_lowercase() && !first.is_ascii_digit() {
            return Err(TenancyError::InvalidPrincipalId);
        }
        if !bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
        {
            return Err(TenancyError::InvalidPrincipalId);
        }
        Ok(Self(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The suffix of this principal's key variable: `SHOGUN_DB_KEY_<SUFFIX>` (`acme-ops` →
    /// `ACME_OPS`).
    pub fn env_suffix(&self) -> String {
        self.0
            .chars()
            .map(|c| if c == '-' { '_' } else { c.to_ascii_uppercase() })
            .collect()
    }
}

impl fmt::Display for PrincipalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for PrincipalId {
    type Err = TenancyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_slugs_and_refuses_everything_else() {
        for ok in ["a", "alice", "acme-ops", "p_01", "0x9", &"z".repeat(64)] {
            assert!(PrincipalId::parse(ok).is_ok(), "{ok}");
        }
        for bad in ["", "Alice", "-a", "_a", "a b", "a/b", "../x", "a.b", &"z".repeat(65), "日本"] {
            assert!(matches!(PrincipalId::parse(bad), Err(TenancyError::InvalidPrincipalId)), "{bad}");
        }
        assert_eq!(PrincipalId::parse("acme-ops").unwrap().env_suffix(), "ACME_OPS");
        assert_eq!("alice".parse::<PrincipalId>().unwrap().to_string(), "alice");
    }
}
