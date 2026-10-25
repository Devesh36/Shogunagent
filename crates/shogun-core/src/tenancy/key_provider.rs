//! Where a principal's SQLCipher key comes from. Keys stay in [`DbKey`] (no `Debug` render) and
//! are never logged; the environment is read once, at construction, so a provider's answer does
//! not change under a running server and no per-request call touches the process environment.
//!
//! Two providers:
//!
//! - [`EnvKeyProvider`]: `SHOGUN_DB_KEY` / `SHOGUN_DB_KEY_<PRINCIPAL>`. Local, single-operator.
//! - [`KmsKeyProvider`] (E26): **envelope encryption under the customer's KMS key.** Each
//!   principal's SQLCipher key is a random 32-byte data key; only its wrapped form is stored, next
//!   to the principal's file (`<root>/<principal>/<principal>.key.wrapped`). Opening a store asks
//!   the KMS to unwrap it; the plaintext lives only in memory ([`DataKey`], zeroized on drop). When
//!   the KMS reports that the key has moved on (a new primary version, another configured key),
//!   the data key is re-wrapped under the current version and the file replaced — the database
//!   itself is never re-encrypted. A KMS that does not answer is an error: the store refuses to
//!   open, and nothing falls back to an environment key or to plaintext.
//!
//! The KMS clients ([`aws`] AWS KMS with SigV4, [`gcp`] GCP Cloud KMS, [`vault`] HashiCorp Vault
//! Transit) are built with feature `net` and send through the single traced egress
//! ([`egress`]). [`provider_from_env`] picks one by name (`--key-provider` / `SHOGUN_KEY_PROVIDER`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use shogun_integrations::token_store::{DataKey, KeyWrapper, WrappedDataKey};
use shogun_memory::DbKey;

use super::{PrincipalId, TenancyError};

#[cfg(feature = "net")]
pub mod aws;
#[cfg(feature = "net")]
pub mod egress;
#[cfg(feature = "net")]
pub mod gcp;
#[cfg(feature = "net")]
pub mod vault;

/// Resolve the key a principal's file is encrypted with.
pub trait KeyProvider: Send + Sync {
    fn key_for(&self, principal: &PrincipalId) -> Result<DbKey, TenancyError>;
}

/// Keys from the environment: `SHOGUN_DB_KEY_<PRINCIPAL>` (the principal's
/// [`PrincipalId::env_suffix`]) for one principal, else `SHOGUN_DB_KEY` for every principal that
/// has no key of its own. Local, single-operator deployments; hosted and BYOC use
/// [`KmsKeyProvider`].
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

/// The wrapped-key file's suffix: `<root>/<principal>/<principal>.key.wrapped`.
pub const WRAPPED_KEY_SUFFIX: &str = ".key.wrapped";

/// The format tag inside a wrapped-key file.
const WRAPPED_FORMAT: &str = "shogun-wrapped-key/v1";

/// The KMS context a principal's SQLCipher data key is bound to.
pub fn sqlcipher_context(principal: &PrincipalId) -> String {
    format!("shogun/sqlcipher/v1/{}", principal.as_str())
}

/// Default lifetime of an unwrapped key in the provider's memory cache.
pub const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(300);

/// What a wrapped-key file holds: no secret, only ciphertext and the names needed to unwrap it.
#[derive(serde::Serialize, serde::Deserialize)]
struct WrappedKeyFile {
    format: String,
    provider: String,
    key_version: String,
    context: String,
    wrapped: String,
}

/// Keys sealed in a customer KMS, one data key per principal (hosted, BYOC). See the module docs.
pub struct KmsKeyProvider {
    root: PathBuf,
    wrapper: Arc<dyn KeyWrapper>,
    cache_ttl: Duration,
    cache: Mutex<HashMap<PrincipalId, (DataKey, Instant)>>,
}

impl KmsKeyProvider {
    /// Wrapped keys under `root` (the multi-principal root), unwrapped through `wrapper`.
    pub fn new(root: impl Into<PathBuf>, wrapper: Arc<dyn KeyWrapper>) -> Self {
        Self { root: root.into(), wrapper, cache_ttl: DEFAULT_CACHE_TTL, cache: Mutex::new(HashMap::new()) }
    }

