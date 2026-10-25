//! The network half: three reads over the shared transport, each written to the ledger
//! (FR-WV-13). Generic over [`HttpTransport`], so the sequence of requests and the ledger rows
//! are tested with the mock; the real socket is `ReqwestTransport` behind the `net` feature.
//!
//! What leaves the device is the URL (or the video / post id inside one) and a browser-like
//! `User-Agent` + `Accept-Language`. No cookie, no credential, nothing from capture or memory
//! (FR-WV-16). Every request records one [`Route::Web`] row before it is sent: host, digest of
//! the URL, its byte length — never the response.

use super::chunk::{cap_transcript, Segment};
use super::html_text::{extract_text, Extracted};
use super::x_oembed::{self, Post};
use super::youtube::{self, VideoMeta};
use super::{
    host_of, ledger_url, WebFetchError, MAX_PAGE_BYTES, MAX_PAGE_CHARS, MAX_TRANSCRIPT_CHARS,
    PURPOSE_PAGE, PURPOSE_SOCIAL_POST, PURPOSE_VIDEO_META, PURPOSE_VIDEO_TRANSCRIPT,
};
use crate::llm::traceability::{Route, TraceRecord, TraceabilitySink};
use crate::llm::transport::{HttpRequest, HttpResponse, HttpTransport, Method};

/// A desktop browser's identity. The caption route answers a player, not a bot, and the oEmbed
/// endpoints are indifferent; this is what every open-source transcript tool sends.
const USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

/// A video's captions plus what the answer needs to say about them.
#[derive(Debug, Clone, PartialEq)]
pub struct Transcript {
    pub video_id: String,
    pub title: Option<String>,
    pub channel: Option<String>,
    pub language: String,
    pub auto_generated: bool,
    pub segments: Vec<Segment>,
    pub truncated: bool,
    pub kept_until_s: f64,
}

impl Transcript {
    /// The source line for the prompt and the panel: title and channel when known, else the id.
    pub fn source_line(&self) -> String {
        match (&self.title, &self.channel) {
            (Some(t), Some(c)) if !c.is_empty() => format!("{t} — {c}"),
            (Some(t), _) => t.clone(),
            _ => format!("video {}", self.video_id),
        }
    }
}

/// The three reads, over one transport and one ledger.
pub struct WebFetcher<'a, T: HttpTransport, S: TraceabilitySink + ?Sized> {
    transport: &'a T,
    sink: &'a S,
    /// The user's language (`LanguagePair.mine`), for caption track choice and `Accept-Language`.
    mine_lang: String,
}

impl<'a, T: HttpTransport, S: TraceabilitySink + ?Sized> WebFetcher<'a, T, S> {
    pub fn new(transport: &'a T, sink: &'a S, mine_lang: impl Into<String>) -> Self {
        Self {
            transport,
            sink,
            mine_lang: mine_lang.into(),
        }
    }

    /// One GET, recorded before it is sent. Maps the status to the error vocabulary; the body of
    /// a failed response is dropped here and never reaches a caller.
    async fn get(&self, url: &str, purpose: &str) -> Result<HttpResponse, WebFetchError> {
        self.get_capped(url, purpose, None).await
    }

