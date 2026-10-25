//! [`FileSealedTokenStore`]: the local Linux server's token store (E5-T5).
//!
//! One file per principal × slot at `<root>/<principal>/<account>.sealed`, the directory mode 0700
//! and the file 0600 (written to a 0600 temporary and renamed, so a crash leaves the old value or
//! the new one, never half of either). The file holds
//! `v1.` + base64url(nonce ‖ ChaCha20-Poly1305 ciphertext) — never the value in clear (invariant
//! 7). The key is HKDF-SHA256 over the principal's own store key ([`SealKey::derive`]; the core
//! hands it the SQLCipher key its `KeyProvider` resolves), so one principal's key cannot open
//! another's tokens, and the associated data binds each file to its principal and slot: a file
//! copied to another principal's directory or renamed to another slot fails to open instead of
//! being read as that credential.
//!
//! Reads the raw value (`.expose(`) to seal it — allowlisted in `scripts/check-secret-exposure.py`.

use std::path::{Path, PathBuf};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};

use super::{valid_principal, CredentialSlot, Secret, TokenStore, TokenStoreError};

const FORMAT_PREFIX: &str = "v1.";
const NONCE_LEN: usize = 12;
const HKDF_SALT: &[u8] = b"shogun-token-store";

/// A principal's 32-byte sealing key. No `Debug`, no accessor that returns the bytes.
pub struct SealKey([u8; 32]);

impl SealKey {
    /// Derive the token-store key of `principal` from its store key (`ikm`, the SQLCipher key).
    /// A distinct HKDF `info` per principal and purpose, so the same `ikm` used as a database key
    /// never doubles as this AEAD key.
    pub fn derive(ikm: &[u8; 32], principal: &str) -> Result<Self, TokenStoreError> {
        let mut key = [0u8; 32];
        let info = format!("shogun/token-store/v1/{principal}");
        hkdf::Hkdf::<sha2::Sha256>::new(Some(HKDF_SALT), ikm)
            .expand(info.as_bytes(), &mut key)
            .map_err(|_| TokenStoreError::Unavailable)?;
        Ok(Self(key))
    }

    fn cipher(&self) -> ChaCha20Poly1305 {
        ChaCha20Poly1305::new(chacha20poly1305::Key::from_slice(&self.0))
    }
}

impl Drop for SealKey {
    fn drop(&mut self) {
        self.0 = [0u8; 32];
    }
}

/// Where the sealing key of a principal comes from (the core implements it over its
/// `KeyProvider`; tests use a fixed key).
pub trait SealKeySource: Send + Sync {
    fn seal_key(&self, principal: &str) -> Result<SealKey, TokenStoreError>;
}

impl<F> SealKeySource for F
where
    F: Fn(&str) -> Result<SealKey, TokenStoreError> + Send + Sync,
{
    fn seal_key(&self, principal: &str) -> Result<SealKey, TokenStoreError> {
        self(principal)
    }
}

/// Sealed files under `root`, keys from `keys`.
pub struct FileSealedTokenStore<K> {
    root: PathBuf,
    keys: K,
}

impl<K: SealKeySource> FileSealedTokenStore<K> {
    pub fn new(root: impl Into<PathBuf>, keys: K) -> Self {
        Self { root: root.into(), keys }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, principal: &str, slot: CredentialSlot) -> Result<PathBuf, TokenStoreError> {
        if !valid_principal(principal) {
            return Err(TokenStoreError::InvalidPrincipal);
        }
        Ok(self.root.join(principal).join(format!("{}.sealed", slot.account())))
    }
}

fn aad(principal: &str, slot: CredentialSlot) -> Vec<u8> {
    format!("shogun-token-store/v1|{principal}|{}", slot.account()).into_bytes()
}

