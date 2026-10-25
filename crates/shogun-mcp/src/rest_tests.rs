use super::*;
use crate::memory_api::{tool_level, ApiLevel, TokenRegistry};
use shogun_agents::approval::{ApprovalOrigin, ApprovalQueue};
use shogun_agents::entitlement::{entitlements, Entitlements, Plan, TRIAL_DURATION_MS};

/// An entitled plan (active trial) for the routing tests.
fn ent() -> Entitlements {
    shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0)
}

fn reg() -> TokenRegistry {
    let mut r = TokenRegistry::new();
    r.issue("t");
    r
}
fn req(method: Method, path: &str, token: Option<&str>) -> RestRequest {
    RestRequest {
        method,
        path: path.into(),
        token: token.map(str::to_string),
        include_low: false,
        query: None,
        search_depth: None,
        search_source_kind: None,
        search_mode: None,
        search_bound_invalid: false,
        body: None,
        from_ms: None,
        to_ms: None,
        cursor_ts: None,
        cursor_id: None,
        cursor_direction: None,
        page_size: None,
        for_generation: false,
        app_bundle_id: None,
        person_id: None,
        project_id: None,
        mail: Default::default(),
    }
}

#[test]
fn bearer_parsing() {
    assert_eq!(bearer(Some("Bearer abc123")), Some("abc123".into()));
    assert_eq!(bearer(Some("Basic abc")), None);
    // The scheme is case-insensitive (RFC 7235 §2.1, issue #779).
    assert_eq!(bearer(Some("bearer abc123")), Some("abc123".into()));
    assert_eq!(bearer(Some("BEARER  abc123 ")), Some("abc123".into()));
    assert_eq!(bearer(Some("Bearerabc123")), None);
    assert_eq!(bearer(None), None);
}

#[test]
fn unknown_path_is_404_even_with_a_token() {
    assert_eq!(
        route(&req(Method::Get, "/v1/nope", Some("t")), &reg(), &ent()),
        Routed::NotFound
    );
}

#[test]
fn wrong_method_is_405() {
    // search is GET-only
    assert_eq!(
        route(
            &req(Method::Post, "/v1/memory/search", Some("t")),
            &reg(),
            &ent()
        ),
        Routed::MethodNotAllowed
    );
}

#[test]
fn tool_endpoints_require_a_token_including_reads() {
    assert_eq!(
        route(&req(Method::Get, "/v1/memory/search", None), &reg(), &ent()),
        Routed::Unauthorized
    );
    assert_eq!(
        route(
            &req(Method::Get, "/v1/state/people", Some("wrong")),
            &reg(),
            &ent()
        ),
        Routed::Unauthorized
    );
}

#[test]
fn locked_plan_is_403_on_every_tool_endpoint_but_status_stays_open() {
    // Issue #97: Standard / expired trial → the Memory API is refused with a valid token,
    // reads included; the unauthenticated health endpoints keep answering.
    let expired = entitlements(
        Plan::Trial {
            started_at_ms: Some(0),
        },
        TRIAL_DURATION_MS,
    );
    for locked in [entitlements(Plan::Standard, 0), expired] {
        for (method, path) in [
            (Method::Get, "/v1/memory/search"),
            (Method::Get, "/v1/state/people"),
            (Method::Post, "/v1/memory/notes"),
            (Method::Post, "/v1/actions/execute"),
        ] {
            let routed = route(&req(method, path, Some("t")), &reg(), &locked);
            assert_eq!(routed, Routed::PlanLocked, "{path} must be plan-locked");
            assert_eq!(status_code(&routed), 403);
        }
        assert_eq!(
            route(&req(Method::Get, "/v1/status", None), &reg(), &locked),
            Routed::Status
        );
        assert_eq!(
            route(&req(Method::Get, "/v1/metrics", None), &reg(), &locked),
            Routed::Metrics
        );
        // no token still reads as 401, not 403 (auth first — the plan is not disclosed)
        assert_eq!(
            route(
                &req(Method::Get, "/v1/memory/search", None),
                &reg(),
                &locked
            ),
            Routed::Unauthorized
        );
    }
    // Pro passes
    let pro = entitlements(Plan::Pro, 0);
    assert!(matches!(
        route(
            &req(Method::Get, "/v1/memory/search", Some("t")),
            &reg(),
            &pro
        ),
        Routed::Read { .. }
    ));
}

#[test]
fn status_is_unauthenticated() {
    assert_eq!(
        route(&req(Method::Get, "/v1/status", None), &reg(), &ent()),
        Routed::Status
    );
    assert_eq!(status_code(&Routed::Status), 200);
}

/// FR-API-01 / #776: `shogun api status | jq -r .port` finds the port actually bound, which is
/// not 7464 when the server fell back to an ephemeral one.
#[test]
fn status_names_the_bound_port_when_the_server_knows_it() {
    assert_eq!(status_body(None), r#"{"status":"ok","service":"shogun-memory-api"}"#);
    let body: serde_json::Value = serde_json::from_str(&status_body(Some(53_447))).unwrap();
    assert_eq!(body["port"], 53_447);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["service"], "shogun-memory-api");
}

#[test]
fn metrics_is_unauthenticated_and_get_only() {
    // health endpoint: open like status (NFR-SLO-00), no capture content, localhost-bound.
    assert_eq!(
        route(&req(Method::Get, "/v1/metrics", None), &reg(), &ent()),
        Routed::Metrics
    );
    assert_eq!(status_code(&Routed::Metrics), 200);
    // still GET-only
    assert_eq!(
        route(&req(Method::Post, "/v1/metrics", Some("t")), &reg(), &ent()),
        Routed::MethodNotAllowed
    );
}

#[test]
fn read_endpoints_resolve_to_read_tools() {
    assert_eq!(
        route(
            &req(Method::Get, "/v1/memory/search", Some("t")),
            &reg(),
            &ent()
        ),
        Routed::Read {
            tool: Tool::MemorySearch,
            id: None
        }
    );
    assert_eq!(
        route(
            &req(Method::Get, "/v1/state/commitments", Some("t")),
            &reg(),
            &ent()
        ),
        Routed::Read {
            tool: Tool::StateCommitmentsList,
            id: None
        }
    );
    // trailing id selects the get variant
    assert_eq!(
        route(
            &req(Method::Get, "/v1/state/people/42", Some("t")),
            &reg(),
            &ent()
        ),
        Routed::Read {
            tool: Tool::StatePeopleGet,
            id: Some(42)
        }
    );
}

#[test]
fn write_endpoints_carry_their_levels_and_202() {
    let note = route(
        &req(Method::Post, "/v1/memory/notes", Some("t")),
        &reg(),
        &ent(),
    );
    assert_eq!(
        note,
        Routed::Write {
            tool: Tool::MemoryAppendNote,
            level: Level::L1
        }
    );
    assert_eq!(status_code(&note), 202);

    let propose = route(
        &req(Method::Post, "/v1/state/proposals", Some("t")),
        &reg(),
        &ent(),
    );
    assert_eq!(status_code(&propose), 404);
    assert_eq!(
        route(&req(Method::Post, "/v1/profile", Some("t")), &reg(), &ent()),
        Routed::Write {
            tool: Tool::ProfileSet,
            level: Level::L1
        }
    );
}

#[test]
fn voice_dictionary_routes_are_token_and_plan_gated_like_other_local_writes() {
    assert_eq!(
        route(
            &req(Method::Get, "/v1/voice_dictionary/terms", Some("t")),
            &reg(),
            &ent(),
        ),
        Routed::Read {
            tool: Tool::VoiceDictionaryList,
            id: None
        }
    );
    assert_eq!(
        route(
            &req(Method::Post, "/v1/voice_dictionary/terms/7", Some("t")),
            &reg(),
            &ent(),
        ),
        Routed::Write {
            tool: Tool::VoiceDictionaryUpdate,
            level: Level::L1
        }
    );
    assert_eq!(
        route(
            &req(Method::Post, "/v1/voice_dictionary/terms/7/delete", None),
            &reg(),
            &ent(),
        ),
        Routed::Unauthorized
    );
}

#[test]
fn web_reads_route_as_gets_and_refuse_posts() {
    for (path, tool) in [
        ("/v1/web/video_transcript", Tool::WebVideoTranscript),
        ("/v1/web/social_post", Tool::WebSocialPost),
        ("/v1/web/page", Tool::WebPage),
    ] {
        assert_eq!(
            route(&req(Method::Get, path, Some("t")), &reg(), &ent()),
            Routed::Read { tool, id: None },
            "{path}"
        );
        assert_eq!(
            route(&req(Method::Post, path, Some("t")), &reg(), &ent()),
            Routed::MethodNotAllowed,
            "{path}"
        );
        assert_eq!(route(&req(Method::Get, path, None), &reg(), &ent()), Routed::Unauthorized);
    }
}

#[test]
fn ui_language_routes_at_ui_levels() {
    assert_eq!(
        route(&req(Method::Get, "/v1/settings/language", Some("t")), &reg(), &ent()),
        Routed::Read { tool: Tool::UiLanguageGet, id: None }
    );
    assert_eq!(
        route(&req(Method::Post, "/v1/settings/language", Some("t")), &reg(), &ent()),
        Routed::Write { tool: Tool::UiLanguageSet, level: Level::L1 }
    );
    assert_eq!(
        route(&req(Method::Post, "/v1/settings/language", None), &reg(), &ent()),
        Routed::Unauthorized
    );
}

#[test]
fn translate_register_routes_at_ui_levels() {
    assert_eq!(
        route(&req(Method::Get, "/v1/translate/register", Some("t")), &reg(), &ent()),
        Routed::Read { tool: Tool::TranslateRegisterGet, id: None }
    );
    assert_eq!(
        route(&req(Method::Post, "/v1/translate/register", Some("t")), &reg(), &ent()),
        Routed::Write { tool: Tool::TranslateRegisterSet, level: Level::L1 }
    );
    assert_eq!(
        route(&req(Method::Post, "/v1/translate/register", None), &reg(), &ent()),
        Routed::Unauthorized
    );
}

#[test]
fn voice_dictionary_import_and_export_routes_match_the_single_term_routes() {
    assert_eq!(
        route(
            &req(Method::Post, "/v1/voice_dictionary/import", Some("t")),
            &reg(),
            &ent(),
        ),
        Routed::Write {
            tool: Tool::VoiceDictionaryImport,
            level: Level::L1
        }
    );
    assert_eq!(
        route(
            &req(Method::Get, "/v1/voice_dictionary/export", Some("t")),
            &reg(),
            &ent(),
        ),
        Routed::Read {
            tool: Tool::VoiceDictionaryExport,
            id: None
        }
    );
    // Import is a write: the wrong verb is a 405 like every other write route, and a call
    // without the token never reaches the backend.
    assert_eq!(
        route(
            &req(Method::Get, "/v1/voice_dictionary/import", Some("t")),
            &reg(),
            &ent(),
        ),
        Routed::MethodNotAllowed
    );
    assert_eq!(
        route(
            &req(Method::Post, "/v1/voice_dictionary/import", None),
            &reg(),
            &ent(),
        ),
        Routed::Unauthorized
    );
}

#[test]
fn meeting_audio_routes_keep_retry_human_confirmed() {
    assert_eq!(route(&req(Method::Get, "/v1/meeting/system_audio", Some("t")), &reg(), &ent()),
        Routed::Read { tool: Tool::MeetingSystemAudioStatus, id: None });
    assert_eq!(route(&req(Method::Post, "/v1/meeting/system_audio/retry", Some("t")), &reg(), &ent()),
        Routed::Write { tool: Tool::MeetingSystemAudioRetry, level: Level::L2 });
    assert_eq!(route(&req(Method::Get, "/v1/meeting/system_audio/retry", Some("t")), &reg(), &ent()), Routed::MethodNotAllowed);
}

#[test]
fn meeting_microphone_routes_at_ui_levels() {
    assert_eq!(
        route(
            &req(Method::Get, "/v1/meeting/microphone", Some("t")),
            &reg(),
            &ent(),
        ),
        Routed::Read {
            tool: Tool::MeetingMicrophoneGet,
            id: None,
        }
    );
    assert_eq!(
        route(
            &req(Method::Post, "/v1/meeting/microphone", Some("t")),
            &reg(),
            &ent(),
        ),
        Routed::Write {
            tool: Tool::MeetingMicrophoneSet,
            level: Level::L1,
        }
    );
}

#[test]
fn meeting_recap_edit_routes_as_an_l1_write_beside_the_read() {
    assert_eq!(
        route(&req(Method::Post, "/v1/meeting/recap/edit", Some("t")), &reg(), &ent()),
        Routed::Write { tool: Tool::MeetingRecapEdit, level: Level::L1 }
    );
    assert_eq!(
        route(&req(Method::Get, "/v1/meeting/recap/edit", Some("t")), &reg(), &ent()),
        Routed::MethodNotAllowed
    );
    // The read by id is untouched by the new segment.
    assert_eq!(
        route(&req(Method::Get, "/v1/meeting/recap/7", Some("t")), &reg(), &ent()),
        Routed::Read { tool: Tool::MeetingRecapGet, id: Some(7) }
    );
}

