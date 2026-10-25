//! Hosted connector credentials on the sidecar (ShogunAgent E50): the internal routes the gateway
//! (`apps/api`) calls to get a principal's connector access token for one router run, and to run
//! the hosted connect flow. Contract: `docs/api/connector-tokens.md`.
//!
//! | Route | Body | Answer |
//! |---|---|---|
//! | `POST /v1/internal/connectors/token` | `{"slot"}` | `200 {access_token, token_type, expires_at}` |
//! | `POST /v1/internal/connectors/connect/start` | `{"provider", "state"}` | `200 {authorize_url, expires_at}` |
//! | `POST /v1/internal/connectors/connect/finish` | `{"state", "code"}` | `200 {connected, provider, slot}` |
//!
//! Who may call: the operator's admin credential in `Authorization` (the gateway holds it as
//! `SHOGUN_SIDECAR_ADMIN_TOKEN`) **and** the principal's own sidecar bearer in
//! [`PRINCIPAL_HEADER`] (the gateway's derived `sgw1` bearer, or a listed token). The principal
//! comes from that bearer, never from the body, so the gateway cannot name another principal and
//! an agent's bearer alone reaches nothing here. The server checks both before this module runs
//! (`server_governance.rs`); this module only parses the body, calls the [`ConnectorVault`] and
//! shapes the answer.
//!
//! The vault itself (open the principal's token store, refresh through the core's single egress
//! under a per-row lock, the PKCE flow) is `shogun_integrations::hosted_connect`, composed with the
//! core's HTTP token exchange in `shogun-api --multi`. Every answer is `Cache-Control: no-store`;
//! nothing here logs or keeps a value. The access token is the only secret that leaves, and only in
//! the one response body (`.expose(` is allowlisted here for that, `scripts/check-secret-exposure.py`).

use serde_json::{json, Value};
use shogun_redact::Secret;

/// The token route.
pub const TOKEN_PATH: &str = "/v1/internal/connectors/token";
/// The connect flow's start (the PKCE pair and the authorize URL).
pub const CONNECT_START_PATH: &str = "/v1/internal/connectors/connect/start";
/// The connect flow's finish (the code exchange and the write into the principal's store).
pub const CONNECT_FINISH_PATH: &str = "/v1/internal/connectors/connect/finish";
/// The path prefix every connector route shares.
pub const PREFIX: &str = "/v1/internal/connectors/";
/// The header carrying the principal's sidecar bearer (`Bearer sgw1.…` or a listed token).
pub const PRINCIPAL_HEADER: &str = "x-shogun-principal-authorization";

/// The largest body any of these routes reads.
pub const MAX_BODY_BYTES: usize = 8 * 1024;

/// An access token the vault hands out. The value stays a [`Secret`] until the response body.
pub struct IssuedToken {
    pub access_token: Secret,
    /// Unix ms. `i64::MAX` for a grant that does not expire (a Slack token without rotation).
    pub expires_at_ms: i64,
}

/// The hosted connect flow has started: send the human to `authorize_url`.
pub struct ConnectStarted {
    pub authorize_url: String,
    pub expires_at_ms: i64,
}

/// The flow finished: the tokenset is in the principal's store under `slot`.
pub struct Connected {
    pub provider: &'static str,
    pub slot: String,
}

/// Why a vault call did not answer with a value. Fixed words only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaultError {
    /// No grant for the slot, a grant the provider revoked, or one that expired with no way to
    /// refresh it: the human connects (again).
    NotConnected,
    /// The store is locked or unreadable, or the provider did not answer a refresh while the stored
    /// token has expired: retry shortly.
    Unavailable,
    /// The slot or provider is not one this vault serves.
    UnknownSlot,
    /// The provider is known but cannot be connected here (no OAuth client configured, or the
    /// core has no web flow for it).
    NotConfigured,
    /// No pending flow for this state (never started, already used, expired, another principal's).
    FlowUnknown,
    /// The provider refused the code, did not grant the scopes, or answered a shape the core does
    /// not accept.
    ProviderRejected,
    /// A malformed field (state, code).
    Invalid(&'static str),
}

