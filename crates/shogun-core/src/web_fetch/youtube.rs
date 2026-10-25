//! YouTube: video ids, the caption track a watch page links to, and the `json3` cue format
//! (FR-WV-01..05).
//!
//! **Where the transcript comes from, and why it is allowed to break.** The Data API cannot hand
//! out another channel's captions, so the only key-less route is the one the player itself uses:
//! the watch page embeds `ytInitialPlayerResponse`, whose `captionTracks` carry a `baseUrl` for
//! each track, and that URL answers with cues. It is unofficial (OPEN-WV-01, accepted by the
//! owner with an opt-in disclosure), so the two places the shape lives — [`extract_caption_tracks`]
//! and [`parse_json3`] — are isolated and return [`WebFetchError::ShapeChanged`] rather than
//! guessing, exactly as the Composio envelope parser does. A shape change costs the user one
//! feature until it is patched; it never costs them a wrong answer.
//!
//! Nothing here downloads media. A caption track is text with millisecond offsets.

use super::chunk::Segment;
use super::WebFetchError;

/// A video id: exactly eleven URL-safe base64 characters.
fn is_video_id(s: &str) -> bool {
    s.len() == 11
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn split_url(url: &str) -> Option<(&str, &str, &str)> {
    let rest = url.strip_prefix("https://")?;
    let (host_path, query) = match rest.split_once('?') {
        Some((hp, q)) => (hp, q.split('#').next().unwrap_or("")),
        None => (rest.split('#').next().unwrap_or(rest), ""),
    };
    let (host, path) = match host_path.split_once('/') {
        Some((h, p)) => (h, p),
        None => (host_path, ""),
    };
    Some((host, path.trim_end_matches('/'), query))
}

fn query_value<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)
}

fn is_youtube_host(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    matches!(
        h.as_str(),
        "youtube.com"
            | "www.youtube.com"
            | "m.youtube.com"
            | "music.youtube.com"
            | "youtube-nocookie.com"
            | "www.youtube-nocookie.com"
    )
}

fn is_short_host(host: &str) -> bool {
    matches!(host.to_ascii_lowercase().as_str(), "youtu.be" | "www.youtu.be")
}

/// The video id in a watch / youtu.be / shorts / live / embed URL. `None` for playlists,
/// channels and everything else (they classify as an ordinary page).
pub fn parse_video_id(url: &str) -> Option<String> {
    let (host, path, query) = split_url(url.trim())?;
    let candidate = if is_short_host(host) {
        path.split('/').next()?
    } else if is_youtube_host(host) {
        match path.split('/').collect::<Vec<_>>().as_slice() {
            ["watch"] => query_value(query, "v")?,
            ["shorts", id] | ["live", id] | ["embed", id] | ["v", id] => id,
            _ => return None,
        }
    } else {
        return None;
    };
    is_video_id(candidate).then(|| candidate.to_string())
}

/// `t=754s`, `t=754`, `t=1h2m3s`, `start=754` → seconds. `None` when absent or malformed.
pub fn parse_start_seconds(url: &str) -> Option<u32> {
    let (_, _, query) = split_url(url.trim())?;
    let raw = query_value(query, "t").or_else(|| query_value(query, "start"))?;
    if let Ok(n) = raw.parse::<u32>() {
        return Some(n);
    }
    let mut total: u32 = 0;
    let mut number = String::new();
    for c in raw.chars() {
        if c.is_ascii_digit() {
            number.push(c);
            continue;
        }
        let n: u32 = number.parse().ok()?;
        number.clear();
        total = total.checked_add(match c {
            'h' => n.checked_mul(3600)?,
            'm' => n.checked_mul(60)?,
            's' => n,
            _ => return None,
        })?;
    }
    if !number.is_empty() {
        return None;
    }
    (total > 0).then_some(total)
}

/// `https://www.youtube.com/watch?v=ID[&t=754s]` — the link an answer's headings carry.
pub fn watch_url(id: &str, start_s: Option<u32>) -> String {
    match start_s {
        Some(s) if s > 0 => format!("https://www.youtube.com/watch?v={id}&t={s}s"),
        _ => format!("https://www.youtube.com/watch?v={id}"),
    }
}