    /// How long an unwrapped key stays in memory before the next open asks the KMS again
    /// (`Duration::ZERO`: every open asks). A revoked KMS grant takes effect within this window.
    #[must_use]
    pub fn with_cache_ttl(mut self, ttl: Duration) -> Self {
        self.cache_ttl = ttl;
        self
    }

    /// The provider name of the KMS behind this key provider (`aws-kms`, `gcp-kms`, `vault`).
    pub fn provider(&self) -> &'static str {
        self.wrapper.provider()
    }

    /// `<root>/<principal>/<principal>.key.wrapped`.
    pub fn wrapped_path(&self, principal: &PrincipalId) -> PathBuf {
        self.root
            .join(principal.as_str())
            .join(format!("{}{WRAPPED_KEY_SUFFIX}", principal.as_str()))
    }

    /// Forget every unwrapped key held in memory (zeroized as they drop).
    pub fn forget_all(&self) {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner).clear();
    }

    /// Re-wrap `principal`'s data key under the KMS key's current version now, without opening or
    /// re-encrypting the database. `Ok(false)` when it was already current.
    pub fn rewrap(&self, principal: &PrincipalId) -> Result<bool, TenancyError> {
        let path = self.wrapped_path(principal);
        let (file, wrapped) = read_wrapped(&path, self.wrapper.provider())?;
        let unwrapped = self.wrapper.unwrap(&wrapped, &file.context).map_err(TenancyError::Kms)?;
        if !unwrapped.stale {
            return Ok(false);
        }
        let fresh = self.wrapper.wrap(&unwrapped.key, &file.context).map_err(TenancyError::Kms)?;
        write_wrapped(&path, self.wrapper.provider(), &file.context, &fresh, true)?;
        Ok(true)
    }

    fn cached(&self, principal: &PrincipalId) -> Option<DbKey> {
        if self.cache_ttl.is_zero() {
            return None;
        }
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        match cache.get(principal) {
            Some((key, at)) if at.elapsed() < self.cache_ttl => Some(key.with_bytes(|b| DbKey::new(*b))),
            Some(_) => {
                cache.remove(principal);
                None
            }
            None => None,
        }
    }

    fn remember(&self, principal: &PrincipalId, key: &DataKey) {
        if self.cache_ttl.is_zero() {
            return;
        }
        self.cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(principal.clone(), (key.duplicate(), Instant::now()));
    }

    fn unwrap_existing(&self, principal: &PrincipalId, path: &Path) -> Result<DataKey, TenancyError> {
        let (file, wrapped) = read_wrapped(path, self.wrapper.provider())?;
        if file.context != sqlcipher_context(principal) {
            return Err(TenancyError::WrappedKey("the wrapped key belongs to another principal"));
        }
        let unwrapped = self.wrapper.unwrap(&wrapped, &file.context).map_err(TenancyError::Kms)?;
        if unwrapped.stale {
            // Rotation: same data key, wrapped under the current version. A failure here does not
            // block the open — the old wrapping still unwraps — and is retried on the next open.
            if let Ok(fresh) = self.wrapper.wrap(&unwrapped.key, &file.context) {
                let _ = write_wrapped(path, self.wrapper.provider(), &file.context, &fresh, true);
            }
        }
        Ok(unwrapped.key)
    }
}

impl KeyProvider for KmsKeyProvider {
    fn key_for(&self, principal: &PrincipalId) -> Result<DbKey, TenancyError> {
        if let Some(key) = self.cached(principal) {
            return Ok(key);
        }
        let path = self.wrapped_path(principal);
        let key = if path.is_file() {
            self.unwrap_existing(principal, &path)?
        } else {
            let store = self.root.join(principal.as_str()).join(super::store::STORE_FILE);
            if store.exists() {
                // A store without its wrapped key cannot be opened, and a new key would not open it
                // either: refuse instead of minting one over it.
                return Err(TenancyError::WrappedKey(
                    "the principal's store exists but its wrapped key file is missing; restore it from the backup taken with the store",
                ));
            }
            let context = sqlcipher_context(principal);
            let (key, wrapped) = self.wrapper.generate(&context).map_err(TenancyError::Kms)?;
            match write_wrapped(&path, self.wrapper.provider(), &context, &wrapped, false) {
                Ok(()) => key,
                // Another open won the race to create it: use the one on disk.
                Err(TenancyError::AlreadyExists) => self.unwrap_existing(principal, &path)?,
                Err(error) => return Err(error),
            }
        };
        self.remember(principal, &key);
        Ok(key.with_bytes(|b| DbKey::new(*b)))
    }
}

