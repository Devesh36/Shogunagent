//! Tenancy (ShogunAgent E2): one principal = one SQLCipher file (CLAUDE.md invariant 6).
//!
//! A `tenant` (organisation) holds `principal`s (people, one world model each), and `agent`s hold
//! the keys that act for a principal. Nothing here puts a tenant column in the schema: the file is
//! the boundary, and a cross-principal read can only happen through an explicit ACL (Team, later),
//! never through a query.
//!
//! - [`PrincipalId`]: a validated slug, which is also the directory name under the store root.
//! - [`KeyProvider`]: where a principal's SQLCipher key comes from — the environment
//!   ([`EnvKeyProvider`]: `SHOGUN_DB_KEY`, or `SHOGUN_DB_KEY_<PRINCIPAL>` per principal) or a KMS
//!   ([`KmsKeyProvider`], a stub until hosted lands). Keys are held as [`shogun_memory::DbKey`]
//!   and never rendered.
//! - [`PrincipalStore`]: `principal_id → <root>/<id>/memory.db`, opened with the daemon's own
//!   [`crate::daemon::Db`] / [`crate::daemon::DaemonDb`] machinery, with an LRU of open handles
//!   and a hard cap on how many stay open.
//! - [`PrincipalResolver`]: bearer token → principal + actor. [`TokenPrincipalMap`] is the
//!   sidecar's own in-memory map (`SHOGUN_PRINCIPAL_TOKENS`); `apps/api` replaces it with real
//!   agent keys in E3.
//! - [`import`]: a ShogunAI single-user `memory.db` becomes a principal's file (E2-T6).
//! - [`dream_tick`]: the hourly Dream Cycle per principal, only when new material exists (E5-T4;
//!   `shogun-api --dream-tick`).
//! - [`multi`] (feature `daemon-server`): the `shogun-api --multi` composition.

pub mod dream_tick;
pub mod import;
pub mod key_provider;
#[cfg(feature = "daemon-server")]
pub mod multi;
pub mod principal;
pub mod resolver;
pub mod store;

pub use import::{import_single_user_store, ImportReport};
pub use key_provider::{EnvKeyProvider, KeyProvider, KmsKeyProvider};
pub use principal::PrincipalId;
pub use resolver::{PrincipalResolver, ResolvedPrincipal, TokenPrincipalMap};
pub use store::{OpenPolicy, OpenPrincipal, PrincipalStore};

/// What can go wrong around a principal's store. Never carries a key or a row.
#[derive(Debug, thiserror::Error)]
pub enum TenancyError {
    #[error("principal id must be 1-64 characters of [a-z0-9_-] and start with a letter or digit")]
    InvalidPrincipalId,
    #[error("no key for this principal: set SHOGUN_DB_KEY, or SHOGUN_DB_KEY_<PRINCIPAL> for it")]
    NoKey,
    /// Names the variable (suffix), never its value.
    #[error("{0} is not a 64-character hex key")]
    InvalidKey(String),
    #[error("the KMS key provider is not available in this build")]
    KmsUnavailable,
    #[error("principal store i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not open the principal's store: {0}")]
    Open(String),
    #[error("the principal's store is unavailable: {0}")]
    Unavailable(String),
    #[error("a store for this principal already exists")]
    AlreadyExists,
    #[error("the source store does not exist or is not a file")]
    SourceMissing,
    #[error("the source store has a live write-ahead log; close the app that holds it first")]
    SourceInUse,
    #[error("invalid principal token map: {0}")]
    InvalidTokenMap(&'static str),
}