/// The official, key-less metadata endpoint.
pub fn oembed_url(id: &str) -> String {
    format!("https://www.youtube.com/oembed?url=https%3A%2F%2Fwww.youtube.com%2Fwatch%3Fv%3D{id}&format=json")
}

/// Title and channel from the oEmbed answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoMeta {
    pub title: String,
    pub channel: String,
}

pub fn parse_oembed(json: &str) -> Option<VideoMeta> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let title = v.get("title")?.as_str()?.trim().to_string();
    let channel = v
        .get("author_name")
        .and_then(|a| a.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    (!title.is_empty()).then_some(VideoMeta { title, channel })
}

/// One caption track the watch page offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptionTrack {
    pub base_url: String,
    /// BCP-47-ish ("en", "ja", "en-US").
    pub language_code: String,
    pub name: String,
    /// `kind == "asr"`: machine-generated. Lower accuracy, and the answer says so (FR-WV-03).
    pub auto_generated: bool,
}

/// Find the `captionTracks` array in a watch page and parse it.
///
/// The page is ~1MB of HTML with the player response inlined as JSON; the array is located by
/// its key and bracket-matched (respecting JSON strings) before being handed to serde, so the
/// rest of the page is never parsed.
pub fn extract_caption_tracks(html: &str) -> Result<Vec<CaptionTrack>, WebFetchError> {
    const KEY: &str = "\"captionTracks\":";
    let Some(at) = html.find(KEY) else {
        // No tracks key at all: either a video with no captions, or not a watch page.
        return if html.contains("ytInitialPlayerResponse") {
            Err(WebFetchError::NoCaptions)
        } else if looks_like_bot_wall(html) {
            Err(WebFetchError::Blocked)
        } else {
            Err(WebFetchError::ShapeChanged)
        };
    };
    let after = &html[at + KEY.len()..];
    let array = json_array_at(after).ok_or(WebFetchError::ShapeChanged)?;
    let values: Vec<serde_json::Value> =
        serde_json::from_str(array).map_err(|_| WebFetchError::ShapeChanged)?;
    let tracks: Vec<CaptionTrack> = values
        .iter()
        .filter_map(|t| {
            let base_url = t.get("baseUrl")?.as_str()?.to_string();
            let language_code = t.get("languageCode")?.as_str()?.to_string();
            let name = t
                .get("name")
                .and_then(|n| {
                    n.get("simpleText")
                        .and_then(|s| s.as_str())
                        .or_else(|| n.get("runs")?.get(0)?.get("text")?.as_str())
                })
                .unwrap_or("")
                .to_string();
            let auto_generated = t.get("kind").and_then(|k| k.as_str()) == Some("asr");
            base_url.starts_with("https://").then_some(CaptionTrack {
                base_url,
                language_code,
                name,
                auto_generated,
            })
        })
        .collect();
    if tracks.is_empty() {
        return Err(WebFetchError::NoCaptions);
    }
    Ok(tracks)
}

fn looks_like_bot_wall(html: &str) -> bool {
    html.contains("confirm you’re not a bot")
        || html.contains("confirm you're not a bot")
        || html.contains("consent.youtube.com")
}

/// The JSON array starting at the first `[` of `s`, bracket-matched with string awareness.
fn json_array_at(s: &str) -> Option<&str> {
    let start = s.find('[')?;
    if !s[..start].trim().is_empty() {
        return None;
    }
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

fn primary_lang(tag: &str) -> String {
    tag.split(['-', '_']).next().unwrap_or("").to_ascii_lowercase()
}

/// Pick the track to read (FR-WV-03): a human track in the user's language, then a machine one
/// in it, then any human track (the video's own language), then any machine one.
pub fn select_track<'a>(tracks: &'a [CaptionTrack], mine: &str) -> Option<&'a CaptionTrack> {
    let mine = primary_lang(mine);
    let in_mine = |t: &&CaptionTrack| primary_lang(&t.language_code) == mine;
    tracks
        .iter()
        .find(|t| !t.auto_generated && in_mine(t))
        .or_else(|| tracks.iter().find(|t| t.auto_generated && in_mine(t)))
        .or_else(|| tracks.iter().find(|t| !t.auto_generated))
        .or_else(|| tracks.iter().find(|t| t.auto_generated))
}

