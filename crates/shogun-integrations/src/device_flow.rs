//! The GitHub App device authorization grant (RFC 8628) as pure steps: the request forms, and the
//! parsers for GitHub's two answers. No network and no clock: the caller sends each form through
//! its [`crate::oauth::TokenExchange`] (the core's single egress) and decides when to poll.
//!
//! Two callers share these steps (E50):
//! - the Mac app's blocking flow, `shogun_core::github_device::connect`, which polls in a loop;
//! - the hosted sidecar's [`crate::hosted_connect`], which starts a flow on one request and polls
//!   once per human "Check" on another, with the device code sealed in the principal's store
//!   between them.
//!
//! Every error is a fixed word: never a GitHub response body, a device code or a token.

use crate::oauth::TokenSet;

/// Where a device flow starts.
pub const DEVICE_ENDPOINT: &str = "https://github.com/login/device/code";
/// Where it is polled (GitHub's OAuth token endpoint).
pub const TOKEN_ENDPOINT: &str = crate::oauth::GITHUB_TOKEN_ENDPOINT;
/// The only page a human is ever sent to; an answer naming any other is refused.
pub const VERIFICATION_URI: &str = "https://github.com/login/device";
/// The poll interval when GitHub names none (RFC 8628 §3.2).
pub const DEFAULT_INTERVAL_SECONDS: i64 = 5;
/// The longest a device flow may last, and the longest interval accepted.
pub const MAX_WAIT_SECONDS: i64 = 3_600;
/// What RFC 8628 §3.5 asks a client to add to its interval on `slow_down`.
pub const SLOW_DOWN_STEP_MS: i64 = 5_000;

/// Content-free failures; never includes GitHub response bodies, device codes, or tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceError {
    InvalidClientId,
    Network,
    InvalidResponse,
    Expired,
    Denied,
    Cancelled,
    ProviderRejected,
}

impl std::fmt::Display for DeviceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidClientId => "invalid GitHub App client ID",
            Self::Network => "GitHub authorization request failed",
            Self::InvalidResponse => "GitHub authorization response was invalid",
            Self::Expired => "GitHub device authorization expired",
            Self::Denied => "GitHub device authorization was denied",
            Self::Cancelled => "GitHub device authorization was cancelled",
            Self::ProviderRejected => "GitHub device authorization failed",
        })
    }
}

impl std::error::Error for DeviceError {}

/// A started device flow. `device_code` is a credential: it is never shown, logged or returned;
/// the hosted vault keeps it sealed in the principal's store.
#[derive(Clone, PartialEq, Eq)]
pub struct DeviceGrant {
    pub device_code: String,
    /// What the human types at [`VERIFICATION_URI`] (`ABCD-1234`). Safe to show.
    pub user_code: String,
    pub expires_at_ms: i64,
    pub interval_ms: i64,
}

impl std::fmt::Debug for DeviceGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceGrant")
            .field("device_code", &"[redacted]")
            .field("user_code", &self.user_code)
            .field("expires_at_ms", &self.expires_at_ms)
            .field("interval_ms", &self.interval_ms)
            .finish()
    }
}

/// One poll's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DevicePoll {
    /// The human has not entered the code yet: poll again after the interval.
    Pending,
    /// Poll less often: add [`SLOW_DOWN_STEP_MS`], or use `interval_s` when it is larger.
    SlowDown { interval_s: Option<i64> },
    /// The human approved: the user-to-server tokenset (expiring, with a rotating refresh token).
    Tokens(TokenSet),
}

/// The form that starts a flow. `offline_access` asks for the refresh token.
pub fn start_form(client_id: &str) -> Vec<(String, String)> {
    vec![("client_id".into(), client_id.into()), ("scope".into(), "offline_access".into())]
}

/// The form of one poll.
pub fn poll_form(client_id: &str, device_code: &str) -> Vec<(String, String)> {
    vec![
        ("client_id".into(), client_id.into()),
        ("device_code".into(), device_code.into()),
        ("grant_type".into(), "urn:ietf:params:oauth:grant-type:device_code".into()),
    ]
}

