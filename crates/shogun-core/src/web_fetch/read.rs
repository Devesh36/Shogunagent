//! The synchronous face of the fetcher (issue #555 §4-8): the one call the Memory API backend
//! and the tool runner make from ordinary code — with no runtime in hand, and possibly *inside*
//! someone else's (the REST handler runs on axum's workers) — to fetch one link of one kind and
//! get a value back.
//!
//! The fetch runs on its own thread with its own current-thread runtime, so it never nests a
//! `block_on` in a tokio worker. The ledger rows the fetcher writes are relayed to the caller's
//! sink as they are produced (a channel drained while the thread runs), so each row still lands
//! before the response it belongs to — the caller's sink, not a copy, is what records them.

use serde_json::{json, Value};

use super::chunk::Segment;
use super::fetch::Transcript;
use super::html_text::Extracted;
use super::x_oembed::Post;
use super::{classify_url, WebFetchError, WebTarget};

/// Which of the three reads a face asked for. The kind is the API's promise — a transcript, a
/// post, a page — so the link must be of that kind: a page link handed to the transcript read is
/// [`WebFetchError::WrongKind`], never a quiet fallback to something else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebReadKind {
    VideoTranscript,
    SocialPost,
    Page,
}

impl WebReadKind {
    /// The wire / ledger noun.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::VideoTranscript => "video_transcript",
            Self::SocialPost => "social_post",
            Self::Page => "page",
        }
    }

    /// The scope-table op (`crates/shogun-mcp/src/scope.rs`, `Service::Web`) this read is.
    pub fn from_scope_op(op: &str) -> Option<Self> {
        match op {
            "video_transcript" => Some(Self::VideoTranscript),
            "social_post" => Some(Self::SocialPost),
            "page" => Some(Self::Page),
            _ => None,
        }
    }

    /// Resolve `url` for this kind (pure). A video or post link asked for *as a page* is a page
    /// — the watch page and the post page are pages — but a page asked for as a transcript is
    /// not a transcript of anything.
    pub fn resolve(self, url: &str) -> Result<WebTarget, WebFetchError> {
        let target = classify_url(url).ok_or_else(|| {
            // A real https link that was refused for where it points, not for its shape.
            let has_host = url::Url::parse(url.trim()).is_ok_and(|u| u.host().is_some());
            if url.trim().starts_with("https://") && has_host && super::is_private_host(url) {
                WebFetchError::PrivateDestination
            } else {
                WebFetchError::NotAUrl
            }
        })?;
        match (self, &target) {
            (Self::VideoTranscript, WebTarget::YouTubeVideo { .. })
            | (Self::SocialPost, WebTarget::XPost { .. })
            | (Self::Page, WebTarget::Page { .. }) => Ok(target),
            (Self::Page, _) => Ok(WebTarget::Page {
                url: url.trim().to_string(),
            }),
            _ => Err(WebFetchError::WrongKind),
        }
    }
}

/// What a read produced, ready to render for a face or for the model.
#[derive(Debug, Clone, PartialEq)]
pub enum WebRead {
    Transcript(Transcript),
    Post(Post),
    Page { url: String, extracted: Extracted },
}

impl WebRead {
    /// The JSON every face returns (spec §4-6 / §4-8): the same fields the hub tool hands the
    /// model and the REST body carries. Text only — no thumbnail, no media URL.
    pub fn to_json(&self) -> Value {
        match self {
            WebRead::Transcript(t) => json!({
                "kind": "video_transcript",
                "video_id": t.video_id,
                "title": t.title,
                "channel": t.channel,
                "language": t.language,
                "auto_generated": t.auto_generated,
                "truncated": t.truncated,
                "kept_until_s": t.kept_until_s,
                "source": t.source_line(),
                "segments": t.segments.iter().map(segment_json).collect::<Vec<_>>(),
            }),
            WebRead::Post(p) => json!({
                "kind": "social_post",
                "author": p.author_name,
                "handle": p.author_handle,
                "posted_at": p.posted_at,
                "text": p.text,
                "url": p.url,
            }),
            WebRead::Page { url, extracted } => json!({
                "kind": "page",
                "url": url,
                "title": extracted.title,
                "text": extracted.text,
                "truncated": extracted.truncated,
            }),
        }
    }
}