/// The track URL in the `json3` format the parser below reads.
pub fn caption_url(track: &CaptionTrack) -> String {
    if track.base_url.contains("fmt=") {
        return track.base_url.clone();
    }
    let sep = if track.base_url.contains('?') { '&' } else { '?' };
    format!("{}{sep}fmt=json3", track.base_url)
}

/// Cues from a `json3` body: `{"events":[{"tStartMs":…,"segs":[{"utf8":"…"}]}]}`.
///
/// Events without text (the rolling "append" markers of machine tracks carry only a newline)
/// are dropped, so every returned segment says something.
pub fn parse_json3(json: &str) -> Result<Vec<Segment>, WebFetchError> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|_| WebFetchError::ShapeChanged)?;
    let events = v
        .get("events")
        .and_then(|e| e.as_array())
        .ok_or(WebFetchError::ShapeChanged)?;
    let mut out = Vec::new();
    for ev in events {
        let Some(segs) = ev.get("segs").and_then(|s| s.as_array()) else {
            continue;
        };
        let text: String = segs
            .iter()
            .filter_map(|s| s.get("utf8").and_then(|u| u.as_str()))
            .collect::<Vec<_>>()
            .join("")
            .replace('\n', " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if text.is_empty() {
            continue;
        }
        let start_ms = ev.get("tStartMs").and_then(|t| t.as_f64()).unwrap_or(0.0);
        out.push(Segment {
            start_s: start_ms / 1000.0,
            text,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_video_url_shape_yields_the_id() {
        for url in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtube.com/watch?feature=share&v=dQw4w9WgXcQ&t=10s",
            "https://m.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ?t=42",
            "https://www.youtube.com/shorts/dQw4w9WgXcQ",
            "https://www.youtube.com/live/dQw4w9WgXcQ?feature=share",
            "https://www.youtube.com/embed/dQw4w9WgXcQ",
        ] {
            assert_eq!(parse_video_id(url).as_deref(), Some("dQw4w9WgXcQ"), "{url}");
        }
    }

    #[test]
    fn non_video_youtube_urls_are_not_videos() {
        for url in [
            "https://www.youtube.com/playlist?list=PLx",
            "https://www.youtube.com/@channel",
            "https://www.youtube.com/results?search_query=x",
            "https://www.youtube.com/watch?v=tooshort",
            "https://www.youtube.com/watch?list=PLx",
            "https://vimeo.com/123",
        ] {
            assert_eq!(parse_video_id(url), None, "{url}");
        }
    }

    #[test]
    fn start_offsets_parse_in_every_spelling() {
        assert_eq!(parse_start_seconds("https://youtu.be/dQw4w9WgXcQ?t=754s"), Some(754));
        assert_eq!(parse_start_seconds("https://youtu.be/dQw4w9WgXcQ?t=754"), Some(754));
        assert_eq!(parse_start_seconds("https://youtu.be/dQw4w9WgXcQ?t=1h2m3s"), Some(3723));
        assert_eq!(parse_start_seconds("https://youtu.be/dQw4w9WgXcQ?start=90"), Some(90));
        assert_eq!(parse_start_seconds("https://youtu.be/dQw4w9WgXcQ"), None);
        assert_eq!(parse_start_seconds("https://youtu.be/dQw4w9WgXcQ?t=abc"), None);
        assert_eq!(watch_url("dQw4w9WgXcQ", Some(754)), "https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=754s");
        assert_eq!(watch_url("dQw4w9WgXcQ", None), "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
    }

    const WATCH_PAGE: &str = r#"<html><script>var ytInitialPlayerResponse = {"captions":{"playerCaptionsTracklistRenderer":{"captionTracks":[{"baseUrl":"https://www.youtube.com/api/timedtext?v=dQw4w9WgXcQ&lang=en&kind=asr","name":{"simpleText":"English (auto-generated)"},"vssId":"a.en","languageCode":"en","kind":"asr","isTranslatable":true},{"baseUrl":"https://www.youtube.com/api/timedtext?v=dQw4w9WgXcQ&lang=ja","name":{"runs":[{"text":"日本語"}]},"vssId":".ja","languageCode":"ja","isTranslatable":true}],"audioTracks":[]}},"videoDetails":{"title":"x [not] a bracket"}};</script></html>"#;

    #[test]
    fn caption_tracks_are_read_out_of_the_watch_page() {
        let tracks = extract_caption_tracks(WATCH_PAGE).unwrap_or_default();
        assert_eq!(tracks.len(), 2);
        assert!(tracks[0].auto_generated);
        assert_eq!(tracks[0].language_code, "en");
        assert!(tracks[0].base_url.contains("&lang=en"), "\\u0026 decoded");
        assert!(!tracks[1].auto_generated);
        assert_eq!(tracks[1].name, "日本語");
    }

    #[test]
    fn a_page_with_a_player_but_no_tracks_means_no_captions_and_anything_else_is_a_shape_change() {
        let no_tracks = r#"<script>var ytInitialPlayerResponse = {"captions":{}};</script>"#;
        assert_eq!(extract_caption_tracks(no_tracks), Err(WebFetchError::NoCaptions));
        assert_eq!(extract_caption_tracks("<html>totally different</html>"), Err(WebFetchError::ShapeChanged));
        assert_eq!(
            extract_caption_tracks("<html>Sign in to confirm you’re not a bot</html>"),
            Err(WebFetchError::Blocked)
        );
        // A key with garbage after it is a shape change, not a panic.
        assert_eq!(extract_caption_tracks(r#""captionTracks":{"oops":1}"#), Err(WebFetchError::ShapeChanged));
    }

    #[test]
    fn track_selection_prefers_a_human_track_in_the_users_language() {
        let tracks = extract_caption_tracks(WATCH_PAGE).unwrap_or_default();
        // Japanese user: the human Japanese track beats the machine English one.
        assert_eq!(select_track(&tracks, "ja-JP").map(|t| t.language_code.as_str()), Some("ja"));
        // English user: only a machine English track exists, and it beats a human track in
        // another language because the user can read it.
        let chosen = select_track(&tracks, "en").unwrap_or(&tracks[1]);
        assert!(chosen.auto_generated && chosen.language_code == "en");
        // German user: nothing in their language — the human track in the video's language.
        assert_eq!(select_track(&tracks, "de").map(|t| t.language_code.as_str()), Some("ja"));
        assert!(select_track(&[], "en").is_none());
    }

    #[test]
    fn the_caption_url_asks_for_json3_exactly_once() {
        let t = CaptionTrack {
            base_url: "https://www.youtube.com/api/timedtext?v=x&lang=en".into(),
            language_code: "en".into(),
            name: String::new(),
            auto_generated: false,
        };
        assert_eq!(caption_url(&t), "https://www.youtube.com/api/timedtext?v=x&lang=en&fmt=json3");
        let already = CaptionTrack { base_url: "https://h/x?fmt=json3".into(), ..t };
        assert_eq!(caption_url(&already), "https://h/x?fmt=json3");
    }

    #[test]
    fn json3_cues_become_segments_and_empty_markers_are_dropped() {
        let body = r#"{"events":[
            {"tStartMs":0,"dDurationMs":2000,"segs":[{"utf8":"Hello"},{"utf8":" world"}]},
            {"tStartMs":2000,"aAppend":1,"segs":[{"utf8":"\n"}]},
            {"tStartMs":2500,"dDurationMs":1000},
            {"tStartMs":3000,"dDurationMs":1000,"segs":[{"utf8":"second\nline"}]}
        ]}"#;
        let segs = parse_json3(body).unwrap_or_default();
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0], Segment { start_s: 0.0, text: "Hello world".into() });
        assert_eq!(segs[1], Segment { start_s: 3.0, text: "second line".into() });
        assert_eq!(parse_json3("not json"), Err(WebFetchError::ShapeChanged));
        assert_eq!(parse_json3(r#"{"nope":[]}"#), Err(WebFetchError::ShapeChanged));
    }

    #[test]
    fn oembed_gives_title_and_channel() {
        let meta = parse_oembed(r#"{"title":"Talk","author_name":"Chan","thumbnail_url":"https://i.ytimg.com/x.jpg"}"#);
        assert_eq!(meta, Some(VideoMeta { title: "Talk".into(), channel: "Chan".into() }));
        assert_eq!(parse_oembed(r#"{"author_name":"Chan"}"#), None);
        assert_eq!(parse_oembed("garbage"), None);
    }
}
