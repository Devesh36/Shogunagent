//! Form tools share the desktop's live review session. API apply only opens human review.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use crate::memory_api::{AuthResult, Tool, TokenRegistry, TokenScope};

/// Maximum request or response payload, excluding its newline frame delimiter.
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

/// Desktop runtime operations. Remote callers cannot supply answers or confirm a fill.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum FormOperation {
    /// Reuse the authenticated desktop IPC for proposal-time connector reads. Never sends.
    ProposeSlackPost { target: String, body: String, origin: String },
    Inspect {},
    Plan {
        session_id: u64,
        revision: u64,
        instruction: String,
    },
    Status {
        session_id: u64,
    },
    Apply {
        session_id: u64,
        revision: u64,
    },
    Undo {
        session_id: u64,
    },
    Close {
        session_id: u64,
    },
}

/// Authentication forwarded from an admitted face and rechecked by the desktop.
/// Deliberately has no Debug: credentials must never enter diagnostic output.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormsAuth {
    pub token: Option<String>,
    pub admitted_by_token: bool,
    pub client: Option<String>,
}

/// The scope a forms-socket request runs under, or `None` to refuse it (audit 2026-10-10 M5).
///
/// A request that names its app (`client`, issue #963) runs under that app's connection; one
/// that presents a bearer token runs under that token's scope. A request with neither is
/// refused — there is **no** empty-registry "process trust" on this socket, as on the stdio
/// `shogun-mcp` entry since audit 2026-10-20 M5 ([`TokenRegistry::authenticate_process`]):
/// before the first token or connection exists, any same-user process could otherwise inspect
/// the focused form and spend the user's Jev key with Full scope. `admitted_by_token` is informational; the token itself is
/// what is re-checked here.
pub fn authorize_scope(registry: &TokenRegistry, auth: &FormsAuth) -> Option<TokenScope> {
    if let Some(client) = &auth.client {
        return registry.authenticate_client(client);
    }
    match auth.token.as_deref() {
        Some(token) => match registry.authenticate(Some(token)) {
            AuthResult::Granted(scope) => Some(scope),
            AuthResult::DeniedNoToken | AuthResult::DeniedInvalidToken => None,
        },
        None => None,
    }
}

/// Who a forms-socket request is counted against for the per-client model-call limit
/// ([`crate::rate_limit`]): the app connection it names, else its token's issued id, else the
/// local caller. The same identity the MCP and REST faces count by, never the token itself.
pub fn rate_key(registry: &TokenRegistry, auth: &FormsAuth) -> crate::rate_limit::ClientKey {
    use crate::rate_limit::ClientKey;
    if let Some(client) = &auth.client {
        return ClientKey::connection(client);
    }
    auth.token
        .as_deref()
        .and_then(|token| registry.token_id(token))
        .map(|id| ClientKey::token(&id))
        .unwrap_or_else(ClientKey::local)
}

/// The refusal for a forms-socket request over its client's model-call limit (2026-10-20 cost
/// audit), or `None` to go ahead. Asked after authentication, scope and plan, as on MCP and REST;
/// only `forms.plan` starts a model call here. The body is REST's 429 body:
/// `{"error":"rate_limited","message":…,"retry_after_ms":n}`.
pub fn rate_limited(
    limiter: &crate::rate_limit::LlmRateLimiter,
    registry: &TokenRegistry,
    auth: &FormsAuth,
    tool: Tool,
    now_ms: u64,
) -> Option<Value> {
    use crate::rate_limit::{triggers_llm, RATE_LIMITED, RATE_LIMITED_MESSAGE};
    if !triggers_llm(tool, None) {
        return None;
    }
    let retry_ms = limiter.check(&rate_key(registry, auth), now_ms).err()?;
    Some(json!({
        "error": RATE_LIMITED,
        "message": RATE_LIMITED_MESSAGE,
        "retry_after_ms": retry_ms,
    }))
}

