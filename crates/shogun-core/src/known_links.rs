//! The fixed set of web pages the desktop app may open in the browser (issue #778).
//!
//! The webview cannot follow a `target="_blank"` link, so every external page goes through one
//! Tauri command that takes a *key*, never a URL. This table is the whole allowlist: the webview
//! cannot widen it, and a key it does not know opens nothing. Keep entries https-only and on the
//! hosts the test below names.

/// Every page the desktop may open, by the key the webview passes.
pub const KNOWN_LINKS: &[(&str, &str)] = &[
    ("website", "https://shogunai.si"),
    ("github_device", "https://github.com/login/device"),
    // Settings › Privacy & Security: the full privacy policy behind the data-use badges.
    ("privacy_policy", "https://shogunai.si/privacy"),
    // Settings › Help: the installer fallback when in-app update does not work.
    ("download", "https://shogunai.si/download"),
    // Settings › Help: current setup requirements and limitations.
    ("known_issues", "https://shogunai.si/known-issues"),
    // Settings › Meeting notes: where a speech transcription key is created (BYOK, 2026-09-01).
    ("speech_key", "https://console.deepgram.com/"),
];

/// The URL for `key`, or `None` for anything outside the table.
pub fn url_for(key: &str) -> Option<&'static str> {
    KNOWN_LINKS
        .iter()
        .find(|(known, _)| *known == key)
        .map(|(_, url)| *url)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hosts an entry may point at. Adding a page on a new host is a deliberate edit here.
    const ALLOWED_HOSTS: &[&str] = &["shogunai.si", "console.deepgram.com", "github.com"];

    fn host(url: &str) -> &str {
        let rest = url.strip_prefix("https://").unwrap_or("");
        rest.split(['/', '?', '#']).next().unwrap_or("")
    }

    #[test]
    fn every_link_is_https_on_an_expected_host() {
        for (key, url) in KNOWN_LINKS {
            assert!(url.starts_with("https://"), "{key} is not https: {url}");
            assert!(
                ALLOWED_HOSTS.contains(&host(url)),
                "{key} points at an unexpected host: {url}"
            );
            assert!(
                !url.contains(char::is_whitespace),
                "{key} has whitespace: {url}"
            );
            assert!(!url.contains('@'), "{key} carries credentials: {url}");
        }
    }

    #[test]
    fn keys_are_unique_snake_case() {
        let mut seen = std::collections::HashSet::new();
        for (key, _) in KNOWN_LINKS {
            assert!(seen.insert(*key), "duplicate key {key}");
            assert!(
                key.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "key {key} is not snake_case"
            );
        }
    }

    #[test]
    fn only_known_keys_resolve() {
        assert_eq!(
            url_for("privacy_policy"),
            Some("https://shogunai.si/privacy")
        );
        assert_eq!(url_for("speech_key"), Some("https://console.deepgram.com/"));
        assert_eq!(url_for("https://example.com"), None);
        assert_eq!(url_for(""), None);
        assert_eq!(url_for("PRIVACY_POLICY"), None);
    }
}
