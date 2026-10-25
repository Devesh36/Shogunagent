//! Envelope encryption under a customer-managed KMS key (E26): the [`KeyWrapper`] seam and the
//! [`KmsSealer`] that implements [`KmsEnvelope`] for the hosted [`super::RemoteTokenStore`].
//!
//! Envelope encryption, the same shape for a principal's SQLCipher key (the core's
//! `KmsKeyProvider`) and for a stored credential (here):
//!
//! - a fresh 32-byte **data key** ([`DataKey`]) encrypts the payload locally;
//! - the customer's KMS key **wraps** the data key ([`KeyWrapper::wrap`]); only the wrapped form is
//!   ever written anywhere;
//! - opening asks the KMS to **unwrap** it ([`KeyWrapper::unwrap`]). The plaintext data key lives
//!   only in memory and is zeroized when the [`DataKey`] drops.
//!
//! Every wrap and unwrap carries a **context** string the KMS authenticates (AWS encryption
//! context, GCP additional authenticated data, Vault associated data), so a wrapped key moved to
//! another principal or slot does not unwrap there.
//!
//! This crate has no HTTP client (invariant 9): the AWS KMS, GCP Cloud KMS and Vault Transit
//! wrappers live in `shogun-core` (`tenancy::key_provider`), behind the single traced egress, and
//! implement [`KeyWrapper`] from there.
//!
//! Reads the raw credential (`.expose(`) once, to seal it — allowlisted in
//! `scripts/check-secret-exposure.py`.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use zeroize::Zeroize;

use super::{KmsEnvelope, SealedRow, Secret, TokenStoreError};

/// A 32-byte data key in clear. No `Debug` that shows it, no `Clone`, zeroized on drop.
pub struct DataKey([u8; 32]);

impl DataKey {
    /// Take ownership of key bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Copy a decoded KMS answer, refusing anything that is not exactly 32 bytes. Zeroizes
    /// `bytes` either way.
    pub fn from_slice(bytes: &mut [u8]) -> Result<Self, KmsError> {
        if bytes.len() != 32 {
            bytes.zeroize();
            return Err(KmsError::Malformed);
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(bytes);
        bytes.zeroize();
        Ok(Self(out))
    }

    /// 32 bytes from the operating system's CSPRNG.
    pub fn random() -> Result<Self, KmsError> {
        let mut out = [0u8; 32];
        getrandom::fill(&mut out).map_err(|_| KmsError::Config("the system random source failed"))?;
        Ok(Self(out))
    }

    /// Run `f` over the key bytes without copying them out.
    pub fn with_bytes<R>(&self, f: impl FnOnce(&[u8; 32]) -> R) -> R {
        f(&self.0)
    }

    /// A second in-memory copy (for a cache). Deliberately not `Clone`, so each copy is explicit.
    pub fn duplicate(&self) -> Self {
        Self(self.0)
    }
}

impl Drop for DataKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for DataKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DataKey(***redacted***)")
    }
}

/// A data key wrapped by the KMS: the opaque ciphertext and the key version that wrapped it. Safe
/// to store (it is ciphertext); `Debug` shows only its length.
#[derive(Clone, PartialEq, Eq)]
pub struct WrappedDataKey {
    /// The provider's name for the KMS key version that wrapped this blob (an AWS key ARN, a GCP
    /// `cryptoKeyVersions/<n>` name, a Vault `v<n>`). Recorded for rotation and audits.
    pub key_version: String,
    pub blob: Vec<u8>,
}

impl std::fmt::Debug for WrappedDataKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WrappedDataKey")
            .field("key_version", &self.key_version)
            .field("blob_len", &self.blob.len())
            .finish()
    }
}

/// What an unwrap answers: the key, and whether the KMS key has moved on since it was wrapped
/// (a newer primary version, or a different configured key), so the caller re-wraps it.
#[derive(Debug)]
pub struct Unwrapped {
    pub key: DataKey,
    pub stale: bool,
}

