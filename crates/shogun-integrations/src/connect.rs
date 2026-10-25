//! Pure helpers for the desktop "Connect" flow: published transport availability and
//! the typed outcome of an interactive OAuth connect attempt.
//!
//! Linux-testable, no effects. The desktop adapter (`apps/desktop/src-tauri/src/connectors.rs`)
//! reads the env vars and maps a [`ConnectError`] onto the FR-INT-06/07 state machine:
//! - a failed *attempt* ([`ConnectError::marks_amber`] = true — denial, timeout, exchange or
//!   persist failure) applies `ConnEvent::ConnectFailed`; only an established service goes amber;
//! - a precondition problem (missing OAuth client config, listener bind failure) leaves the
//!   service Disconnected, so the Connect button itself stays as the retry affordance.
//!
//! Nothing here ever carries a token, code, or verifier — reasons are content-free strings.

use std::fmt;

use shogun_mcp::scope::Service;

/// Env var naming the Google OAuth "Desktop app" client id (docs/oauth-client-setup.md §1-6).
pub const GOOGLE_CLIENT_ID_ENV: &str = "SHOGUN_GOOGLE_CLIENT_ID";
/// Env var naming the Google OAuth client secret (a Desktop-app secret is non-confidential but
/// still never committed; optional for a pure-PKCE client).
pub const GOOGLE_CLIENT_SECRET_ENV: &str = "SHOGUN_GOOGLE_CLIENT_SECRET";
/// Services with a concrete desktop setup path. Provider-specific connection probes, not the
/// legacy developer opt-in list, establish whether a user's account can be marked Connected.
pub fn transport_serves(service: Service) -> bool {
    service != Service::Web
}

/// Why an interactive OAuth connect attempt did not end in a connected service. Reasons are short
/// and content-free (no token, code, or verifier ever appears here — invariant 7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectError {
    /// No OAuth client is configured for this service (env vars absent). A setup problem, not a
    /// failed attempt — the service stays Disconnected.
    MissingClientConfig { service: &'static str },
    /// The loopback listener could not be bound / used.
    ListenerBind(String),
    /// The system browser could not be opened for the consent page.
    BrowserOpen(String),
    /// The user declined consent on the provider page (`error=access_denied` redirect).
    Denied,
    /// A later disconnect, client rotation or successful connect superseded this flow.
    Cancelled,
    /// The plan stopped covering connected reads while the consent page was open (#940). Not a
    /// failed sign-in: the row keeps its state and the webview points at plans.
    PlanRequired,
    /// The shared connection state is busy; retry without changing the grant.
    Busy,
    /// No redirect arrived before the deadline (browser closed / user walked away).
    Timeout,
    /// The redirect was malformed or failed the anti-CSRF state check.
    BadRedirect(String),
    /// The code→token exchange (or token-response parse) failed.
    Exchange(String),
    /// Tokens were obtained but could not be persisted to the Keychain — treated as a failed
    /// attempt (nothing was stored; there is no half-connected state).
    Persist(String),
    /// An internal step failed (entropy, URL building). Should not happen in practice.
    Internal(String),
}

impl ConnectError {
    /// Whether this failure should turn the service amber (`ConnEvent::ConnectFailed`,
    /// FR-INT-06) — true for a real attempt that failed mid-flight. False for precondition
    /// problems, which leave the service Disconnected with the Connect button as the retry path.
    pub fn marks_amber(&self) -> bool {
        !matches!(
            self,
            ConnectError::MissingClientConfig { .. }
                | ConnectError::ListenerBind(_)
                | ConnectError::Cancelled
                | ConnectError::PlanRequired
                | ConnectError::Busy
        )
    }

    /// Whether this failure turns the service amber, given whether it was Connected when the
    /// attempt started. A Refresh of a working connection never stores anything until the new
    /// grant is verified, so a failed refresh (closed tab, timeout, declined consent, a failed
    /// probe) says nothing about the stored grant: the working connection stays Connected and
    /// the read-sync remains the judge of whether that grant still works.
    pub fn marks_amber_after(&self, was_connected: bool) -> bool {
        self.marks_amber() && !was_connected
    }

