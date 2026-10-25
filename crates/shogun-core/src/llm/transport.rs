//! The HTTP transport seam.
//!
//! The Anthropic clients are written against this trait, not a concrete HTTP library, so their
//! request construction, traceability recording, and response parsing are all exercised on Linux
//! with a [`MockTransport`] and no socket. The real socket implementation
//! ([`ReqwestTransport`], feature `net`) is a thin adapter.
//!
//! Two safety properties live here:
//! - **HTTPS only** (NFR-SEC-04): [`HttpRequest::new`] rejects any non-`https://` URL, and the
//!   real transport never disables certificate verification.
//! - **Secrets never print** (G7): [`HttpRequest`]'s `Debug` redacts the `x-api-key` /
//!   `authorization` headers, so a request captured by a mock or dumped in a test cannot leak the
//!   API key.

use std::fmt;
use std::future::Future;

#[cfg(feature = "net")]
mod public_web_dns;

/// HTTP method — the two verbs the Anthropic REST surface uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
        }
    }
}

/// An outbound HTTPS request. Construct with [`HttpRequest::new`], which enforces the `https://`
/// scheme; headers carrying secrets are redacted under `Debug`.
#[derive(Clone)]
pub struct HttpRequest {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    /// Stop reading the response body past this many bytes (issue #555 FR-WV-12: an arbitrary
    /// user URL must be cut at the socket, not buffered whole and then measured). `None` reads
    /// the whole body, as every LLM client here expects.
    pub max_body_bytes: Option<usize>,
    /// Optional total deadline, also honored by streaming. Non-streaming defaults to 300 seconds.
    pub timeout: Option<std::time::Duration>,
    // Website requests use a separate DNS-checked pool, never a provider connection/proxy.
    public_web: bool,
    no_redirects: bool,
}

impl HttpRequest {
    /// Build a request, rejecting any non-HTTPS URL (NFR-SEC-04). Callers never construct the
    /// struct directly, so plaintext can never slip through.
    pub fn new(
        method: Method,
        url: impl Into<String>,
        headers: Vec<(String, String)>,
        body: Option<String>,
    ) -> Result<Self, TransportError> {
        let url = url.into();
        // Plain http only to loopback (local relay dev override); everything else is HTTPS.
        let loopback_http = cfg!(any(test, feature = "local-relay")) && url.starts_with("http://") && super::relay::loopback_relay_url(&url).is_some();
        if !url.starts_with("https://") && !loopback_http {
            return Err(TransportError::InsecureUrl(url));
        }
        Ok(Self { method, url, headers, body, max_body_bytes: None, timeout: None, public_web: false, no_redirects: false })
    }

    /// Restrict this request and every redirect to public web destinations. The real transport
    /// validates DNS answers at the connection seam; this check also covers IP literals, which
    /// reqwest's connector does not pass through a DNS resolver.
    ///
    /// Opt-in: only requests marked here get the guard. Every user-supplied link must be read
    /// through `web_fetch::WebFetcher` (which marks all of its requests), never with a bare
    /// `HttpRequest`.
    pub fn for_public_web(mut self) -> Result<Self, TransportError> {
        if crate::web_fetch::is_private_host(&self.url) {
            return Err(TransportError::PrivateDestination);
        }
        self.public_web = true;
        Ok(self)
    }

    /// Keep licensed gateway requests on their configured origin, including 307/308 bodies.
    pub fn without_redirects(mut self) -> Self {
        self.no_redirects = true;
        self
    }

    /// Set the total deadline for this request (including a streamed response).
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Cap the response body at `bytes`; a longer body is [`TransportError::BodyTooLarge`].
    pub fn with_max_body_bytes(mut self, bytes: usize) -> Self {
        self.max_body_bytes = Some(bytes);
        self
    }
}

/// Header names whose values are secrets and must never be printed.
fn is_secret_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("x-api-key") || name.eq_ignore_ascii_case("authorization")
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(k, v)| (k.as_str(), if is_secret_header(k) { "***redacted***" } else { v.as_str() }))
            .collect();
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("headers", &headers)
            .field("body", &self.body)
            .finish()
    }
}

/// An HTTP response: status code and full body. The transport reads the entire body (Anthropic
/// returns SSE as the body too); incremental token streaming is a later, streaming-transport
/// concern (see [`super::anthropic`]).
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
    /// The server's `Retry-After`, when it sent one in delta-seconds form (issue #29 FR-SVC-02).
    /// The shared-service backoff honours it over its own schedule. An HTTP-date form is not
    /// parsed and reads as `None` — the client then falls back to its own schedule, which is
    /// the conservative choice.
    pub retry_after: Option<std::time::Duration>,
}

