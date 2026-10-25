//! Where a principal's credentials live (plan E5-T5).
//!
//! One trait, [`TokenStore`], keyed by **principal × slot** and holding every value in
//! [`Secret`] — the connector OAuth grants (`<source>-tokenset`, the JSON [`crate::oauth::TokenSet`]
//! the refresh logic reads) and the Batch relay bearer a principal's hourly Dream Cycle presents.
//! Three implementations, one per deployment:
//!
//! | Deployment | Store | Sealing |
//! |---|---|---|
//! | ShogunAI Mac app (feature `desktop`) | `keychain::KeychainTokenStore` | the macOS Keychain |
//! | Local Linux server (`shogun-api --multi`) | [`FileSealedTokenStore`] | ChaCha20-Poly1305 under a key derived from the principal's store key, files 0600 |
//! | Hosted gateway | [`RemoteTokenStore`] | KMS envelope over Postgres rows (interface here; the backing lands with the gateway) |
//!
//! Connector sync never sees which one it has: [`PrincipalGrants`] adapts any store to the
//! per-service [`crate::token::GrantStore`] the [`crate::token::TokenManager`] /
//! [`crate::token::ManagedTokenProvider`] refresh path uses.
//!
//! Nothing here logs or formats a value: errors are content-free codes, `Secret` redacts under
//! `Debug` / `Display`, and the raw value is read (`Secret::expose`) only in the files that seal it or
//! parse it (`grants.rs`, `file.rs`; `scripts/check-secret-exposure.py`).

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use shogun_mcp::scope::Service;

pub use shogun_redact::Secret;

mod file;
mod grants;
mod remote;

pub use file::{FileSealedTokenStore, SealKey, SealKeySource};
pub use grants::PrincipalGrants;
pub use remote::{KmsEnvelope, RemoteTokenStore, SealedRow, SealedRowBackend};

/// The principal a single-user deployment (the Mac app, a one-person local server) stores under.
pub const LOCAL_PRINCIPAL: &str = "local";

/// The slot account of the Batch relay bearer (the licence / trial token a principal's hourly
/// Dream Cycle presents to the relay). Listed in `keychain_accounts::USER_SECRET_ACCOUNTS`.
pub const BATCH_RELAY_ACCOUNT: &str = "batch-relay-bearer";

/// What a stored credential is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CredentialSlot {
    /// A connector's OAuth grant: the serialized [`crate::oauth::TokenSet`].
    Connector(Service),
    /// The Batch relay bearer (licence or trial token) for the principal's Dream Cycle.
    BatchRelay,
}

impl CredentialSlot {
    /// The stable account name of the slot: `<source>-tokenset` for a connector (the Keychain
    /// account name the Mac app has always used), [`BATCH_RELAY_ACCOUNT`] for the relay.
    pub fn account(self) -> String {
        match self {
            Self::Connector(service) => format!("{}-tokenset", service.source_str()),
            Self::BatchRelay => BATCH_RELAY_ACCOUNT.to_string(),
        }
    }
}

/// Why a store call failed. Fixed codes only: never a value, a path or a principal's content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenStoreError {
    /// The store is locked (a locked Keychain, a KMS key that is not reachable yet). Transient.
    Locked,
    /// The value exists but cannot be read (corrupt, tampered, sealed under another key).
    Unavailable,
    /// The principal id is not one this store serves.
    InvalidPrincipal,
    /// The write or delete did not persist.
    Write,
}

impl TokenStoreError {
    pub fn code(self) -> &'static str {
        match self {
            Self::Locked => "token_store_locked",
            Self::Unavailable => "token_store_unavailable",
            Self::InvalidPrincipal => "token_store_invalid_principal",
            Self::Write => "token_store_write_failed",
        }
    }
}

impl fmt::Display for TokenStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for TokenStoreError {}

/// A principal's credentials, per slot. `get` answers `Ok(None)` only when nothing is stored; any
/// other failure is an `Err`, so a locked or tampered store never reads as "not connected".
pub trait TokenStore: Send + Sync {
    fn get(&self, principal: &str, slot: CredentialSlot) -> Result<Option<Secret>, TokenStoreError>;
    fn put(&self, principal: &str, slot: CredentialSlot, value: &Secret) -> Result<(), TokenStoreError>;
    fn delete(&self, principal: &str, slot: CredentialSlot) -> Result<(), TokenStoreError>;
}

