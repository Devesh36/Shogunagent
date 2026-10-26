//! Connection state and non-secret mail policy shared by desktop, MCP, REST and CLI.
//! Direct Gmail uses `gmail-policy.json`. API writers may enable draft-only mode but only
//! a human in Settings may disable it. Credentials never enter this module.
//! Legacy Composio policy remains separately editable for compatibility and cannot revoke
//! or change a direct Gmail connection.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::composio_policy::{self, ComposioPolicy};
use crate::scope::{self, Service, ALL_SERVICES};

/// The desktop's published connection states, beside the DB.
pub const STATUS_FILE: &str = "connections_status.json";
/// Disconnect requests from the API faces, beside the DB, applied by the desktop.
pub const REQUESTS_FILE: &str = "connections_requests.json";
/// How old a status snapshot may be and still describe a running app. The desktop rewrites it
/// every two seconds.
pub const STATUS_MAX_AGE_MS: u64 = 6_000;
/// The states the Connections screen shows, as the desktop's `ConnUi` serializes them.
pub const UI_STATES: [&str; 5] = ["connected", "needs_reauth", "keychain_locked", "disconnected", "coming_soon"];

/// Where the three shared files are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionsPaths {
    /// `composio.json` (the Mail policy).
    pub composio: PathBuf,
    /// Direct Gmail sending policy, independent of legacy Composio consent.
    pub gmail: PathBuf,
    /// [`STATUS_FILE`].
    pub status: PathBuf,
    /// [`REQUESTS_FILE`].
    pub requests: PathBuf,
}

impl ConnectionsPaths {
    /// The daemons' paths: `composio.json` from [`composio_policy::resolve_path`], the other two
    /// beside the DB (where the desktop keeps its heartbeat).
    pub fn for_db(db_path: &str) -> Self {
        let dir = Path::new(db_path).parent().map(Path::to_path_buf).unwrap_or_default();
        Self {
            composio: composio_policy::resolve_path(db_path),
            gmail: crate::gmail_policy::resolve_path(db_path),
            status: dir.join(STATUS_FILE),
            requests: dir.join(REQUESTS_FILE),
        }
    }
}

/// The services on the Connections list, in its order. Web has no row: its switch is the Web
/// fetch consent card.
pub fn listed_services() -> impl Iterator<Item = Service> {
    ALL_SERVICES.iter().copied().filter(|s| *s != Service::Web)
}

/// All services connect directly to their providers.
pub fn third_party(_service: Service) -> Option<&'static str> {
    None
}

// status snapshot (desktop → faces)