impl HttpResponse {
    /// A response with no `Retry-After` — what every mock and most real answers look like.
    pub fn new(status: u16, body: impl Into<String>) -> Self {
        Self { status, body: body.into(), retry_after: None }
    }

    /// Attach the server's `Retry-After` (tests and the real transport).
    pub fn with_retry_after(mut self, retry_after: std::time::Duration) -> Self {
        self.retry_after = Some(retry_after);
        self
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Parse a `Retry-After` header value in delta-seconds form (RFC 9110 §10.2.3). The HTTP-date
/// form is deliberately unsupported (`None`): the relay only ever sends seconds, and a clock
/// skew between device and server would turn a date into a wrong wait either way.
pub fn parse_retry_after(value: &str) -> Option<std::time::Duration> {
    value.trim().parse::<u64>().ok().map(std::time::Duration::from_secs)
}

/// Transport failures. Provider-level (non-2xx) errors are surfaced by the clients, not here;
/// this is only the wire layer.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("insecure url (https required): {0}")]
    InsecureUrl(String),
    #[error("transport error: {0}")]
    Io(String),
    /// The connection never came up (DNS, refused, connect timeout): **no bytes reached the
    /// provider**, so the request provably did not happen. Callers that treat a lost response as
    /// "maybe it went through" (a Batch submit) can safely retry this one instead.
    #[error("provider unreachable (nothing sent): {0}")]
    Unreachable(String),
    /// The body exceeded [`HttpRequest::max_body_bytes`]; the read stopped there.
    #[error("response body larger than {0} bytes")]
    BodyTooLarge(usize),
    /// A public-web request whose destination is this Mac or a private network: an IP literal,
    /// or a DNS answer with no public address. Nothing was sent.
    #[error("web destination is not public")]
    PrivateDestination,
}

/// The transport seam. Static-dispatch only (the clients are generic over `T: HttpTransport`), so
/// the future is `Send` without `async-trait`.
pub trait HttpTransport: Send + Sync {
    fn send(
        &self,
        req: HttpRequest,
    ) -> impl Future<Output = Result<HttpResponse, TransportError>> + Send;
}

/// ストリーミング応答の結末。ステータスをボディより先に確定できるので、非2xxのときに
/// 「エラー本文をデルタとして画面に流してしまう」経路が構造的に存在しない。
#[derive(Debug)]
pub enum StreamOutcome {
    /// 2xx。ボディは到着順に `on_chunk` へ渡し終えた。
    Streamed { status: u16 },
    /// 非2xx。`on_chunk` は一度も呼ばず、エラー本文だけを持って返る。
    Failed { status: u16, body: String },
}

/// 増分でボディを受け取るトランスポート。
///
/// [`HttpTransport`] と分けてある。全実装に streaming を強いるとモックもBatch lane側も
/// 巻き添えになるが、増分が要るのはAgent laneのSSEだけで、しかもそこは「最初の一文字までの
/// 時間」がSLOになっている唯一の経路だから。
///
/// チャンクはコールバックで渡す。チャネルにすると送信側と受信側を同時に走らせる必要が生じ、
/// ライブラリ側にランタイムを持ち込むことになる — が、デコードは「バイトが届いた瞬間に
/// その場で」やれば済むので、同時実行そのものが要らない。
///
/// `on_chunk` が `false` を返したらそこで打ち切る。応答の途中でパネルを閉じるのは正常な
/// 操作であって、失敗ではない。
pub trait StreamingTransport: Send + Sync {
    /// `req` を送り、ボディを届いた順に `on_chunk` へ渡す。返るのは [`StreamOutcome`]。
    ///
    /// `on_chunk` を値渡しにしているのは、await をまたいで借用しないことでフューチャを `Send`
    /// に保つため。
    fn send_streaming<F>(
        &self,
        req: HttpRequest,
        on_chunk: F,
    ) -> impl Future<Output = Result<StreamOutcome, TransportError>> + Send
    where
        F: FnMut(&str) -> bool + Send;
}

/// 決められたチャンクを順に流すだけのテスト用トランスポート。ネットワーク無しで
/// ストリーミング経路を検証するための土台。
pub struct MockStreamingTransport {
    status: u16,
    chunks: std::sync::Mutex<std::collections::VecDeque<String>>,
    sent: std::sync::Mutex<Vec<HttpRequest>>,
}

impl MockStreamingTransport {
    pub fn new(status: u16, chunks: Vec<String>) -> Self {
        Self {
            status,
            chunks: std::sync::Mutex::new(chunks.into()),
            sent: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Every request the transport has seen, in order (same as [`MockTransport::sent`]).
    pub fn sent(&self) -> Vec<HttpRequest> {
        self.sent.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

impl StreamingTransport for MockStreamingTransport {
    fn send_streaming<F>(
        &self,
        req: HttpRequest,
        mut on_chunk: F,
    ) -> impl Future<Output = Result<StreamOutcome, TransportError>> + Send
    where
        F: FnMut(&str) -> bool + Send,
    {
        if let Ok(mut s) = self.sent.lock() {
            s.push(req);
        }
        let queued: Vec<String> = self
            .chunks
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default();
        let status = self.status;
        async move {
            // 非2xxではチャンクを1つも渡さず、本文だけを組み立てて返す — 実トランスポートと
            // 同じ振る舞い。エラー本文が回答としてパネルに出る経路を塞ぐ。
            if !(200..300).contains(&status) {
                return Ok(StreamOutcome::Failed { status, body: queued.concat() });
            }
            for c in queued {
                // コールバックが false を返したら打ち切る。閉じたパネルに向かって流し続けない。
                if !on_chunk(&c) {
                    break;
                }
            }
            Ok(StreamOutcome::Streamed { status })
        }
    }
}

/// A transport that records every request and replays canned responses in order — the offline
/// stand-in that makes the Anthropic clients Linux-testable. Public so downstream crates can use
/// it in their own tests. `pub` items don't trip `dead_code`, so it carries no warning in
/// production builds.
pub struct MockTransport {
    responses: std::sync::Mutex<std::collections::VecDeque<HttpResponse>>,
    sent: std::sync::Mutex<Vec<HttpRequest>>,
}

impl MockTransport {
    /// Queue the responses the transport will return, in call order.
    pub fn new(responses: impl IntoIterator<Item = HttpResponse>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses.into_iter().collect()),
            sent: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Convenience: a single 200 response with `body`.
    pub fn ok(body: impl Into<String>) -> Self {
        Self::new([HttpResponse { status: 200, body: body.into(), retry_after: None }])
    }

    /// Every request the transport has seen, in order.
    pub fn sent(&self) -> Vec<HttpRequest> {
        self.sent.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

impl HttpTransport for MockTransport {
    fn send(
        &self,
        req: HttpRequest,
    ) -> impl Future<Output = Result<HttpResponse, TransportError>> + Send {
        // Capture the request and pop the next canned response synchronously; the returned future
        // is trivially ready (and Send).
        let result = {
            if let Ok(mut s) = self.sent.lock() {
                s.push(req);
            }
            match self.responses.lock() {
                Ok(mut q) => q
                    .pop_front()
                    .ok_or_else(|| TransportError::Io("MockTransport: no queued response".into())),
                Err(_) => Err(TransportError::Io("MockTransport: poisoned".into())),
            }
        };
        std::future::ready(result)
    }
}

// real transport (feature `net`)

/// The production transport: a `reqwest` client pinned to HTTPS with rustls. Certificate
/// verification is never disabled (NFR-SEC-04) and the scheme check there (loopback http only) is the guard beyond
/// [`HttpRequest::new`]'s scheme check.
///
/// Cheap to clone — the inner `reqwest::Client` is a handle onto one connection pool, which is
/// the whole point of [`Self::shared`].
#[cfg(feature = "net")]
#[derive(Clone)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

/// One pool for the whole process. See [`ReqwestTransport::shared`].
#[cfg(feature = "net")]
static SHARED_CLIENT: std::sync::OnceLock<Result<reqwest::Client, String>> =
    std::sync::OnceLock::new();

#[cfg(feature = "net")]
static PUBLIC_WEB_CLIENT: std::sync::OnceLock<Result<reqwest::Client, String>> =
    std::sync::OnceLock::new();

/// The most redirects one request follows.
pub const MAX_REDIRECTS: usize = 5;

/// What to do with one redirect hop: `Err` with the reason to stop, `Ok` to follow. A hop is
/// refused when it points at this Mac or a private network, or is not `https`, by the same rule
/// that judges the first link ([`crate::web_fetch::is_private_host`], #775). Without this a
/// public page a web read was allowed to open could answer 302 to `169.254.169.254` or an
/// intranet host and have that body returned. No provider endpoint redirects there, so the rule
/// costs the other lanes nothing.
pub fn redirect_decision(next_url: &str, hops_so_far: usize) -> Result<(), &'static str> {
    if hops_so_far >= MAX_REDIRECTS {
        return Err("too many redirects");
    }
    if crate::web_fetch::is_private_host(next_url) {
        return Err("redirect to an address that is never read");
    }
    Ok(())
}

#[cfg(feature = "net")]
fn build_client() -> Result<reqwest::Client, TransportError> {
    client_builder().build().map_err(|e| TransportError::Io(e.to_string()))
}

#[cfg(feature = "net")]
fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        // http is let through here only because HttpRequest::new admits loopback http alone.
        .https_only(false)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            match redirect_decision(attempt.url().as_str(), attempt.previous().len()) {
                Ok(()) => attempt.follow(),
                Err(reason) => attempt.error(reason),
            }
        }))
        // Keep the connection alive between turns. Without this every chat turn, every ⌥-tap
        // draft and every recap re-pays DNS + TCP + TLS to the provider before the model has
        // seen a single byte — on a normal link that is most of the budget the "first token in
        // 1s" SLO has to fit inside. The pool is per-host, so an idle window longer than the
        // gap between two turns is what actually gets reused.
        .pool_idle_timeout(std::time::Duration::from_secs(300))
        .pool_max_idle_per_host(4)
        .tcp_keepalive(std::time::Duration::from_secs(60))
        // Connect-only, deliberately: a whole-request timeout would cut long streamed answers
        // off mid-sentence, and a stream that is still producing tokens is not a hung request.
        .connect_timeout(std::time::Duration::from_secs(10))
}

#[cfg(feature = "net")]
fn decisions_client_builder() -> reqwest::ClientBuilder {
    client_builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
}

#[cfg(feature = "net")]
fn public_web_client_builder(resolver: impl reqwest::dns::Resolve + 'static) -> reqwest::ClientBuilder {
    // Redirect hops to IP literals never reach the resolver; they are refused only by the shared
    // `redirect_decision` policy in `client_builder`. Do not override `.redirect(...)` here.
    client_builder()
        // A CONNECT proxy resolves the destination itself and bypasses our resolver. Explicitly
        // disable both environment and system proxies for public web reads only.
        .no_proxy()
        .dns_resolver(std::sync::Arc::new(public_web_dns::PublicWebResolver(resolver)))
}

#[cfg(feature = "net")]
impl ReqwestTransport {
    fn non_streaming_request(
        client: &reqwest::Client,
        req: HttpRequest,
    ) -> reqwest::RequestBuilder {
        let method = match req.method {
            Method::Get => reqwest::Method::GET,
            Method::Post => reqwest::Method::POST,
        };
        let mut rb = client.request(method, &req.url);
        // Total deadline for the NON-streaming path only (the client itself carries just a
        // connect timeout, for the streaming SSE path's sake — see `build_client`). Without
        // it, a server that accepts the connection and then goes silent parks this future
        // forever: a hung Batch submit/poll leaves the Dream Cycle job `Running` all night
        // with no error path ever firing. 300s is generous for the largest Batch results
        // download while still bounded.
        rb = rb.timeout(req.timeout.unwrap_or(std::time::Duration::from_secs(300)));
        for (k, v) in &req.headers {
            rb = rb.header(k, v);
        }
        if let Some(body) = req.body {
            rb = rb.body(body);
        }
        rb
    }

