//! Hosted connector credentials on the sidecar (ShogunAgent E50): the internal routes the gateway
//! (`apps/api`) calls to get a principal's connector access token for one router run, to run the
//! hosted connect flow, and to read and remove the principal's connections. Contract:
//! `docs/api/connector-tokens.md`.
//!
//! | Route | Body | Answer |
//! |---|---|---|
//! | `POST /v1/internal/connectors/token` | `{"slot"}` | `200 {access_token, token_type, expires_at}` |
//! | `POST /v1/internal/connectors/connect/start` | `{"provider", "state"}` | `200 {authorize_url, expires_at, device?}` |
//! | `POST /v1/internal/connectors/connect/finish` | `{"state", "code"}` | `200 {connected, provider, slot}` |
//! | `POST /v1/internal/connectors/connect/poll` | `{"state"}` | `200 {status: "pending", device, expires_at}` or `{connected, provider, slot}` |
//! | `POST /v1/internal/connectors/status` | `{}` | `200 {connectors: [...]}` |
//! | `POST /v1/internal/connectors/disconnect` | `{"provider"}` | `200 {disconnected, provider, slot, removed, vendor_revoke}` |
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
//! under a per-row lock, the PKCE and device flows, status, disconnect with the vendor's
//! revocation, the read-sync queue) is `shogun_integrations::hosted_connect`, composed with the
//! core's HTTP token exchange in `shogun-api --multi`. The same vault serves `connections.list` and
//! `connections.disconnect` on the principal's MCP, REST and CLI faces (`db_backend.rs`). Every
//! answer is `Cache-Control: no-store`; nothing here logs or keeps a value. The access token is the
//! only secret that leaves, and only in the one response body (`.expose(` is allowlisted here for
//! that, `scripts/check-secret-exposure.py`).

use serde_json::{json, Value};
use shogun_redact::Secret;

use crate::scope::Service;

/// The token route.
pub const TOKEN_PATH: &str = "/v1/internal/connectors/token";
/// The connect flow's start (the PKCE pair and the authorize URL, or GitHub's device code).
pub const CONNECT_START_PATH: &str = "/v1/internal/connectors/connect/start";
/// The connect flow's finish (the code exchange and the write into the principal's store).
pub const CONNECT_FINISH_PATH: &str = "/v1/internal/connectors/connect/finish";
/// One poll of a device flow (GitHub): the human pressed "Check" after entering the code.
pub const CONNECT_POLL_PATH: &str = "/v1/internal/connectors/connect/poll";
/// The principal's connections (free read).
pub const STATUS_PATH: &str = "/v1/internal/connectors/status";
/// Remove one connection (narrowing; the vendor grant is revoked where the vendor has an API).
pub const DISCONNECT_PATH: &str = "/v1/internal/connectors/disconnect";
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

/// What a human does for a device flow: open `verification_uri` and type `user_code`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceStep {
    pub user_code: String,
    pub verification_uri: String,
    /// The least time between two polls.
    pub interval_ms: i64,
}

/// The hosted connect flow has started: send the human to `authorize_url` (for a device flow,
/// the verification page, with `device` naming the code to type there).
pub struct ConnectStarted {
    pub authorize_url: String,
    pub expires_at_ms: i64,
    pub device: Option<DeviceStep>,
}

/// The flow finished: the tokenset is in the principal's store under `slot`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connected {
    pub provider: &'static str,
    pub slot: String,
}

/// One poll of a device flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectPoll {
    /// The human has not approved yet (or it is too early to ask again).
    Pending { device: DeviceStep, expires_at_ms: i64 },
    Connected(Connected),
}

/// One connection of a principal, as `connections.list` and the status route show it. Never a
/// token or an account identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectorState {
    /// The router's provider id (`gmail`, `calendar`, …), `None` for a service the hosted
    /// deployment does not serve.
    pub provider: Option<&'static str>,
    pub service: Service,
    /// `connected`, `needs_reauth`, `disconnected` or `coming_soon` (the Connections screen's
    /// words, `connections_api::UI_STATES`).
    pub state: &'static str,
    /// Whether a human can connect it on this deployment now.
    pub connectable: bool,
    /// When the stored access token expires (unix ms), when one is stored.
    pub expires_at_ms: Option<i64>,
}

/// What a disconnect did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disconnected {
    pub provider: &'static str,
    pub slot: String,
    /// Whether a grant was stored (and is now deleted).
    pub removed: bool,
    /// `revoked`, `failed` (the grant is deleted here anyway), `not_supported` (the vendor has no
    /// revocation API the core can call, or the core lacks the credential it needs: GitHub's app
    /// secret) or `nothing_stored`. Every face's `connections.disconnect` returns it (E51).
    pub vendor_revoke: &'static str,
}

/// A change notice queued for the read-sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncQueued {
    /// False when an identical notice was already waiting (coalesced).
    pub added: bool,
    /// Notices waiting for this principal after this one.
    pub depth: usize,
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
    /// not accept; for a device flow, the human denied it or let it expire.
    ProviderRejected,
    /// A malformed field (state, code).
    Invalid(&'static str),
}