fn read_wrapped(path: &Path, provider: &str) -> Result<(WrappedKeyFile, WrappedDataKey), TenancyError> {
    let text = std::fs::read_to_string(path)?;
    let file: WrappedKeyFile =
        serde_json::from_str(&text).map_err(|_| TenancyError::WrappedKey("the wrapped key file is not readable"))?;
    if file.format != WRAPPED_FORMAT {
        return Err(TenancyError::WrappedKey("the wrapped key file has an unknown format"));
    }
    if file.provider != provider {
        return Err(TenancyError::WrappedKey(
            "the wrapped key was made by another KMS provider than the one this server is configured with",
        ));
    }
    let blob = STANDARD
        .decode(file.wrapped.as_bytes())
        .map_err(|_| TenancyError::WrappedKey("the wrapped key file is not readable"))?;
    let wrapped = WrappedDataKey { key_version: file.key_version.clone(), blob };
    Ok((file, wrapped))
}

/// Write the wrapped key: to a 0600 temporary in the same directory, then `replace` it over the
/// old file (rotation) or link it into place only if none exists (first key; a concurrent creator
/// wins and this answers [`TenancyError::AlreadyExists`]).
fn write_wrapped(path: &Path, provider: &str, context: &str, wrapped: &WrappedDataKey, replace: bool) -> Result<(), TenancyError> {
    let file = WrappedKeyFile {
        format: WRAPPED_FORMAT.to_string(),
        provider: provider.to_string(),
        key_version: wrapped.key_version.clone(),
        context: context.to_string(),
        wrapped: STANDARD.encode(&wrapped.blob),
    };
    let text = serde_json::to_string_pretty(&file).map_err(|_| TenancyError::WrappedKey("could not encode the wrapped key"))?;
    let dir = path.parent().ok_or(TenancyError::WrappedKey("the wrapped key has no directory"))?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".wrapped-{}.tmp", temp_suffix()));
    write_private(&tmp, text.as_bytes())?;
    let result = if replace {
        std::fs::rename(&tmp, path).map_err(TenancyError::from)
    } else {
        let linked = std::fs::hard_link(&tmp, path);
        let _ = std::fs::remove_file(&tmp);
        match linked {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(TenancyError::AlreadyExists),
            Err(e) => Err(TenancyError::from(e)),
        }
    };
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// A temporary file name unique within this process and across processes sharing the directory.
fn temp_suffix() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or_default();
    format!("{}-{n}-{nanos}", std::process::id())
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

/// The key providers `shogun-api` can be started with (`--key-provider`, `SHOGUN_KEY_PROVIDER`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyProviderKind {
    Env,
    AwsKms,
    GcpKms,
    Vault,
}

impl KeyProviderKind {
    pub const VAR: &'static str = "SHOGUN_KEY_PROVIDER";
    pub const FLAG: &'static str = "--key-provider";

    /// The name `--key-provider` takes.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::AwsKms => "aws-kms",
            Self::GcpKms => "gcp-kms",
            Self::Vault => "vault",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, TenancyError> {
        match raw.trim() {
            "" | "env" => Ok(Self::Env),
            "aws-kms" => Ok(Self::AwsKms),
            "gcp-kms" => Ok(Self::GcpKms),
            "vault" => Ok(Self::Vault),
            _ => Err(TenancyError::KmsConfig("--key-provider (one of env, aws-kms, gcp-kms, vault)")),
        }
    }

    /// `--key-provider <kind>` on the command line, else `SHOGUN_KEY_PROVIDER`, else `env`.
    pub fn select(args: impl IntoIterator<Item = String>, vars: &HashMap<String, String>) -> Result<Self, TenancyError> {
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            if arg == Self::FLAG {
                return Self::parse(&args.next().unwrap_or_default());
            }
            if let Some(value) = arg.strip_prefix("--key-provider=") {
                return Self::parse(value);
            }
        }
        Self::parse(vars.get(Self::VAR).map(String::as_str).unwrap_or(""))
    }
}