    fn client_for(&self, req: &HttpRequest) -> Result<reqwest::Client, TransportError> {
        if req.no_redirects && !req.public_web {
            static NO_REDIRECT_CLIENT: std::sync::OnceLock<Result<reqwest::Client, String>> = std::sync::OnceLock::new();
            return NO_REDIRECT_CLIENT.get_or_init(|| client_builder()
                .redirect(reqwest::redirect::Policy::none()).build().map_err(|e| e.to_string()))
                .as_ref().cloned().map_err(|e| TransportError::Io(e.clone()));
        }
        if !req.public_web {
            return Ok(self.client.clone());
        }
        // Recheck because `url` is public and can be changed after request construction.
        if crate::web_fetch::is_private_host(&req.url) {
            return Err(TransportError::PrivateDestination);
        }
        match PUBLIC_WEB_CLIENT.get_or_init(|| {
            public_web_client_builder(public_web_dns::SystemResolver)
                .build().map_err(|e| e.to_string())
        }) {
            Ok(client) => Ok(client.clone()),
            Err(error) => Err(TransportError::Io(error.clone())),
        }
    }

    /// A transport over the process-wide connection pool. **Prefer this over [`Self::new`]** for
    /// anything a user waits on: a fresh client starts with an empty pool, so its first request
    /// always pays a full handshake no matter how recently another one finished.
    ///
    /// The build result is memoised, failure included — if TLS cannot be initialised at all,
    /// retrying per request would just re-fail slowly.
    pub fn shared() -> Result<Self, TransportError> {
        match SHARED_CLIENT.get_or_init(|| build_client().map_err(|e| e.to_string())) {
            Ok(client) => Ok(Self { client: client.clone() }),
            Err(e) => Err(TransportError::Io(e.clone())),
        }
    }

