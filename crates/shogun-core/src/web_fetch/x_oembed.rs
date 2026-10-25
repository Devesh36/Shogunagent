//! One X post, through the vendor's own key-less oEmbed endpoint (FR-WV-08).
//!
//! This is the whole of what SHOGUN fetches from X: a single public post whose URL the user
//! named. There is no search, no timeline and no thread walk here, and no cookie goes with the
//! request — a login-only post is "not public", which is the true answer. Search and timelines
//! are read from the user's own screen (spec §4-4), by code that already exists elsewhere.

use super::html_text::inline_text;

/// `(user, id)` from `x.com/<user>/status/<id>` and its twitter.com / mobile spellings, or the
/// `/i/web/status/<id>` form (user unknown, reported as `i`).
pub fn parse_post_url(url: &str) -> Option<(String, String)> {
    let rest = url.trim().strip_prefix("https://")?;
    let path = rest.split(['?', '#']).next().unwrap_or(rest);
    let (host, path) = path.split_once('/')?;
    if !matches!(
        host.to_ascii_lowercase().as_str(),
        "x.com" | "www.x.com" | "twitter.com" | "www.twitter.com" | "mobile.twitter.com"
    ) {
        return None;
    }
    let segs: Vec<&str> = path.trim_end_matches('/').split('/').collect();
    let (user, id) = match segs.as_slice() {
        [user, "status", id, ..] => (*user, *id),
        ["i", "web", "status", id] => ("i", *id),
        _ => return None,
    };
    let valid_user = !user.is_empty()
        && user.len() <= 15
        && user.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    let valid_id = !id.is_empty() && id.len() <= 25 && id.chars().all(|c| c.is_ascii_digit());
    (valid_user && valid_id).then(|| (user.to_string(), id.to_string()))
}

/// The oEmbed endpoint for a post. `omit_script` keeps the widget JS out of the answer;
/// `dnt` asks the vendor not to track the request.
pub fn oembed_url(user: &str, id: &str) -> String {
    format!(
        "https://publish.twitter.com/oembed?url=https%3A%2F%2Ftwitter.com%2F{user}%2Fstatus%2F{id}&omit_script=true&dnt=true"
    )
}

/// One post as the model sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Post {
    pub author_name: String,
    /// `@handle`, from `author_url`.
    pub author_handle: Option<String>,
    pub text: String,
    /// The date the widget prints ("September 5, 2026"), when present.
    pub posted_at: Option<String>,
    pub url: String,
}

/// Parse the oEmbed answer. The post text lives inside the widget HTML's `<p>`; the date is the
/// last link of the blockquote. `None` when the body is not that shape.
pub fn parse_oembed(json: &str) -> Option<Post> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let html = v.get("html")?.as_str()?;
    let author_name = v
        .get("author_name")
        .and_then(|a| a.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let author_handle = v
        .get("author_url")
        .and_then(|a| a.as_str())
        .and_then(|u| u.trim_end_matches('/').rsplit('/').next())
        .filter(|h| !h.is_empty())
        .map(|h| format!("@{h}"));
    let url = v
        .get("url")
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .to_string();

    let p_start = html.find("<p")?;
    let p_open_end = html[p_start..].find('>')? + p_start + 1;
    let p_end = html[p_open_end..].find("</p>")? + p_open_end;
    let text = inline_text(&html[p_open_end..p_end]);
    if text.trim().is_empty() {
        return None;
    }
    let tail = &html[p_end..];
    let posted_at = tail.rfind("<a").and_then(|a| {
        let open_end = tail[a..].find('>')? + a + 1;
        let close = tail[open_end..].find("</a>")? + open_end;
        let date = inline_text(&tail[open_end..close]);
        (!date.trim().is_empty()).then(|| date.trim().to_string())
    });
    Some(Post {
        author_name,
        author_handle,
        text,
        posted_at,
        url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_urls_parse_in_every_host_spelling() {
        for url in [
            "https://x.com/shogunai/status/1700000000000000000",
            "https://twitter.com/shogunai/status/1700000000000000000?s=20",
            "https://mobile.twitter.com/shogunai/status/1700000000000000000/photo/1",
            "https://www.x.com/shogunai/status/1700000000000000000#m",
        ] {
            assert_eq!(
                parse_post_url(url),
                Some(("shogunai".into(), "1700000000000000000".into())),
                "{url}"
            );
        }
        assert_eq!(parse_post_url("https://x.com/i/web/status/17"), Some(("i".into(), "17".into())));
    }

    #[test]
    fn non_post_urls_are_not_posts() {
        for url in [
            "https://x.com/shogunai",
            "https://x.com/search?q=shogun",
            "https://x.com/home",
            "https://x.com/shogunai/status/notdigits",
            "https://example.com/a/status/1",
        ] {
            assert_eq!(parse_post_url(url), None, "{url}");
        }
    }

    const OEMBED: &str = r#"{"url":"https://twitter.com/shogunai/status/17","author_name":"SHOGUN","author_url":"https://twitter.com/shogunai","html":"<blockquote class=\"twitter-tweet\"><p lang=\"en\" dir=\"ltr\">Shipping today &amp; tomorrow.<br>Second line <a href=\"https://t.co/x\">t.co/x</a></p>&mdash; SHOGUN (@shogunai) <a href=\"https://twitter.com/shogunai/status/17?ref_src=twsrc%5Etfw\">September 5, 2026</a></blockquote>\n","width":550}"#;

    #[test]
    fn the_post_text_author_and_date_come_out_of_the_widget_html() {
        let post = parse_oembed(OEMBED);
        assert_eq!(
            post,
            Some(Post {
                author_name: "SHOGUN".into(),
                author_handle: Some("@shogunai".into()),
                text: "Shipping today & tomorrow.\nSecond line t.co/x".into(),
                posted_at: Some("September 5, 2026".into()),
                url: "https://twitter.com/shogunai/status/17".into(),
            })
        );
    }

    #[test]
    fn a_body_without_the_widget_shape_is_none() {
        assert_eq!(parse_oembed(r#"{"html":"<div>no paragraph</div>"}"#), None);
        assert_eq!(parse_oembed(r#"{"error":"not found"}"#), None);
        assert_eq!(parse_oembed("garbage"), None);
    }
}