/// The socket's reply for a refused request. `error` is a machine code, or a JSON refusal the
/// desktop built: a [`rate_limited`] body goes out as it is, a Slack target refusal goes out as
/// its code with the detail.
pub fn error_response(error: &str) -> Value {
    let parsed = serde_json::from_str::<Value>(error).ok();
    match parsed {
        Some(value)
            if value.get("error").and_then(Value::as_str) == Some(crate::rate_limit::RATE_LIMITED) =>
        {
            value
        }
        Some(detail)
            if detail
                .get("code")
                .and_then(Value::as_str)
                .is_some_and(|code| code.starts_with("slack_target_")) =>
        {
            json!({"error": detail["code"], "detail": detail})
        }
        _ => json!({"error": error}),
    }
}

/// One request frame on the private desktop socket.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormsRequest {
    pub operation: FormOperation,
    pub auth: FormsAuth,
}

/// Tools answered by the form runtime instead of the generic memory backend.
pub fn is_forms_tool(tool: Tool) -> bool {
    matches!(
        tool,
        Tool::FormsInspect
            | Tool::FormsPlan
            | Tool::FormsStatus
            | Tool::FormsApply
            | Tool::FormsUndo
            | Tool::FormsClose
    )
}

/// Lift MCP arguments or a REST body into the strict operation contract.
pub fn operation(
    tool: Tool,
    body: &str,
    path_id: Option<u64>,
) -> Result<FormOperation, &'static str> {
    if body.len() > MAX_FRAME_BYTES {
        return Err("form_request_invalid");
    }
    let mut args: Value = serde_json::from_str(if body.is_empty() { "{}" } else { body })
        .map_err(|_| "form_request_invalid")?;
    let object = args.as_object_mut().ok_or("form_request_invalid")?;
    if let Some(id) = path_id {
        if object.contains_key("session_id") {
            return Err("form_request_invalid");
        }
        object.insert("session_id".into(), json!(id));
    }
    if object.contains_key("op") {
        return Err("form_request_invalid");
    }
    let op = match tool {
        Tool::FormsInspect => "inspect",
        Tool::FormsPlan => "plan",
        Tool::FormsStatus => "status",
        Tool::FormsApply => "apply",
        Tool::FormsUndo => "undo",
        Tool::FormsClose => "close",
        _ => return Err("form_request_invalid"),
    };
    object.insert("op".into(), json!(op));
    let operation: FormOperation =
        serde_json::from_value(args).map_err(|_| "form_request_invalid")?;
    validate_operation(&operation)?;
    Ok(operation)
}

/// Validate the shared operation even when received directly over IPC.
pub fn validate_operation(operation: &FormOperation) -> Result<(), &'static str> {
    let (id, instruction) = match operation {
        FormOperation::ProposeSlackPost { target, body, origin } => {
            return if !target.trim().is_empty() && target.len() <= 256
                && !target.chars().any(char::is_control) && body.len() <= 32 * 1024
                && matches!(origin.as_str(), "mcp" | "api") { Ok(()) } else { Err("form_request_invalid") };
        }
        FormOperation::Inspect {} => return Ok(()),
        FormOperation::Plan {
            session_id,
            instruction,
            ..
        } => (*session_id, Some(instruction.as_str())),
        FormOperation::Status { session_id }
        | FormOperation::Apply { session_id, .. }
        | FormOperation::Undo { session_id }
        | FormOperation::Close { session_id } => (*session_id, None),
    };
    if id == 0
        || instruction
            .is_some_and(|text| text.trim().is_empty() || text.chars().take(4001).count() > 4000)
    {
        return Err("form_request_invalid");
    }
    Ok(())
}

fn validate_request(request: &FormsRequest) -> Result<(), &'static str> {
    validate_operation(&request.operation)?;
    if request
        .auth
        .token
        .as_ref()
        .is_some_and(|token| token.is_empty() || token.len() > 512)
        || request
            .auth
            .client
            .as_ref()
            .is_some_and(|client| client.is_empty() || client.len() > 128)
    {
        return Err("form_request_invalid");
    }
    Ok(())
}