    /// Isolated bounded decision requests never follow redirects carrying a bearer key.
    pub fn decisions() -> Result<Self, TransportError> {
        let client = decisions_client_builder()
            .build()
            .map_err(|e| TransportError::Io(e.to_string()))?;
        Ok(Self { client })
    }

    /// A transport with its own private connection pool. Only for callers that genuinely want
    /// isolation; everything on a latency path wants [`Self::shared`].
    pub fn new() -> Result<Self, TransportError> {
        Ok(Self { client: build_client()? })
    }
}

#[cfg(feature = "net")]
impl HttpTransport for ReqwestTransport {
    fn send(
        &self,
        req: HttpRequest,
    ) -> impl Future<Output = Result<HttpResponse, TransportError>> + Send {
        let client = self.client_for(&req);
        async move {
            let client = client?;
            let max_body_bytes = req.max_body_bytes;
            let rb = Self::non_streaming_request(&client, req);
            let mut resp = rb.send().await.map_err(|e| {
                if public_web_dns::refused(&e) {
                    TransportError::PrivateDestination
                } else if e.is_connect() {
                    TransportError::Unreachable(e.to_string())
                } else {
                    TransportError::Io(e.to_string())
                }
            })?;
            let status = resp.status().as_u16();
            let retry_after = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_retry_after);
            let Some(cap) = max_body_bytes else {
                let body = resp.text().await.map_err(|e| TransportError::Io(e.to_string()))?;
                return Ok(HttpResponse { status, body, retry_after });
            };
            // A declared length past the cap is refused before a byte of body is read; an
            // undeclared one is read chunk by chunk and abandoned the moment it crosses it.
            if resp.content_length().is_some_and(|n| n > cap as u64) {
                return Err(TransportError::BodyTooLarge(cap));
            }
            let mut bytes: Vec<u8> = Vec::new();
            while let Some(chunk) = resp
                .chunk()
                .await
                .map_err(|e| TransportError::Io(e.to_string()))?
            {
                if bytes.len() + chunk.len() > cap {
                    return Err(TransportError::BodyTooLarge(cap));
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(HttpResponse {
                status,
                body: String::from_utf8_lossy(&bytes).into_owned(),
                retry_after,
            })
        }
    }
}

/// `carry` に溜まったバイトのうち、有効なUTF-8として確定した前半を切り出す。
///
/// 途中で切れた文字（`error_len() == None`）は `None` を返して次のチャンクを待つ。一方
/// **そもそも不正なバイト列は捨てる** — 待っても直らないので、残しておくと carry の先頭に
/// 居座り、以降のチャンクが永久に出力されなくなる（carry も無限に伸びる）。
#[cfg(feature = "net")]
fn take_valid_utf8(carry: &mut Vec<u8>) -> Option<String> {
    loop {
        let (valid_to, invalid_len) = match std::str::from_utf8(carry) {
            Ok(_) => (carry.len(), None),
            Err(e) => (e.valid_up_to(), e.error_len()),
        };
        if valid_to > 0 {
            // `carry[..valid_to]` は構築上つねに有効なUTF-8なので、ここで文字は失われない。
            let s = String::from_utf8_lossy(&carry[..valid_to]).into_owned();
            carry.drain(..valid_to);
            return Some(s);
        }
        // `None` は途中で切れた文字（または carry が空）— 次のチャンクを待つ。`Some(n)` は
        // 直らないバイト列。捨てて、その後ろに有効な文字が続いていないか見直す。
        let n = invalid_len?;
        carry.drain(..n);
    }
}

#[cfg(feature = "net")]
impl StreamingTransport for ReqwestTransport {
    fn send_streaming<F>(
        &self,
        req: HttpRequest,
        mut on_chunk: F,
    ) -> impl Future<Output = Result<StreamOutcome, TransportError>> + Send
    where
        F: FnMut(&str) -> bool + Send,
    {
        let client = self.client_for(&req);
        async move {
            let client = client?;
            let method = match req.method {
                Method::Get => reqwest::Method::GET,
                Method::Post => reqwest::Method::POST,
            };
            let max_body_bytes = req.max_body_bytes;
            let mut rb = client.request(method, &req.url);
            if let Some(timeout) = req.timeout { rb = rb.timeout(timeout); }
            for (k, v) in &req.headers {
                rb = rb.header(k, v);
            }
            if let Some(body) = req.body {
                rb = rb.body(body);
            }
            let mut resp = rb.send().await.map_err(|e| TransportError::Io(e.to_string()))?;
            let status = resp.status().as_u16();
            // ステータスはボディより先に確定する。非2xxならチャンクを1つも渡さず、本文を
            // 集め切って `Failed` で返す — エラー本文がSSEデルタとして画面に出る経路を塞ぐ。
            if !(200..300).contains(&status) {
                let mut bytes = Vec::new();
                while let Some(chunk) = resp.chunk().await.map_err(|e| TransportError::Io(e.to_string()))? {
                    if let Some(limit) = max_body_bytes {
                        if bytes.len().saturating_add(chunk.len()) > limit { return Err(TransportError::BodyTooLarge(limit)); }
                    }
                    bytes.extend_from_slice(&chunk);
                }
                let body = String::from_utf8_lossy(&bytes).into_owned();
                return Ok(StreamOutcome::Failed { status, body });
            }
            // マルチバイト文字はチャンク境界をまたぐ（日本語の応答ではほぼ毎回起きる）。
            // 到着したバイトをそのまま lossy 変換すると、境界にかかった1文字は置換文字に
            // なって二度と戻らない。`take_valid_utf8` で有効な前半だけを渡し、途中で切れた
            // 文字のバイトは次のチャンクの頭と繋ぐために持ち越す。
            let mut carry: Vec<u8> = Vec::new();
            let mut received = 0usize;
            while let Some(bytes) =
                resp.chunk().await.map_err(|e| TransportError::Io(e.to_string()))?
            {
                received = received.saturating_add(bytes.len());
                if let Some(limit) = max_body_bytes {
                    if received > limit { return Err(TransportError::BodyTooLarge(limit)); }
                }
                carry.extend_from_slice(&bytes);
                if let Some(s) = take_valid_utf8(&mut carry) {
                    // 届いたその場でコールバックへ。false なら打ち切る（パネルが閉じた）。
                    if !on_chunk(&s) {
                        break;
                    }
                }
                // take_valid_utf8 が None を返したときはまだ1文字も完成していない。
                // 次のチャンクを待つ。
            }
            // ストリームが閉じたとき carry に残っているバイトは捨てる。正常な SSE は必ず
            // 行末で終わるので、ここに文字の途中のバイトが来ることはない。
            Ok(StreamOutcome::Streamed { status })
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "net")]
    #[tokio::test]
    async fn licensed_stream_does_not_follow_a_redirect() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = [0u8; 4096];
            let _ = socket.read(&mut bytes).await.unwrap();
            socket.write_all(format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: http://127.0.0.1:{port}/elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        });
        let req = HttpRequest::new(Method::Post, format!("http://127.0.0.1:{port}/v1/ai/chat"),
            vec![("authorization".into(), "Bearer fixture".into())], Some("{}".into())).unwrap()
            .without_redirects().with_timeout(std::time::Duration::from_secs(2));
        let result = ReqwestTransport::new().unwrap().send_streaming(req, |_| panic!("redirect body must not stream")).await.unwrap();
        assert!(matches!(result, StreamOutcome::Failed { status: 307, .. }));
        server.await.unwrap();
    }