pub(super) fn seal(key: &SealKey, principal: &str, slot: CredentialSlot, value: &Secret) -> Result<String, TokenStoreError> {
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce).map_err(|_| TokenStoreError::Write)?;
    let aad = aad(principal, slot);
    let sealed = key
        .cipher()
        .encrypt(Nonce::from_slice(&nonce), Payload { msg: value.expose().as_bytes(), aad: &aad })
        .map_err(|_| TokenStoreError::Write)?;
    let mut blob = Vec::with_capacity(NONCE_LEN + sealed.len());
    blob.extend_from_slice(&nonce);
    blob.extend_from_slice(&sealed);
    Ok(format!("{FORMAT_PREFIX}{}", URL_SAFE_NO_PAD.encode(blob)))
}

pub(super) fn open(key: &SealKey, principal: &str, slot: CredentialSlot, text: &str) -> Result<Secret, TokenStoreError> {
    let encoded = text.trim().strip_prefix(FORMAT_PREFIX).ok_or(TokenStoreError::Unavailable)?;
    let blob = URL_SAFE_NO_PAD.decode(encoded).map_err(|_| TokenStoreError::Unavailable)?;
    if blob.len() <= NONCE_LEN {
        return Err(TokenStoreError::Unavailable);
    }
    let (nonce, sealed) = blob.split_at(NONCE_LEN);
    let aad = aad(principal, slot);
    let plain = key
        .cipher()
        .decrypt(Nonce::from_slice(nonce), Payload { msg: sealed, aad: &aad })
        .map_err(|_| TokenStoreError::Unavailable)?;
    String::from_utf8(plain).map(Secret::new).map_err(|_| TokenStoreError::Unavailable)
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().write(true).create_new(true).open(path)
}