impl<T: TokenStore + ?Sized> TokenStore for &T {
    fn get(&self, principal: &str, slot: CredentialSlot) -> Result<Option<Secret>, TokenStoreError> {
        (**self).get(principal, slot)
    }
    fn put(&self, principal: &str, slot: CredentialSlot, value: &Secret) -> Result<(), TokenStoreError> {
        (**self).put(principal, slot, value)
    }
    fn delete(&self, principal: &str, slot: CredentialSlot) -> Result<(), TokenStoreError> {
        (**self).delete(principal, slot)
    }
}

impl<T: TokenStore + ?Sized> TokenStore for std::sync::Arc<T> {
    fn get(&self, principal: &str, slot: CredentialSlot) -> Result<Option<Secret>, TokenStoreError> {
        (**self).get(principal, slot)
    }
    fn put(&self, principal: &str, slot: CredentialSlot, value: &Secret) -> Result<(), TokenStoreError> {
        (**self).put(principal, slot, value)
    }
    fn delete(&self, principal: &str, slot: CredentialSlot) -> Result<(), TokenStoreError> {
        (**self).delete(principal, slot)
    }
}

/// Whether `principal` is a valid principal slug: `[a-z0-9][a-z0-9_-]{0,63}`, the same rule as
/// `shogun_core::tenancy::PrincipalId` (this crate sits below the core). Safe as a directory
/// name and as part of an AEAD binding with no escaping.
pub fn valid_principal(principal: &str) -> bool {
    let bytes = principal.as_bytes();
    let Some(first) = bytes.first() else {
        return false;
    };
    bytes.len() <= 64
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && bytes.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
}

/// An in-memory store for tests and ephemeral development. Never persists anything.
#[derive(Default)]
pub struct MemorySecretStore {
    values: Mutex<HashMap<(String, CredentialSlot), Secret>>,
}

impl MemorySecretStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl TokenStore for MemorySecretStore {
    fn get(&self, principal: &str, slot: CredentialSlot) -> Result<Option<Secret>, TokenStoreError> {
        if !valid_principal(principal) {
            return Err(TokenStoreError::InvalidPrincipal);
        }
        let values = self.values.lock().map_err(|_| TokenStoreError::Unavailable)?;
        Ok(values.get(&(principal.to_string(), slot)).cloned())
    }

    fn put(&self, principal: &str, slot: CredentialSlot, value: &Secret) -> Result<(), TokenStoreError> {
        if !valid_principal(principal) {
            return Err(TokenStoreError::InvalidPrincipal);
        }
        let mut values = self.values.lock().map_err(|_| TokenStoreError::Write)?;
        values.insert((principal.to_string(), slot), value.clone());
        Ok(())
    }

    fn delete(&self, principal: &str, slot: CredentialSlot) -> Result<(), TokenStoreError> {
        if !valid_principal(principal) {
            return Err(TokenStoreError::InvalidPrincipal);
        }
        let mut values = self.values.lock().map_err(|_| TokenStoreError::Write)?;
        values.remove(&(principal.to_string(), slot));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_accounts_match_the_keychain_names() {
        assert_eq!(CredentialSlot::Connector(Service::Gmail).account(), "gmail-tokenset");
        assert_eq!(CredentialSlot::BatchRelay.account(), BATCH_RELAY_ACCOUNT);
        assert!(crate::keychain_accounts::USER_SECRET_ACCOUNTS.contains(&BATCH_RELAY_ACCOUNT));
    }

    #[test]
    fn principal_slugs_follow_the_tenancy_rule() {
        for ok in ["local", "acme-ops", "a", "0_x"] {
            assert!(valid_principal(ok), "{ok}");
        }
        for bad in ["", "Alice", "../x", ".tokens", "-a", "a/b", &"x".repeat(65)] {
            assert!(!valid_principal(bad), "{bad}");
        }
    }

    #[test]
    fn memory_store_keeps_principals_apart() {
        let store = MemorySecretStore::new();
        let slot = CredentialSlot::BatchRelay;
        store.put("alice", slot, &Secret::new("v1.alice")).unwrap();
        assert_eq!(store.get("alice", slot).unwrap(), Some(Secret::new("v1.alice")));
        assert_eq!(store.get("bob", slot).unwrap(), None);
        store.delete("alice", slot).unwrap();
        assert_eq!(store.get("alice", slot).unwrap(), None);
        assert_eq!(store.get("Bad", slot), Err(TokenStoreError::InvalidPrincipal));
    }

    #[test]
    fn errors_render_as_codes() {
        assert_eq!(TokenStoreError::Locked.to_string(), "token_store_locked");
    }
}