/// Why a KMS call failed. Content-free: never a key, a token, a body or a principal id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KmsError {
    /// The KMS could not be reached (connection refused, DNS, timeout). Nothing was unwrapped.
    Unreachable,
    /// The KMS answered 401/403: the workload's identity may not use this key.
    Denied(u16),
    /// The KMS answered another non-2xx status (a wrong context, a disabled key, throttling).
    Rejected(u16),
    /// The KMS answered 2xx with a body this client cannot read.
    Malformed,
    /// The traceability record for the call could not be written, so the call was not made.
    Untraced,
    /// The provider is not configured correctly; names the setting, never its value.
    Config(&'static str),
}

impl std::fmt::Display for KmsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable => f.write_str("the KMS is unreachable; the store stays closed until it answers"),
            Self::Denied(status) => write!(
                f,
                "the KMS refused this workload (HTTP {status}); check the key policy and the workload identity binding"
            ),
            Self::Rejected(status) => write!(f, "the KMS rejected the request (HTTP {status})"),
            Self::Malformed => f.write_str("the KMS answer could not be read"),
            Self::Untraced => f.write_str("the KMS call could not be recorded, so it was not made"),
            Self::Config(what) => write!(f, "KMS configuration: {what}"),
        }
    }
}

impl std::error::Error for KmsError {}

impl From<KmsError> for TokenStoreError {
    fn from(error: KmsError) -> Self {
        match error {
            KmsError::Unreachable | KmsError::Untraced | KmsError::Rejected(_) => Self::Locked,
            KmsError::Denied(_) | KmsError::Malformed | KmsError::Config(_) => Self::Unavailable,
        }
    }
}

/// The customer's KMS key, as a wrapper of data keys. Implemented in `shogun-core` for AWS KMS,
/// GCP Cloud KMS and Vault Transit; tests use [`fake::FakeWrapper`].
pub trait KeyWrapper: Send + Sync {
    /// `aws-kms`, `gcp-kms`, `vault`: names the provider in errors and in the wrapped-key file.
    fn provider(&self) -> &'static str;

    /// Wrap `key` under the current KMS key version, bound to `context`.
    fn wrap(&self, key: &DataKey, context: &str) -> Result<WrappedDataKey, KmsError>;

    /// Unwrap `wrapped`, which must have been wrapped with the same `context`.
    fn unwrap(&self, wrapped: &WrappedDataKey, context: &str) -> Result<Unwrapped, KmsError>;

    /// A new data key and its wrapped form. The default draws the key locally and wraps it; a KMS
    /// with a data-key API (AWS `GenerateDataKey`) overrides it with one call.
    fn generate(&self, context: &str) -> Result<(DataKey, WrappedDataKey), KmsError> {
        let key = DataKey::random()?;
        let wrapped = self.wrap(&key, context)?;
        Ok((key, wrapped))
    }
}

impl<T: KeyWrapper + ?Sized> KeyWrapper for std::sync::Arc<T> {
    fn provider(&self) -> &'static str {
        (**self).provider()
    }
    fn wrap(&self, key: &DataKey, context: &str) -> Result<WrappedDataKey, KmsError> {
        (**self).wrap(key, context)
    }
    fn unwrap(&self, wrapped: &WrappedDataKey, context: &str) -> Result<Unwrapped, KmsError> {
        (**self).unwrap(wrapped, context)
    }
    fn generate(&self, context: &str) -> Result<(DataKey, WrappedDataKey), KmsError> {
        (**self).generate(context)
    }
}

/// The KMS context a credential's data key is bound to.
pub fn token_context(principal: &str, account: &str) -> String {
    format!("shogun/token/v1/{principal}/{account}")
}

const ROW_VERSION: u8 = 1;
const NONCE_LEN: usize = 12;

fn row_aad(principal: &str, account: &str) -> Vec<u8> {
    format!("shogun-token-kms/v1|{principal}|{account}").into_bytes()
}

fn cipher(key: &DataKey) -> ChaCha20Poly1305 {
    key.with_bytes(|bytes| ChaCha20Poly1305::new(chacha20poly1305::Key::from_slice(bytes)))
}