/// `SHOGUN_KMS_KEY_CACHE_SECS` (default 300; 0 = ask the KMS on every open).
pub const CACHE_SECS_VAR: &str = "SHOGUN_KMS_KEY_CACHE_SECS";
/// `SHOGUN_KMS_TIMEOUT_SECS` (default 10): the deadline of one KMS round trip.
pub const TIMEOUT_SECS_VAR: &str = "SHOGUN_KMS_TIMEOUT_SECS";

fn secs(vars: &HashMap<String, String>, name: &'static str, default: u64) -> Result<Duration, TenancyError> {
    match vars.get(name).map(|v| v.trim()).filter(|v| !v.is_empty()) {
        None => Ok(Duration::from_secs(default)),
        Some(value) => value
            .parse::<u64>()
            .map(Duration::from_secs)
            .map_err(|_| TenancyError::KmsConfig(name)),
    }
}

/// The key provider for the principal stores under `root`, chosen by `kind`, configured from
/// `vars` (the process environment, read once). Every setting is documented in `deploy/README.md`.
pub fn provider_from_env(kind: KeyProviderKind, root: &Path, vars: &HashMap<String, String>) -> Result<Arc<dyn KeyProvider>, TenancyError> {
    if kind == KeyProviderKind::Env {
        return Ok(Arc::new(EnvKeyProvider::from_vars(vars.clone())?));
    }
    let ttl = secs(vars, CACHE_SECS_VAR, DEFAULT_CACHE_TTL.as_secs())?;
    let wrapper = wrapper_from_env(kind, vars)?;
    Ok(Arc::new(KmsKeyProvider::new(root, wrapper).with_cache_ttl(ttl)))
}

/// The [`KeyWrapper`] for `kind` over the real HTTPS transport, for [`KmsKeyProvider`] and for the
/// remote token store's [`shogun_integrations::KmsSealer`].
#[cfg(feature = "net")]
pub fn wrapper_from_env(kind: KeyProviderKind, vars: &HashMap<String, String>) -> Result<Arc<dyn KeyWrapper>, TenancyError> {
    let timeout = secs(vars, TIMEOUT_SECS_VAR, 10)?;
    let http = |provider| egress::KmsHttp::real(provider, timeout);
    Ok(match kind {
        KeyProviderKind::Env => return Err(TenancyError::KmsConfig("the env key provider has no KMS")),
        KeyProviderKind::AwsKms => Arc::new(aws::AwsKms::from_vars(vars, http("aws-kms")?)?),
        KeyProviderKind::GcpKms => Arc::new(gcp::GcpKms::from_vars(vars, http("gcp-kms")?)?),
        KeyProviderKind::Vault => Arc::new(vault::VaultTransit::from_vars(vars, http("vault")?)?),
    })
}

/// The hosted / BYOC credential sealer: the remote token store's [`shogun_integrations::KmsEnvelope`]
/// over the same customer KMS key as the principal stores (one data key per stored credential).
pub fn kms_sealer_from_env(
    kind: KeyProviderKind,
    vars: &HashMap<String, String>,
) -> Result<shogun_integrations::token_store::KmsSealer<Arc<dyn KeyWrapper>>, TenancyError> {
    Ok(shogun_integrations::token_store::KmsSealer::new(wrapper_from_env(kind, vars)?))
}

