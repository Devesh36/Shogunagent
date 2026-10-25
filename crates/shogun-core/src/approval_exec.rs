//! The executor behind a human's remote approval (ShogunAgent E15 follow-up, feature `exec`).
//!
//! [`shogun_mcp::approval_exec`] is the seam: the REST server's decide route hands one approved
//! action to an [`ApprovalExecutor`] after the principal's ledger claimed it. This is the
//! implementation, and it adds no new egress: every action goes through
//! [`crate::send_exec::execute_send`], which prechecks the transport, writes the traceability row
//! to the principal's own store and only then calls the provider (no row, no send).
//!
//! - [`SendExecExecutor`] maps one approved action to an outcome: the transport for the action's
//!   service ([`SendTransports`]), the principal's traceability sink, `execute_send`, and a fixed
//!   failure class ([`classify`]) — never the provider's message.
//! - [`ConnectorTransports`] is the server's [`SendTransports`]: the principal's connector grant
//!   from its [`TokenStore`] (E5-T5), the first-layer [`ConnectorRuntime`] over the remote-MCP HTTPS
//!   client, gated exactly like the desktop's confirmed-write path (wave, scope table, plan,
//!   draft-only stop). A service without a usable grant is `not_connected` before anything is
//!   recorded or sent. Grants are not refreshed here: an expired access token is `not_connected`
//!   (reconnect, or let the app refresh it).

use std::sync::{Arc, Mutex};

use shogun_agents::permission::SendAction;
use shogun_integrations::token::GrantStore;
use shogun_integrations::{ConnectorRuntime, PrincipalGrants, RemoteMcpTransport, TokenStore};
use shogun_mcp::approval_exec::{ApprovalExecutor, ExecOutcome, ExecRequest};
use shogun_mcp::scope::{Service, Wave};

use crate::llm::traceability::TraceabilitySink;
use crate::send_exec::{execute_send, SendExecOutcome, SendTransport, FirstLayerSendTransport, TRACE_UNAVAILABLE_REFUSAL};

/// Where an approved action's transport comes from.
pub trait SendTransports: Send + Sync {
    /// The transport that performs `action` for `principal`, or the failure class when none can
    /// (nothing is recorded or sent then).
    fn transport_for(&self, principal: Option<&str>, action: &SendAction) -> Result<Box<dyn SendTransport>, &'static str>;
}

/// The traceability sink of a principal's store (`None`: the store cannot be opened, so nothing
/// may be sent).
pub type SinkFor = Arc<dyn Fn(Option<&str>) -> Option<Box<dyn TraceabilitySink>> + Send + Sync>;

/// The store of a principal (`None`: it cannot be opened, so nothing is granted or paid).
pub type StoreFor = Arc<dyn Fn(Option<&str>) -> Option<crate::daemon::Db> + Send + Sync>;

/// Runs approved actions through [`execute_send`].
pub struct SendExecExecutor {
    transports: Arc<dyn SendTransports>,
    sinks: SinkFor,
    /// E46: where an approved mandate grant is written and an approved payment recorded. Without
    /// it both are `not_executable` (nothing granted, nothing paid).
    stores: Option<StoreFor>,
}

impl SendExecExecutor {
    pub fn new(transports: Arc<dyn SendTransports>, sinks: SinkFor) -> Self {
        Self { transports, sinks, stores: None }
    }

    /// Apply approved payment mandates and payments (E46) in each principal's store.
    #[must_use]
    pub fn with_payments(mut self, stores: StoreFor) -> Self {
        self.stores = Some(stores);
        self
    }

