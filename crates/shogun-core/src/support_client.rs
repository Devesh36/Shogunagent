//! The support-intake client (CS / bug-report 窓口), feature `net`.
//!
//! FR-TR-03 keeps the single raw HTTP client in shogun-core, so the desktop shell asks this
//! module rather than reaching for reqwest itself. Same origin as the licence API
//! ([`crate::license_client::DEFAULT_LICENSE_API`]).
//!
//! What goes out, and nothing else: the category, the text the user typed into the report box,
//! and — only when the user ticked the diagnostics box — the app version, the macOS version and
//! the plan name. No capture content, no memory content, no licence key, no email unless the
//! user typed one in. The caller records one traceability row (`Route::Support`) after the
//! intake accepts a report or a reply to one, the same post-send ledger policy as the L3 send
//! path: one that never left produces no row. The payload here is user-authored and
//! user-initiated, which is exactly what the traceability screen exists to show.

use std::time::Duration;

/// A report as the user assembled it in the Help & Support panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupportReport {
    /// "bug" | "feedback" | "question" — the server rejects anything else.
    pub category: String,
    /// The user's own words.
    pub message: String,
    /// Optional reply address, only if the user typed one.
    pub email: Option<String>,
    /// Diagnostics tuple — all three `None` unless the user opted in.
    pub app_version: Option<String>,
    pub os_version: Option<String>,
    pub plan: Option<String>,
    /// How often it happens — "once" | "sometimes" | "always" (issue #239). Optional, closed
    /// vocabulary; the server rejects anything else. Only meaningful for a bug report.
    pub frequency: Option<String>,
    /// How much it hurts — "blocking" | "annoying". Same rules as `frequency`.
    pub urgency: Option<String>,
}

/// The closed vocabularies, mirrored from `apps/website/src/lib/support.ts`
/// (`SUPPORT_FREQUENCIES` / `SUPPORT_URGENCIES`). Enforced client-side so a compromised webview
/// cannot use either field as a free-text channel, and so the report never fails at the server
/// for a reason the panel could have caught.
pub const FREQUENCIES: [&str; 3] = ["once", "sometimes", "always"];
pub const URGENCIES: [&str; 2] = ["blocking", "annoying"];

impl SupportReport {
    /// The JSON body the server sees. Optional fields are omitted, not sent as null, so the
    /// wire shape shows exactly what was shared.
    pub fn to_json(&self) -> serde_json::Value {
        let mut body = serde_json::json!({
            "category": self.category,
            "message": self.message,
        });
        if let Some(v) = &self.email {
            body["email"] = serde_json::Value::String(v.clone());
        }
        if let Some(v) = &self.app_version {
            body["app_version"] = serde_json::Value::String(v.clone());
        }
        if let Some(v) = &self.os_version {
            body["os_version"] = serde_json::Value::String(v.clone());
        }
        if let Some(v) = &self.plan {
            body["plan"] = serde_json::Value::String(v.clone());
        }
        if let Some(v) = &self.frequency {
            body["frequency"] = serde_json::Value::String(v.clone());
        }
        if let Some(v) = &self.urgency {
            body["urgency"] = serde_json::Value::String(v.clone());
        }
        body
    }
}

/// Why a submission failed. All transient except `Rejected` — a 400 means this exact report
/// will never be accepted, so retrying it unchanged is noise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitError {
    /// The server refused the report as malformed (400). Terminal for this payload.
    Rejected,
    /// Rate limited or a server-side failure. Try again later.
    Server(String),
    /// Could not reach the API at all.
    Network(String),
    /// A 2xx whose body carried no ticket id.
    BadResponse(String),
}

impl SubmitError {
    pub fn message(&self) -> String {
        match self {
            Self::Rejected => "the server rejected this report".to_string(),
            Self::Server(m) => format!("support intake error: {m}"),
            Self::Network(m) => format!("support intake unreachable: {m}"),
            Self::BadResponse(m) => format!("unexpected support intake response: {m}"),
        }
    }
}