    use super::*;

    #[cfg(feature = "net")]
    #[test]
    fn actual_request_keeps_jev_deadline_and_other_non_streaming_defaults() {
        let decisions = ReqwestTransport::decisions().unwrap();
        let request = crate::forms::jev::prepare_facts(
            &crate::forms::FormSnapshot {
                app: "Browser".into(),
                title: "Form".into(),
                website_host: "example.com".into(),
                fields: vec![],
            },
            &crate::forms::WrittenFacts {
                purpose: "UNKNOWN".into(),
                subject: "UNKNOWN".into(),
                facts: vec![],
            },
            &[],
            "",
        )
        .unwrap()
        .request(&crate::llm::Secret::new("test-key"))
        .unwrap();
        let actual = ReqwestTransport::non_streaming_request(&decisions.client, request)
            .build()
            .unwrap();
        assert_eq!(actual.timeout(), Some(&std::time::Duration::from_secs(45)));
        let request = HttpRequest::new(Method::Post, "https://example.com/batch", vec![], None)
            .unwrap();
        let actual = ReqwestTransport::non_streaming_request(&decisions.client, request)
            .build()
            .unwrap();
        assert_eq!(actual.timeout(), Some(&std::time::Duration::from_secs(300)));
    }

    #[cfg(feature = "net")]
    #[tokio::test]
    async fn decisions_client_does_not_follow_redirects() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut request = [0; 1024];
            assert!(socket.read(&mut request).unwrap() > 0);
            write!(socket, "HTTP/1.1 302 Found\r\nLocation: http://{address}/next\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        // Only this loopback fixture disables HTTPS. All production callers use the builder as-is.
        let client = decisions_client_builder()
            .https_only(false)
            .no_proxy()
            .build()
            .unwrap();
        let response = client
            .post(format!("http://{address}/start"))
            .header("authorization", "Bearer fixture-key")
            .timeout(std::time::Duration::from_secs(2))
            .send()
            .await
            .unwrap();
        server.join().unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::FOUND);
    }

