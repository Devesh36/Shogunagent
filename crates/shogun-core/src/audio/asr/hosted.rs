//! Company-funded speech credentials. Only short-lived grants enter this process.
use super::deepgram::DeepgramAuth;
use crate::llm::{
    traceability::{Route, TraceRecord, TraceabilitySink},
    Secret,
};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub type HostedContext = (String, Arc<dyn TraceabilitySink>);
type ContextProvider = dyn Fn() -> Result<HostedContext, String> + Send + Sync;
static CONTEXT: OnceLock<Arc<ContextProvider>> = OnceLock::new();
const ORIGIN: &str = "https://api.shogunaios.com";
const ERROR: &str = "Speech service unavailable. Check your plan and connection, then try again.";

pub fn configure(provider: Arc<ContextProvider>) {
    let _ = CONTEXT.set(provider);
}
pub fn configured() -> bool {
    CONTEXT.get().is_some()
}

pub struct Session {
    credential: Mutex<(Secret, Instant)>,
    bearer: Secret,
    id: String,
    trace: Arc<dyn TraceabilitySink>,
    pub deadline: Instant,
    pub user_tag: String,
}
impl Session {
    pub fn authorization_header(&self) -> Result<String, String> {
        if Instant::now() >= self.deadline {
            return Err(ERROR.into());
        }
        let mut credential = self.credential.lock().map_err(|_| ERROR.to_string())?;
        if Instant::now() >= credential.1 {
            trace(self.trace.as_ref(), "speech_session_refresh")?;
            let requested_at = Instant::now();
            let response = client()?
                .post(format!("{ORIGIN}/v1/asr/refresh"))
                .bearer_auth(self.bearer.expose())
                .json(&serde_json::json!({"session_id": self.id}))
                .send()
                .map_err(|_| ERROR.to_string())?;
            if !response.status().is_success() {
                return Err(ERROR.into());
            }
            use std::io::Read;
            let mut body = Vec::new();
            response
                .take(16_385)
                .read_to_end(&mut body)
                .map_err(|_| ERROR.to_string())?;
            if body.len() > 16_384 {
                return Err(ERROR.into());
            }
            #[derive(serde::Deserialize)]
            struct Refresh {
                access_token: String,
                expires_in: u64,
            }
            let grant: Refresh = serde_json::from_slice(&body).map_err(|_| ERROR.to_string())?;
            if !(5..=60).contains(&grant.expires_in) || !valid_token(&grant.access_token) {
                return Err(ERROR.into());
            }
            *credential = (
                Secret::new(grant.access_token),
                requested_at + Duration::from_secs(grant.expires_in - 2),
            );
        }
        Ok(format!("Bearer {}", credential.0.expose()))
    }
}
fn valid_token(token: &str) -> bool {
    token.split('.').count() == 3
        && token.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
        })
}
impl Drop for Session {
    fn drop(&mut self) {
        // Best effort: a crash or failed finish keeps the reservation, never grants free usage.
        if trace(self.trace.as_ref(), "speech_session_finish").is_err() {
            return;
        }
        if let Ok(client) = client() {
            let _ = client
                .post(format!("{ORIGIN}/v1/asr/finish"))
                .bearer_auth(self.bearer.expose())
                .json(&serde_json::json!({"session_id": self.id}))
                .send();
        }
    }
}
fn trace(sink: &dyn TraceabilitySink, purpose: &str) -> Result<(), String> {
    sink.try_record(TraceRecord::for_chunk(
        Route::Asr,
        purpose,
        "api.shogunaios.com",
        "",
        true,
    ))
    .map_err(|_| ERROR.to_string())
}
fn client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("shogun/1.0")
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|_| ERROR.to_string())
}
struct Group {
    streams: u8,
    session: Mutex<Option<Arc<Session>>>,
}
impl Group {
    fn session(&self) -> Result<Arc<Session>, String> {
        let mut cached = self.session.lock().map_err(|_| ERROR.to_string())?;
        if let Some(session) = cached.as_ref() {
            if Instant::now() < session.deadline {
                return Ok(session.clone());
            }
            return Err(ERROR.into()); // the reservation deadline never moves
        }
        let provider = CONTEXT.get().ok_or(ERROR)?;
        let (bearer, sink) = provider()?;
        trace(sink.as_ref(), "speech_session_grant")?;
        let requested_at = Instant::now();
        let response = client()?
            .post(format!("{ORIGIN}/v1/asr/grant"))
            .bearer_auth(&bearer)
            .json(&serde_json::json!({"streams": self.streams}))
            .send()
            .map_err(|_| ERROR.to_string())?;
        if !response.status().is_success() {
            return Err(ERROR.into());
        }
        use std::io::Read;
        let mut body = Vec::new();
        response
            .take(16_385)
            .read_to_end(&mut body)
            .map_err(|_| ERROR.to_string())?;
        if body.len() > 16_384 {
            return Err(ERROR.into());
        }
        #[derive(serde::Deserialize)]
        struct Grant {
            user_tag: String,
            access_token: String,
            expires_in: u64,
            session_id: String,
            max_session_seconds: u64,
        }
        let grant: Grant = serde_json::from_slice(&body).map_err(|_| ERROR.to_string())?;
        if grant.user_tag.len() != 64
            || !grant.user_tag.bytes().all(|b| b.is_ascii_hexdigit())
            || grant.expires_in < 5
            || grant.expires_in > 60
            || grant.max_session_seconds == 0
            || grant.max_session_seconds > 3600
            || grant.session_id.len() != 36
            || !valid_token(&grant.access_token)
        {
            return Err(ERROR.into());
        }
        let now = requested_at;
        let session = Arc::new(Session {
            credential: Mutex::new((
                Secret::new(grant.access_token),
                now + Duration::from_secs(grant.expires_in - 2),
            )),
            bearer: Secret::new(bearer),
            user_tag: grant.user_tag,
            id: grant.session_id,
            trace: sink,
            deadline: now + Duration::from_secs(grant.max_session_seconds),
        });
        *cached = Some(session.clone());
        Ok(session)
    }
}
pub struct HostedAuth {
    group: Arc<Group>,
}
impl DeepgramAuth for HostedAuth {
    fn authorization_header(&mut self) -> Result<String, String> {
        self.group.session()?.authorization_header()
    }
    fn hosted_session(&mut self) -> Result<Option<Arc<Session>>, String> {
        self.group.session().map(Some)
    }
    fn is_hosted(&self) -> bool {
        true
    }
}
pub fn auth(streams: u8) -> Box<dyn DeepgramAuth> {
    Box::new(HostedAuth {
        group: Arc::new(Group {
            streams,
            session: Mutex::new(None),
        }),
    })
}
pub fn pair() -> super::deepgram::DeepgramAuthPair {
    let group = Arc::new(Group {
        streams: 2,
        session: Mutex::new(None),
    });
    (
        Box::new(HostedAuth {
            group: group.clone(),
        }),
        Box::new(HostedAuth { group }),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    struct RefuseTrace;
    impl TraceabilitySink for RefuseTrace {
        fn record(&self, _: TraceRecord) {}
        fn try_record(
            &self,
            _: TraceRecord,
        ) -> Result<(), crate::llm::traceability::TraceUnrecorded> {
            Err(crate::llm::traceability::TraceUnrecorded)
        }
    }
    fn session(expires: Instant, deadline: Instant) -> Session {
        Session {
            credential: Mutex::new((Secret::new("test.jwt.signature"), expires)),
            bearer: Secret::new("test-license"),
            id: "test-session".into(),
            trace: Arc::new(RefuseTrace),
            deadline,
            user_tag: "0".repeat(64),
        }
    }
    #[test]
    fn reservation_deadline_wins_even_when_the_token_is_fresh() {
        let now = Instant::now();
        let session = session(now + Duration::from_secs(60), now);
        assert_eq!(session.authorization_header().unwrap_err(), ERROR);
    }
    #[test]
    fn refresh_fails_closed_when_the_trace_cannot_be_written() {
        let now = Instant::now();
        let session = session(now, now + Duration::from_secs(60));
        assert_eq!(session.authorization_header().unwrap_err(), ERROR);
    }
    #[test]
    fn fresh_grant_is_used_in_memory_without_another_paid_mint() {
        let until = Instant::now() + Duration::from_secs(60);
        let session = session(until, until);
        assert_eq!(
            session.authorization_header().unwrap(),
            "Bearer test.jwt.signature"
        );
    }
    #[test]
    fn token_shape_rejects_empty_segments_and_header_injection() {
        assert!(valid_token("abc.def_123.sig-123"));
        for invalid in ["", "abc", "abc..sig", "a.b.c.d", "a.b.c\r\nInjected: yes"] {
            assert!(!valid_token(invalid));
        }
    }
    #[test]
    fn paired_channels_share_one_reservation_without_minting_when_constructed() {
        let group = Arc::new(Group {
            streams: 2,
            session: Mutex::new(None),
        });
        let first = HostedAuth {
            group: group.clone(),
        };
        let second = HostedAuth {
            group: group.clone(),
        };
        assert!(Arc::ptr_eq(&first.group, &second.group));
        assert!(group.session.lock().unwrap().is_none());
    }
}