fn segment_json(s: &Segment) -> Value {
    json!({ "start_s": s.start_s, "text": s.text })
}

/// The JSON a face returns for a failed read: the outcome bucket and the one user-facing
/// sentence — never the vendor's response.
pub fn error_json(e: &WebFetchError) -> Value {
    json!({ "error": e.outcome(), "message": e.message() })
}

/// The user's language for caption-track choice and `Accept-Language`, from the environment's
/// locale (the daemons run without the desktop's locale plumbing); the pair's default otherwise.
pub fn mine_language_tag() -> String {
    let locale = std::env::var("LANG").unwrap_or_default();
    crate::translate::LanguagePair::from_locale(&locale)
        .mine
        .tag()
        .to_string()
}

/// How long a read may take before the caller gives up (the thread is left to finish and its
/// result dropped). Generous for a long caption track; far below the transport's own ceiling.
pub const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

/// A sink that hands each record to the caller's thread the moment it is produced.
#[cfg(feature = "net")]
struct RelaySink(std::sync::mpsc::Sender<crate::llm::traceability::TraceRecord>);

#[cfg(feature = "net")]
impl crate::llm::traceability::TraceabilitySink for RelaySink {
    fn record(&self, rec: crate::llm::traceability::TraceRecord) {
        let _ = self.0.send(rec);
    }

    /// Once the reader stopped listening (Stop or timeout), a row can no longer reach the
    /// ledger, so the fetcher must not send the request it describes.
    fn try_record(
        &self,
        rec: crate::llm::traceability::TraceRecord,
    ) -> Result<(), crate::llm::traceability::TraceUnrecorded> {
        self.0
            .send(rec)
            .map_err(|_| crate::llm::traceability::TraceUnrecorded)
    }
}

/// Fetch one link of one kind, synchronously, from any thread. Every ledger row goes to `sink`
/// as the fetcher produces it. The link is resolved first, so a wrong-kind or non-https link
/// costs no thread and no request.
#[cfg(feature = "net")]
pub fn read_blocking<S: crate::llm::traceability::TraceabilitySink + ?Sized>(
    kind: WebReadKind,
    url: &str,
    mine_lang: &str,
    sink: &S,
) -> Result<WebRead, WebFetchError> {
    read_blocking_until(kind, url, mine_lang, sink, &std::sync::atomic::AtomicBool::new(false))
}