/// Without `net` there is no HTTP client to reach a KMS: every KMS kind fails closed.
#[cfg(not(feature = "net"))]
pub fn wrapper_from_env(_kind: KeyProviderKind, _vars: &HashMap<String, String>) -> Result<Arc<dyn KeyWrapper>, TenancyError> {
    Err(TenancyError::KmsUnavailable)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use shogun_integrations::token_store::kms::fake::FakeWrapper;
    use shogun_integrations::token_store::KmsError;

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

    pub(super) struct TempRoot(pub PathBuf);

    impl TempRoot {
        pub(super) fn new(label: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("shogun_kms_{label}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_new_principal_gets_a_wrapped_data_key_and_the_same_key_back() {
        let root = TempRoot::new("new");
        let kms = Arc::new(FakeWrapper::new());
        let provider = KmsKeyProvider::new(&root.0, kms.clone()).with_cache_ttl(Duration::ZERO);
        let alice = PrincipalId::parse("alice").unwrap();
        let first = provider.key_for(&alice).unwrap().to_hex();
        let path = provider.wrapped_path(&alice);
        assert_eq!(path, root.0.join("alice").join("alice.key.wrapped"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains(&first), "the data key never reaches disk in clear");
        assert!(text.contains("\"provider\": \"fake\"") && text.contains("shogun/sqlcipher/v1/alice"), "{text}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(provider.key_for(&alice).unwrap().to_hex(), first, "unwrapped, not re-minted");
        let bob = PrincipalId::parse("bob").unwrap();
        assert_ne!(provider.key_for(&bob).unwrap().to_hex(), first, "one data key per principal");
        assert!(!std::fs::read_dir(root.0.join("alice")).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().ends_with(".tmp")));
    }

    #[test]
    fn rotation_rewraps_without_changing_the_data_key() {
        let root = TempRoot::new("rotate");
        let kms = Arc::new(FakeWrapper::new());
        let provider = KmsKeyProvider::new(&root.0, kms.clone()).with_cache_ttl(Duration::ZERO);
        let alice = PrincipalId::parse("alice").unwrap();
        let key = provider.key_for(&alice).unwrap().to_hex();
        let before = std::fs::read_to_string(provider.wrapped_path(&alice)).unwrap();
        assert!(before.contains("fake/v1"));
        assert!(!provider.rewrap(&alice).unwrap(), "current");

        kms.rotate();
        assert_eq!(provider.key_for(&alice).unwrap().to_hex(), key, "same data key after rotation");
        let after = std::fs::read_to_string(provider.wrapped_path(&alice)).unwrap();
        assert!(after.contains("fake/v2"), "re-wrapped on open: {after}");
        assert_ne!(before, after);
        assert_eq!(provider.key_for(&alice).unwrap().to_hex(), key);

        kms.rotate();
        assert!(provider.rewrap(&alice).unwrap(), "explicit rewrap");
        assert!(std::fs::read_to_string(provider.wrapped_path(&alice)).unwrap().contains("fake/v3"));
    }

    #[test]
    fn a_kms_outage_refuses_the_key_and_never_falls_back() {
        let root = TempRoot::new("down");
        let kms = Arc::new(FakeWrapper::new());
        let provider = KmsKeyProvider::new(&root.0, kms.clone()).with_cache_ttl(Duration::ZERO);
        let alice = PrincipalId::parse("alice").unwrap();
        provider.key_for(&alice).unwrap();
        kms.set_down(true);
        let err = provider.key_for(&alice).err().unwrap();
        assert!(matches!(err, TenancyError::Kms(KmsError::Unreachable)), "{err}");
        assert!(err.to_string().contains("unreachable"), "{err}");
        // A new principal is not minted a key while the KMS is down either.
        let bob = PrincipalId::parse("bob").unwrap();
        assert!(provider.key_for(&bob).is_err());
        assert!(!provider.wrapped_path(&bob).exists());
    }

    #[test]
    fn the_cache_serves_within_its_ttl_and_forgets_on_request() {
        let root = TempRoot::new("cache");
        let kms = Arc::new(FakeWrapper::new());
        let provider = KmsKeyProvider::new(&root.0, kms.clone()).with_cache_ttl(Duration::from_secs(60));
        let alice = PrincipalId::parse("alice").unwrap();
        let key = provider.key_for(&alice).unwrap().to_hex();
        kms.set_down(true);
        assert_eq!(provider.key_for(&alice).unwrap().to_hex(), key, "held in memory within the TTL");
        provider.forget_all();
        assert!(provider.key_for(&alice).is_err(), "forgotten: the KMS is asked again");
    }

    #[test]
    fn a_store_without_its_wrapped_key_is_refused_not_rekeyed() {
        let root = TempRoot::new("missing");
        let provider = KmsKeyProvider::new(&root.0, Arc::new(FakeWrapper::new()));
        let alice = PrincipalId::parse("alice").unwrap();
        std::fs::create_dir_all(root.0.join("alice")).unwrap();
        std::fs::write(root.0.join("alice").join("memory.db"), b"x").unwrap();
        let err = provider.key_for(&alice).err().unwrap();
        assert!(matches!(err, TenancyError::WrappedKey(_)), "{err}");
        assert!(!provider.wrapped_path(&alice).exists());
    }

    #[test]
    fn a_wrapped_key_moved_to_another_principal_or_provider_is_refused() {
        let root = TempRoot::new("moved");
        let provider = KmsKeyProvider::new(&root.0, Arc::new(FakeWrapper::new())).with_cache_ttl(Duration::ZERO);
        let alice = PrincipalId::parse("alice").unwrap();
        let bob = PrincipalId::parse("bob").unwrap();
        provider.key_for(&alice).unwrap();
        std::fs::create_dir_all(root.0.join("bob")).unwrap();
        std::fs::copy(provider.wrapped_path(&alice), provider.wrapped_path(&bob)).unwrap();
        assert!(matches!(provider.key_for(&bob), Err(TenancyError::WrappedKey(_))));

        let text = std::fs::read_to_string(provider.wrapped_path(&alice)).unwrap().replace("\"fake\"", "\"vault\"");
        std::fs::write(provider.wrapped_path(&alice), text).unwrap();
        assert!(matches!(provider.key_for(&alice), Err(TenancyError::WrappedKey(_))));
    }

    #[test]
    fn the_encrypted_store_opens_with_the_kms_key_and_not_without_it() {
        use crate::tenancy::PrincipalStore;
        let root = TempRoot::new("store");
        let kms = Arc::new(FakeWrapper::new());
        let keys: Arc<dyn KeyProvider> = Arc::new(KmsKeyProvider::new(&root.0, kms.clone()).with_cache_ttl(Duration::ZERO));
        let clock: crate::daemon::Clock = Arc::new(|| 1_700_000_000_000);
        let alice = PrincipalId::parse("alice").unwrap();
        {
            let store = PrincipalStore::new(&root.0, keys.clone(), clock.clone(), 2);
            store.open(&alice).unwrap();
        }
        assert!(!shogun_memory::is_plaintext_db(root.0.join("alice").join("memory.db")));
        kms.rotate();
        {
            // Rotated: the database opens unchanged (no re-encryption), the wrapping moved on.
            let store = PrincipalStore::new(&root.0, keys.clone(), clock.clone(), 2);
            store.open(&alice).unwrap();
        }
        kms.set_down(true);
        let store = PrincipalStore::new(&root.0, keys, clock, 2);
        let err = store.open(&alice).err().unwrap();
        assert!(matches!(err, TenancyError::Kms(_)), "{err}");
    }

    #[test]
    fn the_provider_is_chosen_by_flag_then_env_then_default() {
        let none = HashMap::new();
        assert_eq!(KeyProviderKind::select(Vec::<String>::new(), &none).unwrap(), KeyProviderKind::Env);
        let env = HashMap::from([(KeyProviderKind::VAR.to_string(), "vault".to_string())]);
        assert_eq!(KeyProviderKind::select(Vec::<String>::new(), &env).unwrap(), KeyProviderKind::Vault);
        let args = ["--multi".to_string(), "--key-provider".to_string(), "aws-kms".to_string()];
        assert_eq!(KeyProviderKind::select(args, &env).unwrap(), KeyProviderKind::AwsKms);
        assert_eq!(KeyProviderKind::select(["--key-provider=gcp-kms".to_string()], &none).unwrap(), KeyProviderKind::GcpKms);
        assert!(KeyProviderKind::select(["--key-provider".to_string(), "plaintext".to_string()], &none).is_err());

        let root = TempRoot::new("select");
        let vars = HashMap::from([("SHOGUN_DB_KEY".to_string(), hex(0x33))]);
        let keys = provider_from_env(KeyProviderKind::Env, &root.0, &vars).unwrap();
        assert_eq!(keys.key_for(&PrincipalId::parse("a").unwrap()).unwrap().to_hex(), hex(0x33));
        let bad = HashMap::from([(CACHE_SECS_VAR.to_string(), "5m".to_string())]);
        assert!(matches!(provider_from_env(KeyProviderKind::Vault, &root.0, &bad), Err(TenancyError::KmsConfig(CACHE_SECS_VAR))));
    }
}
