//! Authenticated loopback Gmail test backend. Project keys stay outside the desktop.
use serde_json::{json, Value};
use shogun_integrations::composio::ComposioApi;

pub const LOCAL_GMAIL_URL: &str = "http://127.0.0.1:4319";
pub const LOCAL_GMAIL_TOKEN_ACCOUNT: &str = "gmail-local-backend-token";

pub enum GmailApi {
    Direct(crate::composio_send::HttpComposioApi),
    Local(LocalGmailApi),
}
impl ComposioApi for GmailApi {
    fn execute(&self, tool: &str, user_id: &str, arguments: Value) -> Result<Value, String> {
        match self {
            Self::Direct(api) => api.execute(tool, user_id, arguments),
            Self::Local(api) => api.execute(tool, user_id, arguments),
        }
    }
}

pub struct LocalGmailApi {
    client: reqwest::blocking::Client,
    bearer: reqwest::header::HeaderValue,
}
impl LocalGmailApi {
    pub fn new(url: &str, token: &str) -> Result<Self, String> {
        if url != LOCAL_GMAIL_URL {
            return Err("Local Gmail backend must be http://127.0.0.1:4319.".into());
        }
        if token.len() != 64
            || !token
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err("Run the Gmail backend setup to configure its local credential.".into());
        }
        let mut bearer = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| "Local Gmail credential is invalid.")?;
        bearer.set_sensitive(true);
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(std::time::Duration::from_secs(2))
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|_| "Local Gmail client is unavailable.")?;
        Ok(Self { client, bearer })
    }
    fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, String> {
        let mut request = self
            .client
            .request(method, format!("{LOCAL_GMAIL_URL}{path}"))
            .header(reqwest::header::AUTHORIZATION, self.bearer.clone());
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .map_err(|_| "Start the local Gmail backend and retry Connect.")?;
        if !response.status().is_success() {
            return Err(format!(
                "Local Gmail backend refused the request (HTTP {}). Check the Gmail setup guide.",
                response.status().as_u16()
            ));
        }
        use std::io::Read;
        let mut bytes = Vec::new();
        response
            .take(2 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "Local Gmail response could not be read.")?;
        if bytes.len() > 2 * 1024 * 1024 {
            return Err("Local Gmail response exceeded the limit.".into());
        }
        serde_json::from_slice(&bytes).map_err(|_| "Local Gmail response was invalid.".into())
    }
    /// Starts hosted OAuth; does not mark Gmail connected. Consent must be checked by caller.
    pub fn connect(&self) -> Result<Option<String>, String> {
        let value = self.request(
            reqwest::Method::POST,
            "/v1/gmail/connect",
            Some(json!({"consent":true})),
        )?;
        if value.get("status").and_then(Value::as_str) == Some("ACTIVE") {
            return Ok(None);
        }
        let link = value
            .get("redirect_url")
            .and_then(Value::as_str)
            .ok_or("Gmail connection link is missing.")?;
        let url = url::Url::parse(link).map_err(|_| "Gmail connection link is invalid.")?;
        if url.scheme() != "https"
            || url.host_str() != Some("connect.composio.dev")
            || url.port().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
            || !url.path().starts_with("/link/")
        {
            return Err("Gmail connection link is invalid.".into());
        }
        Ok(Some(url.to_string()))
    }
    pub fn status(&self) -> Result<String, String> {
        self.request(reqwest::Method::GET, "/v1/gmail/status", None)?
            .get("status")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| "Gmail connection status is unavailable.".into())
    }
    pub fn disconnect(&self) -> Result<(), String> {
        self.request(reqwest::Method::DELETE, "/v1/gmail/connection", None)
            .map(|_| ())
    }
}
impl ComposioApi for LocalGmailApi {
    fn execute(&self, tool: &str, _user_id: &str, arguments: Value) -> Result<Value, String> {
        if !matches!(
            tool,
            "GMAIL_FETCH_EMAILS" | "GMAIL_FETCH_MESSAGE_BY_THREAD_ID" | "GMAIL_LIST_LABELS"
        ) {
            return Err(
                "Local Gmail testing is read-only. Sending, drafts and label changes are disabled."
                    .into(),
            );
        }
        // The backend derives the user/account from its stored identity, never from client input.
        self.request(
            reqwest::Method::POST,
            "/v1/gmail/execute",
            Some(json!({"tool":tool,"arguments":arguments})),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_non_loopback_and_malformed_credentials() {
        for url in [
            "https://example.com",
            "http://localhost:4319",
            "http://127.0.0.1:4319/",
            "http://127.0.0.1:9999",
        ] {
            assert!(LocalGmailApi::new(url, &"a".repeat(64)).is_err());
        }
        assert!(LocalGmailApi::new(LOCAL_GMAIL_URL, "short").is_err());
        assert!(LocalGmailApi::new(LOCAL_GMAIL_URL, &"Z".repeat(64)).is_err());
        assert!(LocalGmailApi::new(LOCAL_GMAIL_URL, &"a".repeat(64)).is_ok());
    }
    #[test]
    fn writes_never_reach_the_loopback_backend() -> Result<(), String> {
        let api = LocalGmailApi::new(LOCAL_GMAIL_URL, &"a".repeat(64))?;
        for tool in [
            "GMAIL_SEND_EMAIL",
            "GMAIL_CREATE_EMAIL_DRAFT",
            "GMAIL_CREATE_LABEL",
            "GMAIL_MODIFY_THREAD_LABELS",
            "SLACK_SEND_MESSAGE",
        ] {
            let error = api
                .execute(tool, "untrusted-id", json!({}))
                .err()
                .ok_or("write should be refused")?;
            assert!(error.starts_with("Local Gmail testing is read-only."));
        }
        Ok(())
    }
}
