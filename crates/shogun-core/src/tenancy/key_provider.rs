//! Where a principal's SQLCipher key comes from. Keys stay in [`DbKey`] (no `Debug` render) and
//! are never logged; the environment is read once, at construction, so a provider's answer does
//! not change under a running server and no per-request call touches the process environment.

use std::collections::HashMap;

use shogun_memory::DbKey;

use super::{PrincipalId, TenancyError};

/// Resolve the key a principal's file is encrypted with.
pub trait KeyProvider: Send + Sync {
    fn key_for(&self, principal: &PrincipalId) -> Result<DbKey, TenancyError>;
}

/// Keys from the environment: `SHOGUN_DB_KEY_<PRINCIPAL>` (the principal's
/// [`PrincipalId::env_suffix`]) for one principal, else `SHOGUN_DB_KEY` for every principal that
/// has no key of its own. Local, single-operator deployments; hosted uses [`KmsKeyProvider`].
#[derive(Default)]
pub struct EnvKeyProvider {
    default: Option<DbKey>,
    per_principal: HashMap<String, DbKey>,
}

impl EnvKeyProvider {
    pub const DEFAULT_VAR: &'static str = "SHOGUN_DB_KEY";
    pub const PREFIX: &'static str = "SHOGUN_DB_KEY_";

    /// Read the process environment once.
    pub fn from_env() -> Result<Self, TenancyError> {
        Self::from_vars(std::env::vars())
    }

    /// Build from `(name, value)` pairs, as [`Self::from_env`] does from the environment. A value
    /// that is not 64 hex characters is refused by name; an empty value is ignored.
    pub fn from_vars(vars: impl IntoIterator<Item = (String, String)>) -> Result<Self, TenancyError> {
        let mut provider = Self::default();
        for (name, value) in vars {
            if value.trim().is_empty() {
                continue;
            }
            if name == Self::DEFAULT_VAR {
                provider.default = Some(DbKey::from_hex(&value).ok_or(TenancyError::InvalidKey(name))?);
            } else if let Some(suffix) = name.strip_prefix(Self::PREFIX) {
                let key = DbKey::from_hex(&value).ok_or_else(|| TenancyError::InvalidKey(name.clone()))?;
                provider.per_principal.insert(suffix.to_string(), key);
            }
        }
        Ok(provider)
    }

    /// One key for every principal (tests, single-operator setups).
    pub fn with_default(key: DbKey) -> Self {
        Self { default: Some(key), per_principal: HashMap::new() }
    }

    /// Give `principal` its own key.
    #[must_use]
    pub fn with_principal_key(mut self, principal: &PrincipalId, key: DbKey) -> Self {
        self.per_principal.insert(principal.env_suffix(), key);
        self
    }
}

impl KeyProvider for EnvKeyProvider {
    fn key_for(&self, principal: &PrincipalId) -> Result<DbKey, TenancyError> {
        self.per_principal
            .get(&principal.env_suffix())
            .or(self.default.as_ref())
            .cloned()
            .ok_or(TenancyError::NoKey)
    }
}

/// Keys sealed in a KMS, one per principal (hosted). A stub until the hosted store lands: every
/// lookup answers [`TenancyError::KmsUnavailable`], so a misconfigured hosted sidecar fails
/// closed instead of falling back to an environment key.
pub struct KmsKeyProvider {
    key_ring: String,
}

impl KmsKeyProvider {
    pub fn new(key_ring: impl Into<String>) -> Self {
        Self { key_ring: key_ring.into() }
    }

    /// The key ring this provider would unseal from.
    pub fn key_ring(&self) -> &str {
        &self.key_ring
    }
}

impl KeyProvider for KmsKeyProvider {
    fn key_for(&self, _principal: &PrincipalId) -> Result<DbKey, TenancyError> {
        Err(TenancyError::KmsUnavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(byte: u8) -> String {
        format!("{byte:02x}").repeat(32)
    }

    #[test]
    fn per_principal_key_wins_over_the_default() {
        let provider = EnvKeyProvider::from_vars([
            ("SHOGUN_DB_KEY".to_string(), hex(0x11)),
            ("SHOGUN_DB_KEY_ACME_OPS".to_string(), hex(0x22)),
            ("SHOGUN_DB_KEY_EMPTY".to_string(), String::new()),
            ("UNRELATED".to_string(), "x".to_string()),
        ])
        .unwrap();
        let acme = PrincipalId::parse("acme-ops").unwrap();
        let bob = PrincipalId::parse("bob").unwrap();
        assert_eq!(provider.key_for(&acme).unwrap().to_hex(), hex(0x22));
        assert_eq!(provider.key_for(&bob).unwrap().to_hex(), hex(0x11));

        let only_one = EnvKeyProvider::from_vars([("SHOGUN_DB_KEY_ACME_OPS".to_string(), hex(0x22))]).unwrap();
        assert!(matches!(only_one.key_for(&bob), Err(TenancyError::NoKey)));
        assert!(only_one.key_for(&acme).is_ok());
    }

    #[test]
    fn a_malformed_key_is_refused_by_name_only() {
        let err = EnvKeyProvider::from_vars([("SHOGUN_DB_KEY_BOB".to_string(), "not-hex".to_string())])
            .err()
            .unwrap();
        let text = err.to_string();
        assert!(text.contains("SHOGUN_DB_KEY_BOB"), "{text}");
        assert!(!text.contains("not-hex"), "{text}");
    }

    #[test]
    fn kms_stub_fails_closed() {
        let kms = KmsKeyProvider::new("projects/x/keyRings/shogun");
        assert_eq!(kms.key_ring(), "projects/x/keyRings/shogun");
        assert!(matches!(
            kms.key_for(&PrincipalId::parse("alice").unwrap()),
            Err(TenancyError::KmsUnavailable)
        ));
    }
}