fn read_frame(reader: &mut impl Read) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    BufReader::new(reader)
        .take((MAX_FRAME_BYTES + 2) as u64)
        .read_until(b'\n', &mut bytes)
        .map_err(|_| "form_unavailable".to_owned())?;
    if bytes.last() != Some(&b'\n') || bytes.len() > MAX_FRAME_BYTES + 1 {
        return Err("form_request_invalid".into());
    }
    bytes.pop();
    Ok(bytes)
}

/// Read exactly one bounded newline JSON request. Refuses unrecognized properties and bad IDs.
pub fn read_request(reader: &mut impl Read) -> Result<FormsRequest, String> {
    let bytes = read_frame(reader)?;
    let request: FormsRequest =
        serde_json::from_slice(&bytes).map_err(|_| "form_request_invalid".to_owned())?;
    validate_request(&request).map_err(str::to_owned)?;
    Ok(request)
}

/// Write one bounded JSON value and newline; never logs its content.
pub fn write_response(writer: &mut impl Write, response: &Value) -> Result<(), String> {
    let bytes = serde_json::to_vec(response).map_err(|_| "form_request_invalid".to_owned())?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err("form_request_invalid".into());
    }
    writer
        .write_all(&bytes)
        .and_then(|()| writer.write_all(b"\n"))
        .and_then(|()| writer.flush())
        .map_err(|_| "form_unavailable".into())
}

/// Resolve the same app-data socket for the database used by this face.
pub fn socket_path(db_path: &Path) -> Option<PathBuf> {
    db_path.parent().map(|parent| parent.join("forms.sock"))
}

/// HTTP status for a desktop form runtime response.
pub fn http_status(body: &str) -> u16 {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return 503;
    };
    match value.get("error").and_then(Value::as_str) {
        Some("unauthorized") => 401,
        Some("plan_required" | "scope_insufficient" | "scope_denied" | "api_disabled") => 403,
        Some("form_session_unknown" | "closed") => 404,
        Some(
            "form_revision_changed"
            | "form_busy"
            | "form_source_changed"
            | "form_session_closed"
            | "stale_revision"
            | "busy"
            | "target_changed",
        ) => 409,
        Some("form_unavailable" | "unavailable") => 503,
        Some(crate::rate_limit::RATE_LIMITED) => 429,
        Some(_) => 400,
        None if matches!(
            value.get("status").and_then(Value::as_str),
            Some("pending" | "pending_confirmation")
        ) =>
        {
            202
        }
        None => 200,
    }
}

/// Fixed machine error without input/provider/credential content.
pub fn refusal_json(code: &str) -> String {
    json!({"error":code}).to_string()
}

/// The peer-credential check on the Mac app's owner-only socket. The socket is the app's live
/// review session, so this and [`request`] compile only with feature `desktop` (E1-T7); the
/// server build answers `form_unavailable`, as it does without the app.
#[cfg(all(unix, feature = "desktop"))]
mod peer;
#[cfg(all(unix, feature = "desktop"))]
pub use peer::same_uid;

/// Call the live desktop through its owner-only socket, bounded to one JSON reply.
#[cfg(all(unix, feature = "desktop"))]
pub fn request(
    db_path: &Path,
    operation: FormOperation,
    auth: FormsAuth,
) -> Result<String, String> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;
    let payload = FormsRequest { operation, auth };
    validate_request(&payload).map_err(str::to_owned)?;
    let path = socket_path(db_path).ok_or_else(|| "form_unavailable".to_owned())?;
    let metadata = std::fs::symlink_metadata(&path).map_err(|_| "form_unavailable".to_owned())?;
    // SAFETY: geteuid takes no arguments or pointers.
    let uid = unsafe { libc::geteuid() };
    if !metadata.file_type().is_socket()
        || metadata.uid() != uid
        || metadata.permissions().mode() & 0o777 != 0o600
    {
        return Err("form_unavailable".into());
    }
    let mut stream = UnixStream::connect(path).map_err(|_| "form_unavailable".to_owned())?;
    if !same_uid(&stream) {
        return Err("form_unavailable".into());
    }
    stream
        .set_read_timeout(Some(Duration::from_secs(120)))
        .map_err(|_| "form_unavailable".to_owned())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|_| "form_unavailable".to_owned())?;
    let value = serde_json::to_value(payload).map_err(|_| "form_request_invalid".to_owned())?;
    write_response(&mut stream, &value)?;
    let bytes = read_frame(&mut stream)?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| "form_unavailable".to_owned())?;
    if !value.is_object() {
        return Err("form_unavailable".into());
    }
    Ok(value.to_string())
}

