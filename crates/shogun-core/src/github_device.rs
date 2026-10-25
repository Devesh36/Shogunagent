//! GitHub App device authorization. This blocking flow never exposes the device code to the UI.
//! The desktop owns cancellation, browser/UI presentation, and Keychain persistence.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use shogun_integrations::oauth::{valid_public_client_id, TokenExchange, TokenSet};

use crate::mcp_http::HttpTokenExchange;

pub const DEVICE_ENDPOINT: &str = "https://github.com/login/device/code";
pub const TOKEN_ENDPOINT: &str = shogun_integrations::oauth::GITHUB_TOKEN_ENDPOINT;
pub const VERIFICATION_URI: &str = "https://github.com/login/device";

const DEFAULT_INTERVAL_SECONDS: i64 = 5;
/// Consecutive failed polls (no response, or a non-2xx status) before giving up.
const MAX_FAILED_POLLS: u32 = 3;
const MAX_WAIT_SECONDS: i64 = 3_600;
const WAIT_SLICE_MS: i64 = 250;

/// Data safe to show to the user. The device code remains private to this module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceAuthorization {
    pub user_code: String,
    pub verification_uri: &'static str,
    pub expires_at_ms: i64,
}

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
        write!(
            f,
            "{}",
            match self {
                Self::InvalidClientId => "invalid GitHub App client ID",
                Self::Network => "GitHub authorization request failed",
                Self::InvalidResponse => "GitHub authorization response was invalid",
                Self::Expired => "GitHub device authorization expired",
                Self::Denied => "GitHub device authorization was denied",
                Self::Cancelled => "GitHub device authorization was cancelled",
                Self::ProviderRejected => "GitHub device authorization failed",
            }
        )
    }
}

impl std::error::Error for DeviceError {}

struct PendingDevice {
    code: String,
    user_code: String,
    expires_at_ms: i64,
    interval_ms: i64,
}

trait Clock {
    fn now_ms(&self) -> i64;
    fn sleep(&self, duration: Duration);
}

struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| i64::try_from(duration.as_millis()).ok())
            .unwrap_or(i64::MAX)
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Start GitHub App device authorization and poll until approval, denial, expiry, or cancellation.
/// The callback should display the code and open `verification_uri`; it may return `Cancelled` if
/// the pending connection has already been replaced. The caller must check its generation again
/// before storing returned tokens, because cancellation can race an in-flight HTTP response.
pub fn connect(
    client_id: &str,
    on_authorization: impl FnOnce(DeviceAuthorization) -> Result<(), DeviceError>,
    cancelled: impl Fn() -> bool,
) -> Result<TokenSet, DeviceError> {
    let exchange = HttpTokenExchange::new().map_err(|_| DeviceError::Network)?;
    connect_with(
        &exchange,
        &SystemClock,
        client_id,
        on_authorization,
        cancelled,
    )
}

fn connect_with(
    exchange: &impl TokenExchange,
    clock: &impl Clock,
    client_id: &str,
    on_authorization: impl FnOnce(DeviceAuthorization) -> Result<(), DeviceError>,
    cancelled: impl Fn() -> bool,
) -> Result<TokenSet, DeviceError> {
    if !valid_public_client_id(client_id) {
        return Err(DeviceError::InvalidClientId);
    }
    if cancelled() {
        return Err(DeviceError::Cancelled);
    }

    let body = exchange
        .post_form(
            DEVICE_ENDPOINT,
            &[
                ("client_id".into(), client_id.into()),
                ("scope".into(), "offline_access".into()),
            ],
        )
        .map_err(|_| DeviceError::Network)?;
    if cancelled() {
        return Err(DeviceError::Cancelled);
    }
    let mut pending = parse_device(&body, clock.now_ms())?;
    on_authorization(DeviceAuthorization {
        user_code: pending.user_code.clone(),
        verification_uri: VERIFICATION_URI,
        expires_at_ms: pending.expires_at_ms,
    })?;

    let mut next_poll_ms = clock.now_ms().saturating_add(pending.interval_ms);
    let mut failed_polls = 0;
    loop {
        wait_until(clock, next_poll_ms, pending.expires_at_ms, &cancelled)?;
        if cancelled() {
            return Err(DeviceError::Cancelled);
        }
        let sent_at_ms = clock.now_ms();
        if sent_at_ms >= pending.expires_at_ms {
            return Err(DeviceError::Expired);
        }
        let body = match exchange.post_form(
            TOKEN_ENDPOINT,
            &[
                ("client_id".into(), client_id.into()),
                ("device_code".into(), pending.code.clone()),
                (
                    "grant_type".into(),
                    "urn:ietf:params:oauth:grant-type:device_code".into(),
                ),
            ],
        ) {
            Ok(body) => body,
            // The user may already have entered the code: a dropped connection is retried at
            // the next interval, and only repeated failure ends the flow.
            Err(_) => {
                failed_polls += 1;
                if failed_polls >= MAX_FAILED_POLLS {
                    return Err(DeviceError::Network);
                }
                next_poll_ms = sent_at_ms.saturating_add(pending.interval_ms);
                continue;
            }
        };
        failed_polls = 0;
        if cancelled() {
            return Err(DeviceError::Cancelled);
        }
        let now_ms = clock.now_ms();
        if now_ms >= pending.expires_at_ms {
            return Err(DeviceError::Expired);
        }
        let response = parse_fields(&body)?;
        if let Some(error) = field(&response, "error") {
            match error {
                "authorization_pending" => {}
                "slow_down" => {
                    pending.interval_ms = pending.interval_ms.saturating_add(5_000);
                    if let Some(interval) =
                        optional_seconds(&response, "interval", MAX_WAIT_SECONDS)?
                    {
                        pending.interval_ms =
                            pending.interval_ms.max(interval.saturating_mul(1_000));
                    }
                }
                "expired_token" | "token_expired" => return Err(DeviceError::Expired),
                "access_denied" => return Err(DeviceError::Denied),
                _ => return Err(DeviceError::ProviderRejected),
            }
            // Measure interval between starts of requests, not after each response.
            next_poll_ms = sent_at_ms.saturating_add(pending.interval_ms).max(now_ms);
            continue;
        }
        return parse_tokens(&response, now_ms);
    }
}