/// Write `text` to `path` through a 0600 temporary in the same directory, then rename over it.
fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().ok_or_else(|| std::io::Error::other("no parent"))?;
    create_private_dir(dir)?;
    let mut suffix = [0u8; 8];
    getrandom::fill(&mut suffix).map_err(|_| std::io::Error::other("csprng"))?;
    let tmp = dir.join(format!(".tmp-{}", URL_SAFE_NO_PAD.encode(suffix)));
    let result = (|| {
        let mut file = create_private_file(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

impl<K: SealKeySource> TokenStore for FileSealedTokenStore<K> {
    fn get(&self, principal: &str, slot: CredentialSlot) -> Result<Option<Secret>, TokenStoreError> {
        let path = self.path(principal, slot)?;
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(TokenStoreError::Unavailable),
        };
        let key = self.keys.seal_key(principal)?;
        open(&key, principal, slot, &text).map(Some)
    }

    fn put(&self, principal: &str, slot: CredentialSlot, value: &Secret) -> Result<(), TokenStoreError> {
        let path = self.path(principal, slot)?;
        let key = self.keys.seal_key(principal)?;
        let sealed = seal(&key, principal, slot, value)?;
        write_atomic(&path, &sealed).map_err(|_| TokenStoreError::Write)
    }

    fn delete(&self, principal: &str, slot: CredentialSlot) -> Result<(), TokenStoreError> {
        let path = self.path(principal, slot)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(TokenStoreError::Write),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shogun_mcp::scope::Service;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let mut suffix = [0u8; 6];
            getrandom::fill(&mut suffix).unwrap();
            let dir = std::env::temp_dir().join(format!(
                "shogun_token_store_{label}_{}_{}",
                std::process::id(),
                URL_SAFE_NO_PAD.encode(suffix)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn keys(ikm: u8) -> impl SealKeySource {
        move |principal: &str| SealKey::derive(&[ikm; 32], principal)
    }

    const RELAY: CredentialSlot = CredentialSlot::BatchRelay;

    #[test]
    fn round_trips_and_never_writes_the_value_in_clear() {
        let dir = TempDir::new("roundtrip");
        let store = FileSealedTokenStore::new(&dir.0, keys(7));
        assert_eq!(store.get("alice", RELAY).unwrap(), None);
        store.put("alice", RELAY, &Secret::new("v1.alice-relay-bearer")).unwrap();
        assert_eq!(store.get("alice", RELAY).unwrap(), Some(Secret::new("v1.alice-relay-bearer")));

        let path = dir.0.join("alice").join("batch-relay-bearer.sealed");
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.starts_with(FORMAT_PREFIX));
        assert!(!on_disk.contains("alice-relay-bearer"), "the value is sealed, not stored");

        // Overwrite, then delete (twice: deleting nothing is fine).
        store.put("alice", RELAY, &Secret::new("v1.rotated")).unwrap();
        assert_eq!(store.get("alice", RELAY).unwrap(), Some(Secret::new("v1.rotated")));
        store.delete("alice", RELAY).unwrap();
        store.delete("alice", RELAY).unwrap();
        assert_eq!(store.get("alice", RELAY).unwrap(), None);
        // No temporary is left behind.
        let leftovers: Vec<_> = std::fs::read_dir(dir.0.join("alice")).unwrap().flatten().collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[cfg(unix)]
    #[test]
    fn files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("mode");
        let store = FileSealedTokenStore::new(&dir.0, keys(7));
        store.put("alice", CredentialSlot::Connector(Service::Slack), &Secret::new("x")).unwrap();
        let file = std::fs::metadata(dir.0.join("alice").join("slack-tokenset.sealed")).unwrap();
        assert_eq!(file.permissions().mode() & 0o777, 0o600);
        let principal_dir = std::fs::metadata(dir.0.join("alice")).unwrap();
        assert_eq!(principal_dir.permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn another_key_or_a_moved_file_does_not_open() {
        let dir = TempDir::new("binding");
        let store = FileSealedTokenStore::new(&dir.0, keys(7));
        store.put("alice", RELAY, &Secret::new("v1.secret")).unwrap();

        // The wrong key: unavailable, never garbage and never "absent".
        let other_key = FileSealedTokenStore::new(&dir.0, keys(8));
        assert_eq!(other_key.get("alice", RELAY), Err(TokenStoreError::Unavailable));

        // Copied into bob's directory: bound to alice, so bob cannot read it as his own.
        std::fs::create_dir_all(dir.0.join("bob")).unwrap();
        std::fs::copy(
            dir.0.join("alice").join("batch-relay-bearer.sealed"),
            dir.0.join("bob").join("batch-relay-bearer.sealed"),
        )
        .unwrap();
        assert_eq!(store.get("bob", RELAY), Err(TokenStoreError::Unavailable));

        // Renamed to another slot: bound to the relay slot.
        std::fs::copy(
            dir.0.join("alice").join("batch-relay-bearer.sealed"),
            dir.0.join("alice").join("gmail-tokenset.sealed"),
        )
        .unwrap();
        assert_eq!(
            store.get("alice", CredentialSlot::Connector(Service::Gmail)),
            Err(TokenStoreError::Unavailable)
        );

        // Tampered bytes fail authentication.
        let path = dir.0.join("alice").join("batch-relay-bearer.sealed");
        let mut text = std::fs::read_to_string(&path).unwrap();
        let last = text.pop().unwrap();
        text.push(if last == 'A' { 'B' } else { 'A' });
        std::fs::write(&path, text).unwrap();
        assert_eq!(store.get("alice", RELAY), Err(TokenStoreError::Unavailable));
    }

    #[test]
    fn a_path_escaping_principal_is_refused() {
        let dir = TempDir::new("escape");
        let store = FileSealedTokenStore::new(&dir.0, keys(7));
        for bad in ["../x", "a/b", "", ".tokens", "Alice"] {
            assert_eq!(store.put(bad, RELAY, &Secret::new("x")), Err(TokenStoreError::InvalidPrincipal), "{bad}");
            assert_eq!(store.get(bad, RELAY), Err(TokenStoreError::InvalidPrincipal), "{bad}");
        }
    }

    #[test]
    fn keys_differ_per_principal() {
        let a = SealKey::derive(&[1; 32], "alice").unwrap();
        let b = SealKey::derive(&[1; 32], "bob").unwrap();
        assert_ne!(a.0, b.0);
        assert_ne!(a.0, [1; 32], "the store key is never used as the AEAD key directly");
    }
}