    /// E46: a human approved a mandate grant or a payment. A grant is written into the
    /// principal's store (which re-checks the recorded human decision); a payment runs through
    /// the configured rail, traced first, its spend row claimed once.
    fn execute_payment(&self, request: &ExecRequest<'_>) -> ExecOutcome {
        let Some(db) = self.stores.as_ref().and_then(|stores| stores(request.principal)) else {
            return ExecOutcome::Failed("not_executable");
        };
        match &request.confirmed.action {
            SendAction::GrantPaymentMandate { grant } => {
                let terms = shogun_mcp::payments_api::terms_row(&grant.terms);
                match db.payments_grant(&grant.mandate_id, &grant.agent, &terms, grant.amends, request.approval_id) {
                    Ok(_) => ExecOutcome::Sent,
                    Err(code) => ExecOutcome::Failed(match code.as_str() {
                        "not_approved" => "not_authorized",
                        "mandate_revoked" | "mandate_not_yours" | "mandate_not_found" | "invalid_field" => "invalid_target",
                        _ => "ledger_unavailable",
                    }),
                }
            }
            SendAction::Pay { .. } => {
                let Some(rail) = crate::payment_rail::configured() else {
                    return ExecOutcome::Failed("not_connected");
                };
                match crate::payment_rail::pay_approved(&db, rail, request.confirmed, request.approval_id) {
                    crate::payment_rail::Paid::Settled { .. } => ExecOutcome::Sent,
                    crate::payment_rail::Paid::Failed(class) => ExecOutcome::Failed(class),
                }
            }
            _ => ExecOutcome::Failed("not_executable"),
        }
    }
}

impl ApprovalExecutor for SendExecExecutor {
    fn execute(&self, request: &ExecRequest<'_>) -> ExecOutcome {
        let action = &request.confirmed.action;
        if matches!(action, SendAction::Pay { .. } | SendAction::GrantPaymentMandate { .. }) {
            return self.execute_payment(request);
        }
        if shogun_agents::permission::send_target_refusal(action).is_some() {
            return ExecOutcome::Failed("invalid_target");
        }
        let Some(sink) = (self.sinks)(request.principal) else {
            return ExecOutcome::Failed("trace_unavailable");
        };
        let transport = match self.transports.transport_for(request.principal, action) {
            Ok(transport) => transport,
            Err(class) => return ExecOutcome::Failed(class),
        };
        match execute_send(request.confirmed, transport.as_ref(), sink.as_ref()) {
            SendExecOutcome::Sent => ExecOutcome::Sent,
            SendExecOutcome::Failed(error) => ExecOutcome::Failed(classify(&error)),
        }
    }
}

/// The fixed class of a transport or gate error (`shogun_agents::approval::EXECUTION_FAILURE_CLASSES`).
/// The error text never leaves this function.
pub fn classify(error: &str) -> &'static str {
    let lower = error.to_ascii_lowercase();
    if error == TRACE_UNAVAILABLE_REFUSAL {
        "trace_unavailable"
    } else if lower.contains("connection changed") {
        "connection_changed"
    } else if lower.contains("notconnected") || lower.contains("needsreauth") || lower.contains("not connected") {
        "not_connected"
    } else if lower.contains("local setting") || lower.contains("legacy broker") || lower.contains("has no google mcp tool") {
        "not_executable"
    } else if lower.contains("not authorized") || lower.contains("second-layer") {
        "not_authorized"
    } else if lower.contains("timed out") || lower.contains("timeout") {
        "timeout"
    } else {
        "provider_error"
    }
}

/// An owned first-layer transport: one runtime built for one approved action.
struct OwnedFirstLayer<T> {
    runtime: Mutex<ConnectorRuntime<T>>,
}

impl<T> SendTransport for OwnedFirstLayer<T>
where
    T: shogun_mcp::sync::IntegrationTransport + shogun_integrations::WriteExecutor,
{
    fn accepts_route(&self, route: shogun_agents::approval::Route) -> bool {
        FirstLayerSendTransport::new(&self.runtime).accepts_route(route)
    }

    fn send(&self, action: &SendAction, body: &str) -> Result<(), String> {
        FirstLayerSendTransport::new(&self.runtime).send(action, body)
    }

    fn precheck(&self, action: &SendAction) -> Result<(), String> {
        FirstLayerSendTransport::new(&self.runtime).precheck(action)
    }
}