    #[test]
    fn public_web_policy_is_opt_in_and_rejects_literal_destinations() {
        let provider = HttpRequest::new(Method::Post, "https://127.0.0.1/", vec![], None).expect("HTTPS provider");
        assert!(!provider.public_web);
        assert!(matches!(provider.for_public_web(), Err(TransportError::PrivateDestination)));
        let web = HttpRequest::new(Method::Get, "https://example.com/", vec![], None)
            .expect("request").for_public_web().expect("public web");
        assert!(web.public_web);
    }

    #[cfg(feature = "net")]
    #[tokio::test]
    async fn public_web_policy_rechecks_changed_url_before_connecting() {
        let transport = ReqwestTransport::new().expect("transport");
        let mut request = HttpRequest::new(Method::Get, "https://example.com/", vec![], None)
            .expect("request").for_public_web().expect("public web");
        request.url = "https://127.0.0.1/".into();
        // Must be the policy refusal, not merely a failed connection to 127.0.0.1:443.
        assert!(matches!(transport.send(request).await, Err(TransportError::PrivateDestination)));
    }

    #[test]
    fn rejects_plaintext_url() {
        let err = HttpRequest::new(Method::Get, "http://api.anthropic.com/v1/models", vec![], None);
        assert!(matches!(err, Err(TransportError::InsecureUrl(_))));
    }

