//! Deepgram Nova-3 ASR (meeting default, 2026-08-05).
//!
//! Company-funded short-lived grants are the default (#1101 D2); BYOK is optional.
//! Audio travels directly to Deepgram, process-only with mip_opt_out=true.

pub mod meeting_live;

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::super::{Segment, SAMPLE_RATE};
use super::Transcriber;
use crate::llm::traceability::{digest, Route, TraceRecord, TraceUnrecorded, TraceabilitySink};

const DEFAULT_LISTEN: &str = "https://api.deepgram.com/v1/listen";
const DEFAULT_MODEL: &str = "nova-3";
/// Deepgram endpointing silence (ms) for meetings: how long a pause lasts before a line is
/// finalized. At 300 ms lines were cut at every comma and the multilingual model decided each
/// short piece with little context (a simulated Japanese meeting kept 5 of 12 key facts at 300 ms,
/// 8 of 12 at 500-2,000 ms). Interims keep captions live; only the final line, and the
/// translation that waits for it, arrives up to ~0.7 s later.
const LIVE_ENDPOINTING_MS: u32 = 1000;
/// ~100 ms of 16 kHz mono before flushing a WS binary frame.
pub const LIVE_CHUNK_SAMPLES: usize = (SAMPLE_RATE as usize) / 10;
const PUBLIC_PROVIDER_FAILURE: &str = "Speech provider unavailable. Try again.";
/// Dictation's flush was sent but the provider never acknowledged it within the grace period.
/// Content-free and public like [`PUBLIC_PROVIDER_FAILURE`]; distinct so the caller's log can tell
/// a finalize timeout from a provider that ended the stream or returned nothing (issue #974).
const DICTATION_FINALIZE_TIMED_OUT: &str = "Speech provider did not finish in time. Try again.";

/// Whether a failed dictation flush (`finish_results_with_partial`) was the provider not
/// finishing in time, rather than the stream ending or the provider reporting a failure.
pub fn is_finalize_timeout(error: &str) -> bool {
    error == DICTATION_FINALIZE_TIMED_OUT
}
const MISSING_MEETING_TRANSCRIPTION_KEY: &str =
    "Speech service is not ready. Check your plan and connection.";

/// Provider libraries may include request URLs in errors. URLs carry opt-in keyterms, so no
/// Deepgram transport detail may reach application logs or the voice UI.
pub fn public_error(_error: &str) -> String {
    PUBLIC_PROVIDER_FAILURE.to_string()
}

/// How the client obtains an Authorization header value (`Token …` or `Bearer …`).
pub trait DeepgramAuth: Send {
    fn is_hosted(&self) -> bool {
        false
    }
    fn hosted_session(&mut self) -> Result<Option<Arc<super::hosted::Session>>, String> {
        Ok(None)
    }
    fn authorization_header(&mut self) -> Result<String, String>;
}

/// Keychain-backed API key (`com.selectkk.shogun` / `deepgram-asr`). Cached per session.
/// macOS-only: the Keychain store does not exist on other targets (Linux CI builds `net`).
#[cfg(target_os = "macos")]
#[derive(Default)]
pub struct KeychainKeyAuth {
    cached: Option<String>,
}

#[cfg(target_os = "macos")]
impl DeepgramAuth for KeychainKeyAuth {
    fn authorization_header(&mut self) -> Result<String, String> {
        if let Some(ref key) = self.cached {
            return Ok(format!("Token {key}"));
        }
        let key = shogun_integrations::keychain_store::get_deepgram_asr_key().ok_or_else(|| {
            "Meeting transcription key not in Keychain — add it in Settings → Meeting notes"
                .to_string()
        })?;
        self.cached = Some(key.clone());
        Ok(format!("Token {key}"))
    }
}

/// Debug-only long-lived key from env. **Never compiled into release auth resolution.**
#[derive(Default)]
pub struct DebugEnvKeyAuth;

impl DeepgramAuth for DebugEnvKeyAuth {
    fn authorization_header(&mut self) -> Result<String, String> {
        #[cfg(debug_assertions)]
        {
            let key = std::env::var("SHOGUN_DEEPGRAM_API_KEY").map_err(|_| {
                "set SHOGUN_DEEPGRAM_API_KEY for debug Deepgram ASR (never ships in release)"
                    .to_string()
            })?;
            if key.trim().is_empty() {
                return Err("SHOGUN_DEEPGRAM_API_KEY is empty".into());
            }
            Ok(format!("Token {}", key.trim()))
        }
        #[cfg(not(debug_assertions))]
        {
            Err("Deepgram debug API key is unavailable in release builds".into())
        }
    }
}

/// Wire config. Auth is separate so paid-plan gating can live in Rust core later without
/// baking secrets into the binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeepgramConfig {
    pub listen_endpoint: String,
    pub model: String,
    /// Deepgram `language` query param (`multi`, `en`, `ja`, …).
    pub language: String,
    /// Always true in production construction.
    pub mip_opt_out: bool,
    /// Traceability `purpose` (e.g. `meeting_asr`, `voice_dictation`).
    pub purpose: String,
    /// Bounded, explicit vocabulary hints for Nova-3. Only dictation callers populate this.
    pub keyterms: Vec<String>,
    /// Ask Deepgram to separate speakers within one stream (live meeting lane only). The mixed
    /// system-tap lane carries every remote participant, so without this every voice is one
    /// `other`. Off by default — dictation and the HTTP batch fallback never diarize.
    pub diarize: bool,
}

impl Default for DeepgramConfig {
    fn default() -> Self {
        Self {
            listen_endpoint: DEFAULT_LISTEN.into(),
            model: DEFAULT_MODEL.into(),
            language: "multi".into(),
            mip_opt_out: true,
            purpose: "meeting_asr".into(),
            keyterms: Vec::new(),
            diarize: false,
        }
    }
}

impl DeepgramConfig {
    pub fn with_language(mut self, language: impl Into<String>) -> Self {
        self.language = language.into();
        self
    }

    pub fn with_purpose(mut self, purpose: impl Into<String>) -> Self {
        self.purpose = purpose.into();
        self
    }

    /// Set vocabulary hints. Invalid, duplicate, and excess values are excluded before URL build.
    pub fn with_keyterms(mut self, keyterms: impl IntoIterator<Item = String>) -> Self {
        self.keyterms = bounded_keyterms(keyterms);
        self
    }

    /// Separate speakers within one live stream (meeting lane).
    pub fn with_diarize(mut self, diarize: bool) -> Self {
        self.diarize = diarize;
        self
    }
}

/// The company-funded grant first (#1101); the user's own key only when the grant is refused or
/// no grant can be asked for (the override past the plan allowance, #1334).
/// `SHOGUN_DEEPGRAM_API_KEY` is a development-only override, never a release credential.
pub fn resolve_auth() -> Result<Box<dyn DeepgramAuth>, String> {
    resolve_with(own_key_interactive)
}

/// Resolve credentials for passive/latency-sensitive dictation work without a Keychain dialog
/// or legacy-item migration. Interactive meeting/settings callers keep `resolve_auth`.
pub fn resolve_auth_non_interactive() -> Result<Box<dyn DeepgramAuth>, String> {
    resolve_with(own_key_non_interactive)
}

fn resolve_with(
    own: fn() -> Option<Box<dyn DeepgramAuth>>,
) -> Result<Box<dyn DeepgramAuth>, String> {
    #[cfg(debug_assertions)]
    if std::env::var("SHOGUN_DEEPGRAM_API_KEY").is_ok() {
        return Ok(Box::new(DebugEnvKeyAuth));
    }
    if super::hosted::configured() {
        return Ok(Box::new(HostedThenOwn::new(super::hosted::auth(1), own)));
    }
    own().ok_or_else(|| MISSING_MEETING_TRANSCRIPTION_KEY.into())
}

fn own_key_interactive() -> Option<Box<dyn DeepgramAuth>> {
    #[cfg(target_os = "macos")]
    if shogun_integrations::keychain_store::deepgram_asr_configured() {
        return Some(Box::new(KeychainKeyAuth::default()));
    }
    None
}

/// The user's own key alone, read without a dialog: for paths a company grant cannot serve
/// (dictation's recorded-clip fallback, which has no live grant to lease).
pub fn own_auth_non_interactive() -> Option<Box<dyn DeepgramAuth>> {
    own_key_non_interactive()
}

fn own_key_non_interactive() -> Option<Box<dyn DeepgramAuth>> {
    #[cfg(target_os = "macos")]
    if let Some(key) = shogun_integrations::keychain_store::get_generic_secret_non_interactive(
        shogun_integrations::keychain_store::DEEPGRAM_ASR_ACCOUNT,
    )
    .ok()
    .and_then(|bytes| String::from_utf8(bytes).ok())
    .map(|key| key.trim().to_string())
    .filter(|key| !key.is_empty())
    {
        return Some(Box::new(KeychainKeyAuth { cached: Some(key) }));
    }
    None
}

/// A company grant that hands over to the user's own key the first time the grant is refused.
/// The own key is looked up only then, so a working grant never touches the Keychain.
struct HostedThenOwn {
    hosted: Box<dyn DeepgramAuth>,
    own: fn() -> Option<Box<dyn DeepgramAuth>>,
    fallback: Option<Box<dyn DeepgramAuth>>,
}

impl HostedThenOwn {
    fn new(hosted: Box<dyn DeepgramAuth>, own: fn() -> Option<Box<dyn DeepgramAuth>>) -> Self {
        Self { hosted, own, fallback: None }
    }

    /// Switch to the own key after `error`; without one the grant's error stands.
    fn fall_back(&mut self, error: String) -> Result<(), String> {
        self.fallback = Some((self.own)().ok_or(error)?);
        Ok(())
    }
}

impl DeepgramAuth for HostedThenOwn {
    fn is_hosted(&self) -> bool {
        self.fallback.is_none()
    }
    fn hosted_session(&mut self) -> Result<Option<Arc<super::hosted::Session>>, String> {
        if self.fallback.is_some() {
            return Ok(None);
        }
        match self.hosted.hosted_session() {
            Err(error) => self.fall_back(error).map(|()| None),
            ok => ok,
        }
    }
    fn authorization_header(&mut self) -> Result<String, String> {
        if self.fallback.is_none() {
            match self.hosted.authorization_header() {
                Err(error) => self.fall_back(error)?,
                ok => return ok,
            }
        }
        match self.fallback.as_mut() {
            Some(own) => own.authorization_header(),
            None => Err(MISSING_MEETING_TRANSCRIPTION_KEY.into()),
        }
    }
}