/// GitHub's answer to [`start_form`], checked: the verification page is GitHub's own, the user
/// code has GitHub's shape, the interval is shorter than the lifetime.
pub fn parse_device(body: &str, now_ms: i64) -> Result<DeviceGrant, DeviceError> {
    let fields = parse_fields(body)?;
    if field(&fields, "error").is_some() {
        return Err(DeviceError::ProviderRejected);
    }
    let code = field(&fields, "device_code").ok_or(DeviceError::InvalidResponse)?;
    let user_code = field(&fields, "user_code").ok_or(DeviceError::InvalidResponse)?;
    let verification_uri = field(&fields, "verification_uri").ok_or(DeviceError::InvalidResponse)?;
    if verification_uri != VERIFICATION_URI
        || code.is_empty()
        || code.len() > 128
        || !code.bytes().all(|byte| byte.is_ascii_graphic())
        || user_code.len() != 9
        || user_code.as_bytes()[4] != b'-'
        || !user_code.bytes().enumerate().all(|(i, byte)| i == 4 || byte.is_ascii_alphanumeric())
    {
        return Err(DeviceError::InvalidResponse);
    }
    let expires_in = required_seconds(&fields, "expires_in", MAX_WAIT_SECONDS)?;
    let interval = optional_seconds(&fields, "interval", MAX_WAIT_SECONDS)?.unwrap_or(DEFAULT_INTERVAL_SECONDS);
    if interval >= expires_in {
        return Err(DeviceError::InvalidResponse);
    }
    let expires_at_ms = now_ms.checked_add(expires_in * 1_000).ok_or(DeviceError::InvalidResponse)?;
    Ok(DeviceGrant { device_code: code.into(), user_code: user_code.into(), expires_at_ms, interval_ms: interval * 1_000 })
}

/// GitHub's answer to one [`poll_form`].
pub fn parse_poll(body: &str, now_ms: i64) -> Result<DevicePoll, DeviceError> {
    let fields = parse_fields(body)?;
    if let Some(error) = field(&fields, "error") {
        return match error {
            "authorization_pending" => Ok(DevicePoll::Pending),
            "slow_down" => Ok(DevicePoll::SlowDown { interval_s: optional_seconds(&fields, "interval", MAX_WAIT_SECONDS)? }),
            "expired_token" | "token_expired" => Err(DeviceError::Expired),
            "access_denied" => Err(DeviceError::Denied),
            _ => Err(DeviceError::ProviderRejected),
        };
    }
    parse_tokens(&fields, now_ms).map(DevicePoll::Tokens)
}

/// The tokenset of an approved poll: a bearer access token with a lifetime and a refresh token.
pub fn parse_tokens(fields: &[(String, String)], now_ms: i64) -> Result<TokenSet, DeviceError> {
    let access = field(fields, "access_token").ok_or(DeviceError::InvalidResponse)?;
    let refresh = field(fields, "refresh_token").ok_or(DeviceError::InvalidResponse)?;
    let token_type = field(fields, "token_type").ok_or(DeviceError::InvalidResponse)?;
    let expires_in = required_seconds(fields, "expires_in", 86_400)?;
    if token_type != "bearer" || !valid_token(access) || !valid_token(refresh) {
        return Err(DeviceError::InvalidResponse);
    }
    let expires_at_ms = now_ms.checked_add(expires_in * 1_000).ok_or(DeviceError::InvalidResponse)?;
    Ok(TokenSet {
        access_token: access.into(),
        refresh_token: Some(refresh.into()),
        expires_at_ms,
        account_label: field(fields, "login").map(str::to_owned),
    })
}

fn valid_token(token: &str) -> bool {
    !token.is_empty() && token.len() <= 4_096 && token.bytes().all(|byte| byte.is_ascii_graphic())
}

fn required_seconds(fields: &[(String, String)], key: &str, max: i64) -> Result<i64, DeviceError> {
    let seconds = field(fields, key)
        .ok_or(DeviceError::InvalidResponse)?
        .parse::<i64>()
        .map_err(|_| DeviceError::InvalidResponse)?;
    if !(1..=max).contains(&seconds) {
        return Err(DeviceError::InvalidResponse);
    }
    Ok(seconds)
}

