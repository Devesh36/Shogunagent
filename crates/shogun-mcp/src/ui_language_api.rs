//! The app's display language, shared by every face (invariant 6): the desktop's Settings
//! picker, MCP `settings.language.get|set`, REST `/v1/settings/language` and `shogun language`.
//!
//! The document is `<app-data>/ui_language.json` — `{"language": "ja"}`. English is the primary
//! language and the default: a missing, unreadable or unknown value reads as `en`.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::plan_source::ui_language_json_path;

/// Supported display languages, English first (the default).
pub const LANGUAGES: &[&str] = &["en", "ja", "es", "de"];
pub const DEFAULT: &str = "en";

static PROCESS_WRITE_LOCK: Mutex<()> = Mutex::new(());
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The language in force (`en` unless a supported one was chosen).
pub fn current() -> String {
    current_in(ui_language_json_path().as_deref())
}

/// [`current`] from an explicit document (tests name a temp file rather than rewriting the
/// process-global `SHOGUN_UI_LANGUAGE_JSON` override).
fn current_in(path: Option<&Path>) -> String {
    path.and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
        .and_then(|v| v.get("language").and_then(serde_json::Value::as_str).map(normalize))
        .flatten()
        .unwrap_or_else(|| DEFAULT.to_string())
}

/// `{"language": "...", "supported": [...]}`.
pub fn get() -> String {
    render(&current())
}

/// Body `{"language": "en|ja|es|de"}`. Returns the same shape as [`get`].
pub fn set(body: &str) -> Result<String, String> {
    set_in(ui_language_json_path().as_deref(), body)
}

fn set_in(path: Option<&Path>, body: &str) -> Result<String, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| "invalid language request".to_string())?;
    let language = value
        .get("language")
        .and_then(serde_json::Value::as_str)
        .and_then(normalize)
        .ok_or_else(|| format!("language must be one of {}", LANGUAGES.join(", ")))?;
    save(path, &language)?;
    Ok(render(&language))
}

/// A supported language code, case- and region-insensitive (`ja-JP` → `ja`).
pub fn normalize(code: &str) -> Option<String> {
    let base = code.trim().to_ascii_lowercase();
    let base = base.split(['-', '_']).next().unwrap_or("");
    LANGUAGES.contains(&base).then(|| base.to_string())
}

fn render(language: &str) -> String {
    serde_json::json!({ "language": language, "supported": LANGUAGES }).to_string()
}

fn save(path: Option<&Path>, language: &str) -> Result<(), String> {
    let path = path.ok_or("language setting unavailable")?;
    let _process = PROCESS_WRITE_LOCK
        .lock()
        .map_err(|_| "language setting busy".to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("language setting unavailable: {e}"))?;
    }
    let json = serde_json::json!({ "language": language }).to_string();
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = path.with_extension(format!("json.tmp.{}.{}", std::process::id(), sequence));
    std::fs::write(&temporary, json).map_err(|e| format!("save failed: {e}"))?;
    let result = std::fs::rename(&temporary, path).map_err(|e| format!("save failed: {e}"));
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_is_the_default_and_unknown_values_fall_back() {
        let dir = std::env::temp_dir().join(format!("shogun-ui-lang-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("ui_language.json");
        let at = Some(file.as_path());
        let current = || current_in(at);
        let set = |body: &str| set_in(at, body);

        assert_eq!(current(), "en");
        std::fs::write(&file, r#"{"language":"fr"}"#).unwrap();
        assert_eq!(current(), "en");
        std::fs::write(&file, "not json").unwrap();
        assert_eq!(current(), "en");

        let out: serde_json::Value = serde_json::from_str(&set(r#"{"language":"ja-JP"}"#).unwrap()).unwrap();
        assert_eq!(out["language"], "ja");
        assert_eq!(current(), "ja");
        assert!(set(r#"{"language":"fr"}"#).is_err());
        assert!(set(r#"{}"#).is_err());
        assert_eq!(current(), "ja");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn normalizes_region_and_case() {
        assert_eq!(normalize("DE"), Some("de".into()));
        assert_eq!(normalize("es_MX"), Some("es".into()));
        assert_eq!(normalize(" en-US "), Some("en".into()));
        assert_eq!(normalize("zh"), None);
        assert_eq!(normalize(""), None);
    }
}