/// Without the Mac app's socket (a non-Unix target, or a build without feature `desktop`) a form
/// request has nowhere to go: the same `form_unavailable` the faces give when the app is not
/// running.
#[cfg(not(all(unix, feature = "desktop")))]
pub fn request(
    _db_path: &Path,
    _operation: FormOperation,
    _auth: FormsAuth,
) -> Result<String, String> {
    Err("form_unavailable".into())
}

#[cfg(test)]
mod tests {
    #[test]
    fn undo_requests_cannot_supply_answers_or_confirmation() {
        use super::*;
        assert_eq!(
            operation(Tool::FormsUndo, r#"{"session_id":7}"#, None),
            Ok(FormOperation::Undo { session_id: 7 })
        );
        for body in [
            r#"{"session_id":7,"confirm":true}"#,
            r#"{"session_id":7,"answers":[]}"#,
            r#"{"session_id":0}"#,
        ] {
            assert!(operation(Tool::FormsUndo, body, None).is_err());
        }
    }

    use super::*;

    #[test]
    fn api_apply_cannot_supply_answers_or_confirm() {
        for extra in ["answers", "confirm", "approved", "auth", "op"] {
            let body = json!({"session_id":1,"revision":2,extra:true}).to_string();
            assert_eq!(
                operation(Tool::FormsApply, &body, None),
                Err("form_request_invalid")
            );
        }
        assert_eq!(
            operation(Tool::FormsApply, r#"{"session_id":1,"revision":2}"#, None),
            Ok(FormOperation::Apply {
                session_id: 1,
                revision: 2
            })
        );
    }

    #[test]
    fn wire_refuses_nonpositive_ids_and_oversized_instructions() {
        assert!(operation(Tool::FormsStatus, r#"{"session_id":0}"#, None).is_err());
        assert!(operation(
            Tool::FormsPlan,
            &json!({"session_id":1,"revision":0,"instruction":"a".repeat(4001)}).to_string(),
            None
        )
        .is_err());
        assert!(operation(Tool::FormsInspect, r##"{"selector":"#payment"}"##, None).is_err());
    }

    #[test]
    fn desktop_status_codes_keep_pending_and_refusals_distinct() {
        for (code, status) in [
            ("scope_denied", 403),
            ("stale_revision", 409),
            ("busy", 409),
            ("target_changed", 409),
            ("closed", 404),
            ("unavailable", 503),
        ] {
            assert_eq!(http_status(&refusal_json(code)), status);
        }
        assert_eq!(
            http_status(&json!({"status":"pending_confirmation"}).to_string()),
            202
        );
        assert_eq!(http_status(&json!({"phase":"review"}).to_string()), 200);
    }

    #[test]
    fn ipc_rejects_unknown_properties_inside_inspect() {
        let frame = b"{\"operation\":{\"op\":\"inspect\",\"answers\":[]},\"auth\":{\"token\":null,\"admitted_by_token\":false,\"client\":null}}\n";
        assert!(read_request(&mut frame.as_slice()).is_err());
        assert!(write_response(
            &mut Vec::new(),
            &json!({"value":"a".repeat(MAX_FRAME_BYTES)})
        )
        .is_err());
    }

    #[cfg(all(unix, feature = "desktop"))]
    #[test]
    fn owner_only_socket_roundtrip_checks_peer_and_shared_frames() {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixListener;
        let dir = Path::new("/tmp").join(format!(
            "shogun-forms-socket-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("memory.db");
        let socket = socket_path(&db).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        let serving = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            assert!(same_uid(&stream));
            let incoming = read_request(&mut stream).unwrap();
            assert_eq!(incoming.operation, FormOperation::Inspect {});
            assert!(incoming.auth.admitted_by_token);
            assert!(incoming.auth.token.as_deref() == Some("test-form-token"));
            write_response(&mut stream, &json!({"status":"review","session_id":1})).unwrap();
        });
        let body = request(
            &db,
            FormOperation::Inspect {},
            FormsAuth {
                token: Some("test-form-token".into()),
                admitted_by_token: true,
                client: None,
            },
        )
        .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap()["session_id"],
            1
        );
        serving.join().unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(request(
            &db,
            FormOperation::Inspect {},
            FormsAuth {
                token: None,
                admitted_by_token: false,
                client: None
            }
        )
        .is_err());
        std::fs::remove_file(&socket).unwrap();
        std::os::unix::fs::symlink(db, &socket).unwrap();
        assert!(request(
            &dir.join("memory.db"),
            FormOperation::Inspect {},
            FormsAuth {
                token: None,
                admitted_by_token: false,
                client: None
            }
        )
        .is_err());
        std::fs::remove_file(&socket).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn frames_are_bounded_and_require_a_delimiter() {
        let request = FormsRequest {
            operation: FormOperation::Inspect {},
            auth: FormsAuth {
                token: None,
                admitted_by_token: false,
                client: None,
            },
        };
        let mut bytes = Vec::new();
        write_response(&mut bytes, &serde_json::to_value(&request).unwrap()).unwrap();
        assert!(read_request(&mut bytes.as_slice()).is_ok());
        let mut truncated = &bytes[..bytes.len() - 1];
        assert!(read_request(&mut truncated).is_err());
        assert!(read_request(&mut " ".repeat(MAX_FRAME_BYTES + 2).as_bytes()).is_err());
    }
    #[test]
    fn the_forms_socket_has_no_process_trust() {
        // Audit M5: an empty registry (nothing issued yet) refuses a peer that presents neither
        // an app connection nor a token, instead of the stdio entry's legacy Full scope.
        let empty = TokenRegistry::new();
        let anonymous = FormsAuth { token: None, admitted_by_token: false, client: None };
        assert_eq!(authorize_scope(&empty, &anonymous), None);
        // Nor does a token the registry does not know, however it was flagged.
        let unknown = FormsAuth { token: Some("nope".into()), admitted_by_token: true, client: None };
        assert_eq!(authorize_scope(&empty, &unknown), None);
        // Nor an app that was never connected.
        let stranger = FormsAuth { token: None, admitted_by_token: false, client: Some("claude-code".into()) };
        assert_eq!(authorize_scope(&empty, &stranger), None);
        // An unreadable list refuses everything, as everywhere else.
        let mut registry = TokenRegistry::new();
        registry.issue("tok-full");
        let with_token = FormsAuth { token: Some("tok-full".into()), admitted_by_token: true, client: None };
        assert_eq!(authorize_scope(&TokenRegistry::unavailable(), &with_token), None);
    }

    #[test]
    fn a_valid_token_or_connection_gives_its_own_scope() {
        let mut registry = TokenRegistry::new();
        registry.issue("tok-full");
        registry.issue_scoped("tok-read", TokenScope::Read);
        registry.grant(&crate::memory_api_settings::ClientGrant {
            client: crate::memory_api_settings::grant_id("claude-code").unwrap_or("claude-code").into(),
            scope: TokenScope::Read,
            created_at: 1,
            sensitive_sources: crate::source_policy::SensitiveSources::Allowed,
        });
        let full = FormsAuth { token: Some("tok-full".into()), admitted_by_token: true, client: None };
        assert_eq!(authorize_scope(&registry, &full), Some(TokenScope::Full));
        let read = FormsAuth { token: Some("tok-read".into()), admitted_by_token: true, client: None };
        assert_eq!(authorize_scope(&registry, &read), Some(TokenScope::Read));
        // The token decides, not the flag: a presented token is checked either way.
        let unflagged = FormsAuth { token: Some("tok-read".into()), admitted_by_token: false, client: None };
        assert_eq!(authorize_scope(&registry, &unflagged), Some(TokenScope::Read));
        let wrong = FormsAuth { token: Some("tok-wrong".into()), admitted_by_token: true, client: None };
        assert_eq!(authorize_scope(&registry, &wrong), None);
        // An app connection runs under the connection's scope; naming an app ignores any token.
        let app = FormsAuth { token: Some("tok-full".into()), admitted_by_token: false, client: Some("claude-code".into()) };
        assert_eq!(authorize_scope(&registry, &app), Some(TokenScope::Read));
        let unknown_app = FormsAuth { token: None, admitted_by_token: false, client: Some("never-connected".into()) };
        assert_eq!(authorize_scope(&registry, &unknown_app), None);
        // Still nothing for a peer with neither, even now that tokens exist.
        let anonymous = FormsAuth { token: None, admitted_by_token: false, client: None };
        assert_eq!(authorize_scope(&registry, &anonymous), None);
    }

    #[test]
    fn forms_plan_on_the_socket_is_limited_per_client_like_mcp_and_rest() {
        use crate::rate_limit::{ClientKey, LlmRateLimiter, RATE_LIMITED};
        let limiter = LlmRateLimiter::in_memory().with_limit(2, 60_000);
        let mut registry = TokenRegistry::new();
        registry.issue("tok-full");
        let app = FormsAuth { token: None, admitted_by_token: false, client: Some("claude-code".into()) };
        let token = FormsAuth { token: Some("tok-full".into()), admitted_by_token: true, client: None };
        assert_eq!(rate_key(&registry, &app), ClientKey::connection("claude-code"));
        assert_eq!(rate_key(&registry, &token), ClientKey::token("env"));
        assert_eq!(rate_limited(&limiter, &registry, &app, Tool::FormsPlan, 1_000), None);
        assert_eq!(rate_limited(&limiter, &registry, &app, Tool::FormsPlan, 1_001), None);
        let refusal = rate_limited(&limiter, &registry, &app, Tool::FormsPlan, 1_002)
            .expect("the third plan in a minute is refused");
        assert_eq!(refusal["error"], RATE_LIMITED);
        assert_eq!(refusal["retry_after_ms"], 59_998);
        assert!(refusal["message"].as_str().is_some_and(|m| !m.is_empty()));
        // The reply on the wire keeps the whole body, and REST answers it with 429.
        let wire = error_response(&refusal.to_string());
        assert_eq!(wire, refusal);
        assert_eq!(http_status(&wire.to_string()), 429);
        // Only the model-starting tool is counted; another client has its own window.
        for tool in [Tool::FormsInspect, Tool::FormsStatus, Tool::FormsApply, Tool::FormsClose] {
            assert_eq!(rate_limited(&limiter, &registry, &app, tool, 1_003), None);
        }
        assert_eq!(rate_limited(&limiter, &registry, &token, Tool::FormsPlan, 1_003), None);
    }

    #[test]
    fn refusals_keep_their_wire_shape() {
        assert_eq!(error_response("scope_denied"), json!({"error":"scope_denied"}));
        let slack = json!({"code":"slack_target_unknown","target":"#x"}).to_string();
        assert_eq!(
            error_response(&slack),
            json!({"error":"slack_target_unknown","detail":{"code":"slack_target_unknown","target":"#x"}})
        );
        // Other JSON is a plain code string, as before.
        let other = json!({"code":"x"}).to_string();
        assert_eq!(error_response(&other), json!({"error": other}));
    }
}