    /// Whether a failed browser flow suggests a reused dynamic client registration is no longer
    /// valid, so it should be dropped and registered again on the next Connect. A provider that
    /// does not know the client shows an error page and never redirects (timeout), redirects with
    /// a provider error ([`ConnectError::BadRedirect`]), or refuses the code exchange. The user
    /// declining consent, a transient network fault during the exchange, or a local problem
    /// (browser, listener) keeps the registration.
    pub fn invalidates_registration(&self) -> bool {
        match self {
            Self::Timeout | Self::BadRedirect(_) => true,
            Self::Exchange(reason) => !crate::token::is_transient_token_failure(reason),
            _ => false,
        }
    }

    /// Content-free class: logged, and the code a failed Connect hands the webview (see
    /// [`connect_failure_code`]). Never includes provider or OS diagnostics.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::MissingClientConfig { .. } => "missing_client_config",
            Self::ListenerBind(_) => "listener_bind",
            Self::BrowserOpen(_) => "browser_open",
            Self::Denied => "denied",
            Self::Cancelled => "cancelled",
            Self::PlanRequired => "plan_required",
            Self::Busy => "connections_busy",
            Self::Timeout => "timeout",
            Self::BadRedirect(_) => "bad_redirect",
            Self::Exchange(_) => "exchange",
            Self::Persist(_) => "persist",
            Self::Internal(_) => "internal",
        }
    }

    /// Stable product copy for the webview. Variant payloads can carry native or provider
    /// diagnostics, so this deliberately ignores them.
    pub fn public_message(&self) -> &'static str {
        match self {
            Self::MissingClientConfig { .. } => {
                "Connection setup is incomplete. Add the sign-in client in Settings and try again."
            }
            Self::ListenerBind(_) => {
                "Couldn't start sign-in on this Mac. Close other sign-in windows and try again."
            }
            Self::BrowserOpen(_) => "Couldn't open sign-in. Try again.",
            Self::Denied => "Sign-in was declined. Try again when you're ready.",
            Self::Cancelled => {
                "This sign-in was cancelled by a connection change. Start Connect again when ready."
            }
            // The stable #940 code: the webview words it and links to plans.
            Self::PlanRequired => shogun_agents::entitlement::PLAN_REQUIRED,
            Self::Busy => "connections_busy",
            Self::Timeout => "Sign-in timed out. Try again.",
            Self::BadRedirect(_) | Self::Exchange(_) | Self::Internal(_) => {
                "Couldn't finish sign-in. Try again."
            }
            Self::Persist(_) => "Signed in, but couldn't save the connection securely. Try again.",
        }
    }
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.public_message())
    }
}

/// Every code [`ConnectError::kind`] returns.
pub const CONNECT_ERROR_KINDS: [&str; 12] = [
    "missing_client_config",
    "listener_bind",
    "browser_open",
    "denied",
    "cancelled",
    "plan_required",
    "connections_busy",
    "timeout",
    "bad_redirect",
    "exchange",
    "persist",
    "internal",
];

/// A GitHub or Linear Connect without app sign-in found no saved personal access token either.
pub const TOKEN_MISSING: &str = "token_missing";

/// The stable code a failed Connect hands the webview, which words it per service. A
/// [`ConnectError::kind`] (including `plan_required`) or [`TOKEN_MISSING`] passes through; any
/// other text — older prose, an OS or provider message — becomes `internal`, so no diagnostic text
/// crosses the IPC boundary and the webview shows its generic copy.
pub fn connect_failure_code(error: &str) -> &'static str {
    let error = error.trim();
    if error == TOKEN_MISSING {
        return TOKEN_MISSING;
    }
    CONNECT_ERROR_KINDS
        .iter()
        .copied()
        .find(|kind| *kind == error)
        .unwrap_or("internal")
}

/// True when a [`crate::oauth::parse_redirect`] error means the user denied consent (the provider
/// redirected back with `error=…`), as opposed to a stray or malformed request (e.g. the browser's
/// favicon probe hitting the loopback port) that the listener should answer and keep waiting past.
pub fn redirect_error_is_denial(err: &str) -> bool {
    err.starts_with("authorization denied")
}

/// True when a provider error redirect is the user declining consent (`error=access_denied`).
/// Any other provider error code (`invalid_client`, `unauthorized_client`, `server_error`, ...)
/// is the provider refusing the request, not the user saying no.
pub fn redirect_error_is_user_denial(err: &str) -> bool {
    err == "authorization denied: access_denied"
}

