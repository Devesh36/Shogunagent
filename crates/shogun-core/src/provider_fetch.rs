//! The connector read-sync's HTTP GET (E44): `shogun_integrations::read_sync::ProviderHttp` over
//! the core's HTTP client, the single egress (invariant 9).
//!
//! Only `https://` URLs on the three hosts the read-sync reads (Gmail, Google Calendar, Notion),
//! no redirects followed, a 15 s timeout, bodies capped at 2 MiB. The principal's access token goes
//! into the `Authorization` header and nowhere else (`.expose(` here only for that header;
//! `scripts/check-secret-exposure.py`). Failures are fixed words: never a URL, a body or a token.

use std::io::Read;

use shogun_integrations::read_sync::{FetchError, ProviderHttp};
use shogun_integrations::Secret;

/// The hosts a read-sync GET may reach.
pub const ALLOWED_HOSTS: &[&str] = &["gmail.googleapis.com", "www.googleapis.com", "api.notion.com"];
const TIMEOUT_MS: u64 = 15_000;
const MAX_BODY_BYTES: u64 = 2 * 1024 * 1024;

/// The read-sync's client.
pub struct HttpProviderFetch {
    client: reqwest::blocking::Client,
}

impl HttpProviderFetch {
    pub fn new() -> Result<Self, String> {
        let client = reqwest::blocking::Client::builder()
            .user_agent("shogun/1.0")
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_millis(TIMEOUT_MS))
            .build()
            .map_err(|_| "http client init failed".to_string())?;
        Ok(Self { client })
    }
}

/// Whether `url` is one the read-sync may GET.
pub fn allowed(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|u| u.scheme() == "https" && u.username().is_empty() && u.host_str().is_some_and(|h| ALLOWED_HOSTS.contains(&h)))
}

/// The fetch outcome of an HTTP status.
pub fn classify(status: u16) -> Result<(), FetchError> {
    match status {
        200..=299 => Ok(()),
        401 | 403 => Err(FetchError::Unauthorized),
        404 | 410 => Err(FetchError::NotFound),
        408 | 429 | 500..=599 => Err(FetchError::Transient),
        _ => Err(FetchError::Rejected),
    }
}

impl ProviderHttp for HttpProviderFetch {
    fn get(&self, url: &str, bearer: &Secret, headers: &[(&'static str, &'static str)]) -> Result<String, FetchError> {
        if !allowed(url) {
            return Err(FetchError::Rejected);
        }
        let mut request = self
            .client
            .get(url)
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {}", bearer.expose()))
            .header(reqwest::header::ACCEPT, "application/json");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = request.send().map_err(|_| FetchError::Transient)?;
        classify(response.status().as_u16())?;
        let mut body = Vec::new();
        response.take(MAX_BODY_BYTES + 1).read_to_end(&mut body).map_err(|_| FetchError::Transient)?;
        if body.len() as u64 > MAX_BODY_BYTES {
            return Err(FetchError::Rejected);
        }
        String::from_utf8(body).map_err(|_| FetchError::Rejected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_read_sync_hosts_over_https() {
        assert!(allowed("https://gmail.googleapis.com/gmail/v1/users/me/messages?maxResults=10"));
        assert!(allowed("https://api.notion.com/v1/pages/abc"));
        assert!(!allowed("http://gmail.googleapis.com/gmail/v1/users/me/messages"));
        assert!(!allowed("https://evil.example/gmail"));
        assert!(!allowed("https://user@api.notion.com/v1/pages/abc"));
        assert_eq!(classify(204), Ok(()));
        assert_eq!(classify(401), Err(FetchError::Unauthorized));
        assert_eq!(classify(410), Err(FetchError::NotFound));
        assert_eq!(classify(503), Err(FetchError::Transient));
        assert_eq!(classify(400), Err(FetchError::Rejected));
        let fetch = HttpProviderFetch::new().unwrap();
        assert_eq!(fetch.get("https://evil.example/x", &Secret::new("t"), &[]), Err(FetchError::Rejected), "refused before any request");
    }
}