/// The intake endpoint for a given origin.
pub fn report_url(origin: &str) -> String {
    format!("{}/api/support/report", origin.trim_end_matches('/'))
}

/// What the intake hands back: the ticket id, and (issue #108) the reporter token that
/// reads this one ticket's status. The token is a bearer credential — the caller keeps it in the
/// Keychain (invariant 7), never in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    pub ticket_id: String,
    pub reporter_token: Option<String>,
}

fn client() -> Result<reqwest::blocking::Client, SubmitError> {
    reqwest::blocking::Client::builder()
        .user_agent("shogun/1.0")
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| SubmitError::Network(e.to_string()))
}

/// POST the report. Blocking with a short timeout — the caller runs this off the UI thread and
/// shows the outcome; nothing here may hold a lane open.
pub fn submit(origin: &str, report: &SupportReport) -> Result<Receipt, SubmitError> {
    let resp = client()?
        .post(report_url(origin))
        .json(&report.to_json())
        .send()
        .map_err(|e| SubmitError::Network(e.to_string()))?;

    let status = resp.status();
    if status == reqwest::StatusCode::BAD_REQUEST {
        return Err(SubmitError::Rejected);
    }
    if !status.is_success() {
        return Err(SubmitError::Server(status.as_u16().to_string()));
    }

    let body: serde_json::Value = resp
        .json()
        .map_err(|e| SubmitError::BadResponse(e.to_string()))?;
    parse_submit_response(&body)
}

/// Pull the ticket id (and the reporter token, when the server issued one) out of
/// `{ ok: true, ticket_id: "…", reporter_token: "…" }`.
fn parse_submit_response(body: &serde_json::Value) -> Result<Receipt, SubmitError> {
    match body.get("ticket_id").and_then(|v| v.as_str()) {
        Some(id) if !id.is_empty() => Ok(Receipt {
            ticket_id: id.to_string(),
            reporter_token: body
                .get("reporter_token")
                .and_then(|v| v.as_str())
                .filter(|t| !t.is_empty())
                .map(str::to_string),
        }),
        _ => Err(SubmitError::BadResponse("no ticket_id in response".to_string())),
    }
}

/// Where one ticket stands, as its reporter may see it — never the report text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketStatus {
    pub status: String,
    pub released_in: Option<String>,
    pub question: Option<String>,
}

fn ticket_url(origin: &str, ticket_id: &str) -> String {
    format!("{}/{}", report_url(origin), ticket_id)
}