/// The principal's connector credentials, as the sidecar serves them.
pub trait ConnectorVault: Send + Sync {
    /// A currently valid access token for `principal`'s `slot` (`gmail-tokenset`, …), refreshed
    /// first when it expires within 60 seconds.
    fn access_token(&self, principal: &str, slot: &str, now_ms: i64) -> Result<IssuedToken, VaultError>;
    /// Start the authorization-code + PKCE flow for `provider` (`gmail`, `calendar`, `slack`,
    /// `linear`) with the gateway's opaque `state`.
    fn connect_start(&self, principal: &str, provider: &str, state: &str, now_ms: i64) -> Result<ConnectStarted, VaultError>;
    /// Finish the flow `state` names with the provider's `code`: exchange it and store the
    /// tokenset for `principal`. Single use.
    fn connect_finish(&self, principal: &str, state: &str, code: &str, now_ms: i64) -> Result<Connected, VaultError>;
}

fn refusal(status: u16, code: &str, message: &str, next_action: Option<&str>) -> (u16, String) {
    (status, json!({ "error": code, "message": message, "next_action": next_action }).to_string())
}

fn error_body(error: VaultError) -> (u16, String) {
    match error {
        VaultError::NotConnected => refusal(404, "connector_not_connected", "No usable account is connected for this connector. A human connects it, then the run is retried.", Some("authenticate")),
        VaultError::Unavailable => refusal(503, "connector_unavailable", "The credential store or the provider did not answer. Retry shortly.", Some("retry_after")),
        VaultError::UnknownSlot => refusal(400, "unknown_connector", "This connector is not one the core serves.", Some("fix_input")),
        VaultError::NotConfigured => refusal(503, "connect_not_configured", "This connector cannot be connected on this deployment (no OAuth client configured). Ask the operator.", Some("contact_operator")),
        VaultError::FlowUnknown => refusal(404, "connect_flow_unknown", "This sign-in is unknown, already used or expired. Start again.", Some("restart")),
        VaultError::ProviderRejected => refusal(400, "connect_rejected", "The provider refused the sign-in or did not grant every permission the connector needs. Start again and grant them.", Some("restart")),
        VaultError::Invalid(what) => refusal(400, "invalid_request", &format!("The {what} is missing or malformed."), Some("fix_input")),
    }
}

/// The JSON-safe upper bound of an expiry (the gateway reads it as a JavaScript number).
const MAX_SAFE_MS: i64 = (1_i64 << 53) - 1;

fn parse_object(body: Option<&str>, fields: &[&str]) -> Result<serde_json::Map<String, Value>, VaultError> {
    let value: Value = serde_json::from_str(body.unwrap_or("")).map_err(|_| VaultError::Invalid("body"))?;
    let Value::Object(map) = value else {
        return Err(VaultError::Invalid("body"));
    };
    if map.keys().any(|key| !fields.contains(&key.as_str())) {
        return Err(VaultError::Invalid("body"));
    }
    Ok(map)
}

fn field<'a>(map: &'a serde_json::Map<String, Value>, name: &'static str) -> Result<&'a str, VaultError> {
    map.get(name).and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or(VaultError::Invalid(name))
}