    /// [`Self::get`] with a body cap enforced by the transport while it reads (FR-WV-12): the
    /// page read never buffers more than [`MAX_PAGE_BYTES`], whatever the user pasted.
    async fn get_capped(
        &self,
        url: &str,
        purpose: &str,
        max_body_bytes: Option<usize>,
    ) -> Result<HttpResponse, WebFetchError> {
        let headers = vec![
            ("user-agent".to_string(), USER_AGENT.to_string()),
            (
                "accept-language".to_string(),
                format!("{},en;q=0.8", self.mine_lang),
            ),
        ];
        let mut req = HttpRequest::new(Method::Get, url, headers, None)
            .and_then(HttpRequest::for_public_web)
            .map_err(|e| match e {
                crate::llm::transport::TransportError::PrivateDestination => {
                    WebFetchError::PrivateDestination
                }
                _ => WebFetchError::NotAUrl,
            })?;
        if let Some(cap) = max_body_bytes {
            req = req.with_max_body_bytes(cap);
        }
        // Fail closed: a request whose ledger row was not taken is not sent (invariant 3).
        self.sink
            .try_record(
                TraceRecord::for_chunk(Route::Web, purpose, host_of(url), url, false)
                    .with_url(ledger_url(url)),
            )
            .map_err(|_| WebFetchError::Network("not recorded".into()))?;
        let resp = self.transport.send(req).await.map_err(|e| match e {
            crate::llm::transport::TransportError::BodyTooLarge(_) => WebFetchError::TooLarge,
            crate::llm::transport::TransportError::PrivateDestination => {
                WebFetchError::PrivateDestination
            }
            other => WebFetchError::Network(other.to_string()),
        })?;
        match resp.status {
            200..=299 => Ok(resp),
            // 401 / 403 is "not public" (a private video, a login wall, a bot gate the page put
            // up); 429 alone is "try later" — a rate limit reported as "not public" would tell
            // the user a public video is private.
            429 => Err(WebFetchError::Blocked),
            401 | 403 | 404 | 410 | 451 => Err(WebFetchError::NotPublic),
            _ => Err(WebFetchError::ShapeChanged),
        }
    }

    /// The transcript of one video (FR-WV-02/03): metadata by oEmbed, then the watch page for
    /// its caption tracks, then the chosen track. A video without captions is an error the
    /// caller shows, not a transcript of nothing.
    pub async fn youtube_transcript(&self, video_id: &str) -> Result<Transcript, WebFetchError> {
        // oEmbed first: it is the cheap, official call, and a private or removed video answers
        // here with a 4xx before the heavy page is fetched.
        let meta: Option<VideoMeta> = match self.get(&youtube::oembed_url(video_id), PURPOSE_VIDEO_META).await {
            Ok(resp) => youtube::parse_oembed(&resp.body),
            // Both are the caller's to show as they are: a private video is not "try later",
            // and a rate limit is not "not public".
            Err(e @ (WebFetchError::NotPublic | WebFetchError::Blocked)) => return Err(e),
            // Metadata is nice to have; the transcript is the point.
            Err(_) => None,
        };
        let page = self
            .get(&youtube::watch_url(video_id, None), PURPOSE_VIDEO_TRANSCRIPT)
            .await?;
        let tracks = youtube::extract_caption_tracks(&page.body)?;
        let track = youtube::select_track(&tracks, &self.mine_lang).ok_or(WebFetchError::NoCaptions)?;
        let cues = self
            .get(&youtube::caption_url(track), PURPOSE_VIDEO_TRANSCRIPT)
            .await?;
        let segments = youtube::parse_json3(&cues.body)?;
        if segments.is_empty() {
            return Err(WebFetchError::NoCaptions);
        }
        let capped = cap_transcript(&segments, MAX_TRANSCRIPT_CHARS);
        Ok(Transcript {
            video_id: video_id.to_string(),
            title: meta.as_ref().map(|m| m.title.clone()),
            channel: meta.as_ref().map(|m| m.channel.clone()),
            language: track.language_code.clone(),
            auto_generated: track.auto_generated,
            segments: capped.segments,
            truncated: capped.truncated,
            kept_until_s: capped.kept_until_s,
        })
    }

    /// One public X post (FR-WV-08).
    pub async fn x_post(&self, user: &str, id: &str) -> Result<Post, WebFetchError> {
        let resp = self.get(&x_oembed::oembed_url(user, id), PURPOSE_SOCIAL_POST).await?;
        x_oembed::parse_oembed(&resp.body).ok_or(WebFetchError::ShapeChanged)
    }

    /// The readable text of a page (FR-WV-12).
    pub async fn page(&self, url: &str) -> Result<Extracted, WebFetchError> {
        let resp = self.get_capped(url, PURPOSE_PAGE, Some(MAX_PAGE_BYTES)).await?;
        // Belt and braces for a transport that ignores the cap (the mock), and the one check
        // the transport cannot make: a PDF, image or archive decoded as text is replacement
        // characters, not a page.
        if resp.body.len() > MAX_PAGE_BYTES {
            return Err(WebFetchError::TooLarge);
        }
        if looks_binary(&resp.body) {
            return Err(WebFetchError::ShapeChanged);
        }
        let extracted = extract_text(&resp.body, MAX_PAGE_CHARS);
        if extracted.text.trim().is_empty() {
            return Err(WebFetchError::ShapeChanged);
        }
        Ok(extracted)
    }
}