/// GET one ticket's status with its reporter token. Sends the ticket id and the token only.
pub fn fetch_status(origin: &str, ticket_id: &str, token: &str) -> Result<TicketStatus, SubmitError> {
    let resp = client()?
        .get(ticket_url(origin, ticket_id))
        .bearer_auth(token)
        .send()
        .map_err(|e| SubmitError::Network(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(SubmitError::Server(resp.status().as_u16().to_string()));
    }
    let body: serde_json::Value = resp
        .json()
        .map_err(|e| SubmitError::BadResponse(e.to_string()))?;
    parse_status(&body)
}

fn parse_status(body: &serde_json::Value) -> Result<TicketStatus, SubmitError> {
    let text = |k: &str| body.get(k).and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(str::to_string);
    Ok(TicketStatus {
        status: text("status").ok_or_else(|| SubmitError::BadResponse("no status".to_string()))?,
        released_in: text("released_in"),
        question: text("question"),
    })
}

/// POST the reporter's answer to a need_info question. The caller records the egress row.
pub fn reply(origin: &str, ticket_id: &str, token: &str, message: &str) -> Result<(), SubmitError> {
    let resp = client()?
        .post(format!("{}/reply", ticket_url(origin, ticket_id)))
        .bearer_auth(token)
        .json(&serde_json::json!({ "message": message }))
        .send()
        .map_err(|e| SubmitError::Network(e.to_string()))?;
    match resp.status() {
        s if s.is_success() => Ok(()),
        reqwest::StatusCode::BAD_REQUEST => Err(SubmitError::Rejected),
        s => Err(SubmitError::Server(s.as_u16().to_string())),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_receipt_carries_the_reporter_token_when_issued() {
        let r = parse_submit_response(&serde_json::json!({"ok": true, "ticket_id": "t1", "reporter_token": "abc"})).unwrap();
        assert_eq!(r, Receipt { ticket_id: "t1".into(), reporter_token: Some("abc".into()) });
        let old = parse_submit_response(&serde_json::json!({"ok": true, "ticket_id": "t1"})).unwrap();
        assert_eq!(old.reporter_token, None, "an older server issues none");
        assert!(parse_submit_response(&serde_json::json!({"ok": true})).is_err());
    }

    #[test]
    fn a_status_reads_status_release_and_question_only() {
        let s = parse_status(&serde_json::json!({"ok": true, "status": "need_info", "released_in": null, "question": "Which Mac?", "updated_at": "x"})).unwrap();
        assert_eq!(s, TicketStatus { status: "need_info".into(), released_in: None, question: Some("Which Mac?".into()) });
        assert!(parse_status(&serde_json::json!({"ok": true})).is_err());
        assert_eq!(ticket_url("https://a.b/", "t1"), "https://a.b/api/support/report/t1");
    }

    #[test]
    fn report_url_normalises_trailing_slash() {
        assert_eq!(report_url("https://shogunaios.com/"), "https://shogunaios.com/api/support/report");
        assert_eq!(report_url("https://shogunaios.com"), "https://shogunaios.com/api/support/report");
    }

    #[test]
    fn json_omits_absent_optionals() {
        let r = SupportReport {
            category: "bug".into(),
            message: "it broke".into(),
            email: None,
            app_version: None,
            os_version: None,
            plan: None,
            frequency: None,
            urgency: None,
        };
        let v = r.to_json();
        assert_eq!(v["category"], "bug");
        assert_eq!(v["message"], "it broke");
        assert!(v.get("email").is_none());
        assert!(v.get("app_version").is_none());
        assert!(v.get("os_version").is_none());
        assert!(v.get("plan").is_none());
        assert!(v.get("frequency").is_none());
        assert!(v.get("urgency").is_none());
    }

    #[test]
    fn json_carries_frequency_and_urgency_when_chosen() {
        let r = SupportReport {
            category: "bug".into(),
            message: "it broke".into(),
            email: None,
            app_version: None,
            os_version: None,
            plan: None,
            frequency: Some("always".into()),
            urgency: Some("blocking".into()),
        };
        let v = r.to_json();
        assert_eq!(v["frequency"], "always");
        assert_eq!(v["urgency"], "blocking");
    }

    #[test]
    fn closed_vocabularies_match_the_server() {
        // apps/website/src/lib/support.ts is the source of truth; these must not drift.
        assert_eq!(FREQUENCIES, ["once", "sometimes", "always"]);
        assert_eq!(URGENCIES, ["blocking", "annoying"]);
    }

    #[test]
    fn json_carries_diagnostics_when_present() {
        let r = SupportReport {
            category: "feedback".into(),
            message: "love it".into(),
            email: Some("a@b.co".into()),
            app_version: Some("1.2.3".into()),
            os_version: Some("14.5".into()),
            plan: Some("pro".into()),
            frequency: None,
            urgency: None,
        };
        let v = r.to_json();
        assert_eq!(v["email"], "a@b.co");
        assert_eq!(v["app_version"], "1.2.3");
        assert_eq!(v["os_version"], "14.5");
        assert_eq!(v["plan"], "pro");
    }

    #[test]
    fn submit_response_needs_a_ticket_id() {
        let ok = serde_json::json!({ "ok": true, "ticket_id": "abc" });
        assert_eq!(parse_submit_response(&ok).unwrap().ticket_id, "abc");
        let missing = serde_json::json!({ "ok": true });
        assert!(matches!(parse_submit_response(&missing), Err(SubmitError::BadResponse(_))));
        let empty = serde_json::json!({ "ok": true, "ticket_id": "" });
        assert!(matches!(parse_submit_response(&empty), Err(SubmitError::BadResponse(_))));
    }
}