#[derive(serde::Serialize, serde::Deserialize)]
struct Snapshot {
    observed_at_ms: u64,
    services: Vec<SnapshotRow>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SnapshotRow {
    service: String,
    state: String,
}

/// Publish connection states without account identity metadata.
pub fn write_status(path: &Path, now_ms: u64, rows: &[(Service, &str)]) -> Result<(), String> {
    let snapshot = Snapshot {
        observed_at_ms: now_ms,
        services: rows
            .iter()
            .filter(|(_, state)| UI_STATES.contains(state))
            .map(|(service, state)| SnapshotRow {
                service: service.source_str().to_string(),
                state: (*state).to_string(),
            })
            .collect(),
    };
    let bytes = serde_json::to_vec(&snapshot).map_err(|e| e.to_string())?;
    crate::settings_file::write_atomic(path, &bytes, false)
}

/// The published states, if the snapshot is fresh. Missing, corrupt, stale or future data is
/// `None` (unknown), never a claim about a connection. Unknown services and states are dropped.
pub fn read_status(path: &Path, now_ms: u64) -> Option<Vec<(Service, &'static str)>> {
    let snapshot: Snapshot = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let age = now_ms.checked_sub(snapshot.observed_at_ms)?;
    if age > STATUS_MAX_AGE_MS {
        return None;
    }
    Some(
        snapshot
            .services
            .iter()
            .filter_map(|row| {
                let service = listed_services().find(|s| s.source_str() == row.service)?;
                let state = UI_STATES.iter().copied().find(|s| *s == row.state)?;
                Some((service, state))
            })
            .collect(),
    )
}

// disconnect requests (faces → desktop)

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Requests {
    #[serde(default)]
    disconnect: Vec<String>,
}

fn read_requests(path: &Path) -> Requests {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn write_requests(path: &Path, requests: &Requests) -> Result<(), String> {
    let bytes = serde_json::to_vec(requests).map_err(|e| e.to_string())?;
    crate::settings_file::write_atomic(path, &bytes, true)
}

/// Record that `service` should be disconnected. Idempotent. The desktop applies it
/// ([`pending_disconnects`] then [`clear_disconnects`]).
pub fn request_disconnect(path: &Path, service: Service) -> Result<(), String> {
    let _lock = crate::settings_file::lock(path, "connection requests are busy")?;
    let mut requests = read_requests(path);
    let id = service.source_str().to_string();
    if !requests.disconnect.contains(&id) {
        requests.disconnect.push(id);
        write_requests(path, &requests)?;
    }
    Ok(())
}

/// Services waiting to be disconnected by the desktop, in request order. Unknown ids are skipped.
pub fn pending_disconnects(path: &Path) -> Vec<Service> {
    read_requests(path)
        .disconnect
        .iter()
        .filter_map(|id| listed_services().find(|s| s.source_str() == id))
        .collect()
}

/// Drop the requests the desktop has applied. Any other request (one that arrived meanwhile) stays.
pub fn clear_disconnects(path: &Path, applied: &[Service]) -> Result<(), String> {
    if applied.is_empty() {
        return Ok(());
    }
    let _lock = crate::settings_file::lock(path, "connection requests are busy")?;
    let mut requests = read_requests(path);
    let before = requests.disconnect.len();
    requests
        .disconnect
        .retain(|id| !applied.iter().any(|s| s.source_str() == id));
    if requests.disconnect.len() != before {
        write_requests(path, &requests)?;
    }
    Ok(())
}

/// Why a `connections.*` write is refused. Every face answers 400 / invalid params.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invalid {
    /// Not a JSON object of the known fields, or nothing to change.
    BadRequest,
    /// Not a service on the Connections list.
    UnknownService,
    /// `consent: true` without consent on file: granted only on the Composio card in Settings → Connections.
    ConsentOnlyInApp,
    /// `draft_stop: false` while draft-only mode is on (or with consent revoked): only in the app.
    DraftStopOffOnlyInApp,
}

impl Invalid {
    pub fn code(self) -> &'static str {
        match self {
            Invalid::BadRequest => "invalid_connections_request",
            Invalid::UnknownService => "unknown_service",
            Invalid::ConsentOnlyInApp => "consent_only_in_app",
            Invalid::DraftStopOffOnlyInApp => "draft_stop_off_only_in_app",
        }
    }

    fn detail(self) -> &'static str {
        match self {
            Invalid::BadRequest => "expected the documented fields",
            Invalid::UnknownService => {
                "service must be one of gmail, gcal, gdrive, slack, notion, github, linear, granola"
            }
            Invalid::ConsentOnlyInApp => {
                "the user grants Mail consent in ShogunAI Settings > Connections, where each of the three disclosures is acknowledged"
            }
            Invalid::DraftStopOffOnlyInApp => {
                "the user turns draft-only mode off in ShogunAI Settings > Connections"
            }
        }
    }
}

