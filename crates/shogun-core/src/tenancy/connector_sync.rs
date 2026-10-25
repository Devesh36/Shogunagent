//! The hosted sidecar's connector read-sync and audit rows (plan E44 / E50), per principal.
//!
//! - [`drain_tick`]: the background step `shogun-api --dream-tick` runs for every principal before
//!   the hourly review — fetch the provider change notices waiting in the principal's store
//!   (`shogun_integrations::read_sync`) with the principal's own connector credentials and ingest
//!   what comes back into the principal's own file. A principal with nothing queued costs one store
//!   read and no network call.
//! - [`record_audit`]: one traceability row per vault event (connect, refresh, disconnect,
//!   revocation, read-sync fetch) in the principal's own store, so the vendor calls made for a
//!   principal show on its traceability log (threat model R10). Fixed words and a host only.
//!
//! Nothing here logs content: outcomes are per-principal counts.

use shogun_integrations::hosted_connect::{AuditEvent, HostedConnectors};
use shogun_integrations::oauth::TokenExchange;
use shogun_integrations::read_sync::{self, DrainReport, ProviderHttp};
use shogun_mcp::connector_vault::VaultError;

use crate::daemon::Db;
use crate::llm::traceability::{digest, Route, TraceKind, TraceRecord, TraceabilitySink};

use super::{PrincipalId, PrincipalStore};

/// The traceability purpose of an audit event: `connector.<action>.<provider>.<outcome>`.
pub fn purpose(event: &AuditEvent) -> String {
    format!("connector.{}.{}.{}", event.action, event.provider, event.outcome)
}

/// Write `event` as a traceability row in `db` (the principal's own store). Zero bytes and the
/// digest of nothing: the row records that a call happened and where it went, never what it said.
/// A ledger fault is dropped (the sink counts it): an audit row never fails a connector call.
pub fn record_audit(db: &Db, event: &AuditEvent) {
    let record = TraceRecord {
        route: Route::Mcp,
        purpose: purpose(event),
        destination: event.destination.to_string(),
        chunk_bytes: 0,
        chunk_xxh64: digest(""),
        third_party: false,
        kind: TraceKind::Egress,
        url: None,
    };
    let _ = db.traceability_sink().try_record(record);
}

/// One principal's line of a read-sync tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncTick {
    pub principal: PrincipalId,
    /// `None` when nothing was queued (no store opened, no call made).
    pub outcome: Option<Result<DrainReport, VaultError>>,
}

/// Drain every principal's read-sync queue into its own store. Blocking (network calls): run it
/// off the async workers.
pub fn drain_tick<X: TokenExchange + Send + Sync>(
    store: &PrincipalStore,
    vault: &HostedConnectors<X>,
    http: &dyn ProviderHttp,
    principals: &[PrincipalId],
    now_ms: i64,
) -> Vec<SyncTick> {
    principals
        .iter()
        .map(|principal| {
            if !read_sync::has_pending(vault, principal.as_str()) {
                return SyncTick { principal: principal.clone(), outcome: None };
            }
            let outcome = match store.open(principal) {
                Ok(handle) => read_sync::drain(vault, http, handle.memory_backend().as_ref(), principal.as_str(), now_ms),
                Err(_) => Err(VaultError::Unavailable),
            };
            SyncTick { principal: principal.clone(), outcome: Some(outcome) }
        })
        .collect()
}

/// A one-line summary of a tick for stderr: counts only.
pub fn summary(report: &[SyncTick]) -> String {
    let mut drained = 0;
    let mut failed = 0;
    let mut totals = DrainReport::default();
    for line in report {
        match &line.outcome {
            Some(Ok(r)) => {
                drained += 1;
                totals.jobs += r.jobs;
                totals.objects += r.objects;
                totals.stored += r.stored;
                totals.not_connected += r.not_connected;
                totals.retried += r.retried;
                totals.dropped += r.dropped;
            }
            Some(Err(_)) => failed += 1,
            None => {}
        }
    }
    format!(
        "principals={} drained={drained} failed={failed} notices={} objects={} stored={} not_connected={} retried={} dropped={}",
        report.len(),
        totals.jobs,
        totals.objects,
        totals.stored,
        totals.not_connected,
        totals.retried,
        totals.dropped
    )
}