fn wait_until(
    clock: &impl Clock,
    target_ms: i64,
    expires_at_ms: i64,
    cancelled: &impl Fn() -> bool,
) -> Result<(), DeviceError> {
    loop {
        if cancelled() {
            return Err(DeviceError::Cancelled);
        }
        let now_ms = clock.now_ms();
        if now_ms >= expires_at_ms {
            return Err(DeviceError::Expired);
        }
        if now_ms >= target_ms {
            return Ok(());
        }
        let remaining_ms = (target_ms - now_ms)
            .min(expires_at_ms - now_ms)
            .min(WAIT_SLICE_MS);
        clock.sleep(Duration::from_millis(remaining_ms as u64));
    }
}

fn parse_device(body: &str, now_ms: i64) -> Result<PendingDevice, DeviceError> {
    let fields = parse_fields(body)?;
    if field(&fields, "error").is_some() {
        return Err(DeviceError::ProviderRejected);
    }
    let code = field(&fields, "device_code").ok_or(DeviceError::InvalidResponse)?;
    let user_code = field(&fields, "user_code").ok_or(DeviceError::InvalidResponse)?;
    let verification_uri =
        field(&fields, "verification_uri").ok_or(DeviceError::InvalidResponse)?;
    if verification_uri != VERIFICATION_URI
        || code.is_empty()
        || code.len() > 128
        || !code.bytes().all(|byte| byte.is_ascii_graphic())
        || user_code.len() != 9
        || user_code.as_bytes()[4] != b'-'
        || !user_code
            .bytes()
            .enumerate()
            .all(|(i, byte)| i == 4 || byte.is_ascii_alphanumeric())
    {
        return Err(DeviceError::InvalidResponse);
    }
    let expires_in = required_seconds(&fields, "expires_in", MAX_WAIT_SECONDS)?;
    let interval = optional_seconds(&fields, "interval", MAX_WAIT_SECONDS)?
        .unwrap_or(DEFAULT_INTERVAL_SECONDS);
    if interval >= expires_in {
        return Err(DeviceError::InvalidResponse);
    }
    let expires_at_ms = now_ms
        .checked_add(expires_in * 1_000)
        .ok_or(DeviceError::InvalidResponse)?;
    Ok(PendingDevice {
        code: code.into(),
        user_code: user_code.into(),
        expires_at_ms,
        interval_ms: interval * 1_000,
    })
}

fn parse_tokens(fields: &[(String, String)], now_ms: i64) -> Result<TokenSet, DeviceError> {
    let access = field(fields, "access_token").ok_or(DeviceError::InvalidResponse)?;
    let refresh = field(fields, "refresh_token").ok_or(DeviceError::InvalidResponse)?;
    let token_type = field(fields, "token_type").ok_or(DeviceError::InvalidResponse)?;
    let expires_in = required_seconds(fields, "expires_in", 86_400)?;
    if token_type != "bearer" || !valid_token(access) || !valid_token(refresh) {
        return Err(DeviceError::InvalidResponse);
    }
    let expires_at_ms = now_ms
        .checked_add(expires_in * 1_000)
        .ok_or(DeviceError::InvalidResponse)?;
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

fn optional_seconds(
    fields: &[(String, String)],
    key: &str,
    max: i64,
) -> Result<Option<i64>, DeviceError> {
    if field(fields, key).is_none() {
        return Ok(None);
    }
    required_seconds(fields, key, max).map(Some)
}

fn field<'a>(fields: &'a [(String, String)], key: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
}