    #[test]
    fn accepts_https_url() {
        let ok = HttpRequest::new(Method::Get, "https://api.anthropic.com/v1/models", vec![], None);
        assert!(ok.is_ok());
    }

    #[test]
    fn debug_redacts_api_key_header() {
        let req = HttpRequest::new(
            Method::Post,
            "https://api.anthropic.com/v1/messages",
            vec![
                ("x-api-key".into(), "sk-secret-abcdef".into()),
                ("anthropic-version".into(), "2023-06-01".into()),
            ],
            Some("{}".into()),
        )
        .unwrap();
        let dumped = format!("{req:?}");
        assert!(!dumped.contains("sk-secret-abcdef"), "api key must not appear in Debug");
        assert!(dumped.contains("***redacted***"));
        // Non-secret headers are still visible for debugging.
        assert!(dumped.contains("2023-06-01"));
    }

    #[tokio::test]
    async fn mock_streaming_transport_delivers_chunks_in_order() {
        let t = MockStreamingTransport::new(200, vec!["one ".into(), "two".into()]);
        let req =
            HttpRequest::new(Method::Post, "https://api.anthropic.com/v1/messages", vec![], None)
                .unwrap();

        let mut got: Vec<String> = Vec::new();
        let outcome = t
            .send_streaming(req, |c| {
                got.push(c.to_string());
                true
            })
            .await
            .unwrap();

        assert!(matches!(outcome, StreamOutcome::Streamed { status: 200 }));
        assert_eq!(got, vec!["one ".to_string(), "two".to_string()]);
    }