#[test]
fn meeting_search_and_delete_route_as_a_read_and_an_l1_write() {
    assert_eq!(
        route(&req(Method::Get, "/v1/meeting/search", Some("t")), &reg(), &ent()),
        Routed::Read { tool: Tool::MeetingSearch, id: None }
    );
    assert_eq!(
        route(&req(Method::Post, "/v1/meeting/7/delete", Some("t")), &reg(), &ent()),
        Routed::Write { tool: Tool::MeetingDelete, level: Level::L1 }
    );
    assert_eq!(
        route(&req(Method::Get, "/v1/meeting/7/delete", Some("t")), &reg(), &ent()),
        Routed::MethodNotAllowed
    );
    assert_eq!(
        route(&req(Method::Post, "/v1/meeting/0/delete", Some("t")), &reg(), &ent()),
        Routed::NotFound
    );
    // The path id reaches the backend as the same `session_id` body the MCP face sends.
    assert_eq!(
        super::rest_routing::mail_write_body(Tool::MeetingDelete, "/v1/meeting/7/delete", "").as_deref(),
        Some(r#"{"session_id":7}"#)
    );
}

#[test]
fn meeting_recap_prefs_route_at_ui_levels() {
    assert_eq!(
        route(&req(Method::Get, "/v1/meeting/recap_prefs", Some("t")), &reg(), &ent()),
        Routed::Read { tool: Tool::MeetingRecapPrefsGet, id: None }
    );
    assert_eq!(
        route(&req(Method::Post, "/v1/meeting/recap_prefs", Some("t")), &reg(), &ent()),
        Routed::Write { tool: Tool::MeetingRecapPrefsSet, level: Level::L1 }
    );
}

#[test]
fn meeting_start_route_is_l2() {
    assert_eq!(
        route(
            &req(Method::Post, "/v1/meeting/start", Some("t")),
            &reg(),
            &ent(),
        ),
        Routed::Write {
            tool: Tool::MeetingRecordNow,
            level: Level::L2,
        }
    );
}

#[test]
fn meeting_start_status_is_a_read_by_id() {
    assert_eq!(
        route(
            &req(Method::Get, "/v1/meeting/start/7", Some("t")),
            &reg(),
            &ent(),
        ),
        Routed::Read {
            tool: Tool::MeetingStartStatus,
            id: Some(7),
        }
    );
    assert_eq!(
        route(
            &req(Method::Get, "/v1/meeting/start/0", Some("t")),
            &reg(),
            &ent(),
        ),
        Routed::NotFound
    );
    assert_eq!(
        route(
            &req(Method::Post, "/v1/meeting/start/7", Some("t")),
            &reg(),
            &ent(),
        ),
        Routed::MethodNotAllowed
    );
}

#[test]
fn voice_microphone_routes_at_ui_levels() {
    assert_eq!(
        route(
            &req(Method::Get, "/v1/voice/microphone", Some("t")),
            &reg(),
            &ent(),
        ),
        Routed::Read {
            tool: Tool::VoiceMicrophoneGet,
            id: None,
        }
    );
    assert_eq!(
        route(
            &req(Method::Post, "/v1/voice/microphone", Some("t")),
            &reg(),
            &ent(),
        ),
        Routed::Write {
            tool: Tool::VoiceMicrophoneSet,
            level: Level::L1,
        }
    );
    // Same token + plan gate as every other Memory API face.
    assert_eq!(
        route(
            &req(Method::Post, "/v1/voice/microphone", None),
            &reg(),
            &ent(),
        ),
        Routed::Unauthorized
    );
}

#[test]
fn whoami_endpoint_is_a_token_protected_structured_read() {
    assert_eq!(
        route(
            &req(Method::Get, "/v1/profile/whoami", Some("t")),
            &reg(),
            &ent()
        ),
        Routed::Read {
            tool: Tool::ProfileWhoami,
            id: None
        }
    );
    assert_eq!(
        route(
            &req(Method::Get, "/v1/profile/whoami", None),
            &reg(),
            &ent()
        ),
        Routed::Unauthorized
    );
}

#[test]
fn actions_execute_routes_to_action() {
    assert_eq!(
        route(
            &req(Method::Post, "/v1/actions/execute", Some("t")),
            &reg(),
            &ent()
        ),
        Routed::Action
    );
    assert_eq!(status_code(&Routed::Action), 202);
}

#[test]
fn trailing_slash_is_tolerated() {
    assert_eq!(
        route(
            &req(Method::Get, "/v1/state/projects/", Some("t")),
            &reg(),
            &ent()
        ),
        Routed::Read {
            tool: Tool::StateProjectsList,
            id: None
        }
    );
}

#[test]
fn respond_renders_status_and_json_body() {
    let tokens = reg();
    // unauthenticated status
    let (s, b) = respond(&req(Method::Get, "/v1/status", None), &tokens, &ent());
    assert_eq!(s, 200);
    assert!(b.contains("shogun-memory-api"));
    // authed read → 200 with tool + empty results
    let (s, b) = respond(
        &req(Method::Get, "/v1/memory/search", Some("t")),
        &tokens,
        &ent(),
    );
    assert_eq!(s, 200);
    assert!(b.contains("\"tool\":\"memory.search\""));
    assert!(b.contains("\"results\":[]"));
    // missing token → 401
    let (s, b) = respond(
        &req(Method::Get, "/v1/memory/search", None),
        &tokens,
        &ent(),
    );
    assert_eq!(s, 401);
    assert!(b.contains("unauthorized"));
    // write → 202 with level
    let (s, b) = respond(
        &req(Method::Post, "/v1/memory/notes", Some("t")),
        &tokens,
        &ent(),
    );
    assert_eq!(s, 202);
    assert!(b.contains("\"level\":\"L1\""));
}

#[test]
fn respond_with_backend_returns_data_confidence_filtered() {
    use crate::backend::{MemoryBackend, ReadItem};

    struct Fake;
    impl MemoryBackend for Fake {
        fn read(&self, _tool: Tool, _params: &crate::backend::ReadParams) -> Vec<ReadItem> {
            vec![
                ReadItem::new("high", 0.9),   // included, not possibly
                ReadItem::new("medium", 0.6), // included, possibly
                ReadItem::new("low", 0.3),    // excluded by default
            ]
        }
    }
    let tokens = reg();

    // default: low excluded, medium flagged possibly
    let (s, b) = respond_with(
        &req(Method::Get, "/v1/state/people", Some("t")),
        &tokens,
        &ent(),
        &Fake,
    );
    assert_eq!(s, 200);
    assert!(b.contains("\"text\":\"high\""));
    assert!(b.contains(r#""text":"medium","confidence":0.6,"possibly":true"#));
    assert!(!b.contains("\"low\""), "low confidence excluded by default");

    // include_low pulls the low one in
    let with_low = RestRequest {
        include_low: true,
        ..req(Method::Get, "/v1/state/people", Some("t"))
    };
    let (_, b2) = respond_with(&with_low, &tokens, &ent(), &Fake);
    assert!(b2.contains("\"text\":\"low\""));
}

#[test]
fn respond_with_still_enforces_auth_and_404() {
    use crate::backend::StubBackend;
    let tokens = reg();
    let (s, _) = respond_with(
        &req(Method::Get, "/v1/state/people", None),
        &tokens,
        &ent(),
        &StubBackend,
    );
    assert_eq!(s, 401, "no token still 401 even with a backend");
    let (s, _) = respond_with(
        &req(Method::Get, "/v1/nope", Some("t")),
        &tokens,
        &ent(),
        &StubBackend,
    );
    assert_eq!(s, 404);
}

/// The REST face must be served by the shared voice settings store, not by the DB backend — a
/// `StubBackend` read would render an empty item list rather than a selection (#246).
#[test]
fn voice_microphone_read_is_served_by_the_shared_store_not_the_backend() {
    use crate::backend::StubBackend;
    let (status, body) = respond_with(
        &req(Method::Get, "/v1/voice/microphone", Some("t")),
        &reg(),
        &ent(),
        &StubBackend,
    );
    assert_eq!(status, 200);
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert!(
        parsed.get("microphone").is_some(),
        "expected the shared store's selection, got {body}"
    );
}

/// A malformed device fails visibly with the reason, and never silently resets to the default
/// input. Parsing rejects the body before anything is persisted, so this touches no file.
#[test]
fn a_malformed_voice_microphone_write_is_a_400_with_a_reason() {
    use crate::backend::StubBackend;
    for body in [r#"{"microphone":7}"#, r#"{}"#, "not json"] {
        let request = RestRequest {
            body: Some(body.to_string()),
            ..req(Method::Post, "/v1/voice/microphone", Some("t"))
        };
        let (status, rendered) = respond_with(&request, &reg(), &ent(), &StubBackend);
        assert_eq!(status, 400, "body {body} should be refused");
        let parsed: serde_json::Value = serde_json::from_str(&rendered).expect("json body");
        assert_eq!(parsed["error"], "voice_microphone_request_failed");
        assert!(
            parsed["reason"].as_str().is_some_and(|r| !r.is_empty()),
            "the caller has to see why: {rendered}"
        );
    }
}

#[test]
fn write_errors_map_to_client_or_server_status_by_shape() {
    use crate::backend::{MemoryBackend, ReadItem, WriteResult};

    struct Failing(&'static str);
    impl MemoryBackend for Failing {
        fn read(&self, _tool: Tool, _params: &crate::backend::ReadParams) -> Vec<ReadItem> {
            Vec::new()
        }
        fn write(&self, _tool: Tool, _body: &str) -> WriteResult {
            Err(self.0.to_string())
        }
    }
    let tokens = reg();
    let post = |backend: &Failing| {
        respond_with(
            &req(Method::Post, "/v1/memory/notes", Some("t")),
            &tokens,
            &ent(),
            backend,
        )
    };
    // A body the caller can fix is a 400 — a blanket 500 invites endless retries of a request
    // the backend can never accept.
    let (status, body) = post(&Failing("expected {\"enabled\":bool}"));
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("write_failed"));
    // Profile validation failures are caller-fixable too; avoid retrying malformed settings as
    // server faults.
    for reason in [
        "profile.role must be a string",
        "profile field exceeds 4096 characters",
        "profile field contains control characters",
    ] {
        let (status, body) = post(&Failing(reason));
        assert_eq!(status, 400, "{reason}: {body}");
    }
    // Issue #779: the fixed validation refusals the backend gives schedules, skills, the capture
    // pause and export folders are 400s with their reason, as MCP returns them.
    for reason in [
        "at must be HH:MM",
        "days must be like mon,wed,fri | weekdays | daily",
        "Resume time must be within the next 26 hours of a pause.",
        "Invalid capture pause request.",
        "that folder is not an export folder",
        "no export folder is set",
        "missing skill",
        "format must be md or csv",
        "name must be lowercase letters, digits and -, up to 48",
    ] {
        let (status, body) = post(&Failing(reason));
        assert_eq!(status, 400, "{reason}: {body}");
        assert!(body.contains(reason), "the reason reaches the caller: {body}");
    }
    // A name that is already taken is a conflict, not a malformed body.
    for reason in ["a skill with this name already exists", "another skill already uses this name"] {
        let (status, _) = post(&Failing(reason));
        assert_eq!(status, 409, "{reason}");
    }
    // A store that cannot take the write right now is a retryable server error.
    let (status, body) = post(&Failing("memory store unavailable"));
    assert_eq!((status, body.as_str()), (500, r#"{"error":"write_failed"}"#));
    // Visual recall Off committed but its saved screens are not deleted yet: retryable, and the
    // caller must be able to tell it from "Off failed", so the reason is shown.
    let reason = "purge_pending: visual recall is off, but saved screens could not be deleted; retry to delete them";
    let (status, body) = post(&Failing(reason));
    assert_eq!(status, 503, "{body}");
    assert!(body.contains(reason), "the reason reaches the caller: {body}");
    // A missing row is a 404, not a server fault.
    let (status, _) = post(&Failing("lesson not found"));
    assert_eq!(status, 404);
    // A genuine backend failure stays a 500.
    let (status, body) = post(&Failing("append_note failed"));
    assert_eq!(status, 500);
    assert_eq!(body, r#"{"error":"write_failed"}"#);
    let (status, body) = post(&Failing("database at /private/secret.db is locked"));
    assert_eq!(status, 500);
    assert_eq!(body, r#"{"error":"write_failed"}"#);
}

#[test]
fn search_unavailable_is_a_server_error() {
    use crate::backend::StubBackend;

    let mut request = req(Method::Get, "/v1/memory/search", Some("t"));
    request.query = Some("q".into());
    let (status, body) = respond_with(&request, &reg(), &ent(), &StubBackend);
    assert_eq!(status, 500);
    assert_eq!(body, r#"{"error":"search_unavailable"}"#);
}

#[test]
fn search_response_keeps_legacy_envelope_with_federated_metadata() {
    use crate::backend::{
        MemoryBackend, MemorySearchResponse, MemorySearchResult, ReadItem, SearchDepthV1,
        SearchRequestV1, SearchTimeRangeV1,
    };

    struct SearchBackend;
    impl MemoryBackend for SearchBackend {
        fn read(&self, _tool: Tool, _params: &crate::backend::ReadParams) -> Vec<ReadItem> {
            Vec::new()
        }

        fn search(
            &self,
            request: &SearchRequestV1,
        ) -> Result<MemorySearchResponse, crate::backend::MemorySearchError> {
            assert_eq!(request.depth, SearchDepthV1::All);
            assert_eq!(
                request.range,
                Some(SearchTimeRangeV1 {
                    from_ms: 10,
                    to_ms: 20,
                })
            );
            Ok(MemorySearchResponse {
                tool: "memory.search".into(),
                results: vec![MemorySearchResult {
                    text: "legacy text".into(),
                    source_kind: "derived:period_summary".into(),
                    source_id: "period_summaries:7".into(),
                    provenance: vec!["event:4".into()],
                    title: "period".into(),
                    excerpt: "derived answer".into(),
                    confidence: 0.6,
                    possibly: true,
                    score: 0.9,
                    when: None,
                    place: None,
                    snippet: None,
                    open: None,
                }],
                answerability: crate::backend::AnswerabilityV1 {
                    answerable: true,
                    reason: crate::backend::AnswerabilityReasonV1::Answerable,
                },
                requested_depth: SearchDepthV1::All,
                effective_depth: SearchDepthV1::All,
                requested_range: request.range,
                resolved_range: request.range.unwrap(),
                lexical_scope: "warm".into(),
                complete: true,
                cold_partitions_visited: 1,
                cold_vectors_scanned: 2,
                elapsed_ms: 3,
            })
        }
    }

    let mut request = req(Method::Get, "/v1/memory/search", Some("t"));
    request.query = Some("q".into());
    request.search_depth = Some("all".into());
    request.from_ms = Some(10);
    request.to_ms = Some(20);
    let (status, body) = respond_with(&request, &reg(), &ent(), &SearchBackend);
    assert_eq!(status, 200);
    let payload: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(payload["tool"], "memory.search");
    assert_eq!(payload["results"][0]["text"], "legacy text");
    assert_eq!(payload["results"][0]["provenance"][0], "event:4");
    assert_eq!(payload["cold_partitions_visited"], 1);
    assert_eq!(payload["answerability"]["reason"], "answerable");
}

#[test]
fn search_rejects_a_present_malformed_rest_bound() {
    use crate::backend::StubBackend;

    let mut request = req(Method::Get, "/v1/memory/search", Some("t"));
    request.query = Some("q".into());
    request.search_bound_invalid = true;
    let (status, body) = respond_with(&request, &reg(), &ent(), &StubBackend);
    assert_eq!(status, 400);
    assert_eq!(body, r#"{"error":"invalid_search_range"}"#);
}

#[test]
fn search_rejects_unknown_rest_source_kind_stably() {
    use crate::backend::StubBackend;

    let mut request = req(Method::Get, "/v1/memory/search", Some("t"));
    request.query = Some("q".into());
    request.search_source_kind = Some("Brief".into());
    let (status, body) = respond_with(&request, &reg(), &ent(), &StubBackend);

    assert_eq!(status, 400);
    assert_eq!(body, r#"{"error":"invalid_search_source_kind"}"#);
}

#[test]
fn search_rejects_overlong_unicode_rest_query_stably() {
    use crate::backend::StubBackend;

    let mut request = req(Method::Get, "/v1/memory/search", Some("t"));
    request.query = Some("界".repeat(4_097));
    let (status, body) = respond_with(&request, &reg(), &ent(), &StubBackend);

    assert_eq!(status, 400);
    assert_eq!(body, r#"{"error":"search_query_too_long"}"#);
}

#[test]
fn act_local_never_reports_a_run_it_did_not_do() {
    // Issue #779: without a backend, `act` cannot run a local action. It must say so (501) and
    // never claim `executed`, and it never touches the approval queue.
    let mut q = ApprovalQueue::new();
    let (s, b) = act(
        Some(r#"{"kind":"local_search","query":"budget"}"#),
        0,
        &mut q,
        ApprovalOrigin::Api,
        ApprovalSurface::Present,
    );
    assert_eq!(s, 501, "{b}");
    assert!(b.contains("local_action_unavailable"), "{b}");
    assert!(!b.contains("executed"), "{b}");
    assert_eq!(q.pending_len(), 0, "a local action never enqueues");
}

/// Issue #779: `local_search` runs the real search and returns its results; the screen-side
/// kinds are refused with `local_action_unavailable`; a send is left to the approval queue.
#[test]
fn local_answer_searches_refuses_screen_kinds_and_leaves_sends() {
    use crate::backend::{MemoryBackend, MemorySearchResponse, ReadItem, ReadParams, SearchRequestV1};

    struct Searching;
    impl MemoryBackend for Searching {
        fn read(&self, _tool: Tool, _params: &ReadParams) -> Vec<ReadItem> {
            Vec::new()
        }
        fn search(
            &self,
            request: &SearchRequestV1,
        ) -> Result<MemorySearchResponse, crate::backend::MemorySearchError> {
            assert_eq!(request.query, "budget");
            Err(crate::backend::MemorySearchError::Unavailable)
        }
    }
    let (status, body) =
        local_answer(Some(r#"{"kind":"local_search","query":"budget"}"#), &Searching).expect("local");
    assert_eq!((status, body.as_str()), (500, r#"{"error":"search_unavailable"}"#), "the search really ran");
    for body in [
        r#"{"kind":"open_app","bundle_id":"com.example.App"}"#,
        r#"{"kind":"reveal_file","path":"/tmp/x"}"#,
        r#"{"kind":"show_notification","text":"hi"}"#,
        r#"{"kind":"copy_to_clipboard","text":"hi"}"#,
    ] {
        let (status, answer) = local_answer(Some(body), &Searching).expect("local kind");
        assert_eq!(status, 501, "{body}: {answer}");
        assert!(answer.contains("local_action_unavailable") && !answer.contains("executed"), "{answer}");
    }
    assert!(local_answer(Some(r#"{"kind":"send_email","to":"a@b.c"}"#), &Searching).is_none());
    assert!(local_answer(Some("not json"), &Searching).is_none());
}

#[test]
fn act_send_enqueues_pending_l3_approval() {
    let mut q = ApprovalQueue::new();
    let (s, b) = act(
        Some(r#"{"kind":"send_email","to":"a@b.com","subject":"Hi","body":"hello"}"#),
        1000,
        &mut q,
        ApprovalOrigin::Api,
        ApprovalSurface::Present,
    );
    assert_eq!(s, 202);
    assert!(b.contains("\"pending\":true"));
    assert!(b.contains("\"approval_id\":"));
    assert!(b.contains("\"level\":\"L3\""));
    let approval_id = q.pending_ids()[0];
    assert_eq!(
        q.preview(approval_id).unwrap().route,
        shogun_agents::approval::Route::DirectMcp
    );

    assert_eq!(
        q.pending_len(),
        1,
        "the send awaits UI confirmation (FR-API-04)"
    );
}

#[test]
fn act_rejects_missing_and_malformed_bodies() {
    let mut q = ApprovalQueue::new();
    assert_eq!(
        act(
            None,
            0,
            &mut q,
            ApprovalOrigin::Api,
            ApprovalSurface::Present
        )
        .0,
        400
    );
    assert_eq!(
        act(
            Some("not json"),
            0,
            &mut q,
            ApprovalOrigin::Api,
            ApprovalSurface::Present
        )
        .0,
        400
    );
    assert_eq!(
        act(
            Some(r#"{"kind":"unknown_thing"}"#),
            0,
            &mut q,
            ApprovalOrigin::Api,
            ApprovalSurface::Present
        )
        .0,
        400
    );
    // a send kind missing a required field is also rejected
    assert_eq!(
        act(
            Some(r#"{"kind":"send_email"}"#),
            0,
            &mut q,
            ApprovalOrigin::Api,
            ApprovalSurface::Present
        )
        .0,
        400
    );
}

#[test]
fn act_refuses_rows_the_persisted_store_would_reject_on_load() {
    // Empty destination: the load-time row check (`action_from_wire`) rejects it, so an
    // enqueue that accepted it would brick the persisted store for every face.
    let mut q = ApprovalQueue::new();
    let (s, b) = act(
        Some(r#"{"kind":"post_message","channel":"","body":"x"}"#),
        0,
        &mut q,
        ApprovalOrigin::Api,
        ApprovalSurface::Present,
    );
    assert_eq!(s, 400);
    assert!(b.contains("bad_action_request"));
    assert_eq!(q.pending_len(), 0);

    // Oversized body (> 256 KiB) is refused for the same reason.
    let big = "x".repeat(256 * 1024 + 1);
    let (s, _) = act(
        Some(&format!(
            r#"{{"kind":"post_message","channel":"c","body":"{big}"}}"#
        )),
        0,
        &mut q,
        ApprovalOrigin::Api,
        ApprovalSurface::Present,
    );
    assert_eq!(s, 400);
    assert_eq!(q.pending_len(), 0);
}

#[test]
fn act_refuses_enqueue_beyond_max_pending() {
    let mut q = ApprovalQueue::new();
    let body = r#"{"kind":"post_message","channel":"C0123ABCD","body":"x"}"#;
    for _ in 0..64 {
        let (s, _) = act(
            Some(body),
            0,
            &mut q,
            ApprovalOrigin::Api,
            ApprovalSurface::Present,
        );
        assert_eq!(s, 202);
    }
    let (s, b) = act(
        Some(body),
        0,
        &mut q,
        ApprovalOrigin::Api,
        ApprovalSurface::Present,
    );
    assert_eq!(s, 429);
    assert!(b.contains("approval_queue_full"));
    assert_eq!(q.pending_len(), 64, "the 65th enqueue must not be stored");
}

#[test]
fn a_send_is_refused_outright_when_nothing_can_confirm_it() {
    // Headless (`shogun-api` / `shogun-mcp` standalone): enqueuing here would look like it
    // worked and then expire in silence. Say so instead — invariant 4 was never at risk,
    // invariant 6's "the API face behaves like the human one" is what this protects.
    let mut q = ApprovalQueue::new();
    let (status, body) = act(
        Some(r#"{"kind":"send_email","to":"a@b.com","subject":"Hi","body":"hello"}"#),
        1000,
        &mut q,
        ApprovalOrigin::Api,
        ApprovalSurface::Absent,
    );
    assert_eq!(status, 501);
    assert!(body.contains("no_approval_surface"), "{body}");
    assert_eq!(q.pending_len(), 0, "nothing may be stranded in the queue");

    // A local search does not need a confirm surface: it is answered before the queue
    // (`local_answer`, issue #779), never with `no_approval_surface`.
    let answer = local_answer(
        Some(r#"{"kind":"local_search","query":"x"}"#),
        &crate::backend::StubBackend,
    )
    .expect("a local action is answered before the queue");
    assert!(!answer.1.contains("no_approval_surface"), "{}", answer.1);
    assert_eq!(q.pending_len(), 0);
}

#[test]
fn json_escape_handles_quotes_and_controls() {
    assert_eq!(json_escape(r#"a"b\c"#), "a\\\"b\\\\c");
    assert_eq!(json_escape("line\nbreak"), "line\\nbreak");
}

#[test]
fn backup_retention_endpoints_resolve() {
    assert_eq!(
        route(&req(Method::Get, "/v1/backup/status", Some("t")), &reg(), &ent()),
        Routed::Read {
            tool: Tool::BackupStatus,
            id: None
        }
    );
    assert_eq!(
        route(&req(Method::Post, "/v1/backup/retention", Some("t")), &reg(), &ent()),
        Routed::Write {
            tool: Tool::BackupSetRetention,
            level: Level::L1
        }
    );
    // A GET cannot change the setting, and a POST cannot read it.
    assert_eq!(
        route(&req(Method::Get, "/v1/backup/retention", Some("t")), &reg(), &ent()),
        Routed::MethodNotAllowed
    );
    assert_eq!(
        route(&req(Method::Post, "/v1/backup/status", Some("t")), &reg(), &ent()),
        Routed::MethodNotAllowed
    );
}

#[test]
fn visual_recall_endpoints_resolve() {
    assert_eq!(
        route(
            &req(Method::Get, "/v1/visual_recall/status", Some("t")),
            &reg(),
            &ent()
        ),
        Routed::Read {
            tool: Tool::VisualRecallStatus,
            id: None
        }
    );
    assert_eq!(
        route(
            &req(Method::Post, "/v1/visual_recall/enabled", Some("t")),
            &reg(),
            &ent()
        ),
        Routed::Write {
            tool: Tool::VisualRecallSetEnabled,
            level: Level::L3
        }
    );
    assert_eq!(
        route(
            &req(Method::Post, "/v1/visual_recall/retention", Some("t")),
            &reg(),
            &ent()
        ),
        Routed::Write {
            tool: Tool::VisualRecallSetRetention,
            level: Level::L3
        }
    );
    assert_eq!(
        route(
            &req(Method::Get, "/v1/visual_recall/frames/search", Some("t")),
            &reg(),
            &ent()
        ),
        Routed::Read {
            tool: Tool::VisualRecallSearchFrames,
            id: None
        }
    );
    assert_eq!(
        route(
            &req(Method::Get, "/v1/visual_recall/frames/12", Some("t")),
            &reg(),
            &ent()
        ),
        Routed::Read {
            tool: Tool::VisualRecallGetFrame,
            id: Some(12)
        }
    );
    assert_eq!(
        route(
            &req(
                Method::Post,
                "/v1/visual_recall/frames/12/rescan",
                Some("t")
            ),
            &reg(),
            &ent()
        ),
        Routed::Write {
            tool: Tool::VisualRecallRescanFrame,
            level: Level::L1
        }
    );
    assert_eq!(
        route(
            &req(Method::Get, "/v1/visual_recall/frames/12/rescan", Some("t")),
            &reg(),
            &ent()
        ),
        Routed::MethodNotAllowed
    );
    assert_eq!(
        route(
            &req(Method::Post, "/v1/visual_recall/frames/delete", Some("t")),
            &reg(),
            &ent()
        ),
        Routed::Write {
            tool: Tool::VisualRecallDeleteFrame,
            level: Level::L1
        }
    );
}

#[test]
fn the_calendar_endpoint_resolves_as_a_read() {
    assert_eq!(
        route(
            &req(Method::Get, "/v1/calendar/upcoming", Some("t")),
            &reg(),
            &ent()
        ),
        Routed::Read {
            tool: Tool::CalendarUpcoming,
            id: None
        }
    );
    assert_eq!(
        route(
            &req(Method::Post, "/v1/calendar/upcoming", Some("t")),
            &reg(),
            &ent()
        ),
        Routed::MethodNotAllowed
    );
}

#[test]
fn the_wrap_endpoint_resolves_as_a_read() {
    // Issue #10 (invariant 6): the Evening Wrap is a plain authorized GET, like every read.
    assert_eq!(
        route(
            &req(Method::Get, "/v1/memory/wrap", Some("t")),
            &reg(),
            &ent()
        ),
        Routed::Read {
            tool: Tool::MemoryGetWrap,
            id: None
        }
    );
    assert_eq!(
        route(
            &req(Method::Post, "/v1/memory/wrap", Some("t")),
            &reg(),
            &ent()
        ),
        Routed::MethodNotAllowed
    );
    assert_eq!(
        route(&req(Method::Get, "/v1/memory/wrap", None), &reg(), &ent()),
        Routed::Unauthorized
    );
}

#[test]
fn capture_exclusions_rest_contract_is_read_plus_l3_replace() {
    assert_eq!(
        route(
            &req(Method::Get, "/v1/capture_exclusions", Some("t")),
            &reg(),
            &ent()
        ),
        Routed::Read {
            tool: Tool::CaptureExclusionsGet,
            id: None
        }
    );
    assert_eq!(
        route(
            &req(Method::Post, "/v1/capture_exclusions", Some("t")),
            &reg(),
            &ent()
        ),
        Routed::Write {
            tool: Tool::CaptureExclusionsSet,
            level: Level::L3
        }
    );
}

#[test]
fn capture_exclusions_rest_replace_returns_the_saved_lists() {
    use crate::backend::{MemoryBackend, ReadItem, ReadParams, WriteResult};

    struct CaptureBackend;
    impl MemoryBackend for CaptureBackend {
        fn read(&self, _tool: Tool, _params: &ReadParams) -> Vec<ReadItem> {
            Vec::new()
        }

        fn write(&self, tool: Tool, _body: &str) -> WriteResult {
            assert_eq!(tool, Tool::CaptureExclusionsSet);
            Ok(None)
        }

        fn read_structured(&self, tool: Tool, _params: &ReadParams) -> Option<String> {
            (tool == Tool::CaptureExclusionsGet).then(|| {
                r#"{"apps":["com.example.Secret"],"websites":["private.example"]}"#.to_string()
            })
        }
    }

    let request = RestRequest {
        body: Some(r#"{"apps":["com.example.Secret"],"websites":["private.example"]}"#.into()),
        ..req(Method::Post, "/v1/capture_exclusions", Some("t"))
    };
    let (status, body) = respond_with(&request, &reg(), &ent(), &CaptureBackend);
    let payload: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, 202);
    assert_eq!(payload["apps"][0], "com.example.Secret");
    assert_eq!(payload["websites"][0], "private.example");
}

#[test]
fn skill_routes_list_run_and_poll() {
    // Issue #251: GET /v1/skills, POST /v1/skills/run, GET /v1/skills/run/{id}.
    assert_eq!(
        route(&req(Method::Get, "/v1/skills", Some("t")), &reg(), &ent()),
        Routed::Read { tool: Tool::SkillsList, id: None }
    );
    assert_eq!(
        route(&req(Method::Post, "/v1/skills/run", Some("t")), &reg(), &ent()),
        Routed::Write { tool: Tool::SkillsRun, level: Level::L1 }
    );
    assert_eq!(
        route(&req(Method::Get, "/v1/skills/run/7", Some("t")), &reg(), &ent()),
        Routed::Read { tool: Tool::SkillsRunStatus, id: Some(7) }
    );
    assert_eq!(
        route(&req(Method::Get, "/v1/skills/run", Some("t")), &reg(), &ent()),
        Routed::MethodNotAllowed
    );
    assert_eq!(
        route(&req(Method::Post, "/v1/skills", Some("t")), &reg(), &ent()),
        Routed::Write { tool: Tool::SkillsSave, level: Level::L1 }
    );
}

#[test]
fn support_reports_is_a_get() {
    assert_eq!(
        route(&req(Method::Get, "/v1/support/reports", Some("t")), &reg(), &ent()),
        Routed::Read { tool: Tool::SupportReportsList, id: None }
    );
    assert_eq!(
        route(&req(Method::Post, "/v1/support/reports", Some("t")), &reg(), &ent()),
        Routed::MethodNotAllowed
    );
}

#[test]
fn schedule_routes_list_add_and_remove() {
    assert_eq!(
        route(&req(Method::Get, "/v1/schedules", Some("t")), &reg(), &ent()),
        Routed::Read { tool: Tool::SchedulesList, id: None }
    );
    assert_eq!(
        route(&req(Method::Post, "/v1/schedules", Some("t")), &reg(), &ent()),
        Routed::Write { tool: Tool::SchedulesAdd, level: Level::L1 }
    );
    assert_eq!(
        route(&req(Method::Post, "/v1/schedules/remove", Some("t")), &reg(), &ent()),
        Routed::Write { tool: Tool::SchedulesRemove, level: Level::L1 }
    );
}

#[test]
fn recap_track_is_an_l1_post() {
    // FR-MT-17: POST /v1/meeting/recap/track is the Recap's [Track] (invariant 6).
    assert_eq!(
        route(&req(Method::Post, "/v1/meeting/recap/track", Some("t")), &reg(), &ent()),
        Routed::Write {
            tool: Tool::MeetingRecapTrack,
            level: Level::L1
        }
    );
    assert_eq!(
        route(&req(Method::Get, "/v1/meeting/recap/track", Some("t")), &reg(), &ent()),
        Routed::MethodNotAllowed
    );
}

#[test]
fn world_model_endpoints_resolve_at_the_pane_levels() {
    // Issue #290 / invariant 6: export is a read; update and delete are L1 writes. All three are
    // matched before the generic `state/<noun>/<id>` arms.
    assert_eq!(
        route(&req(Method::Get, "/v1/state/export", Some("t")), &reg(), &ent()),
        Routed::Read { tool: Tool::StateExport, id: None }
    );
    for (path, tool) in [("/v1/state/update", Tool::StateUpdate), ("/v1/state/delete", Tool::StateDelete)] {
        assert_eq!(
            route(&req(Method::Post, path, Some("t")), &reg(), &ent()),
            Routed::Write { tool, level: Level::L1 }
        );
        assert_eq!(
            route(&req(Method::Get, path, Some("t")), &reg(), &ent()),
            Routed::MethodNotAllowed
        );
    }
    assert_eq!(
        route(&req(Method::Post, "/v1/state/export", Some("t")), &reg(), &ent()),
        Routed::MethodNotAllowed
    );
}

#[test]
fn split_and_evidence_endpoints_resolve_at_the_pane_levels() {
    // FR-ST-10 / invariant 6: the split is an L1 write, the evidence it chooses from a read.
    assert_eq!(
        route(&req(Method::Post, "/v1/state/split", Some("t")), &reg(), &ent()),
        Routed::Write { tool: Tool::StateSplit, level: Level::L1 }
    );
    assert_eq!(
        route(&req(Method::Get, "/v1/state/split", Some("t")), &reg(), &ent()),
        Routed::MethodNotAllowed
    );
    for table in ["people", "projects"] {
        let path = format!("/v1/state/{table}/7/evidence");
        assert_eq!(
            route(&req(Method::Get, &path, Some("t")), &reg(), &ent()),
            Routed::Read { tool: Tool::StateSplitEvidence, id: Some(7) }
        );
        assert_eq!(
            super::rest_routing::mail_path_key(Tool::StateSplitEvidence, &path).as_deref(),
            Some(table)
        );
    }
    for path in ["/v1/state/commitments/7/evidence", "/v1/state/people/0/evidence", "/v1/state/people/x/evidence"] {
        assert_eq!(route(&req(Method::Get, path, Some("t")), &reg(), &ent()), Routed::NotFound, "{path}");
    }
}

#[test]
fn dispute_endpoints_resolve_at_the_context_health_levels() {
    // GET /v1/state/disputes → the list read; POST /v1/state/disputes/resolve → the L1
    // decision (issue #253, invariant 6).
    assert_eq!(
        route(&req(Method::Get, "/v1/state/disputes", Some("t")), &reg(), &ent()),
        Routed::Read {
            tool: Tool::StateDisputesList,
            id: None
        }
    );
    assert_eq!(
        route(
            &req(Method::Post, "/v1/state/disputes/resolve", Some("t")),
            &reg(),
            &ent()
        ),
        Routed::Write {
            tool: Tool::StateDisputesResolve,
            level: Level::L1
        }
    );
    assert_eq!(
        route(&req(Method::Post, "/v1/state/disputes", Some("t")), &reg(), &ent()),
        Routed::MethodNotAllowed
    );
    assert_eq!(
        route(
            &req(Method::Get, "/v1/state/disputes/resolve", Some("t")),
            &reg(),
            &ent()
        ),
        Routed::MethodNotAllowed
    );
    assert_eq!(
        route(&req(Method::Get, "/v1/state/disputes", None), &reg(), &ent()),
        Routed::Unauthorized
    );
    let locked = entitlements(Plan::Standard, 0);
    assert_eq!(
        route(&req(Method::Get, "/v1/state/disputes", Some("t")), &reg(), &locked),
        Routed::PlanLocked
    );
}

#[test]
fn lessons_endpoints_resolve_at_the_learned_ui_levels() {
    // GET /v1/lessons → the list read; POST /v1/lessons/active → the L1 toggle (invariant 6).
    assert_eq!(
        route(&req(Method::Get, "/v1/lessons", Some("t")), &reg(), &ent()),
        Routed::Read {
            tool: Tool::LessonsList,
            id: None
        }
    );
    assert_eq!(
        route(
            &req(Method::Post, "/v1/lessons/active", Some("t")),
            &reg(),
            &ent()
        ),
        Routed::Write {
            tool: Tool::LessonsSetActive,
            level: Level::L1
        }
    );
    // wrong methods are 405, auth still applies, and the plan gate holds
    assert_eq!(
        route(&req(Method::Post, "/v1/lessons", Some("t")), &reg(), &ent()),
        Routed::MethodNotAllowed
    );
    assert_eq!(
        route(&req(Method::Get, "/v1/lessons", None), &reg(), &ent()),
        Routed::Unauthorized
    );
    let locked = entitlements(Plan::Standard, 0);
    assert_eq!(
        route(&req(Method::Get, "/v1/lessons", Some("t")), &reg(), &locked),
        Routed::PlanLocked
    );
}

#[test]
fn mail_endpoints_resolve_at_the_mail_pane_levels() {
    // FR-TG-30 / invariant 6: each path lands on its tool at the Mail pane's level — reads,
    // L1 for local state, L2 for the Gmail write-backs. Keys with a colon are plain segments.
    let read = |tool| Routed::Read { tool, id: None };
    let write = |tool, level| Routed::Write { tool, level };
    for (method, path, expected) in [
        (Method::Get, "/v1/mail/triage", read(Tool::MailTriageList)),
        (
            Method::Get,
            "/v1/mail/triage?bucket=other&attention=fyi",
            read(Tool::MailTriageList),
        ),
        (
            Method::Get,
            "/v1/mail/triage/gmail:18f3",
            read(Tool::MailTriageGet),
        ),
        (
            Method::Post,
            "/v1/mail/triage/gmail:18f3",
            write(Tool::MailTriageSet, Level::L1),
        ),
        (
            Method::Post,
            "/v1/mail/triage/gmail:18f3/archive",
            write(Tool::MailTriageArchive, Level::L2),
        ),
        (
            Method::Post,
            "/v1/mail/triage/gmail:18f3/unarchive",
            write(Tool::MailTriageUnarchive, Level::L2),
        ),
        (Method::Get, "/v1/mail/labels", read(Tool::MailLabelsList)),
        (
            Method::Post,
            "/v1/mail/labels",
            write(Tool::MailLabelsUpsert, Level::L1),
        ),
        (
            Method::Post,
            "/v1/mail/labels/custom:vendors/delete",
            write(Tool::MailLabelsDelete, Level::L1),
        ),
        (
            Method::Get,
            "/v1/mail/sender_rules",
            read(Tool::MailSenderRulesList),
        ),
        (
            Method::Post,
            "/v1/mail/sender_rules",
            write(Tool::MailSenderRulesAdd, Level::L1),
        ),
        (
            Method::Post,
            "/v1/mail/sender_rules/7/delete",
            write(Tool::MailSenderRulesDelete, Level::L1),
        ),
        (Method::Get, "/v1/mail/history", read(Tool::MailHistoryList)),
        // The master switch, both directions (#319 §9).
        (
            Method::Get,
            "/v1/mail/settings",
            read(Tool::MailSettingsGet),
        ),
        (
            Method::Post,
            "/v1/mail/settings",
            write(Tool::MailSettingsSet, Level::L1),
        ),
    ] {
        // The query string never reaches the router (the server strips it into RestRequest).
        let path = path.split('?').next().unwrap();
        assert_eq!(
            route(&req(method, path, Some("t")), &reg(), &ent()),
            expected,
            "{path}"
        );
    }
    // Wrong methods are 405; malformed ids / empty keys are 404; auth and plan still apply.
    assert_eq!(
        route(
            &req(Method::Post, "/v1/mail/triage", Some("t")),
            &reg(),
            &ent()
        ),
        Routed::MethodNotAllowed
    );
    assert_eq!(
        route(&req(Method::Get, "/v1/mail/history", None), &reg(), &ent()),
        Routed::Unauthorized
    );
    assert_eq!(
        route(
            &req(Method::Get, "/v1/mail/triage/gmail:x/archive", Some("t")),
            &reg(),
            &ent()
        ),
        Routed::MethodNotAllowed
    );
    assert_eq!(
        route(
            &req(Method::Post, "/v1/mail/sender_rules/x/delete", Some("t")),
            &reg(),
            &ent()
        ),
        Routed::NotFound
    );
    assert_eq!(
        route(
            &req(Method::Post, "/v1/mail/labels//delete", Some("t")),
            &reg(),
            &ent()
        ),
        Routed::NotFound
    );
    let locked = entitlements(Plan::Standard, 0);
    assert_eq!(
        route(
            &req(Method::Get, "/v1/mail/triage", Some("t")),
            &reg(),
            &locked
        ),
        Routed::PlanLocked
    );
}

#[test]
fn mail_archive_is_plan_locked_without_agent_execution_but_reads_are_not() {
    // FR-TG-31: the Gmail write-backs need Pro agent execution on top of the Memory API.
    let mut api_only = ent();
    api_only.agent_execution = false;
    for path in [
        "/v1/mail/triage/gmail:18f3/archive",
        "/v1/mail/triage/gmail:18f3/unarchive",
    ] {
        let routed = route(&req(Method::Post, path, Some("t")), &reg(), &api_only);
        assert_eq!(routed, Routed::PlanLocked, "{path}");
        assert_eq!(status_code(&routed), 403);
    }
    assert!(matches!(
        route(
            &req(Method::Get, "/v1/mail/triage", Some("t")),
            &reg(),
            &api_only
        ),
        Routed::Read { .. }
    ));
    assert!(matches!(
        route(
            &req(Method::Post, "/v1/mail/triage/gmail:18f3", Some("t")),
            &reg(),
            &api_only
        ),
        Routed::Write { .. }
    ));
}

#[test]
fn mail_path_keys_reach_the_backend_in_params_and_bodies() {
    // The REST face folds the path's key into what the backend sees, so REST and MCP hand the
    // backend the same shape (invariant 6): `key` on a read, `thread_key` / `key` / `id` in a
    // write body. The archive write comes back marked pending (FR-API-04).
    use crate::backend::{MemoryBackend, ReadItem, ReadParams, WriteResult};
    use std::sync::Mutex;
    struct Recorder {
        reads: Mutex<Vec<(Tool, ReadParams)>>,
        writes: Mutex<Vec<(Tool, String)>>,
    }
    impl MemoryBackend for Recorder {
        fn read(&self, _tool: Tool, _params: &ReadParams) -> Vec<ReadItem> {
            Vec::new()
        }
        fn read_structured(&self, tool: Tool, params: &ReadParams) -> Option<String> {
            self.reads.lock().unwrap().push((tool, params.clone()));
            Some(r#"{"threads":[]}"#.to_string())
        }
        fn write(&self, tool: Tool, body: &str) -> WriteResult {
            self.writes.lock().unwrap().push((tool, body.to_string()));
            Ok(None)
        }
    }
    let backend = Recorder {
        reads: Mutex::new(Vec::new()),
        writes: Mutex::new(Vec::new()),
    };
    let tokens = reg();

    let mut list = req(Method::Get, "/v1/mail/triage", Some("t"));
    list.mail.bucket = Some("other".into());
    list.mail.include_handled = true;
    let (status, body) = respond_with(&list, &tokens, &ent(), &backend);
    assert_eq!(status, 200);
    assert!(body.contains("\"tool\":\"mail.triage.list\""), "{body}");
    let (status, _) = respond_with(
        &req(Method::Get, "/v1/mail/triage/gmail:18f3", Some("t")),
        &tokens,
        &ent(),
        &backend,
    );
    assert_eq!(status, 200);
    let reads = backend.reads.lock().unwrap();
    assert_eq!(reads[0].0, Tool::MailTriageList);
    assert_eq!(reads[0].1.key, None);
    assert_eq!(reads[0].1.mail.bucket.as_deref(), Some("other"));
    assert!(reads[0].1.mail.include_handled);
    assert_eq!(reads[1].0, Tool::MailTriageGet);
    assert_eq!(reads[1].1.key.as_deref(), Some("gmail:18f3"));

    for (path, body, expected_tool, expected_body) in [
        (
            "/v1/mail/triage/gmail:18f3",
            Some(r#"{"handled":true}"#),
            Tool::MailTriageSet,
            r#"{"handled":true,"thread_key":"gmail:18f3"}"#,
        ),
        (
            "/v1/mail/triage/gmail:18f3/archive",
            None,
            Tool::MailTriageArchive,
            r#"{"thread_key":"gmail:18f3"}"#,
        ),
        (
            "/v1/mail/labels/custom:vendors/delete",
            None,
            Tool::MailLabelsDelete,
            r#"{"key":"custom:vendors"}"#,
        ),
        (
            "/v1/mail/sender_rules/7/delete",
            None,
            Tool::MailSenderRulesDelete,
            r#"{"id":7}"#,
        ),
        // A body-carried write with no path key is passed through untouched.
        (
            "/v1/mail/labels",
            Some(
                r#"{"name":"Vendors","definition":"Invoices from suppliers","default_bucket":"other"}"#,
            ),
            Tool::MailLabelsUpsert,
            r#"{"name":"Vendors","definition":"Invoices from suppliers","default_bucket":"other"}"#,
        ),
    ] {
        let request = RestRequest {
            body: body.map(str::to_string),
            ..req(Method::Post, path, Some("t"))
        };
        let (status, rendered) = respond_with(&request, &tokens, &ent(), &backend);
        assert_eq!(status, 202, "{path}: {rendered}");
        let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(parsed["accepted"], true, "{path}");
        assert_eq!(
            parsed["pending"],
            if expected_tool == Tool::MailTriageArchive {
                serde_json::json!(true)
            } else {
                serde_json::Value::Null
            },
            "{path}: {rendered}"
        );
        let (tool, seen) = backend.writes.lock().unwrap().last().cloned().unwrap();
        assert_eq!(tool, expected_tool, "{path}");
        assert_eq!(seen, expected_body, "{path}");
    }
}

#[test]
fn the_queue_less_rest_path_never_hands_a_writing_voice_enable_to_the_backend() {
    // Issue #654: only `server::handle` holds the approval queue. If an enable reaches the pure
    // render path, it is refused there — the backend (which would write the setting) never sees it.
    use crate::backend::{MemoryBackend, ReadItem, ReadParams, WriteResult};
    struct Panicking;
    impl MemoryBackend for Panicking {
        fn read(&self, _tool: Tool, _params: &ReadParams) -> Vec<ReadItem> {
            Vec::new()
        }
        fn write(&self, tool: Tool, body: &str) -> WriteResult {
            assert!(!body.contains("enable"), "{tool:?} enable reached the backend");
            Ok(None)
        }
    }
    let post = |body: &str| RestRequest {
        body: Some(body.into()),
        ..req(Method::Post, "/v1/writing_voice", Some("t"))
    };
    let (status, rendered) =
        respond_with(&post(r#"{"action":"enable","consent":true}"#), &reg(), &ent(), &Panicking);
    assert_eq!(status, 501, "{rendered}");
    assert!(rendered.contains("no_approval_surface"));
    // Disable / clear are direct and report the level they ran at (L1), not the tool's ceiling.
    for body in [r#"{"action":"disable"}"#, r#"{"action":"clear"}"#] {
        let (status, rendered) = respond_with(&post(body), &reg(), &ent(), &Panicking);
        assert_eq!(status, 202, "{rendered}");
        assert!(rendered.contains(r#""level":"L1""#), "{rendered}");
    }
}

#[test]
fn the_queue_less_rest_path_never_hands_a_capture_exclusion_removal_to_the_backend() {
    // Issue #657: only `server::handle` holds the approval queue. A removal reaching the pure
    // render path is refused there; an addition-only request is written directly.
    use crate::capture_exclusions_api::test_support::FakeExclusions;
    let backend = FakeExclusions::with(&["com.example.Secret"], &[]);
    let post = |body: &str| RestRequest {
        body: Some(body.into()),
        ..req(Method::Post, "/v1/capture_exclusions", Some("t"))
    };
    let (status, rendered) =
        respond_with(&post(r#"{"apps":[],"websites":[]}"#), &reg(), &ent(), &backend);
    assert_eq!(status, 501, "{rendered}");
    assert!(rendered.contains("no_approval_surface"));
    assert_eq!(*backend.writes.lock().unwrap(), 0);
    let (status, rendered) = respond_with(
        &post(r#"{"apps":["com.example.Secret","com.example.New"],"websites":[]}"#),
        &reg(),
        &ent(),
        &backend,
    );
    assert_eq!(status, 202, "{rendered}");
    assert!(rendered.contains("com.example.New"), "{rendered}");
    assert_eq!(*backend.writes.lock().unwrap(), 1);
}

#[test]
fn the_queue_less_rest_path_never_hands_a_visual_recall_widening_to_the_backend() {
    // Issue #782: only `server::handle` holds the approval queue. Turning Visual recall on or
    // keeping screens longer is refused on the pure render path; turning it off or shortening
    // is written directly at L1.
    use crate::visual_recall_settings_api::test_support::FakeVisualRecall;
    let backend = FakeVisualRecall::with(false, 3);
    let post = |path: &str, body: &str| RestRequest {
        body: Some(body.into()),
        ..req(Method::Post, path, Some("t"))
    };
    for (path, body) in [
        ("/v1/visual_recall/enabled", r#"{"enabled":true}"#),
        ("/v1/visual_recall/retention", r#"{"days":3650}"#),
    ] {
        let (status, rendered) = respond_with(&post(path, body), &reg(), &ent(), &backend);
        assert_eq!(status, 501, "{path}: {rendered}");
        assert!(rendered.contains("no_approval_surface"), "{rendered}");
    }
    assert_eq!(*backend.writes.lock().unwrap(), 0);
    assert!(!*backend.enabled.lock().unwrap());
    assert_eq!(*backend.days.lock().unwrap(), 3);
    for (path, body) in [
        ("/v1/visual_recall/enabled", r#"{"enabled":false}"#),
        ("/v1/visual_recall/retention", r#"{"days":1}"#),
    ] {
        let (status, rendered) = respond_with(&post(path, body), &reg(), &ent(), &backend);
        assert_eq!(status, 202, "{path}: {rendered}");
        assert!(rendered.contains(r#""level":"L1""#), "{rendered}");
    }
    assert_eq!(*backend.writes.lock().unwrap(), 2);
    assert_eq!(*backend.days.lock().unwrap(), 1);
}

#[test]
fn the_queue_less_rest_path_never_hands_a_capture_resume_to_the_backend() {
    // Issue #897: only `server::handle` holds the approval queue. Ending the user's pause is
    // refused on the pure render path; pausing is written directly at L1.
    use crate::capture_pause_api::test_support::FakePause;
    let backend = FakePause::with(Some(None));
    let post = |body: &str| RestRequest {
        body: Some(body.into()),
        ..req(Method::Post, "/v1/capture/paused", Some("t"))
    };
    let (status, rendered) = respond_with(&post(r#"{"paused":false}"#), &reg(), &ent(), &backend);
    assert_eq!(status, 501, "{rendered}");
    assert!(rendered.contains("no_approval_surface"), "{rendered}");
    assert_eq!(*backend.writes.lock().unwrap(), 0);
    assert_eq!(*backend.pause.lock().unwrap(), Some(None));
    let (status, rendered) = respond_with(&post(r#"{"paused":true}"#), &reg(), &ent(), &backend);
    assert_eq!(status, 202, "{rendered}");
    assert!(rendered.contains(r#""level":"L1""#), "{rendered}");
    assert_eq!(*backend.writes.lock().unwrap(), 1);
}

#[test]
fn a_standing_approval_refusal_reaches_the_caller_as_a_400() {
    // FR-TG-30: the confirm_standing refusal is the caller's to fix, so the text must come
    // back — never the opaque 500 a backend fault gets.
    use crate::backend::{MemoryBackend, ReadItem, ReadParams, WriteResult};
    struct Refusing;
    impl MemoryBackend for Refusing {
        fn read(&self, _tool: Tool, _params: &ReadParams) -> Vec<ReadItem> {
            Vec::new()
        }
        fn write(&self, _tool: Tool, _body: &str) -> WriteResult {
            Err(crate::memory_api::STANDING_APPROVAL_TEXT.to_string())
        }
    }
    let request = RestRequest {
        body: Some(r#"{"key":"marketing","auto_archive":true}"#.into()),
        ..req(Method::Post, "/v1/mail/labels", Some("t"))
    };
    let (status, rendered) = respond_with(&request, &reg(), &ent(), &Refusing);
    assert_eq!(status, 400, "{rendered}");
    let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
    assert!(
        parsed["reason"]
            .as_str()
            .is_some_and(|r| r.contains("Only the user can grant it, in the ShogunAI app")),
        "{rendered}"
    );
}

#[test]
fn the_queue_less_rest_path_never_hands_an_unapproved_gmail_archive_to_the_backend() {
    // Issue #779: only `server::handle` holds the approval queue. An archive the label has no
    // standing approval for is refused on the pure render path; one it has is recorded directly
    // and reported pending at L2.
    use crate::mail_archive_api::test_support::{FakeMail, Thread};
    let backend = FakeMail::with(&[
        ("gmail:a", Thread { archived: false, requested: false, standing: false }),
        ("gmail:s", Thread { archived: false, requested: false, standing: true }),
        ("gmail:z", Thread { archived: true, requested: false, standing: true }),
    ]);
    let post = |path: &str| req(Method::Post, path, Some("t"));
    for path in ["/v1/mail/triage/gmail:a/archive", "/v1/mail/triage/gmail:z/unarchive"] {
        let (status, rendered) = respond_with(&post(path), &reg(), &ent(), &backend);
        assert_eq!(status, 501, "{path}: {rendered}");
        assert!(rendered.contains("no_approval_surface"), "{rendered}");
    }
    assert_eq!(*backend.writes.lock().unwrap(), 0);
    let (status, rendered) =
        respond_with(&post("/v1/mail/triage/gmail:s/archive"), &reg(), &ent(), &backend);
    assert_eq!(status, 202, "{rendered}");
    assert!(rendered.contains(r#""pending":true"#), "{rendered}");
    assert!(rendered.contains(r#""level":"L2""#), "{rendered}");
    assert_eq!(*backend.writes.lock().unwrap(), 1);
    assert!(backend.thread("gmail:s").is_some_and(|t| t.requested));
    // Unknown thread: the backend's own 404, not a queued request.
    let (status, rendered) =
        respond_with(&post("/v1/mail/triage/gmail:nope/archive"), &reg(), &ent(), &backend);
    assert_eq!(status, 404, "{rendered}");
}

#[test]
fn resolved_read_tool_is_actually_a_read() {
    // guard against a routing table that points a read path at a write tool
    if let Routed::Read { tool, .. } = route(
        &req(Method::Get, "/v1/state/open_loops", Some("t")),
        &reg(),
        &ent(),
    ) {
        assert_eq!(tool_level(tool), ApiLevel::Read);
    } else {
        panic!("expected a read");
    }
}

#[test]
fn memory_import_routes_levels_and_rendering() {
    use crate::backend::{MemoryBackend, ReadItem, ReadParams};
    use crate::memory_import_api::MemoryImportOperation as Op;

    // Routing: the preview is a POST read, commit / delete are L1 writes, all token + plan gated.
    let r = |m: Method, p: &str, t: Option<&str>| route(&req(m, p, t), &reg(), &ent());
    assert_eq!(
        r(Method::Post, "/v1/memory/import/preview", Some("t")),
        Routed::Read { tool: Tool::MemoryImportPreview, id: None }
    );
    assert_eq!(r(Method::Get, "/v1/memory/import/preview", Some("t")), Routed::MethodNotAllowed);
    assert_eq!(
        r(Method::Post, "/v1/memory/import", Some("t")),
        Routed::Write { tool: Tool::MemoryImport, level: Level::L1 }
    );
    assert_eq!(
        r(Method::Get, "/v1/memory/imports/4", Some("t")),
        Routed::Read { tool: Tool::MemoryImportList, id: Some(4) }
    );
    assert_eq!(
        r(Method::Post, "/v1/memory/imports/4/delete", Some("t")),
        Routed::Write { tool: Tool::MemoryImportDelete, level: Level::L1 }
    );
    assert_eq!(r(Method::Post, "/v1/memory/imports/0/delete", Some("t")), Routed::NotFound);
    assert_eq!(r(Method::Post, "/v1/memory/import", None), Routed::Unauthorized);
    let standard = entitlements(Plan::Standard, 0);
    assert_eq!(
        route(&req(Method::Post, "/v1/memory/import/preview", Some("t")), &reg(), &standard),
        Routed::PlanLocked
    );

    struct Fake;
    impl MemoryBackend for Fake {
        fn read(&self, _tool: Tool, _params: &ReadParams) -> Vec<ReadItem> {
            Vec::new()
        }
        fn memory_import(&self, operation: Op) -> Result<String, String> {
            match operation {
                Op::Preview { body } if body.contains("computer_history") => Ok(r#"{"items":[]}"#.into()),
                Op::Preview { .. } => Ok(r#"{"error":"invalid_package","errors":[]}"#.into()),
                Op::Commit { .. } => Ok(r#"{"import_id":9,"stored":1}"#.into()),
                Op::List { id: Some(4), include_low: true } => Ok(r#"{"import":{"id":4}}"#.into()),
                Op::List { .. } => Ok(r#"{"error":"not_found"}"#.into()),
                Op::Delete { id } => Err(format!("sqlite detail {id}")),
            }
        }
    }
    let send = |m: Method, p: &str, body: Option<&str>, include_low: bool| {
        let rq = RestRequest { body: body.map(str::to_string), include_low, ..req(m, p, Some("t")) };
        respond_with(&rq, &reg(), &ent(), &Fake)
    };
    let (s, b) = send(Method::Post, "/v1/memory/import/preview", Some(r#"{"computer_history":[]}"#), false);
    assert_eq!((s, b.as_str()), (200, r#"{"items":[]}"#));
    assert_eq!(send(Method::Post, "/v1/memory/import/preview", Some("{}"), false).0, 400);
    assert_eq!(send(Method::Post, "/v1/memory/import", Some("{}"), false).0, 202);
    assert_eq!(send(Method::Get, "/v1/memory/imports/4", None, true).0, 200);
    assert_eq!(send(Method::Get, "/v1/memory/imports/4", None, false).0, 404);
    let (s, b) = send(Method::Post, "/v1/memory/imports/4/delete", None, false);
    assert_eq!(s, 500);
    assert!(!b.contains("sqlite"), "a store fault stays out of the response: {b}");
}

#[test]
fn frame_rescan_is_an_l1_write_with_the_same_answer_on_rest_and_mcp() {
    // REST and MCP return the same envelope for the rescan answer (invariant 6).
    use crate::backend::{MemoryBackend, ReadItem, ReadParams, WriteResult};
    struct Rescanner;
    impl MemoryBackend for Rescanner {
        fn read(&self, _tool: Tool, _params: &ReadParams) -> Vec<ReadItem> {
            Vec::new()
        }
        fn write(&self, tool: Tool, _body: &str) -> WriteResult {
            panic!("{} must not go through the generic write channel", tool.wire_name())
        }
        fn read_structured(&self, tool: Tool, params: &ReadParams) -> Option<String> {
            assert_eq!(tool, Tool::VisualRecallRescanFrame);
            Some(match params.id {
                Some(12) => r#"{"frame_id":12,"ocr_text":"fresh","chars":5,"excerpt":"fresh"}"#.to_string(),
                other => serde_json::json!({ "error": "not_found", "frame_id": other }).to_string(),
            })
        }
    }

    assert_eq!(
        tool_level(Tool::VisualRecallRescanFrame),
        ApiLevel::Write(Level::L1)
    );
    // No token is still refused, like every tool.
    assert_eq!(
        route(&req(Method::Post, "/v1/visual_recall/frames/12/rescan", None), &reg(), &ent()),
        Routed::Unauthorized
    );

    let (status, rest_body) = respond_with(
        &req(Method::Post, "/v1/visual_recall/frames/12/rescan", Some("t")),
        &reg(),
        &ent(),
        &Rescanner,
    );
    assert_eq!(status, 200, "{rest_body}");
    let parsed: serde_json::Value = serde_json::from_str(&rest_body).unwrap();
    assert_eq!(parsed["tool"], "visual_recall.rescan_frame");
    assert_eq!(parsed["result"]["ocr_text"], "fresh");
    let (status, missing) = respond_with(
        &req(Method::Post, "/v1/visual_recall/frames/99/rescan", Some("t")),
        &reg(),
        &ent(),
        &Rescanner,
    );
    assert_eq!(status, 404, "{missing}");

    let mcp = crate::mcp::McpServer::new(
        Rescanner,
        std::sync::Arc::new(std::sync::Mutex::new(ApprovalQueue::new())),
        || 1000,
        ent,
    );
    let line = mcp
        .handle_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"visual_recall.rescan_frame","arguments":{"id":12}}}"#,
        )
        .unwrap();
    let response: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["result"]["content"][0]["text"], rest_body.as_str());
    assert_eq!(response["result"]["isError"], false);
}

// token scopes (issue #400)

fn scoped() -> TokenRegistry {
    let mut r = TokenRegistry::new();
    r.issue_scoped("reader", crate::memory_api::TokenScope::Read);
    r.issue_scoped("writer", crate::memory_api::TokenScope::Full);
    r
}

#[test]
fn read_token_is_refused_every_write_endpoint_generated_from_the_tool_table() {
    // The same `authorize` step `route` runs after resolving a path, driven by every tool in
    // ALL_TOOLS: a new write tool is refused to read tokens without touching this test.
    use crate::memory_api::{AuthResult, TokenScope, ALL_TOOLS};
    let mut writes = 0;
    for &tool in ALL_TOOLS {
        let resolved = match tool_level(tool) {
            ApiLevel::Read => Routed::Read { tool, id: None },
            ApiLevel::Write(level) => Routed::Write { tool, level },
            ApiLevel::PerAction => Routed::Action,
        };
        let read = super::rest_auth::authorize(resolved.clone(), AuthResult::Granted(TokenScope::Read), &ent());
        let full = super::rest_auth::authorize(resolved.clone(), AuthResult::Granted(TokenScope::Full), &ent());
        assert_eq!(full, resolved, "{}", tool.wire_name());
        if tool_level(tool) == ApiLevel::Read {
            assert_eq!(read, resolved, "{}", tool.wire_name());
        } else {
            writes += 1;
            assert_eq!(read, Routed::ScopeInsufficient, "{}", tool.wire_name());
            assert_eq!(status_code(&read), 403);
        }
    }
    assert!(writes > 40, "{writes}");
    // The L3 poller is a read; every read-only token may poll.
    assert_eq!(
        super::rest_auth::authorize(Routed::ApprovalStatus { id: 1 }, AuthResult::Granted(TokenScope::Read), &ent()),
        Routed::ApprovalStatus { id: 1 }
    );
}

#[test]
fn read_token_gets_a_403_scope_insufficient_body_on_real_write_paths() {
    let tokens = scoped();
    for (method, path) in [
        (Method::Post, "/v1/memory/notes"),
        (Method::Post, "/v1/actions/execute"),
        (Method::Post, "/v1/writing_voice"),
        (Method::Post, "/v1/capture_exclusions"),
        (Method::Post, "/v1/agent/source"),
        (Method::Post, "/v1/connections/disconnect"),
        (Method::Post, "/v1/connections/composio"),
        (Method::Post, "/v1/backup/retention"),
        (Method::Post, "/v1/settings/language"),
    ] {
        let (status, body) = respond(&req(method, path, Some("reader")), &tokens, &ent());
        assert_eq!(status, 403, "{path}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["error"], "scope_insufficient", "{path}: {body}");
        assert_eq!(v["scope"], "read");
        assert!(v["message"].as_str().unwrap().contains("Settings > Memory API"));
        // The same path with a full token passes the gate.
        let full = route(&req(method, path, Some("writer")), &tokens, &ent());
        assert!(!matches!(full, Routed::ScopeInsufficient | Routed::Unauthorized | Routed::PlanLocked), "{path}");
    }
    // Reads pass for the read token.
    for path in [
        "/v1/memory/search",
        "/v1/state/people",
        "/v1/api/tokens",
        "/v1/actions/status/3",
        "/v1/connections",
        "/v1/backup/status",
        "/v1/dream/status",
    ] {
        let routed = route(&req(Method::Get, path, Some("reader")), &tokens, &ent());
        assert!(!matches!(routed, Routed::ScopeInsufficient | Routed::Unauthorized), "{path}: {routed:?}");
    }
}

#[test]
fn auth_and_plan_are_decided_before_scope() {
    // No token stays 401 and a locked plan stays plan_required: the scope answer never tells an
    // unauthenticated caller or a locked device anything new.
    let tokens = scoped();
    assert_eq!(route(&req(Method::Post, "/v1/memory/notes", None), &tokens, &ent()), Routed::Unauthorized);
    assert_eq!(
        route(&req(Method::Post, "/v1/memory/notes", Some("reader")), &tokens, &entitlements(Plan::Standard, 0)),
        Routed::PlanLocked
    );
}

#[test]
fn legacy_tokens_act_as_full_on_rest() {
    // `reg()` issues the pre-scope way; its token still reaches writes.
    assert_eq!(
        route(&req(Method::Post, "/v1/memory/notes", Some("t")), &reg(), &ent()),
        Routed::Write { tool: crate::memory_api::Tool::MemoryAppendNote, level: shogun_agents::permission::Level::L1 }
    );
}

#[test]
fn api_tokens_list_is_a_read_only_route_that_never_leaks_a_token() {
    use crate::backend::{ReadItem, ReadParams, WriteResult};
    struct Nothing;
    impl crate::backend::MemoryBackend for Nothing {
        fn read(&self, _tool: crate::memory_api::Tool, _p: &ReadParams) -> Vec<ReadItem> {
            Vec::new()
        }
        fn write(&self, _tool: crate::memory_api::Tool, _body: &str) -> WriteResult {
            Err("not a write tool".into())
        }
    }
    let tokens = scoped();
    let (status, body) = respond_with(&req(Method::Get, "/v1/api/tokens", Some("reader")), &tokens, &ent(), &Nothing);
    assert_eq!(status, 200, "{body}");
    for secret in ["\"reader\"", "\"writer\"", "sha256:"] {
        assert!(!body.contains(secret), "{body}");
    }
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["caller_scope"], "read");
    assert_eq!(v["tokens"].as_array().unwrap().len(), 2);
    assert_eq!(v["tokens"][1]["scope"], "full");
    let (_, body) = respond_with(&req(Method::Get, "/v1/api/tokens", Some("writer")), &tokens, &ent(), &Nothing);
    assert!(body.contains("\"caller_scope\":\"full\""), "{body}");
    // No route issues a token or changes a scope: raising one is only in Settings.
    for (method, path) in [
        (Method::Post, "/v1/api/tokens"),
        (Method::Post, "/v1/api/tokens/scope"),
        (Method::Post, "/v1/api/tokens/issue"),
    ] {
        assert!(
            matches!(route(&req(method, path, Some("writer")), &tokens, &ent()), Routed::MethodNotAllowed | Routed::NotFound),
            "{path}"
        );
    }
}

/// Snapshot retention over REST (issue #647): the status is the backend's JSON, a valid set is a
/// 202 at L1, and an out-of-range value is a 400 with the reason, never a silent clamp.
#[test]
fn backup_retention_is_read_and_set_over_rest() {
    use crate::backend::{MemoryBackend, ReadItem, ReadParams, WriteResult};

    struct Backups;
    impl MemoryBackend for Backups {
        fn read(&self, _tool: Tool, _params: &ReadParams) -> Vec<ReadItem> {
            Vec::new()
        }
        fn write(&self, tool: Tool, body: &str) -> WriteResult {
            assert_eq!(tool, Tool::BackupSetRetention);
            if body == r#"{"days":3}"# {
                Ok(None)
            } else {
                Err("expected backup retention days between 1 and 30".to_string())
            }
        }
        fn read_structured(&self, tool: Tool, _params: &ReadParams) -> Option<String> {
            (tool == Tool::BackupStatus).then(|| r#"{"retention_days":7}"#.to_string())
        }
    }

    let (status, body) = respond_with(
        &req(Method::Get, "/v1/backup/status", Some("t")),
        &reg(),
        &ent(),
        &Backups,
    );
    assert_eq!(status, 200, "{body}");
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["result"]["retention_days"], 7, "{body}");

    let post = |body: &str| RestRequest {
        body: Some(body.into()),
        ..req(Method::Post, "/v1/backup/retention", Some("t"))
    };
    let (status, body) = respond_with(&post(r#"{"days":3}"#), &reg(), &ent(), &Backups);
    assert_eq!(status, 202, "{body}");
    assert!(body.contains(r#""level":"L1""#), "{body}");
    let (status, body) = respond_with(&post(r#"{"days":31}"#), &reg(), &ent(), &Backups);
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("between 1 and 30"), "{body}");
}

/// "Compact search index" over REST (#714 step 3): one read and two L1 writes, the wrong method
/// refused, the backend's refusal passed through as a 400, and the Memory API plan gate applied.
#[test]
fn search_index_compaction_is_read_started_and_stopped_over_rest() {
    use crate::backend::{MemoryBackend, ReadItem, ReadParams, WriteResult};
    use shogun_agents::entitlement::{entitlements, Plan};

    assert_eq!(
        route(&req(Method::Get, "/v1/search_index/status", Some("t")), &reg(), &ent()),
        Routed::Read { tool: Tool::SearchIndexStatus, id: None }
    );
    for (path, tool) in [
        ("/v1/search_index/compact/start", Tool::SearchIndexCompactStart),
        ("/v1/search_index/compact/stop", Tool::SearchIndexCompactStop),
    ] {
        assert_eq!(
            route(&req(Method::Post, path, Some("t")), &reg(), &ent()),
            Routed::Write { tool, level: Level::L1 }
        );
        assert_eq!(route(&req(Method::Get, path, Some("t")), &reg(), &ent()), Routed::MethodNotAllowed);
        assert_eq!(
            route(&req(Method::Post, path, Some("t")), &reg(), &entitlements(Plan::Standard, 0)),
            Routed::PlanLocked,
            "{path}: the Memory API is Pro"
        );
    }
    assert_eq!(
        route(&req(Method::Post, "/v1/search_index/status", Some("t")), &reg(), &ent()),
        Routed::MethodNotAllowed
    );

    struct Index;
    impl MemoryBackend for Index {
        fn read(&self, _tool: Tool, _params: &ReadParams) -> Vec<ReadItem> {
            Vec::new()
        }
        fn write(&self, tool: Tool, body: &str) -> WriteResult {
            match (tool, body) {
                (Tool::SearchIndexCompactStart, "{}") => Err("insufficient_disk: needs at least 1073741824 bytes free".into()),
                (Tool::SearchIndexCompactStart | Tool::SearchIndexCompactStop, _) => Ok(None),
                _ => Err("unexpected".into()),
            }
        }
        fn read_structured(&self, tool: Tool, _params: &ReadParams) -> Option<String> {
            (tool == Tool::SearchIndexStatus).then(|| r#"{"job":{"state":"completed"}}"#.to_string())
        }
    }
    let (status, body) =
        respond_with(&req(Method::Get, "/v1/search_index/status", Some("t")), &reg(), &ent(), &Index);
    assert_eq!(status, 200, "{body}");
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["result"]["job"]["state"], "completed", "{body}");

    let post = |path: &str, body: Option<&str>| RestRequest {
        body: body.map(Into::into),
        ..req(Method::Post, path, Some("t"))
    };
    let (status, body) = respond_with(&post("/v1/search_index/compact/stop", None), &reg(), &ent(), &Index);
    assert_eq!(status, 202, "{body}");
    assert!(body.contains(r#""level":"L1""#), "{body}");
    let (status, body) =
        respond_with(&post("/v1/search_index/compact/start", Some("{}")), &reg(), &ent(), &Index);
    assert_eq!(status, 507, "{body}");
    assert!(body.contains("insufficient_disk"), "{body}");
}

#[test]
fn translate_routes_are_l1_writes_a_status_read_and_the_pair_beside_the_register() {
    // Issue #279 (invariant 6): the panel's translate and reply draft on REST.
    for (method, path, expected) in [
        (Method::Post, "/v1/translate", Routed::Write { tool: Tool::TranslateText, level: Level::L1 }),
        (
            Method::Post,
            "/v1/translate/reply_draft",
            Routed::Write { tool: Tool::TranslateReplyDraft, level: Level::L1 },
        ),
        (Method::Get, "/v1/translate/jobs/7", Routed::Read { tool: Tool::TranslateStatus, id: Some(7) }),
        (Method::Get, "/v1/translate/pair", Routed::Read { tool: Tool::TranslatePairGet, id: None }),
        (Method::Post, "/v1/translate/pair", Routed::Write { tool: Tool::TranslatePairSet, level: Level::L1 }),
    ] {
        assert_eq!(route(&req(method, path, Some("t")), &reg(), &ent()), expected, "{path}");
    }
    assert_eq!(route(&req(Method::Get, "/v1/translate", Some("t")), &reg(), &ent()), Routed::MethodNotAllowed);
    assert_eq!(route(&req(Method::Get, "/v1/translate/jobs/0", Some("t")), &reg(), &ent()), Routed::NotFound);
    assert_eq!(route(&req(Method::Post, "/v1/translate", None), &reg(), &ent()), Routed::Unauthorized);
    // The Memory API without agent execution may read and set the pair but not spend the
    // user's model access.
    let mut api_only = ent();
    api_only.agent_execution = false;
    assert!(!matches!(
        route(&req(Method::Post, "/v1/translate", Some("t")), &reg(), &api_only),
        Routed::Write { .. }
    ));
    assert_eq!(
        route(&req(Method::Get, "/v1/translate/jobs/7", Some("t")), &reg(), &api_only),
        Routed::Read { tool: Tool::TranslateStatus, id: Some(7) }
    );
}

#[test]
fn translate_requests_reach_the_backend_with_the_api_origin() {
    use crate::backend::{MemoryBackend, ReadItem, ReadParams};
    use crate::translate_api::TranslateOperation;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<TranslateOperation>>);
    impl MemoryBackend for Recorder {
        fn read(&self, _tool: Tool, _params: &ReadParams) -> Vec<ReadItem> {
            Vec::new()
        }
        fn translate(&self, op: TranslateOperation) -> Result<String, String> {
            let answer = match &op {
                TranslateOperation::Request { .. } => {
                    crate::translate_api::done_json(3, serde_json::json!({ "text": "やあ" }))
                }
                TranslateOperation::Status { .. } => crate::translate_api::pending_json(3, "running"),
            };
            self.0.lock().unwrap().push(op);
            Ok(answer)
        }
    }
    let backend = Recorder::default();
    let post = RestRequest {
        body: Some(r#"{"text":"hi"}"#.into()),
        ..req(Method::Post, "/v1/translate", Some("t"))
    };
    let (status, body) = respond_with(&post, &reg(), &ent(), &backend);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("やあ"));
    let (status, _) =
        respond_with(&req(Method::Get, "/v1/translate/jobs/3", Some("t")), &reg(), &ent(), &backend);
    assert_eq!(status, 202, "a request still running is 202 with its job id");
    let seen = backend.0.lock().unwrap();
    assert_eq!(
        seen[0],
        TranslateOperation::Request { kind: "translate", body: r#"{"text":"hi"}"#.into(), origin: "api" }
    );
    assert_eq!(seen[1], TranslateOperation::Status { id: 3 });
    // A backend without the queue says so rather than pretending.
    let (status, body) = respond_with(&post, &reg(), &ent(), &crate::backend::StubBackend);
    assert_eq!(status, 503, "{body}");
}

#[test]
fn storage_preview_is_an_explicit_read() {
    assert_eq!(
        route(&req(Method::Get, "/v1/storage/preview", Some("t")), &reg(), &ent()),
        Routed::Read { tool: Tool::StoragePreview, id: None }
    );
    assert_eq!(
        route(&req(Method::Post, "/v1/storage/preview", Some("t")), &reg(), &ent()),
        Routed::MethodNotAllowed
    );
}

/// Issue #779: a speakers or templates read for a non-positive meeting id is not found, as the
/// recap and transcript reads are, instead of a 200 for a meeting that cannot exist.
#[test]
fn meeting_speakers_and_templates_refuse_non_positive_ids() {
    for path in ["/v1/meeting/speakers/0", "/v1/meeting/speakers/-5", "/v1/meeting/templates/-5"] {
        let (status, body) = respond(&req(Method::Get, path, Some("t")), &reg(), &ent());
        assert_eq!(status, 404, "{path}: {body}");
    }
    let (status, _) = respond(&req(Method::Get, "/v1/meeting/speakers/3", Some("t")), &reg(), &ent());
    assert_eq!(status, 200);
}

/// Issue #779: the context pack takes memory.search's query bound before the backend runs.
#[test]
fn the_context_pack_refuses_an_overlong_query() {
    use crate::backend::StubBackend;

    let mut request = req(Method::Get, "/v1/memory/context_pack", Some("t"));
    request.query = Some("x".repeat(crate::backend::MAX_SEARCH_QUERY_CHARS + 1));
    let (status, body) = respond_with(&request, &reg(), &ent(), &StubBackend);
    assert_eq!((status, body.as_str()), (400, r#"{"error":"search_query_too_long"}"#));
}

/// Issue #779: a percent-encoded path key (a label named in Japanese, as Python requests and the
/// CLI send it) reaches the backend decoded, the same key MCP passes.
#[test]
fn a_percent_encoded_label_key_is_decoded_for_the_backend() {
    let body = super::rest_routing::mail_write_body(
        Tool::MailLabelsDelete,
        "/v1/mail/labels/custom:%E4%BC%9A%E8%AD%B0/delete",
        "",
    )
    .expect("a label delete carries its key");
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["key"], "custom:会議");
    // `+` is literal in a path, a bad escape is kept, and non-UTF-8 bytes leave the key as sent.
    assert_eq!(super::rest_routing::decode_path_segment("a+b%2"), "a+b%2");
    assert_eq!(super::rest_routing::decode_path_segment("x%FFy"), "x%FFy");
    assert_eq!(super::rest_routing::decode_path_segment("gmail%3A18f3"), "gmail:18f3");
}

/// Issue #779: a state list item carries its row (table and id) for `state edit|delete|split`,
/// and a get for an id that names no row is 404 not_found rather than an empty 200.
#[test]
fn state_items_carry_their_row_and_a_missing_id_is_not_found() {
    use crate::backend::{MemoryBackend, ReadItem, ReadParams};

    struct OneCommitment;
    impl MemoryBackend for OneCommitment {
        fn read(&self, tool: Tool, params: &ReadParams) -> Vec<ReadItem> {
            match (tool, params.id) {
                (Tool::StateCommitmentsList, _) | (Tool::StateCommitmentsGet, Some(3)) => {
                    vec![ReadItem::new("send the deck", 0.9).with_row("commitments", 3)]
                }
                _ => Vec::new(),
            }
        }
    }
    let (status, body) = respond_with(
        &req(Method::Get, "/v1/state/commitments", Some("t")),
        &reg(),
        &ent(),
        &OneCommitment,
    );
    assert_eq!(status, 200);
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["results"][0]["table"], "commitments");
    assert_eq!(value["results"][0]["id"], 3);
    let (status, _) = respond_with(
        &req(Method::Get, "/v1/state/commitments/3", Some("t")),
        &reg(),
        &ent(),
        &OneCommitment,
    );
    assert_eq!(status, 200);
    let (status, body) = respond_with(
        &req(Method::Get, "/v1/state/commitments/999", Some("t")),
        &reg(),
        &ent(),
        &OneCommitment,
    );
    assert_eq!((status, body.as_str()), (404, r#"{"error":"not_found"}"#));
}

#[test]
fn calendar_reminder_routes_match_ui_preference_levels() {
    assert_eq!(
        route(
            &req(Method::Get, "/v1/meeting/reminder", Some("t")),
            &reg(),
            &ent()
        ),
        Routed::Read {
            tool: Tool::MeetingReminderGet,
            id: None
        }
    );
    assert_eq!(
        route(
            &req(Method::Post, "/v1/meeting/reminder", Some("t")),
            &reg(),
            &ent()
        ),
        Routed::Write {
            tool: Tool::MeetingReminderSet,
            level: Level::L1
        }
    );
}

#[test]
fn forms_rest_scope_plan_auth_and_pending_review() {
    use crate::backend::{MemoryBackend, ReadItem, ReadParams, WriteResult};
    use crate::forms_api::{FormOperation, FormsAuth};
    use serde_json::json;
    use std::sync::Mutex;
    struct Forms(Mutex<Vec<(FormOperation, FormsAuth)>>);
    impl MemoryBackend for Forms {
        fn read(&self, _: Tool, _: &ReadParams) -> Vec<ReadItem> {
            panic!("wrong form path")
        }
        fn write(&self, _: Tool, _: &str) -> WriteResult {
            panic!("wrong form path")
        }
        fn forms(&self, operation: FormOperation, auth: FormsAuth) -> Result<String, String> {
            self.0.lock().unwrap().push((operation, auth));
            Ok(json!({"status":"pending","session_id":7}).to_string())
        }
    }
    let backend = Forms(Mutex::new(vec![]));
    let mut readonly = TokenRegistry::new();
    readonly.issue_scoped("read", crate::memory_api::TokenScope::Read);
    let routes = [
        (Method::Post, "/v1/forms/inspect"),
        (Method::Post, "/v1/forms/plan"),
        (Method::Get, "/v1/forms/status/7"),
        (Method::Post, "/v1/forms/apply"),
        (Method::Post, "/v1/forms/close"),
        (Method::Post, "/v1/forms/undo"),
    ];
    for (method, path) in routes {
        assert_eq!(
            respond_with(&req(method, path, None), &reg(), &ent(), &backend).0,
            401
        );
        assert_eq!(
            respond_with(
                &req(method, path, Some("t")),
                &reg(),
                &entitlements(Plan::Standard, 0),
                &backend
            )
            .0,
            403
        );
        if method == Method::Post {
            assert_eq!(
                respond_with(
                    &req(method, path, Some("read")),
                    &readonly,
                    &ent(),
                    &backend
                )
                .0,
                403
            );
        }
    }
    assert!(backend.0.lock().unwrap().is_empty());
    let mut request = req(Method::Post, "/v1/forms/apply", Some("t"));
    request.body = Some(json!({"session_id":7,"revision":2,"confirm":true}).to_string());
    assert_eq!(respond_with(&request, &reg(), &ent(), &backend).0, 400);
    assert!(backend.0.lock().unwrap().is_empty());
    request.body = Some(json!({"session_id":7,"revision":2}).to_string());
    assert_eq!(respond_with(&request, &reg(), &ent(), &backend).0, 202);
    let calls = backend.0.lock().unwrap();
    assert_eq!(
        calls[0].0,
        FormOperation::Apply {
            session_id: 7,
            revision: 2
        }
    );
    assert!(calls[0].1.admitted_by_token && calls[0].1.token.as_deref() == Some("t"));
    drop(calls);
    let mut empty = TokenRegistry::new();
    empty.issue("other");
    assert_eq!(respond_with(&request, &empty, &ent(), &backend).0, 401);
    assert_eq!(backend.0.lock().unwrap().len(), 1);
}

#[test]
fn memory_notes_routes_keep_auth_scope_and_revision_refusals() {
    use crate::memory_api::{TokenScope, Tool};
    let mut tokens=reg();
    tokens.issue_scoped("reader",TokenScope::Read);
    let list=req(Method::Get,"/v1/memory/user-notes",Some("reader"));
    assert_eq!(route(&list,&tokens,&ent()),Routed::Read{tool:Tool::MemoryNotesList,id:None});
    let mut change=req(Method::Post,"/v1/memory/user-notes/change",Some("reader"));
    assert_eq!(route(&change,&tokens,&ent()),Routed::ScopeInsufficient);
    change.token=None;
    assert_eq!(route(&change,&tokens,&ent()),Routed::Unauthorized);
    change.token=Some("t".into());
    assert_eq!(route(&change,&tokens,&ent()),Routed::Write{tool:Tool::MemoryNotesChange,level:Level::L1});
    assert_eq!(route(&req(Method::Get,"/v1/memory/user-notes/0",Some("t")),&tokens,&ent()),Routed::NotFound);
    assert_eq!(route(&req(Method::Get,"/v1/memory/user-notes/7",Some("t")),&tokens,&ent()),Routed::Read{tool:Tool::MemoryNotesList,id:Some(7)});
}

#[test]
fn activity_reads_take_the_mcp_arguments_as_a_post_body() {
    use crate::backend::{MemoryBackend, ReadItem};
    struct Sessions;
    impl MemoryBackend for Sessions {
        fn read(&self, _tool: Tool, _params: &crate::backend::ReadParams) -> Vec<ReadItem> {
            Vec::new()
        }
        fn activity(&self, _request: &crate::activity_api::ActivityRequest) -> Result<String, String> {
            Ok(r#"{"sessions":[],"truncated":false}"#.into())
        }
    }
    let post = |path: &str, body: &str| {
        let mut r = req(Method::Post, path, Some("t"));
        r.body = Some(body.into());
        respond_with(&r, &reg(), &ent(), &Sessions)
    };
    let (status, body) = post("/v1/memory/session", r#"{"from_ms":0,"to_ms":1000}"#);
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"tool": "memory.read_session", "sessions": [], "truncated": false})
    );
    let (status, body) = post("/v1/memory/events", r#"{"ids":[]}"#);
    assert_eq!(status, 400, "{body}");
    assert!(body.contains(r#""error":"invalid_ids""#), "{body}");
    assert_eq!(
        route(&req(Method::Get, "/v1/memory/events", Some("t")), &reg(), &ent()),
        Routed::MethodNotAllowed
    );
}

#[test]
fn connection_labels_follow_rest_and_cli_token_scope() {
    use crate::backend::{MemoryBackend, ReadItem, ReadParams, WriteResult};
    use crate::memory_api::TokenScope;

    struct Connections;
    impl MemoryBackend for Connections {
        fn read(&self, _tool: Tool, _params: &ReadParams) -> Vec<ReadItem> {
            Vec::new()
        }
        fn write(&self, _tool: Tool, _body: &str) -> WriteResult {
            Ok(None)
        }
        fn read_structured(&self, tool: Tool, params: &ReadParams) -> Option<String> {
            assert_eq!(tool, Tool::ConnectionsList);
            Some(serde_json::json!({ "include_account_labels": params.include_account_labels }).to_string())
        }
    }

    for scope in [TokenScope::Read, TokenScope::Full] {
        let mut tokens = TokenRegistry::new();
        tokens.issue_scoped("connections", scope);
        let request = req(Method::Get, "/v1/connections", Some("connections"));
        let (status, body) = respond_with(&request, &tokens, &ent(), &Connections);
        assert_eq!(status, 200);
        let body: serde_json::Value = serde_json::from_str(&body).expect("REST JSON");
        assert_eq!(body["result"]["include_account_labels"], scope == TokenScope::Full);
    }
}

/// E6-T2: `memory.fs` on REST is `POST /v1/memory/fs` at L1 with the MCP arguments as the body;
/// a malformed command is 400 with its code, a backend without a store answers 503.
#[test]
fn memory_fs_routes_as_an_l1_post_and_shares_the_mcp_parser() {
    let routed = route(&req(Method::Post, "/v1/memory/fs", Some("t")), &reg(), &ent());
    assert_eq!(routed, Routed::Write { tool: Tool::MemoryFs, level: Level::L1 });
    assert_eq!(status_code(&route(&req(Method::Get, "/v1/memory/fs", Some("t")), &reg(), &ent())), 405);
    let with_body = |body: &str| RestRequest { body: Some(body.to_string()), ..req(Method::Post, "/v1/memory/fs", Some("t")) };
    let (status, body) =
        respond_with(&with_body(r#"{"command":"chmod","path":"/memories"}"#), &reg(), &ent(), &crate::backend::StubBackend);
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("invalid_command"), "{body}");
    let (status, body) =
        respond_with(&with_body(r#"{"command":"view","path":"/etc"}"#), &reg(), &ent(), &crate::backend::StubBackend);
    assert_eq!(status, 400, "{body}");
    let (status, body) =
        respond_with(&with_body(r#"{"command":"view","path":"/memories"}"#), &reg(), &ent(), &crate::backend::StubBackend);
    assert_eq!((status, body.contains("memory_fs_unavailable")), (503, true), "{body}");
}
