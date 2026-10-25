//! [`RemoteTokenStore`]: the hosted gateway's token store, as an interface (E5-T5).
//!
//! The hosted deployment keeps each principal's credentials as rows in Postgres
//! (`apps/api/sql/connector-tokens.sql`), each row an envelope sealed by a KMS key:
//! the store never holds a value in clear and the database never sees one. This module fixes the
//! shape of that split so the gateway can only fill it in one way:
//!
//! - [`KmsEnvelope`] seals and opens a value, binding it to the principal and slot as associated
//!   data. The KMS client implements it (no network here: this crate has no HTTP client, and the
//!   gateway's KMS call goes through the traced egress in `shogun-core`).
//! - [`SealedRowBackend`] stores and fetches opaque [`SealedRow`]s. The Postgres client implements
//!   it; the type it is handed has no plaintext field to write.
//!
//! [`RemoteTokenStore`] composes the two into a [`TokenStore`]. [`super::KmsSealer`] (E26) is the
//! production [`KmsEnvelope`]: envelope encryption under the customer's KMS key through a
//! [`super::KeyWrapper`] the core implements for AWS KMS, GCP Cloud KMS and Vault Transit. The
//! Postgres [`SealedRowBackend`] lands with the gateway's store.

use super::{valid_principal, CredentialSlot, Secret, TokenStore, TokenStoreError};

/// One stored envelope: the ciphertext and the id of the KMS key that sealed it (for rotation).
/// Opaque bytes only — there is deliberately no field a plaintext could go in.
#[derive(Clone, PartialEq, Eq)]
pub struct SealedRow {
    pub key_id: String,
    pub ciphertext: Vec<u8>,
}

impl std::fmt::Debug for SealedRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SealedRow")
            .field("key_id", &self.key_id)
            .field("ciphertext_len", &self.ciphertext.len())
            .finish()
    }
}

/// The KMS envelope: seal a value for `(principal, account)`, and open it again. Implementations
/// must authenticate the binding (AAD), so a row moved to another principal or slot fails to open.
pub trait KmsEnvelope: Send + Sync {
    fn seal(&self, principal: &str, account: &str, value: &Secret) -> Result<SealedRow, TokenStoreError>;
    fn open(&self, principal: &str, account: &str, row: &SealedRow) -> Result<Secret, TokenStoreError>;
}

/// The row storage (hosted: Postgres). Sees principal, account and the sealed row, never a value.
pub trait SealedRowBackend: Send + Sync {
    fn fetch(&self, principal: &str, account: &str) -> Result<Option<SealedRow>, TokenStoreError>;
    fn upsert(&self, principal: &str, account: &str, row: SealedRow) -> Result<(), TokenStoreError>;
    fn remove(&self, principal: &str, account: &str) -> Result<(), TokenStoreError>;
}

/// A [`TokenStore`] over a [`SealedRowBackend`] and a [`KmsEnvelope`].
pub struct RemoteTokenStore<B, K> {
    rows: B,
    kms: K,
}

impl<B: SealedRowBackend, K: KmsEnvelope> RemoteTokenStore<B, K> {
    pub fn new(rows: B, kms: K) -> Self {
        Self { rows, kms }
    }
}

impl<B: SealedRowBackend, K: KmsEnvelope> TokenStore for RemoteTokenStore<B, K> {
    fn get(&self, principal: &str, slot: CredentialSlot) -> Result<Option<Secret>, TokenStoreError> {
        if !valid_principal(principal) {
            return Err(TokenStoreError::InvalidPrincipal);
        }
        let account = slot.account();
        match self.rows.fetch(principal, &account)? {
            Some(row) => self.kms.open(principal, &account, &row).map(Some),
            None => Ok(None),
        }
    }

    fn put(&self, principal: &str, slot: CredentialSlot, value: &Secret) -> Result<(), TokenStoreError> {
        if !valid_principal(principal) {
            return Err(TokenStoreError::InvalidPrincipal);
        }
        let account = slot.account();
        let row = self.kms.seal(principal, &account, value)?;
        self.rows.upsert(principal, &account, row)
    }