fn parse_fields(body: &str) -> Result<Vec<(String, String)>, DeviceError> {
    if body.len() > 65_536 {
        return Err(DeviceError::InvalidResponse);
    }
    let fields = if body.trim_start().starts_with('{') {
        let value: serde_json::Value =
            serde_json::from_str(body).map_err(|_| DeviceError::InvalidResponse)?;
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
        url::form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect::<Vec<_>>()
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
    use std::{
        cell::{Cell, RefCell},
        collections::VecDeque,
        rc::Rc,
    };

    struct FakeClock(Rc<Cell<i64>>);
    impl Clock for FakeClock {
        fn now_ms(&self) -> i64 {
            self.0.get()
        }
        fn sleep(&self, duration: Duration) {
            self.0.set(self.0.get() + duration.as_millis() as i64);
        }
    }

    type RecordedCall = (String, Vec<(String, String)>, i64);

    struct FakeExchange {
        responses: RefCell<VecDeque<String>>,
        calls: RefCell<Vec<RecordedCall>>,
        now: Rc<Cell<i64>>,
    }
    impl TokenExchange for FakeExchange {
        fn post_form(&self, endpoint: &str, form: &[(String, String)]) -> Result<String, String> {
            self.calls
                .borrow_mut()
                .push((endpoint.into(), form.to_vec(), self.now.get()));
            match self.responses.borrow_mut().pop_front() {
                Some(response) if response != "!network" => Ok(response),
                _ => Err("no response".into()),
            }
        }
    }

    fn fake(responses: &[&str], now: &Rc<Cell<i64>>) -> FakeExchange {
        FakeExchange {
            responses: RefCell::new(responses.iter().map(|s| (*s).into()).collect()),
            calls: RefCell::new(Vec::new()),
            now: Rc::clone(now),
        }
    }
    const CODE: &str = "device_code=secret-device-code&user_code=ABCD-1234&verification_uri=https%3A%2F%2Fgithub.com%2Flogin%2Fdevice&expires_in=60&interval=2";
    const TOKEN: &str =
        "access_token=ghu_secret&refresh_token=ghr_secret&expires_in=28800&token_type=bearer";

    #[test]
    fn posts_offline_scope_polls_with_interval_and_returns_rotating_tokens() {
        let now = Rc::new(Cell::new(1_000));
        let clock = FakeClock(Rc::clone(&now));
        let exchange = fake(
            &[
                CODE,
                "error=authorization_pending",
                "error=slow_down&interval=7",
                TOKEN,
            ],
            &now,
        );
        let view = RefCell::new(None);
        let tokens = connect_with(
            &exchange,
            &clock,
            "Iv1_abc",
            |authorization| {
                *view.borrow_mut() = Some(authorization);
                Ok(())
            },
            || false,
        )
        .unwrap();
        assert_eq!(tokens.access_token, "ghu_secret");
        assert_eq!(tokens.refresh_token.as_deref(), Some("ghr_secret"));
        assert_eq!(tokens.expires_at_ms, 28_812_000);
        assert_eq!(view.borrow().as_ref().unwrap().expires_at_ms, 61_000);
        assert_eq!(
            view.borrow().as_ref().unwrap().verification_uri,
            VERIFICATION_URI
        );
        let calls = exchange.calls.borrow();
        assert_eq!(calls.len(), 4);
        assert_eq!(calls[0].0, DEVICE_ENDPOINT);
        assert!(calls[0]
            .1
            .contains(&("scope".into(), "offline_access".into())));
        assert_eq!(calls[1].0, TOKEN_ENDPOINT);
        assert_eq!(
            calls[1]
                .1
                .iter()
                .find(|(key, _)| key == "device_code")
                .unwrap()
                .1,
            "secret-device-code"
        );
        assert_eq!(
            calls[1]
                .1
                .iter()
                .find(|(key, _)| key == "grant_type")
                .unwrap()
                .1,
            "urn:ietf:params:oauth:grant-type:device_code"
        );
        assert_eq!(calls[2].2 - calls[1].2, 2_000);
        assert_eq!(calls[3].2 - calls[2].2, 7_000);
    }

    #[test]
    fn poll_failures_retry_until_repeated() {
        let now = Rc::new(Cell::new(0));
        let clock = FakeClock(Rc::clone(&now));
        let exchange = fake(
            &[CODE, "!network", "error=authorization_pending", "!network", TOKEN],
            &now,
        );
        let tokens = connect_with(&exchange, &clock, "Iv1_abc", |_| Ok(()), || false).unwrap();
        assert_eq!(tokens.access_token, "ghu_secret");
        let exchange = fake(&[CODE, "!network", "!network", "!network", TOKEN], &now);
        assert_eq!(
            connect_with(&exchange, &clock, "Iv1_abc", |_| Ok(()), || false),
            Err(DeviceError::Network)
        );
        assert_eq!(exchange.calls.borrow().len(), 4);
    }

    #[test]
    fn cancellation_prevents_network_and_waiting_poll() {
        let now = Rc::new(Cell::new(0));
        let clock = FakeClock(Rc::clone(&now));
        let exchange = fake(&[CODE, TOKEN], &now);
        assert_eq!(
            connect_with(&exchange, &clock, "Iv1_abc", |_| Ok(()), || true),
            Err(DeviceError::Cancelled)
        );
        assert!(exchange.calls.borrow().is_empty());
        let cancelled = Cell::new(false);
        assert_eq!(
            connect_with(
                &exchange,
                &clock,
                "Iv1_abc",
                |_| {
                    cancelled.set(true);
                    Ok(())
                },
                || cancelled.get()
            ),
            Err(DeviceError::Cancelled)
        );
        assert_eq!(exchange.calls.borrow().len(), 1);

        let now = Rc::new(Cell::new(0));
        let clock = FakeClock(Rc::clone(&now));
        let exchange = fake(&[CODE, TOKEN], &now);
        assert_eq!(
            connect_with(
                &exchange,
                &clock,
                "Iv1_abc",
                |_| Ok(()),
                || now.get() >= 1_000
            ),
            Err(DeviceError::Cancelled)
        );
        assert_eq!(exchange.calls.borrow().len(), 1);
    }

    #[test]
    fn rejects_unsafe_responses_and_expiry() {
        assert_eq!(
            parse_device(&CODE.replace("github.com", "evil.example"), 0).err(),
            Some(DeviceError::InvalidResponse)
        );
        assert_eq!(
            parse_device(&CODE.replace("interval=2", "interval=0"), 0).err(),
            Some(DeviceError::InvalidResponse)
        );
        assert_eq!(
            parse_device(&CODE.replace("expires_in=60", "expires_in=2"), 0).err(),
            Some(DeviceError::InvalidResponse)
        );
        assert_eq!(
            parse_tokens(
                &parse_fields("access_token=x&refresh_token=y&expires_in=0&token_type=bearer")
                    .unwrap(),
                0
            ),
            Err(DeviceError::InvalidResponse)
        );
        assert_eq!(
            parse_fields("error=authorization_pending&error=slow_down").err(),
            Some(DeviceError::InvalidResponse)
        );
        let now = Rc::new(Cell::new(0));
        let clock = FakeClock(Rc::clone(&now));
        let exchange = fake(&[CODE, "error=access_denied"], &now);
        assert_eq!(
            connect_with(&exchange, &clock, "Iv1_abc", |_| Ok(()), || false),
            Err(DeviceError::Denied)
        );
        let exchange = fake(&[CODE, "error=authorization_pending"], &now);
        assert_eq!(
            connect_with(&exchange, &clock, "Iv1_abc", |_| Ok(()), || false),
            Err(DeviceError::Network)
        );

        let now = Rc::new(Cell::new(0));
        let clock = FakeClock(Rc::clone(&now));
        let short_code = CODE.replace("expires_in=60", "expires_in=3");
        let exchange = fake(&[&short_code, "error=authorization_pending", TOKEN], &now);
        assert_eq!(
            connect_with(&exchange, &clock, "Iv1_abc", |_| Ok(()), || false),
            Err(DeviceError::Expired)
        );
        assert_eq!(exchange.calls.borrow().len(), 2);
        let json = r#"{"access_token":"ghu_json","refresh_token":"ghr_json","expires_in":28800,"token_type":"bearer"}"#;
        assert_eq!(
            parse_tokens(&parse_fields(json).unwrap(), 0)
                .unwrap()
                .refresh_token
                .as_deref(),
            Some("ghr_json")
        );
    }

    #[test]
    fn redacted_errors_and_client_id_validation() {
        assert!(valid_public_client_id("Iv1.8a61f9b3a7aba766"));
        assert!(!valid_public_client_id("Iv1.8a61f9b3a7aba766\n"));
        let now = Rc::new(Cell::new(0));
        assert_eq!(
            connect_with(
                &fake(&[], &now),
                &FakeClock(now),
                "\nsecret",
                |_| Ok(()),
                || false
            ),
            Err(DeviceError::InvalidClientId)
        );
        assert_eq!(
            parse_device("device_code=secret&error=bad_verification_code", 0).err(),
            Some(DeviceError::ProviderRejected)
        );
        assert!(!format!("{:?}", DeviceError::ProviderRejected).contains("secret"));
    }
}