/// Credentials for the two independently connected meeting channels.
pub type DeepgramAuthPair = (Box<dyn DeepgramAuth>, Box<dyn DeepgramAuth>);

/// A dual-channel meeting shares one grant and one accounting lease.
pub fn resolve_auth_pair() -> Result<DeepgramAuthPair, String> {
    let first = resolve_auth()?;
    if first.is_hosted() {
        let (a, b) = super::hosted::pair();
        Ok((
            Box::new(HostedThenOwn::new(a, own_key_interactive)),
            Box::new(HostedThenOwn::new(b, own_key_interactive)),
        ))
    } else {
        Ok((first, resolve_auth()?))
    }
}

/// What `GET /v1/projects` said about a candidate Deepgram key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCheck {
    /// Deepgram accepted the key.
    Valid,
    /// Deepgram answered 401/403 — the key is definitively rejected.
    Rejected,
    /// The check could not run (offline, DNS, TLS, timeout): says nothing about the key.
    Unreachable,
}

fn classify_key_check_status(status: u16) -> KeyCheck {
    match status {
        200..=299 => KeyCheck::Valid,
        401 | 403 => KeyCheck::Rejected,
        _ => KeyCheck::Unreachable,
    }
}

/// Where a candidate key is verified: the endpoint Deepgram itself documents for key checks.
const KEY_CHECK_ENDPOINT: &str = "https://api.deepgram.com/v1/projects";
const KEY_CHECK_HOST: &str = "api.deepgram.com";

/// Ask Deepgram whether `key` works before the app stores and displays it as connected.
///
/// `Authorization: Token <key>` against `GET https://api.deepgram.com/v1/projects`. The response
/// body is never read — only the status is consulted, via [`classify_key_check_status`]:
///
/// - 401 / 403 → [`KeyCheck::Rejected`]: the only verdict that blocks the save;
/// - 2xx → [`KeyCheck::Valid`];
/// - anything else (429, 5xx, DNS, TLS, timeout) → [`KeyCheck::Unreachable`].
///
/// **Fail-open on purpose**: a BYOK key must be savable on an offline Mac, so `Unreachable`
/// saves anyway. The cost is that a rate limit or a provider outage leaves the UI claiming
/// "Connected" for a key that may not work; the ASR call itself surfaces a rejection later.
///
/// Recorded in the traceability ledger with an empty chunk, like `Route::Billing`: the request
/// carries only the key itself, but the ledger exists to show that a connection happened, and
/// to whom (invariant 3).
pub fn check_key(key: &str, sink: &dyn TraceabilitySink) -> KeyCheck {
    check_key_at(key, sink, KEY_CHECK_ENDPOINT)
}

/// [`check_key`] against an explicit endpoint, so a test can prove the ledger row is written
/// before any send without ever opening a socket to the real provider.
fn check_key_at(key: &str, sink: &dyn TraceabilitySink, endpoint: &str) -> KeyCheck {
    // Record before the request — an unrecorded send is the state the ledger exists to prevent.
    sink.record(TraceRecord::for_chunk(
        Route::Asr,
        "deepgram_key_check",
        KEY_CHECK_HOST,
        "",
        true,
    ));
    // Same conventions as the licence / support / transport clients: identify ourselves, refuse
    // plaintext, and never hang the caller past a bounded timeout.
    let client = match reqwest::blocking::Client::builder()
        .user_agent("shogun/1.0")
        .https_only(true)
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(client) => client,
        Err(_) => return KeyCheck::Unreachable,
    };
    let response = client
        .get(endpoint)
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Token {}", key.trim()),
        )
        .send();
    match response {
        Ok(resp) => classify_key_check_status(resp.status().as_u16()),
        Err(_) => KeyCheck::Unreachable,
    }
}

/// Utterance-batch Deepgram listen client implementing [`Transcriber`].
///
/// The traceability sink is **required**, not optional (invariant 3): audio leaving the device
/// for a third party is exactly the egress the ledger exists to show, and a client that could be
/// built without a sink would make "sent, unrecorded" reachable whenever the DB was not in
/// Tauri state. A caller that cannot supply a sink cannot start ASR.
pub struct Deepgram {
    config: DeepgramConfig,
    auth: Box<dyn DeepgramAuth>,
    client: reqwest::blocking::Client,
    trace: Arc<dyn TraceabilitySink>,
}

impl Deepgram {
    pub fn new(
        config: DeepgramConfig,
        auth: Box<dyn DeepgramAuth>,
        trace: Arc<dyn TraceabilitySink>,
    ) -> Result<Self, String> {
        if !config.mip_opt_out {
            return Err("Deepgram mip_opt_out must be true (company policy)".into());
        }
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| format!("deepgram http client: {e}"))?;
        Ok(Self {
            config,
            auth,
            client,
            trace,
        })
    }

    fn listen_url(&self) -> String {
        http_listen_url(&self.config)
    }

    fn record_egress(&self, pcm_bytes: usize, duration_ms: u64) -> Result<(), String> {
        record_asr_attempt(
            self.trace.as_ref(),
            &self.config.purpose,
            pcm_bytes,
            duration_ms,
            !self.config.keyterms.is_empty(),
        )
        .map_err(|_| TRACE_UNRECORDED.to_string())
    }
}

/// Meeting vs hold-to-talk live query params.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveMode {
    /// Interim partials + endpointing for the meeting overlay.
    Meeting,
    /// Dictation finalize-on-release. Interims are on for the live preview; a completed
    /// dictation inserts only `is_final` text. A failed hold's labeled `[Partial dictation]`
    /// may end with the last interim, so the words spoken just before the failure are not lost.
    Voice,
}

/// One transcript event from a live WebSocket session.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveResult {
    pub text: String,
    pub is_final: bool,
    pub speech_final: bool,
    pub confidence: f64,
    /// Diarization slot within this stream (`diarize=true` only). `None` when Deepgram did not
    /// attach a speaker — never guessed (FR-MT-15).
    pub speaker: Option<u32>,
    /// Stream audio time (seconds since the first sample sent) this result covers up to: the end
    /// of its last timed word, so a batch re-transcription starting there neither clips nor
    /// repeats a word. A wordless dictation final covers its window (see `wordless_final`).
    /// `None` when the words carry no timing: what the text covers is then unknown.
    pub end_secs: Option<f64>,
}

enum LiveCmd {
    Audio(Vec<u8>),
    Close,
    Discard,
}

struct LiveSessionContext {
    mode: LiveMode,
    hosted: Option<Arc<super::hosted::Session>>,
}

struct LiveSessionChannels {
    cmd_rx: Receiver<LiveCmd>,
    result_tx: Sender<LiveResult>,
    ready_tx: Sender<Result<(), String>>,
}

/// A live path should fall back quickly; two meeting streams may connect sequentially.
const LIVE_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
/// A standby dictation socket carries no microphone audio and expires without automatic rewarm.
pub const VOICE_WARM_TTL: Duration = Duration::from_secs(30);
const VOICE_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(4);

/// Non-blocking handle to a Deepgram live listen session (dedicated WS thread).
///
/// Push PCM from the audio poll thread; drain [`LiveResult`]s without waiting on the network.
pub struct DeepgramLive {
    mode: LiveMode,
    cmd_tx: Sender<LiveCmd>,
    result_rx: Receiver<LiveResult>,
    join: Option<JoinHandle<Result<(), String>>>,
}