/// An optional positive number of seconds no larger than `max`.
pub fn optional_seconds(fields: &[(String, String)], key: &str, max: i64) -> Result<Option<i64>, DeviceError> {
    if field(fields, key).is_none() {
        return Ok(None);
    }
    required_seconds(fields, key, max).map(Some)
}

/// The value of `key`, when present.
pub fn field<'a>(fields: &'a [(String, String)], key: &str) -> Option<&'a str> {
    fields.iter().find(|(name, _)| name == key).map(|(_, value)| value.as_str())
}

/// GitHub answers form-encoded by default and JSON when asked: both become flat pairs. A repeated
/// name, a nested value or more than 32 fields is refused.
pub fn parse_fields(body: &str) -> Result<Vec<(String, String)>, DeviceError> {
    if body.len() > 65_536 {
        return Err(DeviceError::InvalidResponse);
    }
    let fields = if body.trim_start().starts_with('{') {
        let value: serde_json::Value = serde_json::from_str(body).map_err(|_| DeviceError::InvalidResponse)?;
        let object = value.as_object().ok_or(DeviceError::InvalidResponse)?;
        object
            .iter()
            .map(|(key, value)| {
                let value = match value {
                    serde_json::Value::String(value) => value.clone(),
                    serde_json::Value::Number(value) => value.to_string(),
                    _ => return Err(DeviceError::InvalidResponse),
                };
                Ok((key.clone(), value))
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        url::form_urlencoded::parse(body.as_bytes()).into_owned().collect::<Vec<_>>()
    };
    if fields.is_empty() || fields.len() > 32 {
        return Err(DeviceError::InvalidResponse);
    }
    let mut names = std::collections::HashSet::new();
    if fields.iter().any(|(key, _)| !names.insert(key)) {
        return Err(DeviceError::InvalidResponse);
    }
    Ok(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODE: &str = "device_code=secret-device-code&user_code=ABCD-1234&verification_uri=https%3A%2F%2Fgithub.com%2Flogin%2Fdevice&expires_in=900&interval=5";

    #[test]
    fn a_start_and_its_polls_parse_into_typed_steps() {
        let grant = parse_device(CODE, 1_000).unwrap();
        assert_eq!((grant.user_code.as_str(), grant.expires_at_ms, grant.interval_ms), ("ABCD-1234", 901_000, 5_000));
        assert!(!format!("{grant:?}").contains("secret-device-code"));
        assert_eq!(parse_poll("error=authorization_pending", 0), Ok(DevicePoll::Pending));
        assert_eq!(parse_poll("error=slow_down&interval=10", 0), Ok(DevicePoll::SlowDown { interval_s: Some(10) }));
        assert_eq!(parse_poll("error=access_denied", 0), Err(DeviceError::Denied));
        assert_eq!(parse_poll("error=expired_token", 0), Err(DeviceError::Expired));
        assert_eq!(parse_poll("error=unsupported_grant_type", 0), Err(DeviceError::ProviderRejected));
        match parse_poll("access_token=ghu_x1&refresh_token=ghr_x1&expires_in=28800&token_type=bearer", 10).unwrap() {
            DevicePoll::Tokens(t) => assert_eq!((t.access_token.as_str(), t.expires_at_ms), ("ghu_x1", 28_800_010)),
            other => panic!("{other:?}"),
        }
        assert_eq!(start_form("Iv1_abc")[1], ("scope".to_string(), "offline_access".to_string()));
        assert!(poll_form("Iv1_abc", "dc").contains(&("device_code".into(), "dc".into())));
    }

    #[test]
    fn unsafe_answers_are_refused() {
        assert_eq!(parse_device(&CODE.replace("github.com", "evil.example"), 0).err(), Some(DeviceError::InvalidResponse));
        assert_eq!(parse_device(&CODE.replace("interval=5", "interval=900"), 0).err(), Some(DeviceError::InvalidResponse));
        assert_eq!(parse_device("device_code=x&error=bad", 0).err(), Some(DeviceError::ProviderRejected));
        assert_eq!(parse_fields("a=1&a=2").err(), Some(DeviceError::InvalidResponse));
        assert_eq!(parse_poll("access_token=x&refresh_token=y&expires_in=0&token_type=bearer", 0), Err(DeviceError::InvalidResponse));
    }
}