/// A principal's stored access token, without refresh: an expired grant is refused.
struct StoredAccessToken<S> {
    grants: PrincipalGrants<S>,
}

impl<S: TokenStore> shogun_integrations::TokenProvider for StoredAccessToken<S> {
    fn access_token(&self, service: Service) -> Result<String, String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        match self.grants.load(service) {
            Ok(Some(tokens)) if !tokens.is_expired(now, 30_000) => Ok(tokens.access_token),
            _ => Err(format!("{} not connected", service.source_str())),
        }
    }
}

/// The server's transports: each principal's connector grants in a [`TokenStore`].
pub struct ConnectorTransports {
    tokens: Arc<dyn TokenStore>,
}

impl ConnectorTransports {
    pub fn new(tokens: Arc<dyn TokenStore>) -> Self {
        Self { tokens }
    }
}

impl SendTransports for ConnectorTransports {
    fn transport_for(&self, principal: Option<&str>, action: &SendAction) -> Result<Box<dyn SendTransport>, &'static str> {
        let service = shogun_integrations::send_bridge::service_for(action).ok_or("not_executable")?;
        let principal = principal.unwrap_or(shogun_integrations::token_store::LOCAL_PRINCIPAL);
        let grants = PrincipalGrants::new(self.tokens.clone(), principal);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        match grants.load(service) {
            Ok(Some(tokens)) if !tokens.is_expired(now, 30_000) => {}
            _ => return Err("not_connected"),
        }
        let rpc = crate::mcp_http::HttpMcpRpc::new(StoredAccessToken {
            grants: PrincipalGrants::new(self.tokens.clone(), principal),
        })
        .map_err(|_| "provider_error")?;
        let mut runtime = ConnectorRuntime::new(RemoteMcpTransport::new(rpc), Wave::Three, true);
        runtime.set_plan(shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0));
        runtime.restore_from_store(service, &grants);
        Ok(Box::new(OwnedFirstLayer { runtime: Mutex::new(runtime) }))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::llm::traceability::{TraceRecord, TraceUnrecorded};
    use shogun_agents::approval::{ConfirmedSend, Preview, Route};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Sink {
        rows: Mutex<Vec<TraceRecord>>,
        fail: bool,
    }

    impl TraceabilitySink for Sink {
        fn record(&self, rec: TraceRecord) {
            self.rows.lock().unwrap().push(rec);
        }
        fn try_record(&self, rec: TraceRecord) -> Result<(), TraceUnrecorded> {
            if self.fail {
                return Err(TraceUnrecorded);
            }
            self.record(rec);
            Ok(())
        }
    }

    struct Counting(Arc<AtomicUsize>, Result<(), String>);

    impl SendTransport for Counting {
        fn send(&self, _action: &SendAction, _body: &str) -> Result<(), String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            self.1.clone()
        }
    }

    struct Fixed(Arc<AtomicUsize>, Result<(), String>);

    impl SendTransports for Fixed {
        fn transport_for(&self, _principal: Option<&str>, _action: &SendAction) -> Result<Box<dyn SendTransport>, &'static str> {
            Ok(Box::new(Counting(self.0.clone(), self.1.clone())))
        }
    }

    fn confirmed(action: SendAction) -> ConfirmedSend {
        let preview = Preview::for_send(&action, "Forecast attached.", Route::DirectMcp);
        ConfirmedSend { action, preview }
    }

    fn executor(result: Result<(), String>, fail_sink: bool) -> (SendExecExecutor, Arc<AtomicUsize>, Arc<Sink>) {
        let sent = Arc::new(AtomicUsize::new(0));
        let sink = Arc::new(Sink { fail: fail_sink, ..Sink::default() });
        let shared = sink.clone();
        let sinks: SinkFor = Arc::new(move |_| {
            let sink = shared.clone();
            Some(Box::new(Forward(sink)) as Box<dyn TraceabilitySink>)
        });
        (SendExecExecutor::new(Arc::new(Fixed(sent.clone(), result)), sinks), sent, sink)
    }

    struct Forward(Arc<Sink>);

    impl TraceabilitySink for Forward {
        fn record(&self, rec: TraceRecord) {
            self.0.record(rec);
        }
        fn try_record(&self, rec: TraceRecord) -> Result<(), TraceUnrecorded> {
            self.0.try_record(rec)
        }
    }

    #[test]
    fn a_send_is_traced_before_it_leaves_and_failures_become_classes() {
        let action = SendAction::PostMessage { channel: "C0123ABCD".into() };
        let (exec, sent, sink) = executor(Ok(()), false);
        let request = confirmed(action.clone());
        let outcome = exec.execute(&ExecRequest { approval_id: 1, principal: Some("alice"), confirmed: &request });
        assert_eq!(outcome, ExecOutcome::Sent);
        assert_eq!(sent.load(Ordering::SeqCst), 1);
        assert_eq!(sink.rows.lock().unwrap().len(), 1, "one traceability row");

        let (exec, sent, _) = executor(Err("upstream said 500: <html>secret</html>".into()), false);
        let outcome = exec.execute(&ExecRequest { approval_id: 2, principal: None, confirmed: &request });
        assert_eq!(outcome, ExecOutcome::Failed("provider_error"));
        assert_eq!(sent.load(Ordering::SeqCst), 1);

        // No row, no send.
        let (exec, sent, _) = executor(Ok(()), true);
        let outcome = exec.execute(&ExecRequest { approval_id: 3, principal: None, confirmed: &request });
        assert_eq!(outcome, ExecOutcome::Failed("trace_unavailable"));
        assert_eq!(sent.load(Ordering::SeqCst), 0);

        // A target no provider call can use is refused before anything.
        let (exec, sent, sink) = executor(Ok(()), false);
        let bad = confirmed(SendAction::PostMessage { channel: "#design".into() });
        assert_eq!(exec.execute(&ExecRequest { approval_id: 4, principal: None, confirmed: &bad }), ExecOutcome::Failed("invalid_target"));
        assert_eq!((sent.load(Ordering::SeqCst), sink.rows.lock().unwrap().len()), (0, 0));
    }

    #[test]
    fn classes_are_fixed_labels() {
        assert_eq!(classify(TRACE_UNAVAILABLE_REFUSAL), "trace_unavailable");
        assert_eq!(classify("write not authorized: Denied(NotConnected)"), "not_connected");
        assert_eq!(classify("write not authorized: Denied(DraftStop)"), "not_authorized");
        assert_eq!(classify("Gmail connection changed; create a new approval."), "connection_changed");
        assert_eq!(classify("a local setting approval has no transport"), "not_executable");
        assert_eq!(classify("operation timed out"), "timeout");
        assert_eq!(classify("anything else"), "provider_error");
        for class in ["trace_unavailable", "not_connected", "not_authorized", "connection_changed", "not_executable", "timeout", "provider_error"] {
            assert!(shogun_agents::approval::EXECUTION_FAILURE_CLASSES.contains(&class), "{class}");
        }
    }

    #[test]
    fn a_principal_without_a_grant_is_not_connected_before_anything_runs() {
        let store: Arc<dyn TokenStore> = Arc::new(shogun_integrations::token_store::MemorySecretStore::new());
        let transports = ConnectorTransports::new(store);
        let action = SendAction::PostMessage { channel: "C0123ABCD".into() };
        assert_eq!(transports.transport_for(Some("alice"), &action).err(), Some("not_connected"));
        assert_eq!(transports.transport_for(Some("alice"), &SendAction::ShareWritingStyle).err(), Some("not_executable"));
    }
}