/// A lossy-decoded binary body: replacement characters or NULs in the first few kilobytes. HTML
/// has neither; a few stray ones in a mislabelled charset are tolerated.
fn looks_binary(body: &str) -> bool {
    let bad = body
        .chars()
        .take(4096)
        .filter(|c| *c == '\u{FFFD}' || *c == '\0')
        .count();
    bad > 8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::traceability::RecordingSink;
    use crate::llm::transport::MockTransport;

    fn ok(body: &str) -> HttpResponse {
        HttpResponse {
            retry_after: None,
            status: 200,
            body: body.to_string(),
        }
    }
    fn status(code: u16) -> HttpResponse {
        HttpResponse {
            retry_after: None,
            status: code,
            body: "<html>vendor error page — must never surface</html>".to_string(),
        }
    }

    #[tokio::test]
    async fn a_rate_limited_oembed_is_reported_as_blocked_not_private() {
        let sink = RecordingSink::default();
        let transport = MockTransport::new([status(429)]);
        let fetcher = WebFetcher::new(&transport, &sink, "en");
        assert_eq!(fetcher.youtube_transcript("dQw4w9WgXcQ").await, Err(WebFetchError::Blocked));
    }

    #[tokio::test]
    async fn a_binary_body_is_not_a_page() {
        let sink = RecordingSink::default();
        let junk: String = std::iter::repeat_n('\u{FFFD}', 40).chain("%PDF-1.7".chars()).collect();
        let transport = MockTransport::new([ok(&junk)]);
        let fetcher = WebFetcher::new(&transport, &sink, "en");
        assert_eq!(fetcher.page("https://example.com/report.pdf").await, Err(WebFetchError::ShapeChanged));
    }

    const WATCH: &str = r#"<script>var ytInitialPlayerResponse = {"captions":{"playerCaptionsTracklistRenderer":{"captionTracks":[{"baseUrl":"https://www.youtube.com/api/timedtext?v=dQw4w9WgXcQ&lang=ja","name":{"simpleText":"日本語"},"languageCode":"ja"}]}}};</script>"#;
    const CUES: &str = r#"{"events":[{"tStartMs":0,"segs":[{"utf8":"こんにちは"}]},{"tStartMs":60000,"segs":[{"utf8":"次の話題"}]}]}"#;

    #[tokio::test]
    async fn private_caption_destinations_stop_before_request_or_trace() {
        for destination in ["https://127.0.0.1/captions", "https://intranet.internal/captions", "https://[fd00::1]/captions"] {
            let page = WATCH.replace("https://www.youtube.com/api/timedtext?v=dQw4w9WgXcQ&lang=ja", destination);
            let transport = MockTransport::new([ok(r#"{"title":"Talk","author_name":"Chan"}"#), ok(&page)]);
            let sink = RecordingSink::default();
            let fetcher = WebFetcher::new(&transport, &sink, "ja");
            assert!(fetcher.youtube_transcript("dQw4w9WgXcQ").await.is_err());
            assert_eq!(transport.sent().len(), 2);
            assert_eq!(sink.records().len(), 2);
        }
    }

    #[tokio::test]
    async fn direct_page_calls_reject_private_literals_before_transport() {
        let transport = MockTransport::new([]);
        let sink = RecordingSink::default();
        let fetcher = WebFetcher::new(&transport, &sink, "en");
        assert_eq!(fetcher.page("https://10.0.0.1/").await, Err(WebFetchError::PrivateDestination));
        assert!(transport.sent().is_empty());
        assert!(sink.records().is_empty());
    }

    #[tokio::test]
    async fn a_transcript_is_three_requests_each_on_the_ledger_without_a_body() {
        let transport = MockTransport::new([
            ok(r#"{"title":"Talk","author_name":"Chan"}"#),
            ok(WATCH),
            ok(CUES),
        ]);
        let sink = RecordingSink::default();
        let fetcher = WebFetcher::new(&transport, &sink, "ja");
        let t = fetcher.youtube_transcript("dQw4w9WgXcQ").await.unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(t.source_line(), "Talk — Chan");
        assert_eq!(t.language, "ja");
        assert!(!t.auto_generated);
        assert_eq!(t.segments.len(), 2);
        assert!(!t.truncated);

        let sent = transport.sent();
        assert_eq!(sent.len(), 3);
        assert!(sent[0].url.starts_with("https://www.youtube.com/oembed?"));
        assert_eq!(sent[1].url, "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
        assert!(sent[2].url.ends_with("&fmt=json3"));
        assert!(sent.iter().all(|r| r.body.is_none()), "GETs carry nothing");
        assert!(sent.iter().all(|r| !r.headers.iter().any(|(k, _)| k == "cookie")));

        let rows = sink.records();
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| r.route == Route::Web && !r.third_party));
        assert_eq!(rows[0].purpose, PURPOSE_VIDEO_META);
        assert_eq!(rows[1].purpose, PURPOSE_VIDEO_TRANSCRIPT);
        assert!(rows.iter().all(|r| r.destination == "www.youtube.com"));
        // Issue #928: the row keeps the full address it read, not just the site.
        assert_eq!(rows[1].url.as_deref(), Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ"));
        assert!(rows.iter().all(|r| r.url.is_some()));
    }

    #[tokio::test]
    async fn a_private_video_stops_at_the_oembed_call() {
        let transport = MockTransport::new([status(401)]);
        let sink = RecordingSink::default();
        let fetcher = WebFetcher::new(&transport, &sink, "en");
        assert_eq!(
            fetcher.youtube_transcript("dQw4w9WgXcQ").await,
            Err(WebFetchError::NotPublic)
        );
        assert_eq!(transport.sent().len(), 1);
    }

    #[tokio::test]
    async fn a_video_without_captions_is_said_so_after_the_page() {
        let transport = MockTransport::new([
            status(500), // metadata is optional
            ok(r#"<script>var ytInitialPlayerResponse = {"captions":{}};</script>"#),
        ]);
        let sink = RecordingSink::default();
        let fetcher = WebFetcher::new(&transport, &sink, "en");
        assert_eq!(
            fetcher.youtube_transcript("dQw4w9WgXcQ").await,
            Err(WebFetchError::NoCaptions)
        );
    }

    #[tokio::test]
    async fn a_post_is_one_request_and_a_deleted_post_is_not_public() {
        let transport = MockTransport::new([ok(
            r#"{"author_name":"A","author_url":"https://twitter.com/a","url":"https://twitter.com/a/status/1","html":"<blockquote><p>hi</p>&mdash; A (@a) <a href=\"x\">May 1, 2026</a></blockquote>"}"#,
        )]);
        let sink = RecordingSink::default();
        let fetcher = WebFetcher::new(&transport, &sink, "en");
        let post = fetcher.x_post("a", "1").await.unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(post.text, "hi");
        assert_eq!(post.posted_at.as_deref(), Some("May 1, 2026"));
        assert_eq!(sink.records()[0].destination, "publish.twitter.com");

        let transport = MockTransport::new([status(404)]);
        let fetcher = WebFetcher::new(&transport, &sink, "en");
        assert_eq!(fetcher.x_post("a", "2").await, Err(WebFetchError::NotPublic));
    }

    #[tokio::test]
    async fn pages_are_capped_by_bytes_and_rate_limits_are_named() {
        let sink = RecordingSink::default();
        let huge = format!("<p>{}</p>", "x".repeat(MAX_PAGE_BYTES + 1));
        let transport = MockTransport::new([ok(&huge)]);
        let fetcher = WebFetcher::new(&transport, &sink, "en");
        assert_eq!(
            fetcher.page("https://example.com/big").await,
            Err(WebFetchError::TooLarge)
        );

        let transport = MockTransport::new([status(429)]);
        let fetcher = WebFetcher::new(&transport, &sink, "en");
        assert_eq!(
            fetcher.page("https://example.com/x").await,
            Err(WebFetchError::Blocked)
        );

        let transport = MockTransport::new([ok("<html><body><article><p>Real text</p></article></body></html>")]);
        let fetcher = WebFetcher::new(&transport, &sink, "en");
        let page = fetcher.page("https://example.com/x").await.unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(page.text, "Real text");
        assert!(fetcher.page("http://example.com/x").await.is_err(), "plaintext refused");
    }
}