/// [`read_blocking`] that gives up within one poll slice (25 ms) of `cancelled` being set; the
/// worker thread is left to finish and its result dropped, as on timeout.
#[cfg(feature = "net")]
pub fn read_blocking_until<S: crate::llm::traceability::TraceabilitySink + ?Sized>(
    kind: WebReadKind,
    url: &str,
    mine_lang: &str,
    sink: &S,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<WebRead, WebFetchError> {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    if cancelled.load(std::sync::atomic::Ordering::Acquire) {
        return Err(WebFetchError::Network("cancelled".into()));
    }
    let target = kind.resolve(url)?;
    let (tx, rx) = mpsc::channel();
    let mine = mine_lang.to_string();
    let worker = std::thread::Builder::new()
        .name("shogun-web-fetch".into())
        .spawn(move || -> Result<WebRead, WebFetchError> {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| WebFetchError::Network(e.to_string()))?;
            let transport = crate::llm::transport::ReqwestTransport::shared()
                .map_err(|e| WebFetchError::Network(e.to_string()))?;
            let relay = RelaySink(tx);
            let fetcher = super::fetch::WebFetcher::new(&transport, &relay, mine);
            rt.block_on(async {
                match target {
                    WebTarget::YouTubeVideo { id, .. } => {
                        fetcher.youtube_transcript(&id).await.map(WebRead::Transcript)
                    }
                    WebTarget::XPost { user, id } => {
                        fetcher.x_post(&user, &id).await.map(WebRead::Post)
                    }
                    WebTarget::Page { url } => fetcher
                        .page(&url)
                        .await
                        .map(|extracted| WebRead::Page { url, extracted }),
                }
            })
        })
        .map_err(|e| WebFetchError::Network(e.to_string()))?;
    let started = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_millis(25)) {
            Ok(rec) => sink.record(rec),
            // The relay is dropped with the fetcher: the thread is done.
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if worker.is_finished() {
                    break;
                }
                if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                    return Err(WebFetchError::Network("cancelled".into()));
                }
                if started.elapsed() > READ_TIMEOUT {
                    return Err(WebFetchError::Network("timed out".into()));
                }
            }
        }
    }
    for rec in rx.try_iter() {
        sink.record(rec);
    }
    worker
        .join()
        .map_err(|_| WebFetchError::Network("the fetch thread stopped".into()))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "net")]
    #[test]
    fn a_relay_whose_reader_left_refuses_the_row() {
        use crate::llm::traceability::{Route, TraceRecord, TraceabilitySink};
        let (tx, rx) = std::sync::mpsc::channel();
        let relay = RelaySink(tx);
        let row = || TraceRecord::for_chunk(Route::Web, "web_read", "example.com", "x", false);
        assert!(relay.try_record(row()).is_ok());
        drop(rx);
        assert!(relay.try_record(row()).is_err());
    }

    #[cfg(feature = "net")]
    #[test]
    fn a_cancelled_read_returns_without_a_request() {
        let sink = crate::llm::traceability::RecordingSink::default();
        let stopped = std::sync::atomic::AtomicBool::new(true);
        let started = std::time::Instant::now();
        let read = read_blocking_until(
            WebReadKind::Page,
            "https://example.com/article",
            "en",
            &sink,
            &stopped,
        );
        assert!(read.is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn a_kind_accepts_only_its_own_links_and_page_takes_any() {
        let video = "https://www.youtube.com/watch?v=dQw4w9WgXcQ";
        let post = "https://x.com/shogunai/status/1700000000000000000";
        let page = "https://example.com/article";
        assert!(matches!(
            WebReadKind::VideoTranscript.resolve(video),
            Ok(WebTarget::YouTubeVideo { .. })
        ));
        assert_eq!(WebReadKind::VideoTranscript.resolve(page), Err(WebFetchError::WrongKind));
        assert_eq!(WebReadKind::VideoTranscript.resolve(post), Err(WebFetchError::WrongKind));
        assert!(matches!(WebReadKind::SocialPost.resolve(post), Ok(WebTarget::XPost { .. })));
        assert_eq!(WebReadKind::SocialPost.resolve(video), Err(WebFetchError::WrongKind));
        // A page read of a video link reads the watch page as a page: the caller asked for a page.
        assert_eq!(
            WebReadKind::Page.resolve(video),
            Ok(WebTarget::Page { url: video.to_string() })
        );
        assert_eq!(WebReadKind::Page.resolve(page), Ok(WebTarget::Page { url: page.to_string() }));
        // Not https: no kind takes it.
        assert_eq!(WebReadKind::Page.resolve("http://example.com/"), Err(WebFetchError::NotAUrl));
        assert_eq!(WebReadKind::Page.resolve("https://"), Err(WebFetchError::NotAUrl));
        assert_eq!(
            WebReadKind::Page.resolve("https://192.168.1.1/admin"),
            Err(WebFetchError::PrivateDestination)
        );
        assert_eq!(WebReadKind::Page.resolve("example.com"), Err(WebFetchError::NotAUrl));
    }

    #[test]
    fn scope_ops_round_trip_to_kinds() {
        for kind in [WebReadKind::VideoTranscript, WebReadKind::SocialPost, WebReadKind::Page] {
            assert_eq!(WebReadKind::from_scope_op(kind.as_str()), Some(kind));
        }
        assert_eq!(WebReadKind::from_scope_op("open_url"), None);
    }

    #[test]
    fn json_carries_text_and_offsets_only() {
        let read = WebRead::Transcript(Transcript {
            video_id: "abc".into(),
            title: Some("T".into()),
            channel: Some("C".into()),
            language: "en".into(),
            auto_generated: true,
            segments: vec![Segment { start_s: 1.5, text: "hi".into() }],
            truncated: false,
            kept_until_s: 1.5,
        });
        let v = read.to_json();
        assert_eq!(v["kind"], "video_transcript");
        assert_eq!(v["source"], "T — C");
        assert_eq!(v["segments"][0]["start_s"], 1.5);
        assert_eq!(v["segments"][0]["text"], "hi");
        assert!(v.get("thumbnail").is_none());
        let err = error_json(&WebFetchError::NoCaptions);
        assert_eq!(err["error"], "no_captions");
        assert!(err["message"].as_str().unwrap().contains("captions"));
    }
}