    /// コールバックが `false` を返す（＝パネルが閉じた）と、その場でストリームを打ち切り、
    /// エラーにせず `Ok` で戻る。応答の途中で閉じるのは正常な操作で、失敗ではない。
    #[tokio::test]
    async fn a_callback_returning_false_stops_the_stream_early() {
        let t = MockStreamingTransport::new(200, vec!["one".into(), "two".into()]);
        let req =
            HttpRequest::new(Method::Post, "https://api.anthropic.com/v1/messages", vec![], None)
                .unwrap();

        let mut seen: Vec<String> = Vec::new();
        let outcome = t
            .send_streaming(req, |c| {
                seen.push(c.to_string());
                // 最初の1つだけ受け取って打ち切る。
                false
            })
            .await
            .unwrap();

        assert!(matches!(outcome, StreamOutcome::Streamed { status: 200 }));
        assert_eq!(seen, vec!["one".to_string()], "打ち切り後もチャンクを渡している");
    }

    /// 非2xxではデルタを1つも流さない。エラー本文が回答としてパネルに出る経路を塞ぐ。
    #[tokio::test]
    async fn a_failed_status_streams_no_chunks() {
        let t = MockStreamingTransport::new(401, vec!["{\"error\":\"bad key\"}".into()]);
        let req = HttpRequest::new(Method::Post, "https://api.anthropic.com/v1/messages", vec![], None).unwrap();
        let mut seen = 0usize;

        let outcome = t.send_streaming(req, |_| { seen += 1; true }).await.unwrap();

        assert_eq!(seen, 0, "エラー本文がチャンクとして流れた");
        assert!(matches!(outcome, StreamOutcome::Failed { status: 401, .. }));
    }

    #[tokio::test]
    async fn mock_records_requests_and_replays_responses() {
        let t = MockTransport::new([
            HttpResponse { status: 200, body: "first".into(), retry_after: None },
            HttpResponse { status: 500, body: "second".into(), retry_after: None },
        ]);
        let r1 = t
            .send(HttpRequest::new(Method::Get, "https://x/a", vec![], None).unwrap())
            .await
            .unwrap();
        let r2 = t
            .send(HttpRequest::new(Method::Get, "https://x/b", vec![], None).unwrap())
            .await
            .unwrap();
        assert_eq!(r1.body, "first");
        assert_eq!(r2.status, 500);
        assert!(!r2.is_success());
        assert_eq!(t.sent().len(), 2);
        assert_eq!(t.sent()[1].url, "https://x/b");
    }

    /// マルチバイト文字がチャンク境界にかかっても壊れない。日本語の応答では常に起きる。
    #[cfg(feature = "net")]
    #[test]
    fn a_multibyte_character_split_across_chunks_survives() {
        // "日本" = 6 bytes. 最初のチャンクが1文字目の途中で切れる。
        let full = "日本".as_bytes();
        let (head, tail) = full.split_at(4);

        let mut carry = head.to_vec();
        let first = take_valid_utf8(&mut carry).expect("1文字目は確定しているはず");
        assert_eq!(first, "日");
        assert_eq!(carry.len(), 1, "2文字目の途中のバイトが持ち越されていない");

        carry.extend_from_slice(tail);
        let second = take_valid_utf8(&mut carry).expect("2文字目が確定するはず");
        assert_eq!(second, "本");
        assert!(carry.is_empty());

        assert_eq!(format!("{first}{second}"), "日本", "文字が壊れた");
    }

    #[cfg(feature = "net")]
    #[test]
    fn an_incomplete_first_character_is_held_not_dropped() {
        let mut carry = "日".as_bytes()[..2].to_vec();
        assert!(take_valid_utf8(&mut carry).is_none());
        assert_eq!(carry.len(), 2, "確定前のバイトが捨てられた");
    }

    /// 直らない不正バイトは捨てて先へ進む。残すと carry の先頭に居座って、以降の応答が
    /// 永久に出なくなる（レビューで見つかった livelock）。
    #[cfg(feature = "net")]
    #[test]
    fn an_invalid_byte_is_dropped_rather_than_blocking_the_stream() {
        let mut carry = vec![0xFF];
        carry.extend_from_slice("ok".as_bytes());

        assert_eq!(take_valid_utf8(&mut carry).as_deref(), Some("ok"));
        assert!(carry.is_empty());
    }

    #[cfg(feature = "net")]
    #[test]
    fn a_lone_invalid_byte_does_not_accumulate() {
        let mut carry = vec![0xFF];
        assert!(take_valid_utf8(&mut carry).is_none());
        assert!(carry.is_empty(), "不正バイトが carry に残っている（livelockの条件）");

        // 次のチャンクは普通に読める。
        carry.extend_from_slice("hi".as_bytes());
        assert_eq!(take_valid_utf8(&mut carry).as_deref(), Some("hi"));
    }
}