/// The principal's connector credentials, as the sidecar serves them.
pub trait ConnectorVault: Send + Sync {
    /// A currently valid access token for `principal`'s `slot` (`gmail-tokenset`, …), refreshed
    /// first when it expires within 60 seconds.
    fn access_token(&self, principal: &str, slot: &str, now_ms: i64) -> Result<IssuedToken, VaultError>;
    /// Start the connect flow for `provider` (`gmail`, `calendar`, `slack`, `linear`: the
    /// authorization-code + PKCE flow; `github`: the device flow) with the gateway's opaque `state`.
    fn connect_start(&self, principal: &str, provider: &str, state: &str, now_ms: i64) -> Result<ConnectStarted, VaultError>;
    /// Finish the flow `state` names with the provider's `code`: exchange it and store the
    /// tokenset for `principal`. Single use.
    fn connect_finish(&self, principal: &str, state: &str, code: &str, now_ms: i64) -> Result<Connected, VaultError>;
    /// Poll the device flow `state` names once (at most once per interval reaches the vendor).
    fn connect_poll(&self, _principal: &str, _state: &str, _now_ms: i64) -> Result<ConnectPoll, VaultError> {
        Err(VaultError::NotConfigured)
    }
    /// Every connection the principal could have, with its state. A free read.
    fn status(&self, _principal: &str, _now_ms: i64) -> Result<Vec<ConnectorState>, VaultError> {
        Err(VaultError::NotConfigured)
    }
    /// Remove `service`'s grant: revoke it at the vendor where the core can, delete it from the
    /// store either way. Only narrows access, so any face may ask.
    fn disconnect(&self, _principal: &str, _service: Service, _now_ms: i64) -> Result<Disconnected, VaultError> {
        Err(VaultError::NotConfigured)
    }
    /// Queue a provider change notice (`provider` as `memory.ingest_webhook` names it, `detail` the
    /// ids it carried) for the background read-sync, which fetches the content with the principal's
    /// own credentials.
    fn enqueue_sync(&self, _principal: &str, _provider: &str, _detail: &Value, _now_ms: i64) -> Result<SyncQueued, VaultError> {
        Err(VaultError::NotConfigured)
    }
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

fn device_json(device: &DeviceStep) -> Value {
    json!({ "user_code": device.user_code, "verification_uri": device.verification_uri, "interval_ms": device.interval_ms })
}

fn connected_json(done: &Connected) -> Value {
    json!({ "connected": true, "provider": done.provider, "slot": done.slot })
}

/// One connection as JSON (the status route and `connections.list` on a hosted sidecar).
pub fn state_json(state: &ConnectorState) -> Value {
    json!({
        "service": state.service.source_str(),
        "provider": state.provider,
        "state": state.state,
        "connected": matches!(state.state, "connected" | "needs_reauth"),
        "connectable": state.connectable,
        "expires_at": state.expires_at_ms.map(|ms| ms.clamp(0, MAX_SAFE_MS)),
    })
}

/// The disconnect answer as JSON.
pub fn disconnected_json(done: &Disconnected) -> Value {
    json!({ "disconnected": true, "provider": done.provider, "slot": done.slot, "removed": done.removed, "vendor_revoke": done.vendor_revoke })
}

/// The service a disconnect names: a router provider id (`gmail`, `calendar`, …) or a
/// Connections service id (`gmail`, `gcal`, …).
pub fn service_of(name: &str) -> Option<Service> {
    match name {
        "calendar" => Some(Service::GoogleCalendar),
        other => crate::scope::from_source(other).filter(|s| *s != Service::Web),
    }
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
            let mut answer = json!({ "authorize_url": started.authorize_url, "expires_at": started.expires_at_ms.clamp(0, MAX_SAFE_MS) });
            if let Some(device) = &started.device {
                answer["device"] = device_json(device);
            }
            Ok(answer.to_string())
        }),
        CONNECT_FINISH_PATH => parse_object(body, &["state", "code"]).and_then(|map| {
            let done = vault.connect_finish(principal, field(&map, "state")?, field(&map, "code")?, now_ms)?;
            Ok(connected_json(&done).to_string())
        }),
        CONNECT_POLL_PATH => parse_object(body, &["state"]).and_then(|map| {
            Ok(match vault.connect_poll(principal, field(&map, "state")?, now_ms)? {
                ConnectPoll::Pending { device, expires_at_ms } => {
                    json!({ "status": "pending", "device": device_json(&device), "expires_at": expires_at_ms.clamp(0, MAX_SAFE_MS) }).to_string()
                }
                ConnectPoll::Connected(done) => connected_json(&done).to_string(),
            })
        }),
        STATUS_PATH => parse_object(body.or(Some("{}")), &[]).and_then(|_| {
            let states = vault.status(principal, now_ms)?;
            Ok(json!({ "connectors": states.iter().map(state_json).collect::<Vec<_>>() }).to_string())
        }),
        DISCONNECT_PATH => parse_object(body, &["provider"]).and_then(|map| {
            let service = service_of(field(&map, "provider")?).ok_or(VaultError::UnknownSlot)?;
            Ok(disconnected_json(&vault.disconnect(principal, service, now_ms)?).to_string())
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
            Ok(ConnectStarted { authorize_url: "https://accounts.example/auth?state=s".into(), expires_at_ms: 5, device: None })
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

    /// A vault that serves the E50 follow-up routes (status, disconnect, poll).
    struct Hosted;

    impl ConnectorVault for Hosted {
        fn access_token(&self, _: &str, _: &str, _: i64) -> Result<IssuedToken, VaultError> {
            Err(VaultError::NotConnected)
        }
        fn connect_start(&self, _: &str, provider: &str, _: &str, _: i64) -> Result<ConnectStarted, VaultError> {
            assert_eq!(provider, "github");
            Ok(ConnectStarted {
                authorize_url: "https://github.com/login/device".into(),
                expires_at_ms: 900_000,
                device: Some(DeviceStep { user_code: "ABCD-1234".into(), verification_uri: "https://github.com/login/device".into(), interval_ms: 5_000 }),
            })
        }
        fn connect_finish(&self, _: &str, _: &str, _: &str, _: i64) -> Result<Connected, VaultError> {
            Err(VaultError::FlowUnknown)
        }
        fn connect_poll(&self, _: &str, state: &str, _: i64) -> Result<ConnectPoll, VaultError> {
            if state == "done" {
                return Ok(ConnectPoll::Connected(Connected { provider: "github", slot: "github-tokenset".into() }));
            }
            Ok(ConnectPoll::Pending { device: DeviceStep { user_code: "ABCD-1234".into(), verification_uri: "https://github.com/login/device".into(), interval_ms: 5_000 }, expires_at_ms: 9 })
        }
        fn status(&self, _: &str, _: i64) -> Result<Vec<ConnectorState>, VaultError> {
            Ok(vec![ConnectorState { provider: Some("gmail"), service: Service::Gmail, state: "connected", connectable: true, expires_at_ms: Some(7) }])
        }
        fn disconnect(&self, _: &str, service: Service, _: i64) -> Result<Disconnected, VaultError> {
            assert_eq!(service, Service::GoogleCalendar);
            Ok(Disconnected { provider: "calendar", slot: "gcal-tokenset".into(), removed: true, vendor_revoke: "revoked" })
        }
    }

    #[test]
    fn follow_up_routes_poll_list_and_disconnect() {
        let v = |(status, body): (u16, String)| -> (u16, Value) { (status, serde_json::from_str(&body).unwrap()) };
        let (status, body) = v(respond(&Hosted, CONNECT_START_PATH, "prn_a", Some(r#"{"provider":"github","state":"s"}"#), 0).unwrap());
        assert_eq!(status, 200);
        assert_eq!(body["device"]["user_code"], "ABCD-1234");
        let (status, body) = v(respond(&Hosted, CONNECT_POLL_PATH, "prn_a", Some(r#"{"state":"waiting"}"#), 0).unwrap());
        assert_eq!((status, body["status"].as_str()), (200, Some("pending")));
        let (_, body) = v(respond(&Hosted, CONNECT_POLL_PATH, "prn_a", Some(r#"{"state":"done"}"#), 0).unwrap());
        assert_eq!(body, json!({ "connected": true, "provider": "github", "slot": "github-tokenset" }));
        for empty in [None, Some("{}")] {
            let (status, body) = v(respond(&Hosted, STATUS_PATH, "prn_a", empty, 0).unwrap());
            assert_eq!(status, 200);
            assert_eq!(body["connectors"][0], json!({ "service": "gmail", "provider": "gmail", "state": "connected", "connected": true, "connectable": true, "expires_at": 7 }));
        }
        assert_eq!(respond(&Hosted, STATUS_PATH, "prn_a", Some(r#"{"principal":"prn_b"}"#), 0).unwrap().0, 400);
        let (status, body) = v(respond(&Hosted, DISCONNECT_PATH, "prn_a", Some(r#"{"provider":"calendar"}"#), 0).unwrap());
        assert_eq!((status, body["vendor_revoke"].as_str()), (200, Some("revoked")));
        assert_eq!(respond(&Hosted, DISCONNECT_PATH, "prn_a", Some(r#"{"provider":"web"}"#), 0).unwrap().0, 400);
        // A vault without the follow-ups answers not configured, never a guess.
        assert_eq!(respond(&Fake::default(), STATUS_PATH, "prn_a", None, 0).unwrap().0, 503);
        assert_eq!(service_of("gcal"), Some(Service::GoogleCalendar));
        assert_eq!(service_of("calendar"), Some(Service::GoogleCalendar));
    }
}