/// The commit fence every interactive connect applies after consent and before it saves a grant
/// or marks the service Connected (under the runtime lock). The loser of a concurrent flow never
/// saves: a disconnect (UI or `connections.disconnect`), a client change or another successful
/// connect since the flow started changes `current_revision`; a disconnect request still pending
/// cancels too. A plan that stopped covering connected reads is [`ConnectError::PlanRequired`].
pub fn commit_fence(
    started_revision: u64,
    current_revision: u64,
    disconnect_pending: bool,
    reads_entitled: bool,
) -> Result<(), ConnectError> {
    if current_revision != started_revision || disconnect_pending {
        return Err(ConnectError::Cancelled);
    }
    if !reads_entitled {
        return Err(ConnectError::PlanRequired);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn published_transport_serves_every_connector_but_web() {
        for service in [Service::Gmail, Service::GoogleCalendar, Service::GoogleDrive,
            Service::Slack, Service::Notion, Service::GitHub, Service::Linear] {
            assert!(transport_serves(service));
        }
        assert!(!transport_serves(Service::Web));
    }

    #[test]
    fn amber_mapping_matches_the_state_machine_contract() {
        // Real failed attempts → amber (ConnEvent::ConnectFailed upstream).
        assert!(ConnectError::Denied.marks_amber());
        assert!(!ConnectError::Cancelled.marks_amber());
        assert!(ConnectError::Timeout.marks_amber());
        assert!(ConnectError::BrowserOpen("x".into()).marks_amber());
        assert!(ConnectError::BadRedirect("x".into()).marks_amber());
        assert!(ConnectError::Exchange("x".into()).marks_amber());
        assert!(ConnectError::Persist("x".into()).marks_amber());
        assert!(ConnectError::Internal("x".into()).marks_amber());
        // Precondition problems → stay Disconnected (Connect button is the retry affordance).
        assert!(!ConnectError::MissingClientConfig { service: "gcal" }.marks_amber());
        assert!(!ConnectError::ListenerBind("x".into()).marks_amber());
        assert!(!ConnectError::PlanRequired.marks_amber());
        assert!(!ConnectError::Busy.marks_amber());
    }

    #[test]
    fn failed_refresh_keeps_a_working_connection() {
        for e in [ConnectError::Timeout, ConnectError::Denied, ConnectError::BrowserOpen("x".into()),
            ConnectError::Exchange("x".into()), ConnectError::Persist("x".into())] {
            assert!(!e.marks_amber_after(true), "{e:?} demoted a working connection");
            assert!(e.marks_amber_after(false), "{e:?} records an attempted sign-in failure");
        }
        assert!(!ConnectError::PlanRequired.marks_amber_after(false));
        assert!(!ConnectError::Cancelled.marks_amber_after(false));
    }

    #[test]
    fn commit_fence_rejects_the_loser_and_points_at_plans() {
        assert_eq!(commit_fence(7, 7, false, true), Ok(()));
        // A disconnect, client change or another connect moved the revision.
        assert_eq!(commit_fence(7, 8, false, true), Err(ConnectError::Cancelled));
        // A disconnect request not yet applied by the watcher.
        assert_eq!(commit_fence(7, 7, true, true), Err(ConnectError::Cancelled));
        // Cancellation wins over the plan: the loser is not told to buy a plan.
        assert_eq!(commit_fence(7, 8, false, false), Err(ConnectError::Cancelled));
        assert_eq!(commit_fence(7, 7, false, false), Err(ConnectError::PlanRequired));
    }

    #[test]
    fn every_kind_is_a_listed_code_and_passes_through() {
        let all = [
            ConnectError::MissingClientConfig { service: "slack" },
            ConnectError::ListenerBind("addr in use".into()),
            ConnectError::BrowserOpen("x".into()),
            ConnectError::Denied,
            ConnectError::Cancelled,
            ConnectError::PlanRequired,
            ConnectError::Busy,
            ConnectError::Timeout,
            ConnectError::BadRedirect("x".into()),
            ConnectError::Exchange("token endpoint http 400".into()),
            ConnectError::Persist("Keychain status -25308".into()),
            ConnectError::Internal("x".into()),
        ];
        // No wildcard: a new variant stops this from compiling until it is added to `all` above
        // and to CONNECT_ERROR_KINDS, so a new code can never silently become `internal`.
        for e in &all {
            match e {
                ConnectError::MissingClientConfig { .. }
                | ConnectError::ListenerBind(_)
                | ConnectError::BrowserOpen(_)
                | ConnectError::Denied
                | ConnectError::Cancelled
                | ConnectError::PlanRequired
                | ConnectError::Busy
                | ConnectError::Timeout
                | ConnectError::BadRedirect(_)
                | ConnectError::Exchange(_)
                | ConnectError::Persist(_)
                | ConnectError::Internal(_) => {}
            }
        }
        let kinds: Vec<&str> = all.iter().map(ConnectError::kind).collect();
        assert_eq!(kinds, CONNECT_ERROR_KINDS.to_vec());
        for kind in kinds {
            assert_eq!(connect_failure_code(kind), kind);
        }
        assert_eq!(connect_failure_code(TOKEN_MISSING), TOKEN_MISSING);
        assert_eq!(connect_failure_code(shogun_agents::entitlement::PLAN_REQUIRED), "plan_required");
    }

    #[test]
    fn any_other_connect_failure_text_becomes_internal() {
        for prose in [
            "Configure your registered Slack app in Settings first.",
            "Connections are unavailable right now.",
            "GitHub device authorization was denied",
            "token endpoint http 400 Bad Request: {\"error\":\"invalid_grant\"}",
            "private-content@example.com",
            "",
            "Denied",
        ] {
            assert_eq!(connect_failure_code(prose), "internal", "{prose}");
        }
    }

    #[test]
    fn plan_required_is_the_stable_plan_code() {
        assert_eq!(ConnectError::PlanRequired.public_message(), shogun_agents::entitlement::PLAN_REQUIRED);
        assert_eq!(ConnectError::PlanRequired.kind(), "plan_required");
    }

    #[test]
    fn only_registration_shaped_failures_drop_a_reused_client() {
        assert!(ConnectError::Timeout.invalidates_registration());
        assert!(ConnectError::BadRedirect("provider error".into()).invalidates_registration());
        assert!(ConnectError::Exchange("token endpoint http 400 Bad Request".into()).invalidates_registration());
        assert!(ConnectError::Exchange("token endpoint http 401 Unauthorized".into()).invalidates_registration());
        assert!(!ConnectError::Denied.invalidates_registration());
        assert!(!ConnectError::Exchange("token exchange failed: error sending request".into()).invalidates_registration());
        assert!(!ConnectError::Exchange("token endpoint http 503 Service Unavailable".into()).invalidates_registration());
        assert!(!ConnectError::BrowserOpen("x".into()).invalidates_registration());
        assert!(!ConnectError::ListenerBind("x".into()).invalidates_registration());
        assert!(!ConnectError::Cancelled.invalidates_registration());
    }

    #[test]
    fn public_message_never_includes_diagnostic_payload() {
        for e in [
            ConnectError::ListenerBind("addr in use".into()),
            ConnectError::Exchange("token endpoint http 400".into()),
            ConnectError::Persist("Keychain status -25308".into()),
            ConnectError::Internal("private-content@example.com".into()),
        ] {
            let m = e.to_string();
            assert!(
                !m.contains("addr in use")
                    && !m.contains("http 400")
                    && !m.contains("-25308")
                    && !m.contains("example.com")
            );
        }
    }

    #[test]
    fn denial_classification_pins_parse_redirect_output() {
        // Tie the classifier to the real parse_redirect error strings, so they cannot drift apart.
        let denied =
            crate::oauth::parse_redirect("GET /callback?error=access_denied HTTP/1.1").unwrap_err();
        assert!(redirect_error_is_denial(&denied));
        let stray = crate::oauth::parse_redirect("GET /favicon.ico HTTP/1.1").unwrap_err();
        assert!(!redirect_error_is_denial(&stray));
        assert!(redirect_error_is_user_denial(&denied));
        let refused = crate::oauth::parse_redirect("GET /callback?error=invalid_client&state=s HTTP/1.1").unwrap_err();
        assert!(redirect_error_is_denial(&refused));
        assert!(!redirect_error_is_user_denial(&refused));
    }
}