/// [`KmsEnvelope`] over a [`KeyWrapper`]: one data key per sealed row (one KMS call to seal, one
/// to open), the row `[1][u16 wrapped len][wrapped][nonce][ChaCha20-Poly1305 ciphertext]` with the
/// principal and account bound both at the KMS (context) and in the AEAD (associated data).
/// `SealedRow::key_id` is the KMS key version that wrapped the row's data key.
pub struct KmsSealer<W> {
    wrapper: W,
}

impl<W: KeyWrapper> KmsSealer<W> {
    pub fn new(wrapper: W) -> Self {
        Self { wrapper }
    }

    pub fn wrapper(&self) -> &W {
        &self.wrapper
    }

    /// Re-wrap a row's data key under the current KMS key version without touching the sealed
    /// value. `None` when the row is already current.
    pub fn rewrap(&self, principal: &str, account: &str, row: &SealedRow) -> Result<Option<SealedRow>, TokenStoreError> {
        let (wrapped, body) = split_row(row)?;
        let context = token_context(principal, account);
        let unwrapped = self.wrapper.unwrap(&wrapped, &context)?;
        if !unwrapped.stale {
            return Ok(None);
        }
        let fresh = self.wrapper.wrap(&unwrapped.key, &context)?;
        Ok(Some(join_row(&fresh, body)?))
    }
}

fn join_row(wrapped: &WrappedDataKey, body: &[u8]) -> Result<SealedRow, TokenStoreError> {
    let len = u16::try_from(wrapped.blob.len()).map_err(|_| TokenStoreError::Write)?;
    let mut out = Vec::with_capacity(3 + wrapped.blob.len() + body.len());
    out.push(ROW_VERSION);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&wrapped.blob);
    out.extend_from_slice(body);
    Ok(SealedRow { key_id: wrapped.key_version.clone(), ciphertext: out })
}

fn split_row(row: &SealedRow) -> Result<(WrappedDataKey, &[u8]), TokenStoreError> {
    let bytes = &row.ciphertext;
    if bytes.len() < 3 || bytes[0] != ROW_VERSION {
        return Err(TokenStoreError::Unavailable);
    }
    let len = usize::from(u16::from_be_bytes([bytes[1], bytes[2]]));
    let rest = &bytes[3..];
    if rest.len() <= len + NONCE_LEN {
        return Err(TokenStoreError::Unavailable);
    }
    let (blob, body) = rest.split_at(len);
    Ok((WrappedDataKey { key_version: row.key_id.clone(), blob: blob.to_vec() }, body))
}

impl<W: KeyWrapper> KmsEnvelope for KmsSealer<W> {
    fn seal(&self, principal: &str, account: &str, value: &Secret) -> Result<SealedRow, TokenStoreError> {
        let (key, wrapped) = self.wrapper.generate(&token_context(principal, account))?;
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::fill(&mut nonce).map_err(|_| TokenStoreError::Write)?;
        let aad = row_aad(principal, account);
        let sealed = cipher(&key)
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: value.expose().as_bytes(), aad: &aad })
            .map_err(|_| TokenStoreError::Write)?;
        let mut body = Vec::with_capacity(NONCE_LEN + sealed.len());
        body.extend_from_slice(&nonce);
        body.extend_from_slice(&sealed);
        join_row(&wrapped, &body)
    }

    fn open(&self, principal: &str, account: &str, row: &SealedRow) -> Result<Secret, TokenStoreError> {
        let (wrapped, body) = split_row(row)?;
        let unwrapped = self.wrapper.unwrap(&wrapped, &token_context(principal, account))?;
        let (nonce, sealed) = body.split_at(NONCE_LEN);
        let aad = row_aad(principal, account);
        let plain = cipher(&unwrapped.key)
            .decrypt(Nonce::from_slice(nonce), Payload { msg: sealed, aad: &aad })
            .map_err(|_| TokenStoreError::Unavailable)?;
        match String::from_utf8(plain) {
            Ok(text) => Ok(Secret::new(text)),
            Err(error) => {
                let mut bytes = error.into_bytes();
                bytes.zeroize();
                Err(TokenStoreError::Unavailable)
            }
        }
    }
}