impl DeepgramLive {
    /// Open `wss://…/v1/listen`. Fails fast so callers can fall back to HTTP batch.
    pub fn connect(
        config: &DeepgramConfig,
        auth: &mut dyn DeepgramAuth,
        mode: LiveMode,
        trace: Arc<dyn TraceabilitySink>,
    ) -> Result<Self, String> {
        if !config.mip_opt_out {
            return Err("Deepgram mip_opt_out must be true (company policy)".into());
        }
        let mut url = live_listen_url(config, mode);
        let authorization = auth
            .authorization_header()
            .map_err(|error| public_error(&format!("deepgram auth failed: {error}")))?;

        let hosted = auth.hosted_session()?;
        if let Some(session) = &hosted { url.push_str(&format!("&tag={}", session.user_tag)); }
        let (cmd_tx, cmd_rx) = mpsc::channel::<LiveCmd>();
        let (result_tx, result_rx) = mpsc::channel::<LiveResult>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let purpose = config.purpose.clone();
        let has_keyterms = !config.keyterms.is_empty();
        let trace_for_thread = Arc::clone(&trace);
        let purpose_for_thread = purpose.clone();

        let join = thread::Builder::new()
            .name("deepgram-live".into())
            .spawn(move || {
                live_session_loop(
                    &url,
                    &authorization,
                    LiveSessionChannels {
                        cmd_rx,
                        result_tx,
                        ready_tx,
                    },
                    trace_for_thread.as_ref(),
                    &purpose_for_thread,
                    has_keyterms,
                    LiveSessionContext { mode, hosted },
                )
            })
            .map_err(|error| public_error(&format!("deepgram live thread: {error}")))?;

        match ready_rx.recv_timeout(LIVE_HANDSHAKE_TIMEOUT) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let _ = join.join();
                return Err(error);
            }
            Err(RecvTimeoutError::Timeout) => {
                // Dropping the sender below tells a late handshake to close. Do not join here:
                // DNS/TLS setup is outside our control and the caller needs a bounded fallback.
                drop(cmd_tx);
                return Err(public_error("deepgram live handshake timed out"));
            }
            Err(RecvTimeoutError::Disconnected) => {
                let _ = join.join();
                return Err(public_error(
                    "deepgram live handshake failed before readiness",
                ));
            }
        }

        Ok(Self {
            mode,
            cmd_tx,
            result_rx,
            join: Some(join),
        })
    }

    /// Whether the worker is still available; callers must also match current auth and config.
    pub fn is_alive(&self) -> bool {
        self.join.as_ref().is_some_and(|join| !join.is_finished())
    }

    /// Drop an unused standby socket without joining a network worker on a shortcut/settings path.
    pub fn discard(mut self) {
        let _ = self.cmd_tx.send(LiveCmd::Discard);
        let _ = self.join.take();
    }

    /// Queue linear16 PCM (non-blocking for the caller beyond a short channel send).
    pub fn push_pcm(&mut self, pcm: &[f32]) -> Result<(), String> {
        if pcm.is_empty() {
            return Ok(());
        }
        let bytes = f32_to_linear16(pcm);
        self.cmd_tx
            .send(LiveCmd::Audio(bytes))
            .map_err(|_| "deepgram live session closed".to_string())
    }

    /// Non-blocking drain of one transcript event.
    pub fn try_recv(&self) -> Option<LiveResult> {
        self.result_rx.try_recv().ok()
    }

    /// Drain all currently available results.
    pub fn drain(&self) -> Vec<LiveResult> {
        let mut out = Vec::new();
        while let Some(r) = self.try_recv() {
            out.push(r);
        }
        out
    }

    /// Send `CloseStream`, wait for remaining finals (bounded), join the WS thread.
    /// Returns each final result with its diarization slot (the meeting lane's flush path).
    pub fn finish_results(self) -> Result<Vec<LiveResult>, String> {
        let (finals, result) = self.finish_results_with_partial();
        // Wordless dictation finals only carry coverage for recovery; these callers want words.
        result.map(|()| {
            finals
                .into_iter()
                .filter(|r| !r.text.trim().is_empty())
                .collect()
        })
    }

    /// `finish_results`, but the finals that arrived before a failed flush are kept, each with
    /// the audio time it covers, so the caller can recover only the audio after them. In dictation
    /// this includes wordless finals (see `parse_live_messages_for`).
    pub fn finish_results_with_partial(mut self) -> (Vec<LiveResult>, Result<(), String>) {
        let _ = self.cmd_tx.send(LiveCmd::Close);
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut finals = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match self
                .result_rx
                .recv_timeout(remaining.min(Duration::from_millis(200)))
            {
                Ok(r) => {
                    if r.is_final {
                        finals.push(r);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    if self.join.as_ref().is_some_and(|j| j.is_finished()) {
                        break;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        for r in self.drain() {
            if r.is_final {
                finals.push(r);
            }
        }
        let result = self.join.take().map_or(Ok(()), |join| {
            join.join()
                .map_err(|_| public_error("deepgram live worker failed"))
                .and_then(|result| result)
        });
        (
            finals,
            if self.mode == LiveMode::Voice {
                result
            } else {
                Ok(())
            },
        )
    }

    /// [`Self::finish_results`] as bare text: each final transcript segment (voice joins these;
    /// meetings emit one-by-one).
    pub fn finish_finals(self) -> Result<Vec<String>, String> {
        Ok(self
            .finish_results()?
            .into_iter()
            .map(|r| r.text.trim().to_string())
            .collect())
    }

    /// [`Self::finish_finals`] joined into one string (for voice dictation).
    pub fn finish(self) -> Result<String, String> {
        Ok(self.finish_finals()?.join(" "))
    }
}

impl Drop for DeepgramLive {
    fn drop(&mut self) {
        // Every audio frame is traced in `send_live_audio` before the socket call, so Drop needs
        // no success-only aggregate record (and cannot erase a failed or in-flight attempt).
        let _ = self.cmd_tx.send(LiveCmd::Close);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn live_session_loop(
    url: &str,
    authorization: &str,
    channels: LiveSessionChannels,
    trace: &dyn TraceabilitySink,
    purpose: &str,
    has_keyterms: bool,
    session: LiveSessionContext,
) -> Result<(), String> {
    let LiveSessionContext { mode, hosted } = session;
    use tungstenite::client::IntoClientRequest;
    use tungstenite::http::header::{HeaderValue, AUTHORIZATION};
    let LiveSessionChannels {
        cmd_rx,
        result_tx,
        ready_tx,
    } = channels;

    let setup = (|| {
        let mut request = url
            .into_client_request()
            .map_err(|e| format!("deepgram live request: {e}"))?;
        let auth_val = HeaderValue::from_str(authorization)
            .map_err(|e| format!("deepgram live auth header: {e}"))?;
        request.headers_mut().insert(AUTHORIZATION, auth_val);

        // Issue #831: the hints ride the handshake, so no row means no connection.
        record_keyterm_attempt(trace, purpose, has_keyterms)
            .map_err(|_| TRACE_UNRECORDED.to_string())?;
        connect_bounded(request, LIVE_HANDSHAKE_TIMEOUT)
    })();
    let socket = match setup {
        Ok(socket) => socket,
        Err(error) => {
            let _ = ready_tx.send(Err(public_error(&error)));
            return Err(error);
        }
    };
    run_live_socket(
        socket,
        LiveSessionChannels {
            cmd_rx,
            result_tx,
            ready_tx,
        },
        trace,
        purpose,
        mode,
        hosted.as_ref().map(|session| session.deadline),
    )
}

/// The live socket as a blocking WebSocket over TCP, plain or TLS.
type LiveSocket = tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>;

/// `tungstenite::connect`, but every network step is bounded (issue #1005): the TCP connect gets
/// `limit` per address, and the TLS and WebSocket handshakes read and write with that timeout.
/// The caller abandons a handshake after the same limit; without these bounds the abandoned
/// worker could sit in a blocking connect or handshake read indefinitely. DNS stays the OS
/// resolver's own (blocking, outside our control). The read timeout is replaced by the session
/// loop; the write timeout is cleared here for meetings and re-set for dictation.
fn connect_bounded(
    request: tungstenite::handshake::client::Request,
    limit: Duration,
) -> Result<LiveSocket, String> {
    use std::net::ToSocketAddrs;
    let uri = request.uri();
    let host = uri.host().ok_or("deepgram live connect: no host")?;
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    let port = uri
        .port_u16()
        .unwrap_or(if uri.scheme_str() == Some("wss") {
            443
        } else {
            80
        });
    let addresses = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("deepgram live resolve: {e}"))?;
    let mut last = None;
    let mut tcp = None;
    for address in addresses {
        match std::net::TcpStream::connect_timeout(&address, limit) {
            Ok(stream) => {
                tcp = Some(stream);
                break;
            }
            Err(error) => last = Some(error),
        }
    }
    let tcp = tcp.ok_or_else(|| match last {
        Some(error) => format!("deepgram live connect: {error}"),
        None => "deepgram live connect: no address".to_string(),
    })?;
    tcp.set_read_timeout(Some(limit))
        .and_then(|()| tcp.set_write_timeout(Some(limit)))
        .map_err(|e| format!("deepgram live socket: {e}"))?;
    let _ = tcp.set_nodelay(true);
    let (mut socket, _response) = tungstenite::client_tls(request, tcp)
        .map_err(|e| format!("deepgram live handshake: {e}"))?;
    set_live_write_timeout(socket.get_mut(), None);
    Ok(socket)
}

fn set_live_write_timeout(
    stream: &mut tungstenite::stream::MaybeTlsStream<std::net::TcpStream>,
    timeout: Option<Duration>,
) {
    use tungstenite::stream::MaybeTlsStream;
    match stream {
        MaybeTlsStream::Plain(tcp) => {
            let _ = tcp.set_write_timeout(timeout);
        }
        MaybeTlsStream::Rustls(s) => {
            let _ = s.sock.set_write_timeout(timeout);
        }
        _ => {}
    }
}

fn run_live_socket(
    mut socket: tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    channels: LiveSessionChannels,
    trace: &dyn TraceabilitySink,
    purpose: &str,
    mode: LiveMode,
    deadline: Option<Instant>,
) -> Result<(), String> {
    use tungstenite::{Error as WsError, Message};
    let LiveSessionChannels {
        cmd_rx,
        result_tx,
        ready_tx,
    } = channels;
    set_live_read_timeout(socket.get_mut(), Duration::from_millis(20), mode);
    if ready_tx.send(Ok(())).is_err() {
        // The caller gave up on this handshake (timeout) before it completed. Nobody will use
        // this authenticated socket: close it now instead of idling out a flush grace.
        let _ = socket.close(None);
        let _ = socket.flush();
        return Ok(());
    }
    let opened_at = Instant::now();
    let mut keep_alive_at = opened_at;
    let mut has_audio = false;
    let mut closing = false;
    let mut close_since: Option<Instant> = None;

    loop {
        if deadline.is_some_and(|limit| Instant::now() >= limit) {
            let _ = socket.close(None);
            let _ = socket.flush();
            return Err("Speech session time limit reached".into());
        }
        if voice_warm_expired(mode, has_audio, opened_at.elapsed()) {
            let _ = socket.close(None);
            // A frame may have been queued at the expiry boundary. Propagate failure so the
            // caller replays its RAM ring rather than treating an unused socket as empty speech.
            return Err(public_error("deepgram standby expired"));
        }
        if mode == LiveMode::Voice
            && !has_audio
            && !closing
            && keep_alive_at.elapsed() >= VOICE_KEEP_ALIVE_INTERVAL
        {
            socket
                .send(Message::Text(r#"{"type":"KeepAlive"}"#.into()))
                .map_err(|_| public_error("deepgram standby closed"))?;
            keep_alive_at = Instant::now();
        }
        // Outbound audio / CloseStream first so we never starve sends behind reads.
        match cmd_rx.try_recv() {
            Ok(LiveCmd::Audio(bytes)) => {
                if !closing {
                    has_audio = true;
                    send_live_audio(trace, purpose, bytes, |bytes| {
                        socket
                            .send(Message::Binary(bytes.into()))
                            .map_err(|e| format!("deepgram live send audio: {e}"))
                    })?;
                }
            }
            Ok(LiveCmd::Close) => {
                if !closing {
                    closing = true;
                    close_since = Some(Instant::now());
                    send_close_or_hand_over(&mut socket, mode, &result_tx)?;
                }
            }
            Ok(LiveCmd::Discard) => break,
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                if !closing {
                    closing = true;
                    close_since = Some(Instant::now());
                    send_close_or_hand_over(&mut socket, mode, &result_tx)?;
                }
            }
        }

        match socket.read() {
            Ok(Message::Text(text)) => {
                if mode == LiveMode::Voice
                    && serde_json::from_str::<serde_json::Value>(&text).is_ok_and(|body| {
                        body.get("type").and_then(|value| value.as_str()) == Some("Error")
                    })
                {
                    // A later Metadata frame is completion, not success after a provider error.
                    return Err(public_error("dictation provider reported failure"));
                }
                // Empty finalization responses still acknowledge the flush. Do not use an
                // ordinary streaming is_final/speech_final as proof that the release tail arrived.
                let mut done = voice_flush_complete(mode, closing, &text);
                for r in parse_live_messages_for(mode, &text) {
                    let close_complete =
                        mode == LiveMode::Meeting && close_is_complete(closing, &r);
                    if result_tx.send(r).is_err() {
                        done = true;
                        break;
                    }
                    if close_complete {
                        done = true;
                    }
                }
                if done {
                    break;
                }
            }
            Ok(Message::Binary(_)) | Ok(Message::Pong(_)) | Ok(Message::Frame(_)) => {}
            Ok(Message::Ping(p)) => {
                let _ = socket.send(Message::Pong(p));
            }
            Ok(Message::Close(_)) => {
                if mode == LiveMode::Voice && has_audio {
                    return Err(public_error(
                        "dictation closed before flush acknowledgement",
                    ));
                }
                break;
            }
            Err(WsError::Io(ref e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                if closing {
                    let timed_out =
                        close_since.is_some_and(|t| t.elapsed() > Duration::from_secs(5));
                    // Grace period after CloseStream — peer should have flushed finals. A
                    // disconnected command channel (the handle was dropped) also ends the wait:
                    // nobody is left to consume finals, and spinning out the full 5s grace only
                    // blocks the caller's join().
                    if timed_out {
                        if mode == LiveMode::Voice && has_audio {
                            return Err(DICTATION_FINALIZE_TIMED_OUT.to_string());
                        }
                        break;
                    }
                    if matches!(
                        cmd_rx.try_recv(),
                        Ok(LiveCmd::Discard) | Err(TryRecvError::Disconnected)
                    ) {
                        break;
                    }
                } else {
                    // Idle: wait briefly for the next PCM chunk instead of busy-spinning.
                    match cmd_rx.recv_timeout(Duration::from_millis(5)) {
                        Ok(LiveCmd::Audio(bytes)) => {
                            has_audio = true;
                            send_live_audio(trace, purpose, bytes, |bytes| {
                                socket
                                    .send(Message::Binary(bytes.into()))
                                    .map_err(|e| format!("deepgram live send audio: {e}"))
                            })?;
                        }
                        Ok(LiveCmd::Close) => {
                            closing = true;
                            close_since = Some(Instant::now());
                            send_close_or_hand_over(&mut socket, mode, &result_tx)?;
                        }
                        Ok(LiveCmd::Discard) => break,
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => {
                            closing = true;
                            close_since = Some(Instant::now());
                            send_close_or_hand_over(&mut socket, mode, &result_tx)?;
                        }
                    }
                }
            }
            Err(WsError::ConnectionClosed) | Err(WsError::AlreadyClosed) => {
                if mode == LiveMode::Voice && has_audio {
                    return Err(public_error(
                        "dictation closed before flush acknowledgement",
                    ));
                }
                break;
            }
            Err(e) => {
                if closing && mode == LiveMode::Meeting {
                    break;
                }
                return Err(public_error(&format!("deepgram live read: {e}")));
            }
        }
    }

    let _ = socket.close(None);
    Ok(())
}

fn voice_warm_expired(mode: LiveMode, has_audio: bool, elapsed: Duration) -> bool {
    mode == LiveMode::Voice && !has_audio && elapsed >= VOICE_WARM_TTL
}

fn send_close(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    mode: LiveMode,
) -> Result<(), String> {
    if mode == LiveMode::Voice {
        socket
            .send(tungstenite::Message::Text(r#"{"type":"Finalize"}"#.into()))
            .map_err(|_| public_error("dictation finalize failed"))?;
    }
    let result = socket.send(tungstenite::Message::Text(
        r#"{"type":"CloseStream"}"#.into(),
    ));
    if mode == LiveMode::Voice {
        result.map_err(|_| public_error("deepgram close failed"))
    } else {
        Ok(())
    }
}

/// [`send_close`], but when the peer is already gone, first hand the caller the results it sent
/// before going (issue #824). Outbound commands are handled before reads, so a provider that
/// finalized some words and then dropped the connection can leave them unread here, and
/// returning at once lost them. The error still propagates: the caller keeps those words as an
/// incomplete partial.
fn send_close_or_hand_over(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    mode: LiveMode,
    result_tx: &Sender<LiveResult>,
) -> Result<(), String> {
    let sent = send_close(socket, mode);
    if sent.is_err() {
        hand_over_unread_results(socket, mode, result_tx);
    }
    sent
}

/// Forward the results already waiting on the socket. Stops at the peer's close frame, an error,
/// or the read timeout, and after a bounded number of frames, so a failed flush cannot be held
/// open by a peer that is still sending.
fn hand_over_unread_results(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    mode: LiveMode,
    result_tx: &Sender<LiveResult>,
) {
    for _ in 0..64 {
        match socket.read() {
            Ok(tungstenite::Message::Text(text)) => {
                for r in parse_live_messages_for(mode, &text) {
                    if result_tx.send(r).is_err() {
                        return;
                    }
                }
            }
            Ok(tungstenite::Message::Close(_)) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

fn voice_flush_complete(mode: LiveMode, closing: bool, text: &str) -> bool {
    if mode != LiveMode::Voice || !closing {
        return false;
    }
    let Ok(body) = serde_json::from_str::<serde_json::Value>(text) else {
        return false;
    };
    body.get("type").and_then(|v| v.as_str()) == Some("Metadata")
        || (body.get("type").and_then(|v| v.as_str()) == Some("Results")
            && body.get("is_final").and_then(|v| v.as_bool()) == Some(true)
            && body.get("from_finalize").and_then(|v| v.as_bool()) == Some(true))
}

/// A `speech_final` final ends the bounded post-CloseStream grace period only after we sent
/// CloseStream. Earlier finals remain normal streaming results, not permission to truncate audio.
fn close_is_complete(closing: bool, result: &LiveResult) -> bool {
    closing && result.is_final && result.speech_final
}

fn set_live_read_timeout(
    stream: &mut tungstenite::stream::MaybeTlsStream<std::net::TcpStream>,
    dur: Duration,
    mode: LiveMode,
) {
    use tungstenite::stream::MaybeTlsStream;
    match stream {
        MaybeTlsStream::Plain(tcp) => {
            let _ = tcp.set_read_timeout(Some(dur));
            if mode == LiveMode::Voice {
                let _ = tcp.set_write_timeout(Some(LIVE_HANDSHAKE_TIMEOUT));
            }
        }
        MaybeTlsStream::Rustls(s) => {
            let _ = s.sock.set_read_timeout(Some(dur));
            if mode == LiveMode::Voice {
                let _ = s.sock.set_write_timeout(Some(LIVE_HANDSHAKE_TIMEOUT));
            }
        }
        _ => {}
    }
}

/// One word of a live alternative, as needed for diarization splitting.
struct LiveWord {
    text: String,
    speaker: Option<u32>,
}

fn parse_live_words(alt: &serde_json::Value) -> Vec<LiveWord> {
    let Some(words) = alt.get("words").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    words
        .iter()
        .filter_map(|w| {
            let text = w
                .get("punctuated_word")
                .or_else(|| w.get("word"))
                .and_then(|v| v.as_str())?
                .trim()
                .to_string();
            if text.is_empty() {
                return None;
            }
            let speaker = w.get("speaker").and_then(|v| v.as_u64()).map(|s| s as u32);
            Some(LiveWord { text, speaker })
        })
        .collect()
}

/// A wordless final's coverage stops this far before its window end: a word that only begins in
/// the window's last moments is carried by the next result, and must not be skipped by a recovery
/// that starts there.
const WINDOW_END_MARGIN_SECS: f64 = 0.5;

/// `parse_live_messages`, plus, for dictation, finals with no words. Those tell the dictation lane
/// how far the provider has already processed audio (silence included), so a long pause is not
/// re-sent for recovery. Meeting consumers never receive them.
fn parse_live_messages_for(mode: LiveMode, text: &str) -> Vec<LiveResult> {
    let mut results = parse_live_messages(text);
    if mode == LiveMode::Voice && results.is_empty() {
        results.extend(wordless_final(text));
    }
    results
}

fn wordless_final(text: &str) -> Option<LiveResult> {
    let body = serde_json::from_str::<serde_json::Value>(text).ok()?;
    let is_final = body.get("is_final").and_then(|v| v.as_bool()) == Some(true);
    let wordless = body
        .pointer("/channel/alternatives/0/transcript")
        .and_then(|v| v.as_str())
        .is_some_and(|transcript| transcript.trim().is_empty());
    if body.get("type").and_then(|v| v.as_str()) != Some("Results") || !is_final || !wordless {
        return None;
    }
    let start = body.get("start").and_then(|v| v.as_f64())?;
    let duration = body.get("duration").and_then(|v| v.as_f64())?;
    if !(start.is_finite() && duration.is_finite() && start >= 0.0 && duration >= 0.0) {
        return None;
    }
    Some(LiveResult {
        text: String::new(),
        is_final: true,
        speech_final: body
            .get("speech_final")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        confidence: 0.0,
        speaker: None,
        end_secs: Some((start + duration - WINDOW_END_MARGIN_SECS).max(start)),
    })
}

/// Parse one WS text frame into transcript events.
///
/// Without diarization this is at most one event. With `diarize=true` a final whose words carry
/// two or more distinct speakers is split into one event per contiguous same-speaker run, so a
/// remote back-and-forth inside a single endpointed utterance keeps its speaker boundaries. A
/// uniform result keeps Deepgram's own `transcript` string (better formatting than re-joining
/// words, and the only correct choice for languages without word spacing).
fn parse_live_messages(text: &str) -> Vec<LiveResult> {
    let Ok(body) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let ty = body.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if ty != "Results" {
        return Vec::new();
    }
    let Some(alt) = body.pointer("/channel/alternatives/0") else {
        return Vec::new();
    };
    let transcript = alt
        .get("transcript")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if transcript.is_empty() {
        return Vec::new();
    }
    let confidence = alt
        .get("confidence")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.9)
        .clamp(0.0, 1.0);
    let is_final = body
        .get("is_final")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let speech_final = body
        .get("speech_final")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // The window cannot stand in for missing word timings: a margin before its end would repeat
    // a word the text already holds, none would clip one the next result carries.
    let end_secs = alt
        .get("words")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|w| w.get("end").and_then(|v| v.as_f64()))
        .filter(|end| end.is_finite() && *end >= 0.0)
        .reduce(f64::max);

    let words = parse_live_words(alt);
    let speakers: Vec<Option<u32>> = {
        let mut distinct: Vec<Option<u32>> = Vec::new();
        for w in &words {
            if !distinct.contains(&w.speaker) {
                distinct.push(w.speaker);
            }
        }
        distinct
    };

    // Interims repaint one in-progress line; splitting them would flicker two half-lines, so an
    // interim keeps the whole transcript under its first attributed speaker.
    if !is_final || speakers.len() <= 1 {
        return vec![LiveResult {
            text: transcript.to_string(),
            is_final,
            speech_final,
            confidence,
            speaker: words.iter().find_map(|w| w.speaker),
            end_secs,
        }];
    }

    // A final spanning several speakers: one event per contiguous same-speaker run. Only the last
    // run carries `speech_final`, so the CloseStream grace period still ends exactly once.
    let mut out: Vec<LiveResult> = Vec::new();
    for w in words {
        match out.last_mut() {
            Some(last) if last.speaker == w.speaker => {
                last.text.push(' ');
                last.text.push_str(&w.text);
            }
            _ => out.push(LiveResult {
                text: w.text,
                is_final,
                speech_final: false,
                confidence,
                speaker: w.speaker,
                end_secs,
            }),
        }
    }
    if let Some(last) = out.last_mut() {
        last.speech_final = speech_final;
    }
    out
}

/// Record one ASR send before it happens. `Err` when the ledger row could not be written: the
/// caller then does not send (issue #831 — a missing row must not become an untraced upload).
fn record_asr_attempt(
    sink: &dyn TraceabilitySink,
    purpose: &str,
    pcm_bytes: usize,
    duration_ms: u64,
    has_keyterms: bool,
) -> Result<(), TraceUnrecorded> {
    // Record before an external request. A transport failure can still occur after bytes leave
    // the device, so success-only finalization is not an auditable egress boundary.
    // Digest duration meta only — never the waveform (invariant 2 / G8).
    let meta = format!("duration_ms={duration_ms}");
    sink.try_record(TraceRecord {
        route: Route::Asr,
        // Preserve the established purpose names so existing privacy/audit consumers retain
        // their route taxonomy; this record's pre-send timing defines it as an attempt.
        purpose: purpose.to_string(),
        destination: "api.deepgram.com".into(),
        chunk_bytes: pcm_bytes,
        chunk_xxh64: digest(&meta),
        third_party: true,
        kind: crate::llm::traceability::TraceKind::Egress,
        url: None,
    })?;
    record_keyterm_attempt(sink, purpose, has_keyterms)
}

fn record_keyterm_attempt(
    sink: &dyn TraceabilitySink,
    purpose: &str,
    has_keyterms: bool,
) -> Result<(), TraceUnrecorded> {
    if !has_keyterms {
        return Ok(());
    }
    // Query hints travel during request construction/handshake. Record that boundary once, with
    // no hint content, before any audio frame can be sent.
    sink.try_record(TraceRecord::for_chunk(
        Route::Asr,
        format!("{purpose}_keyterms"),
        "api.deepgram.com",
        "",
        true,
    ))
}

/// The error a send refused for want of a ledger row reports. Content-free.
pub(crate) const TRACE_UNRECORDED: &str = "traceability log unavailable; audio was not sent";

fn send_live_audio(
    sink: &dyn TraceabilitySink,
    purpose: &str,
    bytes: Vec<u8>,
    send: impl FnOnce(Vec<u8>) -> Result<(), String>,
) -> Result<(), String> {
    let pcm_bytes = bytes.len();
    let duration_ms = ((pcm_bytes as u64 / 2) * 1000) / u64::from(SAMPLE_RATE);
    record_asr_attempt(sink, purpose, pcm_bytes, duration_ms, false)
        .map_err(|_| TRACE_UNRECORDED.to_string())?;
    send(bytes)
}

/// HTTPS batch listen URL (VAD-cut fallback).
pub fn http_listen_url(config: &DeepgramConfig) -> String {
    let mut url = format!(
        "{}?model={}&language={}&mip_opt_out=true&smart_format=true&encoding=linear16&sample_rate={}&channels=1",
        config.listen_endpoint,
        urlencoding_lite(&config.model),
        urlencoding_lite(&config.language),
        SAMPLE_RATE
    );
    if is_dictation_purpose(&config.purpose) {
        url.push_str("&dictation=true");
    }
    append_keyterms(&mut url, &config.keyterms);
    url
}

/// Live WebSocket listen URL (`wss://` + streaming query params).
pub fn live_listen_url(config: &DeepgramConfig, mode: LiveMode) -> String {
    let ws_base = https_to_wss(&config.listen_endpoint);
    let mut url = format!(
        "{}?model={}&language={}&mip_opt_out=true&smart_format=true&encoding=linear16&sample_rate={}&channels=1",
        ws_base,
        urlencoding_lite(&config.model),
        urlencoding_lite(&config.language),
        SAMPLE_RATE
    );
    match mode {
        LiveMode::Meeting => {
            // Long enough to finalize whole sentences (see LIVE_ENDPOINTING_MS); interims feed
            // the overlay meanwhile.
            url.push_str(&format!(
                "&interim_results=true&endpointing={LIVE_ENDPOINTING_MS}"
            ));
            if config.diarize {
                url.push_str("&diarize=true");
            }
        }
        LiveMode::Voice => {
            // Interim results feed the ephemeral hold-to-talk preview. Finalize only on
            // CloseStream (key release); do not endpoint mid-hold.
            url.push_str("&dictation=true&interim_results=true&endpointing=false");
        }
    }
    append_keyterms(&mut url, &config.keyterms);
    url
}

/// The listen endpoint as a WebSocket URL — always TLS. The key travels as an
/// `Authorization` header and the audio as frames, so a plaintext `ws://` arm would be a
/// downgrade path waiting for a config knob; `http://`/`ws://` are rewritten up to `wss://`.
fn https_to_wss(endpoint: &str) -> String {
    if let Some(rest) = endpoint.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = endpoint.strip_prefix("http://") {
        format!("wss://{rest}")
    } else if let Some(rest) = endpoint.strip_prefix("ws://") {
        format!("wss://{rest}")
    } else if endpoint.starts_with("wss://") {
        endpoint.to_string()
    } else {
        format!("wss://{endpoint}")
    }
}

fn is_dictation_purpose(purpose: &str) -> bool {
    // voice_lane uses `voice_dictation`; match any purpose containing "dictation".
    purpose.contains("dictation")
}

const MAX_KEYTERM_HINTS: usize = 100;
const MAX_KEYTERM_CHARS: usize = 120;
/// Deepgram limits all keyterms in one request to 500 model tokens. Its tokenizer is not exposed,
/// so UTF-8 byte count is used as a conservative upper bound: a byte-level tokenizer cannot emit
/// more tokens than input bytes. This intentionally leaves headroom for vendor tokenization.
const MAX_KEYTERM_TOKENS: usize = 500;

fn normalized_keyterm(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn bounded_keyterms(keyterms: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut valid = Vec::new();
    let mut token_upper_bound: usize = 0;
    for keyterm in keyterms {
        let keyterm = keyterm.trim();
        if keyterm.is_empty()
            || keyterm.chars().count() > MAX_KEYTERM_CHARS
            || keyterm.chars().any(char::is_control)
        {
            continue;
        }
        let normalized = normalized_keyterm(keyterm);
        if normalized.is_empty()
            || valid
                .iter()
                .any(|existing: &String| normalized_keyterm(existing) == normalized)
        {
            continue;
        }
        let cost = keyterm.len();
        if valid.len() == MAX_KEYTERM_HINTS
            || token_upper_bound.saturating_add(cost) > MAX_KEYTERM_TOKENS
        {
            continue;
        }
        valid.push(keyterm.to_string());
        token_upper_bound += cost;
    }
    valid
}

fn append_keyterms(url: &mut String, keyterms: &[String]) {
    for keyterm in keyterms {
        url.push_str("&keyterm=");
        url.push_str(&urlencoding_lite(keyterm));
    }
}

impl Deepgram {
    /// Batch transcribe one utterance; surfaces auth/HTTP errors to callers (voice dictation).
    pub fn transcribe_utterance(&mut self, pcm: &[f32]) -> Result<String, String> {
        let segs = self.listen(pcm)?;
        let text: String = segs
            .iter()
            .map(|s| s.text.trim())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        Ok(text)
    }

    fn listen(&mut self, pcm: &[f32]) -> Result<Vec<Segment>, String> {
        if pcm.is_empty() {
            return Ok(Vec::new());
        }
        let linear16 = f32_to_linear16(pcm);
        let duration_ms = (pcm.len() as u64 * 1000) / u64::from(SAMPLE_RATE);
        let auth = self
            .auth
            .authorization_header()
            .map_err(|error| public_error(&format!("deepgram auth failed: {error}")))?;
        let url = self.listen_url();
        self.record_egress(linear16.len(), duration_ms)?;
        let resp = self
            .client
            .post(&url)
            .header("Authorization", auth)
            .header("Content-Type", "audio/raw")
            .body(linear16.clone())
            .send()
            .map_err(|error| public_error(&format!("deepgram listen request failed: {error}")))?;
        if !resp.status().is_success() {
            return Err(format!("deepgram listen HTTP {}", resp.status()));
        }
        parse_listen_response(resp).map_err(|error| public_error(&error))
    }
}

impl Transcriber for Deepgram {
    fn transcribe(&mut self, pcm: &[f32]) -> Vec<Segment> {
        match self.listen(pcm) {
            Ok(segs) => segs,
            Err(e) => {
                eprintln!("[asr] {}", public_error(&e));
                Vec::new()
            }
        }
    }

    // Deepgram has no whisper-style translate task. JA→EN uses Select KK in the desktop sink.
    fn translate_to_english(&mut self, _pcm: &[f32]) -> Vec<Segment> {
        Vec::new()
    }
}

fn f32_to_linear16(pcm: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pcm.len() * 2);
    for &s in pcm {
        let clamped = s.clamp(-1.0, 1.0);
        let i = (clamped * 32767.0) as i16;
        out.extend_from_slice(&i.to_le_bytes());
    }
    out
}

fn parse_listen_response(resp: reqwest::blocking::Response) -> Result<Vec<Segment>, String> {
    let body: serde_json::Value = resp.json().map_err(|e| format!("deepgram JSON: {e}"))?;
    let alt = body
        .pointer("/results/channels/0/alternatives/0")
        .ok_or_else(|| "deepgram response missing alternatives".to_string())?;
    let text = alt
        .get("transcript")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let confidence = alt
        .get("confidence")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.9);
    Ok(vec![Segment {
        text,
        confidence: confidence.clamp(0.0, 1.0),
    }])
}

/// Minimal query escaping without a new dependency.
fn urlencoding_lite(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Stub for paid-plan gating in Rust core (not webview-only). Always true until billing wires it.
pub fn deepgram_allowed_by_plan(_plan_allows_meeting_asr: bool) -> bool {
    // ponytail: gating lives here later; meetings already ship on all plans (FR-MT).
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::traceability::RecordingSink;

    #[test]
    fn key_check_status_maps_only_401_403_to_rejected() {
        assert_eq!(classify_key_check_status(200), KeyCheck::Valid);
        assert_eq!(classify_key_check_status(204), KeyCheck::Valid);
        assert_eq!(classify_key_check_status(401), KeyCheck::Rejected);
        assert_eq!(classify_key_check_status(403), KeyCheck::Rejected);
        for status in [400, 404, 429, 500, 503] {
            assert_eq!(classify_key_check_status(status), KeyCheck::Unreachable);
        }
    }

    #[test]
    fn key_check_records_the_egress_boundary_before_sending() {
        let sink = RecordingSink::new();
        // A plaintext endpoint: `https_only` refuses it before any socket opens, so the test is
        // deterministic offline and never presents a key to the real provider — yet the ledger
        // row must already exist, because it is written before the send is attempted.
        let verdict = check_key_at("not-a-real-key", &sink, "http://127.0.0.1:1/v1/projects");
        assert_eq!(verdict, KeyCheck::Unreachable);
        let records = sink.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].route, Route::Asr);
        assert_eq!(records[0].purpose, "deepgram_key_check");
        assert_eq!(records[0].destination, "api.deepgram.com");
        assert_eq!(records[0].chunk_bytes, 0);
        assert!(records[0].third_party);
    }

    #[test]
    fn key_check_endpoint_is_https_on_the_recorded_host() {
        // The ledger names a host; the request must actually go there, over TLS.
        assert!(KEY_CHECK_ENDPOINT.starts_with(&format!("https://{KEY_CHECK_HOST}/")));
    }

    #[test]
    fn missing_meeting_transcription_key_error_matches_the_settings_label() {
        assert_eq!(
            MISSING_MEETING_TRANSCRIPTION_KEY,
            "Speech service is not ready. Check your plan and connection."
        );
        assert!(!MISSING_MEETING_TRANSCRIPTION_KEY.contains("Deepgram"));
    }

    #[test]
    fn rejects_mip_opt_in() {
        let cfg = DeepgramConfig {
            mip_opt_out: false,
            ..DeepgramConfig::default()
        };
        let err = match Deepgram::new(
            cfg,
            Box::new(DebugEnvKeyAuth),
            Arc::new(RecordingSink::new()),
        ) {
            Ok(_) => panic!("expected mip_opt_out rejection"),
            Err(e) => e,
        };
        assert!(err.contains("mip_opt_out"));
    }

    #[test]
    fn company_secret_is_never_a_release_environment_input() {
        assert!(include_str!("hosted.rs").contains("/v1/asr/grant"));
        assert!(!include_str!("hosted.rs").contains("DEEPGRAM_API_KEY"));
    }

    #[test]
    fn linear16_packs_little_endian() {
        let bytes = f32_to_linear16(&[0.0, 1.0, -1.0]);
        assert_eq!(bytes.len(), 6);
        assert_eq!(&bytes[0..2], &0i16.to_le_bytes());
        assert_eq!(&bytes[2..4], &32767i16.to_le_bytes());
        assert_eq!(&bytes[4..6], &(-32767i16).to_le_bytes());
    }

    #[test]
    fn empty_pcm_yields_no_segments_without_network() {
        let mut d = Deepgram::new(
            DeepgramConfig::default(),
            Box::new(DebugEnvKeyAuth),
            Arc::new(RecordingSink::new()),
        )
        .expect("build");
        assert!(d.transcribe(&[]).is_empty());
    }

    #[test]
    fn duration_digest_has_no_waveform_bytes() {
        let meta = "duration_ms=250";
        let d = digest(meta);
        assert_eq!(d.len(), 16);
        assert!(!d.is_empty());
    }

    #[test]
    fn meeting_listen_url_has_smart_format_not_dictation() {
        let d = Deepgram::new(
            DeepgramConfig::default(),
            Box::new(DebugEnvKeyAuth),
            Arc::new(RecordingSink::new()),
        )
        .expect("build");
        let url = d.listen_url();
        assert!(url.contains("mip_opt_out=true"));
        assert!(url.contains("smart_format=true"));
        assert!(!url.contains("dictation=true"));
        assert!(!url.contains("paragraphs="));
        assert!(!url.contains("diarize="));
        assert!(!url.contains("sentiment="));
    }

    #[test]
    fn voice_listen_url_adds_dictation() {
        let d = Deepgram::new(
            DeepgramConfig::default().with_purpose("voice_dictation"),
            Box::new(DebugEnvKeyAuth),
            Arc::new(RecordingSink::new()),
        )
        .expect("build");
        let url = d.listen_url();
        assert!(url.contains("smart_format=true"));
        assert!(url.contains("dictation=true"));
        assert!(url.contains("mip_opt_out=true"));
    }

    #[test]
    fn voice_urls_encode_bounded_keyterm_hints() {
        let config = DeepgramConfig::default()
            .with_purpose("voice_dictation")
            .with_keyterms([
                "Shogun AI".into(),
                "C++".into(),
                "unsafe&keyterm=injected".into(),
                " shogun   ai ".into(),
                "\u{0000}unsafe".into(),
            ]);
        let http = http_listen_url(&config);
        let live = live_listen_url(&config, LiveMode::Voice);
        for url in [http, live] {
            assert!(url.contains("keyterm=Shogun%20AI"));
            assert!(url.contains("keyterm=C%2B%2B"));
            assert!(url.contains("keyterm=unsafe%26keyterm%3Dinjected"));
            assert_eq!(url.matches("&keyterm=").count(), 3);
        }
    }

    #[test]
    fn keyterms_enforce_documented_count_and_token_budgets() {
        let count_limited =
            DeepgramConfig::default().with_keyterms((0..101).map(|index| format!("t{index:03}")));
        assert_eq!(count_limited.keyterms.len(), MAX_KEYTERM_HINTS);

        let token_limited =
            DeepgramConfig::default().with_keyterms((0..6).map(|index| format!("{index:0>100}")));
        assert_eq!(token_limited.keyterms.len(), 5);
        assert_eq!(
            token_limited
                .keyterms
                .iter()
                .map(String::len)
                .sum::<usize>(),
            MAX_KEYTERM_TOKENS
        );
    }

    #[test]
    fn keyterm_egress_attempt_has_purpose_without_hint_content() {
        let sink = RecordingSink::new();
        record_asr_attempt(&sink, "voice_dictation", 640, 20, true).unwrap();
        let records = sink.records();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].purpose, "voice_dictation");
        assert_eq!(records[1].purpose, "voice_dictation_keyterms");
        assert_eq!(records[1].chunk_bytes, 0);
        assert_eq!(records[1].chunk_xxh64, digest(""));
        assert!(!format!("{:?}", records[1]).contains("ShogunAI"));
    }

    #[test]
    fn failed_live_audio_send_is_traced_before_the_transport_returns() {
        let sink = RecordingSink::new();
        let error = send_live_audio(&sink, "voice_dictation", vec![0; 640], |_| {
            Err("simulated disconnected socket".into())
        })
        .expect_err("send failure propagates after trace");
        assert_eq!(error, "simulated disconnected socket");
        let records = sink.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].purpose, "voice_dictation");
        assert_eq!(records[0].chunk_bytes, 640);
    }

    #[test]
    fn live_audio_is_not_sent_when_its_ledger_row_cannot_be_written() {
        // Issue #831: a failed ledger insert must not count as recorded, or the audio goes out untraced.
        struct FailingSink;
        impl TraceabilitySink for FailingSink {
            fn record(&self, _: TraceRecord) {}
            fn try_record(&self, _: TraceRecord) -> Result<(), TraceUnrecorded> {
                Err(TraceUnrecorded)
            }
        }
        let mut sent = false;
        let error = send_live_audio(&FailingSink, "voice_dictation", vec![0; 640], |_| {
            sent = true;
            Ok(())
        })
        .expect_err("refused without a row");
        assert!(!sent, "nothing reached the transport");
        assert_eq!(error, TRACE_UNRECORDED);
        assert_eq!(
            record_asr_attempt(&FailingSink, "meeting_asr", 640, 20, true),
            Err(TraceUnrecorded)
        );
    }

    #[test]
    fn transport_errors_never_expose_keyterms_or_urls() {
        let raw = "WebSocket connect failed: wss://api.deepgram.com/v1/listen?keyterm=Shogun%20AI";
        let safe = public_error(raw);
        assert_eq!(safe, PUBLIC_PROVIDER_FAILURE);
        assert!(!safe.contains("Shogun"));
        assert!(!safe.contains("deepgram.com"));
        assert!(!safe.contains("keyterm"));
    }

    #[test]
    fn meeting_live_url_has_interim_and_endpointing() {
        let cfg = DeepgramConfig::default();
        let url = live_listen_url(&cfg, LiveMode::Meeting);
        assert!(url.starts_with("wss://"));
        assert!(url.contains("mip_opt_out=true"));
        assert!(url.contains("smart_format=true"));
        assert!(url.contains("interim_results=true"));
        assert!(url.contains("endpointing=1000"));
        assert!(!url.contains("dictation=true"));
        assert!(url.contains("encoding=linear16"));
        assert!(url.contains(&format!("sample_rate={SAMPLE_RATE}")));
    }

    #[test]
    fn voice_live_url_has_dictation_interim_without_endpointing() {
        let cfg = DeepgramConfig::default().with_purpose("voice_dictation");
        let url = live_listen_url(&cfg, LiveMode::Voice);
        assert!(url.starts_with("wss://"));
        assert!(url.contains("dictation=true"));
        assert!(url.contains("interim_results=true"));
        assert!(url.contains("endpointing=false"));
        assert!(!url.contains("endpointing=1000"));
        assert!(url.contains("mip_opt_out=true"));
        assert!(url.contains("smart_format=true"));
    }

    #[test]
    fn https_to_wss_rewrites_scheme() {
        assert_eq!(
            https_to_wss("https://api.deepgram.com/v1/listen"),
            "wss://api.deepgram.com/v1/listen"
        );
        assert_eq!(
            https_to_wss("wss://api.deepgram.com/v1/listen"),
            "wss://api.deepgram.com/v1/listen"
        );
        // Never plaintext: a config value that asks for it is upgraded, not honoured.
        assert_eq!(
            https_to_wss("http://api.deepgram.com/v1/listen"),
            "wss://api.deepgram.com/v1/listen"
        );
        assert_eq!(
            https_to_wss("ws://api.deepgram.com/v1/listen"),
            "wss://api.deepgram.com/v1/listen"
        );
    }

    #[test]
    fn voice_flush_ack_handles_empty_final_and_ignores_earlier_speech_finals() {
        let ordinary = r#"{"type":"Results","is_final":true,"speech_final":true}"#;
        let flushed = r#"{"type":"Results","is_final":true,"speech_final":false,"from_finalize":true,"channel":{"alternatives":[{"transcript":""}]}}"#;
        assert!(!voice_flush_complete(LiveMode::Voice, true, ordinary));
        assert!(!voice_flush_complete(LiveMode::Voice, false, flushed));
        assert!(!voice_flush_complete(LiveMode::Meeting, true, flushed));
        assert!(voice_flush_complete(LiveMode::Voice, true, flushed));
        assert!(voice_flush_complete(
            LiveMode::Voice,
            true,
            r#"{"type":"Metadata"}"#
        ));
        assert!(!voice_flush_complete(
            LiveMode::Voice,
            true,
            r#"{"type":"Error","from_finalize":true,"is_final":true}"#
        ));
    }

    struct FakeAuth(Result<String, String>);
    impl DeepgramAuth for FakeAuth {
        fn is_hosted(&self) -> bool {
            true
        }
        fn authorization_header(&mut self) -> Result<String, String> {
            self.0.clone()
        }
    }
    fn own_key() -> Option<Box<dyn DeepgramAuth>> {
        Some(Box::new(FakeAuth(Ok("Token own".into()))))
    }
    fn no_own_key() -> Option<Box<dyn DeepgramAuth>> {
        None
    }

    #[test]
    fn company_grant_comes_first_and_the_own_key_only_takes_over_on_refusal() {
        let mut granted = HostedThenOwn::new(Box::new(FakeAuth(Ok("Bearer grant".into()))), own_key);
        assert_eq!(granted.authorization_header().unwrap(), "Bearer grant");
        assert!(granted.is_hosted());

        let mut refused = HostedThenOwn::new(Box::new(FakeAuth(Err("cap".into()))), own_key);
        assert_eq!(refused.authorization_header().unwrap(), "Token own");
        assert!(!refused.is_hosted());
        assert!(refused.hosted_session().unwrap().is_none());

        let mut alone = HostedThenOwn::new(Box::new(FakeAuth(Err("cap".into()))), no_own_key);
        assert_eq!(alone.authorization_header().unwrap_err(), "cap");
        assert!(alone.is_hosted());
    }

    #[test]
    fn dictation_auth_is_non_interactive_and_does_not_migrate_keychain_items() {
        // Privacy guard: passive preconnect must not replace a non-interactive read with the
        // meeting resolver, which permits an authentication prompt and legacy-item migration.
        let source = include_str!("deepgram.rs");
        let body = |signature: &str| {
            source.split(signature).nth(1).unwrap().split("\n}\n").next().unwrap().to_string()
        };
        let passive = body("pub fn resolve_auth_non_interactive()");
        assert!(passive.contains("resolve_with(own_key_non_interactive)"));
        let own = body("fn own_key_non_interactive()");
        assert!(own.contains("get_generic_secret_non_interactive"));
        assert!(!own.contains("get_deepgram_asr_key"));
        assert!(!own.contains("deepgram_asr_configured"));
        assert!(!own.contains("set_generic_secret"));
    }

    #[test]
    fn standby_expiry_never_caps_active_voice_or_meetings() {
        assert!(!voice_warm_expired(
            LiveMode::Voice,
            false,
            Duration::from_secs(29)
        ));
        assert!(voice_warm_expired(LiveMode::Voice, false, VOICE_WARM_TTL));
        assert!(!voice_warm_expired(
            LiveMode::Voice,
            true,
            Duration::from_secs(600)
        ));
        assert!(!voice_warm_expired(
            LiveMode::Meeting,
            false,
            Duration::from_secs(600)
        ));
    }

    // Real loopback WebSocket, no microphone, credentials, provider call or media file.
    fn loopback_voice(
        peer: impl FnOnce(tungstenite::WebSocket<std::net::TcpStream>) + Send + 'static,
    ) -> (DeepgramLive, JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            tcp.set_read_timeout(Some(Duration::from_secs(6))).unwrap();
            peer(tungstenite::accept(tcp).unwrap());
        });
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let join = thread::spawn(move || {
            let (socket, _) = tungstenite::connect(format!("ws://{address}")).unwrap();
            run_live_socket(
                socket,
                LiveSessionChannels {
                    cmd_rx,
                    result_tx,
                    ready_tx,
                },
                &RecordingSink::new(),
                "voice_dictation",
                LiveMode::Voice,
                None,
            )
        });
        assert!(ready_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .is_ok());
        (
            DeepgramLive {
                mode: LiveMode::Voice,
                cmd_tx,
                result_rx,
                join: Some(join),
            },
            server,
        )
    }

    #[test]
    fn voice_release_flush_keeps_last_words_without_waiting_for_peer_close() {
        use tungstenite::Message;
        let (mut live, server) = loopback_voice(|mut socket| {
            assert!(matches!(socket.read().unwrap(), Message::Binary(_)));
            socket.send(Message::Text(r#"{"type":"Results","is_final":true,"speech_final":true,"channel":{"alternatives":[{"transcript":"first words"}]}}"#.into())).unwrap();
            assert_eq!(
                socket.read().unwrap(),
                Message::Text(r#"{"type":"Finalize"}"#.into())
            );
            assert_eq!(
                socket.read().unwrap(),
                Message::Text(r#"{"type":"CloseStream"}"#.into())
            );
            socket.send(Message::Text(r#"{"type":"Results","is_final":true,"speech_final":false,"channel":{"alternatives":[{"transcript":"last words"}]}}"#.into())).unwrap();
            socket
                .send(Message::Text(r#"{"type":"Metadata"}"#.into()))
                .unwrap();
            // The client finishes from the flush acknowledgement while the peer stays open.
            assert!(matches!(socket.read().unwrap(), Message::Close(_)));
        });
        live.push_pcm(&[0.0; 1600]).unwrap();
        let started = Instant::now();
        assert_eq!(live.finish().unwrap(), "first words last words");
        assert!(started.elapsed() < Duration::from_secs(1));
        server.join().unwrap();
    }

    #[test]
    fn voice_release_loopback_latency_distribution() {
        use tungstenite::Message;
        let mut timings = Vec::new();
        for _ in 0..30 {
            let (mut live, server) = loopback_voice(|mut socket| {
                assert!(matches!(socket.read().unwrap(), Message::Binary(_)));
                assert_eq!(
                    socket.read().unwrap(),
                    Message::Text(r#"{"type":"Finalize"}"#.into())
                );
                assert_eq!(
                    socket.read().unwrap(),
                    Message::Text(r#"{"type":"CloseStream"}"#.into())
                );
                socket.send(Message::Text(r#"{"type":"Results","is_final":true,"from_finalize":true,"channel":{"alternatives":[{"transcript":"complete text"}]}}"#.into())).unwrap();
                let _ = socket.read();
            });
            live.push_pcm(&[0.0; 1600]).unwrap();
            let started = Instant::now();
            assert_eq!(live.finish().unwrap(), "complete text");
            timings.push(started.elapsed().as_micros());
            server.join().unwrap();
        }
        timings.sort_unstable();
        eprintln!(
            "voice_loopback_flush_us n=30 p50={} p95={}",
            timings[14], timings[28]
        );
    }

    #[test]
    fn voice_disconnect_propagates_error_instead_of_committing_partial_text() {
        use tungstenite::Message;
        for provider_error in [false, true] {
            let (mut live, server) = loopback_voice(move |mut socket| {
                assert!(matches!(socket.read().unwrap(), Message::Binary(_)));
                socket.send(Message::Text(r#"{"type":"Results","is_final":true,"speech_final":false,"channel":{"alternatives":[{"transcript":"only the beginning"}]}}"#.into())).unwrap();
                if provider_error {
                    assert_eq!(
                        socket.read().unwrap(),
                        Message::Text(r#"{"type":"Finalize"}"#.into())
                    );
                    assert_eq!(
                        socket.read().unwrap(),
                        Message::Text(r#"{"type":"CloseStream"}"#.into())
                    );
                    socket
                        .send(Message::Text(
                            r#"{"type":"Error","description":"local simulated failure"}"#.into(),
                        ))
                        .unwrap();
                    let _ = socket.send(Message::Text(r#"{"type":"Metadata"}"#.into()));
                    let _ = socket.read();
                } else {
                    socket.close(None).unwrap();
                }
            });
            live.push_pcm(&[0.0; 1600]).unwrap();
            let (finals, finished) = live.finish_results_with_partial();
            assert!(finished.is_err());
            let partial: Vec<&str> = finals.iter().map(|r| r.text.as_str()).collect();
            assert_eq!(partial.join(" "), "only the beginning");
            server.join().unwrap();
        }
    }

    /// Issue #1005: a handshake the caller already abandoned closes its socket at once instead
    /// of keeping a second authenticated stream open through a flush grace.
    #[test]
    fn an_abandoned_handshake_closes_its_socket_when_it_completes() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (result_tx, _result_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        drop(ready_rx); // The caller timed out before the handshake finished.
        let worker = thread::spawn(move || {
            live_session_loop(
                &format!("ws://{address}"),
                "Token test",
                LiveSessionChannels {
                    cmd_rx,
                    result_tx,
                    ready_tx,
                },
                &RecordingSink::new(),
                "voice_dictation",
                false,
                LiveSessionContext { mode: LiveMode::Voice, hosted: None },
            )
        });
        let (tcp, _) = listener.accept().unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut socket = tungstenite::accept(tcp).unwrap();
        let started = Instant::now();
        // The orphan says goodbye (a Close frame) or just goes; either way it does not linger.
        assert!(matches!(
            socket.read(),
            Ok(tungstenite::Message::Close(_)) | Err(_)
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(worker.join().unwrap().is_ok());
        drop(cmd_tx);
    }

    /// A peer that accepts TCP but never answers the WebSocket handshake cannot hold the worker:
    /// the handshake read is bounded by the same limit the caller waits.
    #[test]
    fn a_stalled_handshake_is_bounded() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (_cmd_tx, cmd_rx) = mpsc::channel();
        let (result_tx, _result_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let started = Instant::now();
        let worker = thread::spawn(move || {
            live_session_loop(
                &format!("ws://{address}"),
                "Token test",
                LiveSessionChannels {
                    cmd_rx,
                    result_tx,
                    ready_tx,
                },
                &RecordingSink::new(),
                "voice_dictation",
                false,
                LiveSessionContext { mode: LiveMode::Voice, hosted: None },
            )
        });
        let (_held, _) = listener.accept().unwrap(); // Accept, then never answer.
        assert!(ready_rx
            .recv_timeout(LIVE_HANDSHAKE_TIMEOUT + Duration::from_secs(3))
            .unwrap()
            .is_err());
        assert!(worker.join().unwrap().is_err());
        assert!(started.elapsed() < LIVE_HANDSHAKE_TIMEOUT + Duration::from_secs(3));
    }

    /// Issue #974: a flush the provider never acknowledges is reported as a finalize timeout,
    /// distinct from the stream ending or an empty result.
    #[test]
    fn an_unacknowledged_dictation_flush_is_a_finalize_timeout() {
        use tungstenite::Message;
        let (mut live, server) = loopback_voice(move |mut socket| {
            assert!(matches!(socket.read().unwrap(), Message::Binary(_)));
            assert_eq!(
                socket.read().unwrap(),
                Message::Text(r#"{"type":"Finalize"}"#.into())
            );
            assert_eq!(
                socket.read().unwrap(),
                Message::Text(r#"{"type":"CloseStream"}"#.into())
            );
            // Stay connected and silent past the grace period.
            let _ = socket.read();
        });
        live.push_pcm(&[0.0; 1600]).unwrap();
        let (finals, finished) = live.finish_results_with_partial();
        assert!(finals.is_empty());
        let error = finished.unwrap_err();
        assert!(is_finalize_timeout(&error), "{error}");
        assert!(!is_finalize_timeout(PUBLIC_PROVIDER_FAILURE));
        server.join().unwrap();
    }

    #[test]
    fn a_failed_flush_hands_over_the_results_already_received() {
        use tungstenite::Message;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            tcp.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut socket = tungstenite::accept(tcp).unwrap();
            socket.send(Message::Text(r#"{"type":"Results","is_final":true,"speech_final":false,"channel":{"alternatives":[{"transcript":"only the beginning"}]}}"#.into())).unwrap();
            socket.close(None).unwrap();
        });
        let (mut socket, _) = tungstenite::connect(format!("ws://{address}")).unwrap();
        set_live_read_timeout(socket.get_mut(), Duration::from_millis(20), LiveMode::Voice);
        // The peer has finalized some words and gone before this side reads anything: the state
        // a failed `Finalize` leaves the loop in.
        server.join().unwrap();
        let (result_tx, result_rx) = mpsc::channel();
        hand_over_unread_results(&mut socket, LiveMode::Voice, &result_tx);
        let handed: Vec<String> = result_rx.try_iter().map(|r| r.text).collect();
        assert_eq!(handed, ["only the beginning"]);
    }

    #[test]
    fn standby_keepalive_uses_control_messages_without_microphone_audio() {
        use tungstenite::Message;
        let (live, server) = loopback_voice(|mut socket| {
            assert_eq!(
                socket.read().unwrap(),
                Message::Text(r#"{"type":"KeepAlive"}"#.into())
            );
        });
        server.join().unwrap();
        live.discard();
    }

    #[test]
    fn unused_standby_discard_is_nonblocking_and_sends_no_audio() {
        use tungstenite::Message;
        let (live, server) = loopback_voice(|mut socket| {
            assert!(matches!(socket.read().unwrap(), Message::Close(_)));
        });
        let started = Instant::now();
        live.discard();
        assert!(started.elapsed() < Duration::from_millis(100));
        server.join().unwrap();
    }

    #[test]
    fn parse_live_results_interim_and_final() {
        let interim = r#"{"type":"Results","is_final":false,"speech_final":false,"channel":{"alternatives":[{"transcript":"hel","confidence":0.5}]}}"#;
        let rs = parse_live_messages(interim);
        assert_eq!(rs.len(), 1);
        let r = &rs[0];
        assert_eq!(r.text, "hel");
        assert!(!r.is_final);
        assert!(!r.speech_final);
        assert_eq!(r.speaker, None);

        let fin = r#"{"type":"Results","is_final":true,"speech_final":true,"channel":{"alternatives":[{"transcript":"hello","confidence":0.98}]}}"#;
        let rs = parse_live_messages(fin);
        assert_eq!(rs.len(), 1);
        let r = &rs[0];
        assert_eq!(r.text, "hello");
        assert!(r.is_final);
        assert!(r.speech_final);
        assert!((r.confidence - 0.98).abs() < f64::EPSILON);
        assert_eq!(r.end_secs, None, "no start/duration, no coverage claim");
    }

    #[test]
    fn parse_live_results_carry_the_audio_time_they_cover() {
        let fin = r#"{"type":"Results","start":2.5,"duration":1.25,"is_final":true,"speech_final":true,"channel":{"alternatives":[{"transcript":"hello","confidence":0.98}]}}"#;
        let rs = parse_live_messages(fin);
        assert_eq!(rs.len(), 1);
        // Words without timings: what the text covers is unknown, not guessed from the window.
        assert_eq!(rs[0].end_secs, None);
        // The last word, not the window, bounds what the result covers: audio after it may hold
        // the start of a word the next result will carry.
        let words = r#"{"type":"Results","start":2.5,"duration":2.5,"is_final":true,"channel":{"alternatives":[{"transcript":"hi there","words":[{"word":"hi","start":2.6,"end":2.9},{"word":"there","start":3.0,"end":3.4}]}]}}"#;
        assert_eq!(parse_live_messages(words)[0].end_secs, Some(3.4));
    }

    #[test]
    fn close_completion_requires_close_and_speech_final() {
        let interim = LiveResult {
            text: "hello".into(),
            is_final: false,
            speech_final: false,
            confidence: 0.9,
            speaker: None,
            end_secs: None,
        };
        let non_speech_final = LiveResult {
            text: "hello".into(),
            is_final: true,
            speech_final: false,
            confidence: 0.9,
            speaker: None,
            end_secs: None,
        };
        let speech_final = LiveResult {
            text: "hello".into(),
            is_final: true,
            speech_final: true,
            confidence: 0.9,
            speaker: None,
            end_secs: None,
        };

        assert!(!close_is_complete(false, &interim));
        assert!(!close_is_complete(true, &interim));
        assert!(!close_is_complete(false, &speech_final));
        assert!(!close_is_complete(true, &non_speech_final));
        assert!(close_is_complete(true, &speech_final));
    }

    #[test]
    fn dictation_keeps_wordless_finals_as_coverage_and_meetings_never_see_them() {
        let silent = r#"{"type":"Results","start":60.0,"duration":30.0,"is_final":true,"channel":{"alternatives":[{"transcript":""}]}}"#;
        assert!(parse_live_messages_for(LiveMode::Meeting, silent).is_empty());
        let voice = parse_live_messages_for(LiveMode::Voice, silent);
        assert_eq!(voice.len(), 1);
        assert!(voice[0].is_final && voice[0].text.is_empty());
        assert_eq!(
            voice[0].end_secs,
            Some(89.5),
            "a margin before the window end"
        );
        // Interims and untimed frames claim nothing.
        let interim = r#"{"type":"Results","start":1.0,"duration":1.0,"is_final":false,"channel":{"alternatives":[{"transcript":""}]}}"#;
        assert!(parse_live_messages_for(LiveMode::Voice, interim).is_empty());
        let untimed =
            r#"{"type":"Results","is_final":true,"channel":{"alternatives":[{"transcript":""}]}}"#;
        assert!(parse_live_messages_for(LiveMode::Voice, untimed).is_empty());
        // A worded final is parsed once.
        let worded = r#"{"type":"Results","start":0.0,"duration":2.0,"is_final":true,"channel":{"alternatives":[{"transcript":"hi"}]}}"#;
        assert_eq!(parse_live_messages_for(LiveMode::Voice, worded).len(), 1);
    }

    #[test]
    fn parse_live_skips_metadata_and_empty() {
        assert!(parse_live_messages(r#"{"type":"Metadata"}"#).is_empty());
        assert!(parse_live_messages(
            r#"{"type":"Results","is_final":true,"channel":{"alternatives":[{"transcript":"  "}]}}"#
        )
        .is_empty());
    }

    #[test]
    fn a_single_speaker_final_keeps_deepgrams_own_formatting() {
        // Uniform speaker → the `transcript` string is used verbatim (word re-joining would break
        // Japanese, which has no word spacing), with the speaker attached.
        let fin = r#"{"type":"Results","is_final":true,"speech_final":true,"channel":{"alternatives":[{
            "transcript":"こんにちは、皆さん。","confidence":0.95,
            "words":[{"word":"こんにちは","punctuated_word":"こんにちは、","speaker":1},{"word":"皆さん","punctuated_word":"皆さん。","speaker":1}]
        }]}}"#;
        let rs = parse_live_messages(fin);
        assert_eq!(rs.len(), 1);
        assert_eq!(rs[0].text, "こんにちは、皆さん。");
        assert_eq!(rs[0].speaker, Some(1));
        assert!(rs[0].speech_final);
    }

    #[test]
    fn a_multi_speaker_final_splits_into_contiguous_runs() {
        let fin = r#"{"type":"Results","is_final":true,"speech_final":true,"channel":{"alternatives":[{
            "transcript":"yes agreed no wait","confidence":0.9,
            "words":[
                {"word":"yes","punctuated_word":"Yes,","speaker":0},
                {"word":"agreed","punctuated_word":"agreed.","speaker":0},
                {"word":"no","punctuated_word":"No,","speaker":1},
                {"word":"wait","punctuated_word":"wait.","speaker":1}
            ]
        }]}}"#;
        let rs = parse_live_messages(fin);
        assert_eq!(rs.len(), 2);
        assert_eq!(rs[0].text, "Yes, agreed.");
        assert_eq!(rs[0].speaker, Some(0));
        assert!(
            !rs[0].speech_final,
            "only the last run may end the close grace"
        );
        assert_eq!(rs[1].text, "No, wait.");
        assert_eq!(rs[1].speaker, Some(1));
        assert!(rs[1].speech_final);
        assert!(rs.iter().all(|r| r.is_final));
    }

    #[test]
    fn an_interim_is_never_split_by_speaker() {
        // Interims repaint one in-progress line; splitting would flicker two half-lines.
        let interim = r#"{"type":"Results","is_final":false,"speech_final":false,"channel":{"alternatives":[{
            "transcript":"yes no","confidence":0.5,
            "words":[{"word":"yes","speaker":0},{"word":"no","speaker":1}]
        }]}}"#;
        let rs = parse_live_messages(interim);
        assert_eq!(rs.len(), 1);
        assert_eq!(rs[0].text, "yes no");
        assert_eq!(rs[0].speaker, Some(0));
    }

    #[test]
    fn meeting_live_url_diarizes_only_when_configured() {
        let plain = live_listen_url(&DeepgramConfig::default(), LiveMode::Meeting);
        assert!(!plain.contains("diarize"));
        let cfg = DeepgramConfig::default().with_diarize(true);
        let url = live_listen_url(&cfg, LiveMode::Meeting);
        assert!(url.contains("&diarize=true"));
        // Dictation never diarizes, whatever the config says.
        let voice = live_listen_url(&cfg, LiveMode::Voice);
        assert!(!voice.contains("diarize"));
    }
}