/// The refusal text. Starts with "invalid " so the REST face answers 400.
pub fn refusal(invalid: Invalid) -> String {
    format!("invalid connections request: {}: {}", invalid.code(), invalid.detail())
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DisconnectBody {
    service: String,
}

/// One face's `connections.disconnect` answer (E51): its standard write fields (`base`: MCP's
/// `{accepted}`, REST's `{tool, level, accepted}`) with the backend's disconnect receipt
/// (`disconnected`, `provider`, `slot`, `removed`, `vendor_revoke`) added. A receipt key never
/// overwrites a standard field, so the faces differ only in the fields they already differed in.
pub fn disconnect_answer(base: serde_json::Value, receipt: &serde_json::Value) -> String {
    let mut answer = match base {
        serde_json::Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    if let Some(receipt) = receipt.as_object() {
        for (key, value) in receipt {
            answer.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }
    serde_json::Value::Object(answer).to_string()
}

/// The service a `connections.disconnect` body names.
pub fn parse_disconnect(body: &str) -> Result<Service, Invalid> {
    let body: DisconnectBody = serde_json::from_str(body).map_err(|_| Invalid::BadRequest)?;
    let id = body.service.trim();
    listed_services()
        .find(|s| s.source_str() == id)
        .ok_or(Invalid::UnknownService)
}

/// `connections.disconnect` from an API face: record the request for the desktop. It only
/// removes access, so it is direct (L1).
pub fn disconnect(paths: &ConnectionsPaths, body: &str) -> Result<(), String> {
    let service = parse_disconnect(body).map_err(refusal)?;
    request_disconnect(&paths.requests, service)
}

/// A parsed `connections.composio.set` body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposioRequest {
    pub consent: Option<bool>,
    pub draft_stop: Option<bool>,
}

/// Parse a `connections.composio.set` body: a JSON object with `consent` and/or `draft_stop`.
pub fn parse_composio(body: &str) -> Result<ComposioRequest, Invalid> {
    let request: ComposioRequest = serde_json::from_str(body).map_err(|_| Invalid::BadRequest)?;
    if request.consent.is_none() && request.draft_stop.is_none() {
        return Err(Invalid::BadRequest);
    }
    Ok(request)
}

/// The policy `request` leaves in force, or why it is refused. Only tightening applies: revoking
/// consent (which also turns draft-only mode back on, clears the disclosures and disconnects
/// Mail, exactly like Revoke in the app) and turning draft-only mode on. Restating what is already
/// in force is a no-op. Granting consent or turning draft-only mode off is refused.
pub fn plan_composio(
    current: &ComposioPolicy,
    request: ComposioRequest,
) -> Result<ComposioPolicy, Invalid> {
    if request.consent == Some(true) && !current.consent_acknowledged {
        return Err(Invalid::ConsentOnlyInApp);
    }
    let consent = current.consent_acknowledged && request.consent != Some(false);
    if request.draft_stop == Some(false) && (current.draft_stop || !consent) {
        return Err(Invalid::DraftStopOffOnlyInApp);
    }
    let mut next = current.clone();
    next.consent_acknowledged = consent;
    next.draft_stop = !consent || request.draft_stop.unwrap_or(current.draft_stop);
    if !consent {
        next.disclosures = Default::default();
        next.gmail_connected = false;
    }
    debug_assert!(composio_policy::policy_is_valid(next.draft_stop, next.consent_acknowledged));
    Ok(next)
}

/// `connections.composio.set` from an API face. The policy is re-read and planned under the file
/// lock. Legacy changes never modify a direct Gmail grant or sending policy.
pub fn composio_set(paths: &ConnectionsPaths, body: &str) -> Result<(), String> {
    let request = parse_composio(body).map_err(refusal)?;
    composio_policy::update_strict(&paths.composio, |policy| {
        let next = plan_composio(policy, request).map_err(refusal)?;
        *policy = next;
        Ok(())
    })?;
    Ok(())
}

/// Tighten the direct Gmail policy. The assistant cannot enable sending, even if sending
/// was already enabled, so replaying an old API call cannot undo a later human restriction.
pub fn gmail_set(paths: &ConnectionsPaths, body: &str) -> Result<(), String> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct GmailRequest {
        draft_stop: bool,
    }
    let request: GmailRequest =
        serde_json::from_str(body).map_err(|_| refusal(Invalid::BadRequest))?;
    if !request.draft_stop {
        return Err(refusal(Invalid::DraftStopOffOnlyInApp));
    }
    crate::gmail_policy::update_strict(&paths.gmail, |policy| {
        policy.draft_stop = true;
        Ok(())
    })
    .map(|_| ())
}

/// The `connections.list` answer at `now_ms` (unix ms). Per service: the state the Connections
/// screen shows (`null` when the app is not running), whether it counts as connected, the access
/// ceiling from the scope table, the third party it runs through, and whether a disconnect is
/// waiting for the app, plus Mail policy. Account labels are never read from the snapshot.
pub fn list_json(paths: &ConnectionsPaths, now_ms: u64) -> Value {
    let live = read_status(&paths.status, now_ms);
    let pending = pending_disconnects(&paths.requests);
    let policy = composio_policy::load_effective(&paths.composio);
    let gmail = crate::gmail_policy::load_effective(&paths.gmail);
    let services: Vec<Value> = listed_services()
        .map(|service| {
            let state = live
                .as_ref()
                .and_then(|rows| rows.iter().find(|(s, _)| *s == service).map(|(_, st)| *st));
            json!({
                "service": service.source_str(),
                "state": state,
                "connected": state.map(|s| matches!(s, "connected" | "needs_reauth")),
                "access": scope::access(service).as_str(),
                "third_party": third_party(service),
                "disconnect_pending": pending.contains(&service),
            })
        })
        .collect();
    json!({
        "app_running": live.is_some(),
        "services": services,
        "gmail": { "draft_stop": gmail.draft_stop },
        "composio": {
            "consent": policy.consent_acknowledged,
            "draft_stop": policy.draft_stop,
        },
    })
}

/// A backend that serves the three tools from real files in a temp directory, with the real rules
/// above. Shared by the face tests (MCP, REST, the socket).
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::backend::{MemoryBackend, ReadItem, ReadParams, WriteResult};
    use crate::memory_api::Tool;

    pub(crate) struct FileConnections {
        pub(crate) paths: ConnectionsPaths,
    }

    impl FileConnections {
        /// Mail consented and connected, draft-only on; the app published Gmail and Calendar as
        /// connected just now.
        pub(crate) fn new(tag: &str) -> Self {
            let composio = crate::composio_policy::tests::temp_path(tag);
            let paths = ConnectionsPaths {
                status: composio.with_extension("status.json"),
                requests: composio.with_extension("requests.json"),
                gmail: composio.with_extension("gmail.json"),
                composio,
            };
            crate::composio_policy::update(&paths.composio, |p| {
                *p = crate::composio_policy::tests::consented(true, true);
                Ok(())
            })
            .unwrap();
            write_status(
                &paths.status,
                crate::desktop_heartbeat::now_ms(),
                &[(Service::Gmail, "connected"), (Service::GoogleCalendar, "connected")],
            )
            .unwrap();
            Self { paths }
        }

        pub(crate) fn policy(&self) -> ComposioPolicy {
            composio_policy::load_effective(&self.paths.composio)
        }

        pub(crate) fn pending(&self) -> Vec<Service> {
            pending_disconnects(&self.paths.requests)
        }
    }

    impl Drop for FileConnections {
        fn drop(&mut self) {
            for path in [
                &self.paths.composio,
                &self.paths.gmail,
                &self.paths.status,
                &self.paths.requests,
            ] {
                let _ = std::fs::remove_file(path);
                let mut lock = path.as_os_str().to_owned();
                lock.push(".lock");
                let _ = std::fs::remove_file(PathBuf::from(lock));
            }
        }
    }

    impl MemoryBackend for FileConnections {
        fn read(&self, _tool: Tool, _params: &ReadParams) -> Vec<ReadItem> {
            Vec::new()
        }

        fn read_structured(&self, tool: Tool, _params: &ReadParams) -> Option<String> {
            (tool == Tool::ConnectionsList)
                .then(|| list_json(&self.paths, crate::desktop_heartbeat::now_ms()).to_string())
        }

        fn write(&self, tool: Tool, body: &str) -> WriteResult {
            match tool {
                Tool::ConnectionsDisconnect => disconnect(&self.paths, body).map(|_| None),
                Tool::ConnectionsComposioSet => composio_set(&self.paths, body).map(|_| None),
                Tool::ConnectionsGmailSet => gmail_set(&self.paths, body).map(|_| None),
                _ => Err("not a write tool".into()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composio_policy::tests::{consented, temp_path};

    fn paths(tag: &str) -> ConnectionsPaths {
        let composio = temp_path(tag);
        ConnectionsPaths {
            status: composio.with_extension("status.json"),
            requests: composio.with_extension("requests.json"),
            gmail: composio.with_extension("gmail.json"),
            composio,
        }
    }

    fn cleanup(paths: &ConnectionsPaths) {
        for path in [
            &paths.composio,
            &paths.gmail,
            &paths.status,
            &paths.requests,
        ] {
            let _ = std::fs::remove_file(path);
            let mut lock = path.as_os_str().to_owned();
            lock.push(".lock");
            let _ = std::fs::remove_file(PathBuf::from(lock));
        }
    }

    fn save(paths: &ConnectionsPaths, policy: ComposioPolicy) {
        composio_policy::update(&paths.composio, |p| {
            *p = policy;
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn tightening_the_mail_policy_is_direct() {
        let request = |body: &str| parse_composio(body).unwrap();
        // Revoke: consent off, draft-only back on, disclosures cleared, Mail disconnected.
        let next = plan_composio(&consented(false, true), request(r#"{"consent":false}"#)).unwrap();
        assert!(!next.consent_acknowledged && next.draft_stop && !next.gmail_connected);
        assert!(!next.disclosures.via_third_party);
        assert_eq!(next.user_id, "user-123", "the account setting itself is untouched");
        // Draft-only mode on.
        let next = plan_composio(&consented(false, true), request(r#"{"draft_stop":true}"#)).unwrap();
        assert!(next.consent_acknowledged && next.draft_stop && next.gmail_connected);
        // Both at once.
        let next =
            plan_composio(&consented(false, true), request(r#"{"consent":false,"draft_stop":true}"#)).unwrap();
        assert!(!next.consent_acknowledged && next.draft_stop);
        // Restating what is in force changes nothing.
        let current = consented(false, true);
        assert_eq!(plan_composio(&current, request(r#"{"consent":true}"#)), Ok(current.clone()));
        assert_eq!(plan_composio(&current, request(r#"{"draft_stop":false}"#)), Ok(current));
        let off = ComposioPolicy::default();
        assert_eq!(plan_composio(&off, request(r#"{"consent":false}"#)), Ok(off.clone()));
        assert_eq!(plan_composio(&off, request(r#"{"draft_stop":true}"#)), Ok(off));
    }

    #[test]
    fn loosening_the_mail_policy_is_refused() {
        let request = |body: &str| parse_composio(body).unwrap();
        // Granting consent: only on the Settings card.
        for current in [ComposioPolicy::default(), ComposioPolicy { user_id: "u".into(), ..Default::default() }] {
            assert_eq!(
                plan_composio(&current, request(r#"{"consent":true}"#)),
                Err(Invalid::ConsentOnlyInApp)
            );
            assert_eq!(
                plan_composio(&current, request(r#"{"consent":true,"draft_stop":false}"#)),
                Err(Invalid::ConsentOnlyInApp)
            );
            assert_eq!(
                plan_composio(&current, request(r#"{"draft_stop":false}"#)),
                Err(Invalid::DraftStopOffOnlyInApp)
            );
        }
        // Draft-only mode off, even with consent on file.
        assert_eq!(
            plan_composio(&consented(true, true), request(r#"{"draft_stop":false}"#)),
            Err(Invalid::DraftStopOffOnlyInApp)
        );
        // Revoking consent while asking for sends: contradictory, and sends need consent.
        assert_eq!(
            plan_composio(&consented(false, true), request(r#"{"consent":false,"draft_stop":false}"#)),
            Err(Invalid::DraftStopOffOnlyInApp)
        );
        // Bodies.
        for body in ["{}", "not json", r#"{"consent":"no"}"#, r#"{"consent":false,"user_id":"x"}"#, "[]"] {
            assert_eq!(parse_composio(body), Err(Invalid::BadRequest), "{body}");
        }
        assert!(refusal(Invalid::ConsentOnlyInApp).starts_with("invalid connections request: consent_only_in_app"));
    }

    #[test]
    fn composio_set_writes_tightening_and_fails_closed_on_loosening() {
        let paths = paths("set");
        save(&paths, consented(true, true));
        let before = std::fs::read_to_string(&paths.composio).unwrap();
        // Refused: nothing written, nothing queued.
        for body in [r#"{"draft_stop":false}"#, r#"{"bogus":true}"#] {
            let err = composio_set(&paths, body).unwrap_err();
            assert!(err.starts_with("invalid "), "{err}");
        }
        assert_eq!(std::fs::read_to_string(&paths.composio).unwrap(), before);
        assert!(pending_disconnects(&paths.requests).is_empty());

        // Revoke: written, and Mail's disconnect queued for the desktop.
        composio_set(&paths, r#"{"consent":false}"#).unwrap();
        let now = composio_policy::load_effective(&paths.composio);
        assert!(!now.consent_acknowledged && now.draft_stop && !now.gmail_connected);
        assert!(
            pending_disconnects(&paths.requests).is_empty(),
            "legacy consent cannot disconnect direct Gmail"
        );
        // And granting it back over the API is refused.
        assert!(composio_set(&paths, r#"{"consent":true}"#).unwrap_err().contains("consent_only_in_app"));
        assert!(!composio_policy::load_effective(&paths.composio).consent_acknowledged);

        // A corrupt policy is never replaced by an API write.
        std::fs::write(&paths.composio, "{broken").unwrap();
        assert!(composio_set(&paths, r#"{"draft_stop":true}"#).is_err());
        assert_eq!(std::fs::read_to_string(&paths.composio).unwrap(), "{broken");
        cleanup(&paths);
    }

    #[test]
    fn gmail_api_tightens_only_and_legacy_revocation_cannot_change_it() {
        let paths = paths("gmail-set");
        crate::gmail_policy::update(&paths.gmail, |p| {
            p.draft_stop = false;
            Ok(())
        })
        .unwrap();
        save(&paths, consented(false, true));
        composio_set(&paths, r#"{"consent":false}"#).unwrap();
        assert!(!crate::gmail_policy::load_effective(&paths.gmail).draft_stop);
        assert!(pending_disconnects(&paths.requests).is_empty());
        for body in [
            r#"{"draft_stop":false}"#,
            "{}",
            r#"{"draft_stop":true,"consent":true}"#,
        ] {
            assert!(gmail_set(&paths, body).is_err());
            assert!(!crate::gmail_policy::load_effective(&paths.gmail).draft_stop);
        }
        gmail_set(&paths, r#"{"draft_stop":true}"#).unwrap();
        assert!(crate::gmail_policy::load_effective(&paths.gmail).draft_stop);
        assert!(gmail_set(&paths, r#"{"draft_stop":false}"#)
            .unwrap_err()
            .contains("draft_stop_off_only_in_app"));
        std::fs::write(&paths.gmail, "broken").unwrap();
        assert!(gmail_set(&paths, r#"{"draft_stop":true}"#).is_err());
        assert_eq!(std::fs::read_to_string(&paths.gmail).unwrap(), "broken");
        cleanup(&paths);
    }

    #[test]
    fn disconnect_only_queues_a_request_for_the_desktop() {
        let paths = paths("disconnect");
        disconnect(&paths, r#"{"service":"gcal"}"#).unwrap();
        disconnect(&paths, r#"{"service":"gcal"}"#).unwrap();
        disconnect(&paths, r#"{"service":"gmail"}"#).unwrap();
        assert_eq!(pending_disconnects(&paths.requests), vec![Service::GoogleCalendar, Service::Gmail]);
        assert!(!paths.composio.exists(), "the daemon touches no credential and no policy");
        for (body, invalid) in [
            (r#"{"service":"web"}"#, Invalid::UnknownService),
            (r#"{"service":"dropbox"}"#, Invalid::UnknownService),
            (r#"{"service":"gcal","delete_events":true}"#, Invalid::BadRequest),
            ("{}", Invalid::BadRequest),
        ] {
            assert_eq!(parse_disconnect(body), Err(invalid), "{body}");
            assert!(disconnect(&paths, body).unwrap_err().starts_with("invalid "));
        }
        // The refusal names every service a caller may disconnect.
        for service in listed_services() {
            assert!(Invalid::UnknownService.detail().contains(service.source_str()));
        }
        // The desktop clears what it applied; a request that arrived meanwhile stays.
        clear_disconnects(&paths.requests, &[Service::GoogleCalendar]).unwrap();
        assert_eq!(pending_disconnects(&paths.requests), vec![Service::Gmail]);
        clear_disconnects(&paths.requests, &[Service::Gmail]).unwrap();
        assert!(pending_disconnects(&paths.requests).is_empty());
        cleanup(&paths);
    }

    #[test]
    fn snapshots_and_unscoped_answers_omit_account_labels() {
        let paths = paths("labels");
        write_status(&paths.status, 10_000, &[(Service::Gmail, "connected")])
            .expect("publish status");
        let snapshot = std::fs::read_to_string(&paths.status).expect("read snapshot");
        assert!(!snapshot.contains("account_label"));
        // An old snapshot's identity metadata must also be ignored.
        std::fs::write(&paths.status, r#"{"observed_at_ms":10000,"services":[{"service":"gmail","state":"connected","account_label":"you@example.com"}]}"#)
            .expect("write legacy snapshot");
        let answer = list_json(&paths, 10_000);
        assert_eq!(answer["app_running"], true);
        assert!(!answer.to_string().contains("account_label"));
        assert!(!answer.to_string().contains("you@example.com"));
        cleanup(&paths);
    }

    #[test]
    fn the_status_snapshot_is_trusted_only_while_fresh() {
        let paths = paths("status");
        assert_eq!(read_status(&paths.status, 10_000), None);
        write_status(
            &paths.status,
            10_000,
            &[(Service::Gmail, "connected"), (Service::GoogleCalendar, "needs_reauth"), (Service::GitHub, "keychain_locked"), (Service::Slack, "bogus")],
        )
        .unwrap();
        assert_eq!(
            read_status(&paths.status, 10_000 + STATUS_MAX_AGE_MS),
            Some(vec![(Service::Gmail, "connected"), (Service::GoogleCalendar, "needs_reauth"), (Service::GitHub, "keychain_locked")])
        );
        assert_eq!(read_status(&paths.status, 10_000 + STATUS_MAX_AGE_MS + 1), None, "stale");
        assert_eq!(read_status(&paths.status, 9_999), None, "a future stamp");
        std::fs::write(&paths.status, "nope").unwrap();
        assert_eq!(read_status(&paths.status, 10_000), None);
        cleanup(&paths);
    }

    #[test]
    fn the_list_defaults_unknown_accounts_to_null() {
        let paths = paths("list");
        save(&paths, consented(true, true));
        // SHOGUN not running: states unknown, the Mail policy still known.
        let v = list_json(&paths, 50_000);
        assert_eq!(v["app_running"], false);
        let services = v["services"].as_array().unwrap();
        assert_eq!(services.len(), 8, "every connector row but Web");
        assert!(services
            .iter()
            .all(|s| s["state"].is_null() && s["connected"].is_null()));
        assert_eq!(
            v["composio"],
            json!({ "consent": true, "draft_stop": true })
        );
        assert_eq!(v["gmail"], json!({ "draft_stop": true }));

        write_status(
            &paths.status,
            50_000,
            &[
                (Service::Gmail, "connected"),
                (Service::GoogleCalendar, "needs_reauth"),
                (Service::GoogleDrive, "disconnected"),
                (Service::Slack, "coming_soon"),
            ],
        )
        .unwrap();
        request_disconnect(&paths.requests, Service::GoogleCalendar).unwrap();
        let v = list_json(&paths, 51_000);
        assert_eq!(v["app_running"], true);
        let row = |id: &str| v["services"].as_array().unwrap().iter().find(|s| s["service"] == id).unwrap().clone();
        assert_eq!(
            row("gmail"),
            json!({
                "service": "gmail", "state": "connected", "connected": true,
                "access": scope::access(Service::Gmail).as_str(), "third_party": Value::Null,
                "disconnect_pending": false,
            })
        );
        assert_eq!(row("gcal")["connected"], true, "amber still holds a connection");
        assert_eq!(row("gcal")["disconnect_pending"], true);
        assert_eq!(row("gcal")["third_party"], Value::Null);
        assert_eq!(row("gcal")["access"], "read");
        assert_eq!(row("gdrive")["connected"], false);
        assert_eq!(row("slack")["state"], "coming_soon");
        assert_eq!(row("notion")["state"], Value::Null, "a row the app did not publish is unknown");

        // No identity is invented when the publisher had none; credentials and paths stay absent.
        let text = v.to_string();
        for leak in ["user-123", "user_id", "token", "@", "/"] {
            assert!(!text.contains(leak), "{leak} in {text}");
        }
        cleanup(&paths);
    }

    #[test]
    fn the_daemons_find_the_files_beside_the_db() {
        let paths = ConnectionsPaths::for_db("/data/shogun/memory.db");
        assert_eq!(paths.status, PathBuf::from("/data/shogun/connections_status.json"));
        assert_eq!(paths.requests, PathBuf::from("/data/shogun/connections_requests.json"));
        if std::env::var(composio_policy::POLICY_ENV).is_err() {
            assert_eq!(paths.composio, PathBuf::from("/data/shogun/composio.json"));
        }
    }
}