/// An in-process stand-in KMS for tests in this crate and downstream: XORs the data key with a
/// pad derived from the key version and the context. **Not encryption**; never use it outside
/// tests. A wrong context yields a wrong key, which the caller's AEAD (or SQLCipher) refuses.
#[doc(hidden)]
pub mod fake {
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

    use sha2::{Digest, Sha256};

    use super::{DataKey, KeyWrapper, KmsError, Unwrapped, WrappedDataKey};

    /// A fake KMS key with numbered versions; `rotate` makes a new primary version, `set_down`
    /// makes every call fail as unreachable.
    pub struct FakeWrapper {
        primary: AtomicU32,
        down: AtomicBool,
        pub calls: AtomicUsize,
    }

    impl Default for FakeWrapper {
        fn default() -> Self {
            Self::new()
        }
    }

    impl FakeWrapper {
        pub fn new() -> Self {
            Self { primary: AtomicU32::new(1), down: AtomicBool::new(false), calls: AtomicUsize::new(0) }
        }
        pub fn rotate(&self) {
            self.primary.fetch_add(1, Ordering::SeqCst);
        }
        pub fn set_down(&self, down: bool) {
            self.down.store(down, Ordering::SeqCst);
        }
        fn pad(version: u32, context: &str) -> [u8; 32] {
            let mut h = Sha256::new();
            h.update(b"fake-kms-pad");
            h.update(version.to_be_bytes());
            h.update(context.as_bytes());
            h.finalize().into()
        }
        fn enter(&self) -> Result<(), KmsError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.down.load(Ordering::SeqCst) {
                return Err(KmsError::Unreachable);
            }
            Ok(())
        }
    }

    impl KeyWrapper for FakeWrapper {
        fn provider(&self) -> &'static str {
            "fake"
        }

        fn wrap(&self, key: &DataKey, context: &str) -> Result<WrappedDataKey, KmsError> {
            self.enter()?;
            let version = self.primary.load(Ordering::SeqCst);
            let pad = Self::pad(version, context);
            let mut blob = version.to_be_bytes().to_vec();
            key.with_bytes(|bytes| blob.extend(bytes.iter().zip(pad.iter()).map(|(a, b)| a ^ b)));
            Ok(WrappedDataKey { key_version: format!("fake/v{version}"), blob })
        }

        fn unwrap(&self, wrapped: &WrappedDataKey, context: &str) -> Result<Unwrapped, KmsError> {
            self.enter()?;
            if wrapped.blob.len() != 36 {
                return Err(KmsError::Rejected(400));
            }
            let version = u32::from_be_bytes([wrapped.blob[0], wrapped.blob[1], wrapped.blob[2], wrapped.blob[3]]);
            let pad = Self::pad(version, context);
            let mut out: Vec<u8> = wrapped.blob[4..].iter().zip(pad.iter()).map(|(a, b)| a ^ b).collect();
            let key = DataKey::from_slice(&mut out)?;
            Ok(Unwrapped { key, stale: version < self.primary.load(Ordering::SeqCst) })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    use super::fake::FakeWrapper;
    use super::*;
    use crate::token_store::{CredentialSlot, RemoteTokenStore, SealedRowBackend, TokenStore};

    #[derive(Default)]
    struct Rows(std::sync::Mutex<std::collections::HashMap<(String, String), SealedRow>>);

    impl SealedRowBackend for &Rows {
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
    fn a_remote_store_over_the_kms_sealer_round_trips_and_holds_only_ciphertext() {
        let rows = Rows::default();
        let kms = Arc::new(FakeWrapper::new());
        let store = RemoteTokenStore::new(&rows, KmsSealer::new(kms.clone()));
        let slot = CredentialSlot::BatchRelay;
        store.put("alice", slot, &Secret::new("v1.kms-bearer")).unwrap();
        assert_eq!(store.get("alice", slot).unwrap(), Some(Secret::new("v1.kms-bearer")));

        let row = rows.0.lock().unwrap().values().next().cloned().unwrap();
        assert_eq!(row.key_id, "fake/v1");
        assert!(!String::from_utf8_lossy(&row.ciphertext).contains("kms-bearer"));
        assert!(!format!("{row:?}").contains("kms-bearer"));

        // Moved to another principal: the KMS context and the AEAD both refuse it.
        (&rows).upsert("bob", &slot.account(), row).unwrap();
        assert!(store.get("bob", slot).is_err());
    }

    #[test]
    fn rotation_rewraps_the_data_key_and_leaves_the_value_sealed_as_it_was() {
        let kms = Arc::new(FakeWrapper::new());
        let sealer = KmsSealer::new(kms.clone());
        let row = sealer.seal("alice", "gmail-tokenset", &Secret::new("{\"a\":1}")).unwrap();
        assert!(sealer.rewrap("alice", "gmail-tokenset", &row).unwrap().is_none(), "current row");
        kms.rotate();
        let fresh = sealer.rewrap("alice", "gmail-tokenset", &row).unwrap().expect("stale row is rewrapped");
        assert_eq!(fresh.key_id, "fake/v2");
        // The AEAD body (nonce ‖ ciphertext) is byte-for-byte the same: only the wrapping moved.
        let (_, old_body) = split_row(&row).unwrap();
        let (_, new_body) = split_row(&fresh).unwrap();
        assert_eq!(old_body, new_body);
        assert_eq!(sealer.open("alice", "gmail-tokenset", &fresh).unwrap(), Secret::new("{\"a\":1}"));
        assert_eq!(sealer.open("alice", "gmail-tokenset", &row).unwrap(), Secret::new("{\"a\":1}"));
    }

    #[test]
    fn a_kms_outage_is_an_error_never_a_missing_or_clear_value() {
        let rows = Rows::default();
        let kms = Arc::new(FakeWrapper::new());
        let store = RemoteTokenStore::new(&rows, KmsSealer::new(kms.clone()));
        let slot = CredentialSlot::BatchRelay;
        store.put("alice", slot, &Secret::new("v1.x")).unwrap();
        kms.set_down(true);
        assert_eq!(store.get("alice", slot), Err(TokenStoreError::Locked));
        assert_eq!(store.put("alice", slot, &Secret::new("v1.y")), Err(TokenStoreError::Locked));
        kms.set_down(false);
        assert_eq!(store.get("alice", slot).unwrap(), Some(Secret::new("v1.x")));
        assert!(kms.calls.load(Ordering::SeqCst) >= 4);
    }

    #[test]
    fn data_keys_never_render_and_wrapped_keys_show_only_their_length() {
        let key = DataKey::from_bytes([7; 32]);
        assert_eq!(format!("{key:?}"), "DataKey(***redacted***)");
        let wrapped = WrappedDataKey { key_version: "v1".into(), blob: vec![1, 2, 3] };
        assert!(format!("{wrapped:?}").contains("blob_len: 3"));
        let mut short = vec![1u8; 31];
        assert_eq!(DataKey::from_slice(&mut short).err(), Some(KmsError::Malformed));
        assert!(short.iter().all(|b| *b == 0), "a refused answer is zeroized too");
        assert_eq!(TokenStoreError::from(KmsError::Unreachable), TokenStoreError::Locked);
        assert_eq!(TokenStoreError::from(KmsError::Denied(403)), TokenStoreError::Unavailable);
    }

    #[test]
    fn a_truncated_row_is_unavailable() {
        let sealer = KmsSealer::new(FakeWrapper::new());
        let bad = SealedRow { key_id: "fake/v1".into(), ciphertext: vec![1, 0, 40, 1, 2] };
        assert_eq!(sealer.open("alice", "x", &bad), Err(TokenStoreError::Unavailable));
        let wrong_version = SealedRow { key_id: "fake/v1".into(), ciphertext: vec![9; 80] };
        assert_eq!(sealer.open("alice", "x", &wrong_version), Err(TokenStoreError::Unavailable));
    }
}