/// Answer one connector route for an already authenticated `principal`, or `None` when `path` is
/// not one of them. Blocking (a refresh or a code exchange is a network call): the server runs it
/// off the async workers.
pub fn respond(vault: &dyn ConnectorVault, path: &str, principal: &str, body: Option<&str>, now_ms: i64) -> Option<(u16, String)> {
    let answer = match path {
        TOKEN_PATH => parse_object(body, &["slot"]).and_then(|map| {
            let issued = vault.access_token(principal, field(&map, "slot")?, now_ms)?;
            Ok(json!({
                "access_token": issued.access_token.expose(),
                "token_type": "Bearer",
                "expires_at": issued.expires_at_ms.clamp(0, MAX_SAFE_MS),
            })
            .to_string())
        }),
        CONNECT_START_PATH => parse_object(body, &["provider", "state"]).and_then(|map| {
            let started = vault.connect_start(principal, field(&map, "provider")?, field(&map, "state")?, now_ms)?;
            Ok(json!({ "authorize_url": started.authorize_url, "expires_at": started.expires_at_ms.clamp(0, MAX_SAFE_MS) }).to_string())
        }),
        CONNECT_FINISH_PATH => parse_object(body, &["state", "code"]).and_then(|map| {
            let done = vault.connect_finish(principal, field(&map, "state")?, field(&map, "code")?, now_ms)?;
            Ok(json!({ "connected": true, "provider": done.provider, "slot": done.slot }).to_string())
        }),
        _ => return None,
    };
    Some(match answer {
        Ok(body) => (200, body),
        Err(error) => error_body(error),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Fake {
        calls: Mutex<Vec<String>>,
    }

    impl ConnectorVault for Fake {
        fn access_token(&self, principal: &str, slot: &str, _now: i64) -> Result<IssuedToken, VaultError> {
            self.calls.lock().unwrap().push(format!("token {principal} {slot}"));
            match slot {
                "gmail-tokenset" => Ok(IssuedToken { access_token: Secret::new("ya29.sentinel"), expires_at_ms: i64::MAX }),
                "slack-tokenset" => Err(VaultError::NotConnected),
                _ => Err(VaultError::UnknownSlot),
            }
        }
        fn connect_start(&self, principal: &str, provider: &str, state: &str, _now: i64) -> Result<ConnectStarted, VaultError> {
            self.calls.lock().unwrap().push(format!("start {principal} {provider} {state}"));
            Ok(ConnectStarted { authorize_url: "https://accounts.example/auth?state=s".into(), expires_at_ms: 5 })
        }
        fn connect_finish(&self, _principal: &str, _state: &str, _code: &str, _now: i64) -> Result<Connected, VaultError> {
            Err(VaultError::FlowUnknown)
        }
    }

    #[test]
    fn routes_shape_answers_and_take_the_principal_from_the_caller() {
        let vault = Fake::default();
        let (status, body) = respond(&vault, TOKEN_PATH, "prn_a", Some(r#"{"slot":"gmail-tokenset"}"#), 0).unwrap();
        assert_eq!(status, 200);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v, json!({ "access_token": "ya29.sentinel", "token_type": "Bearer", "expires_at": MAX_SAFE_MS }));

        let (status, body) = respond(&vault, TOKEN_PATH, "prn_a", Some(r#"{"slot":"slack-tokenset"}"#), 0).unwrap();
        assert_eq!(status, 404);
        assert!(body.contains("connector_not_connected"));

        // A body that names a principal is refused before the vault runs.
        let (status, _) = respond(&vault, TOKEN_PATH, "prn_a", Some(r#"{"slot":"gmail-tokenset","principal":"prn_b"}"#), 0).unwrap();
        assert_eq!(status, 400);
        for bad in [None, Some("[]"), Some("{"), Some(r#"{"slot":""}"#), Some(r#"{"slot":3}"#)] {
            assert_eq!(respond(&vault, TOKEN_PATH, "prn_a", bad, 0).unwrap().0, 400, "{bad:?}");
        }

        let (status, body) = respond(&vault, CONNECT_START_PATH, "prn_a", Some(r#"{"provider":"gmail","state":"st"}"#), 0).unwrap();
        assert_eq!(status, 200);
        assert!(body.contains("authorize_url"));
        assert_eq!(respond(&vault, CONNECT_FINISH_PATH, "prn_a", Some(r#"{"state":"st","code":"c"}"#), 0).unwrap().0, 404);
        assert!(respond(&vault, "/v1/internal/connectors/other", "prn_a", None, 0).is_none());
        assert_eq!(
            *vault.calls.lock().unwrap(),
            vec!["token prn_a gmail-tokenset", "token prn_a slack-tokenset", "start prn_a gmail st"]
        );
    }

    #[test]
    fn error_bodies_are_fixed_words() {
        for error in [
            VaultError::NotConnected,
            VaultError::Unavailable,
            VaultError::UnknownSlot,
            VaultError::NotConfigured,
            VaultError::FlowUnknown,
            VaultError::ProviderRejected,
            VaultError::Invalid("code"),
        ] {
            let (status, body) = error_body(error);
            assert!((400..600).contains(&status));
            let v: Value = serde_json::from_str(&body).unwrap();
            assert!(v["error"].is_string() && v["message"].is_string());
        }
    }
}