    fn delete(&self, principal: &str, slot: CredentialSlot) -> Result<(), TokenStoreError> {
        if !valid_principal(principal) {
            return Err(TokenStoreError::InvalidPrincipal);
        }
        self.rows.remove(principal, &slot.account())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;
    use crate::token_store::{SealKey, Secret};

    /// A stand-in KMS: the file store's AEAD under a fixed key, the binding as associated data.
    struct FakeKms;

    impl KmsEnvelope for FakeKms {
        fn seal(&self, principal: &str, account: &str, value: &Secret) -> Result<SealedRow, TokenStoreError> {
            let key = SealKey::derive(&[9; 32], "kms")?;
            let slot = slot_of(account);
            // Reuse the file format's sealing (nonce ‖ ciphertext, AAD-bound) as the envelope.
            let text = super::super::file::seal(&key, principal, slot, value)?;
            Ok(SealedRow { key_id: "fake-kms/1".into(), ciphertext: text.into_bytes() })
        }

        fn open(&self, principal: &str, account: &str, row: &SealedRow) -> Result<Secret, TokenStoreError> {
            let key = SealKey::derive(&[9; 32], "kms")?;
            let text = String::from_utf8(row.ciphertext.clone()).map_err(|_| TokenStoreError::Unavailable)?;
            super::super::file::open(&key, principal, slot_of(account), &text)
        }
    }

    fn slot_of(account: &str) -> CredentialSlot {
        if account == crate::token_store::BATCH_RELAY_ACCOUNT {
            CredentialSlot::BatchRelay
        } else {
            CredentialSlot::Connector(shogun_mcp::scope::Service::Gmail)
        }
    }

    #[derive(Default)]
    struct FakeRows(Mutex<HashMap<(String, String), SealedRow>>);

    impl SealedRowBackend for &FakeRows {
        fn fetch(&self, principal: &str, account: &str) -> Result<Option<SealedRow>, TokenStoreError> {
            Ok(self.0.lock().unwrap().get(&(principal.into(), account.into())).cloned())
        }
        fn upsert(&self, principal: &str, account: &str, row: SealedRow) -> Result<(), TokenStoreError> {
            self.0.lock().unwrap().insert((principal.into(), account.into()), row);
            Ok(())
        }
        fn remove(&self, principal: &str, account: &str) -> Result<(), TokenStoreError> {
            self.0.lock().unwrap().remove(&(principal.into(), account.into()));
            Ok(())
        }
    }

    #[test]
    fn the_backend_only_ever_holds_sealed_rows() {
        let rows = FakeRows::default();
        let store = RemoteTokenStore::new(&rows, FakeKms);
        let slot = CredentialSlot::BatchRelay;
        store.put("alice", slot, &Secret::new("v1.hosted-bearer")).unwrap();
        assert_eq!(store.get("alice", slot).unwrap(), Some(Secret::new("v1.hosted-bearer")));
        assert_eq!(store.get("bob", slot).unwrap(), None);

        let held = rows.0.lock().unwrap().values().cloned().collect::<Vec<_>>();
        assert_eq!(held.len(), 1);
        assert!(!String::from_utf8_lossy(&held[0].ciphertext).contains("hosted-bearer"));
        assert!(!format!("{:?}", held[0]).contains("hosted-bearer"));

        // A row moved to another principal does not open there.
        let row = held[0].clone();
        (&rows).upsert("bob", &slot.account(), row).unwrap();
        assert_eq!(store.get("bob", slot), Err(TokenStoreError::Unavailable));

        store.delete("alice", slot).unwrap();
        assert_eq!(store.get("alice", slot).unwrap(), None);
        assert_eq!(store.get("../x", slot), Err(TokenStoreError::InvalidPrincipal));
    }
}
