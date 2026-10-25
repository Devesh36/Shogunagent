//! Shared MCP/REST helpers for the translation preferences (issue #279): the register (FR-TL-07,
//! the politeness level a draft is written in, remembered per app) and the language pair
//! (FR-TL-03, `translate.pair.get` / `translate.pair.set`).
//!
//! The document is the desktop's `translate.json` — the same file the Reply draft panel's
//! register chips write (invariant 6: one store, two faces). This crate cannot depend on
//! shogun-core, so the document's two rules are restated here and pinned by tests against the
//! core's `translate_prefs`: a per-app entry wins over the default, and choosing `match` for an
//! app forgets its entry rather than storing "match" beside it.

use std::fs::{File, OpenOptions};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use crate::plan_source::translate_json_path;

const LOCK_RETRIES: usize = 200;
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(10);
static PROCESS_WRITE_LOCK: Mutex<()> = Mutex::new(());
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The registers, as `translate.json` and the wire spell them.
pub const REGISTERS: &[&str] = &["match", "casual", "polite", "formal"];

struct SettingsFileLock(File);

impl Drop for SettingsFileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::flock(std::os::fd::AsRawFd::as_raw_fd(&self.0), libc::LOCK_UN);
        }
    }
}

/// The whole document: `{"default_formality": "...", "per_app": {bundle_id: register}}`.
pub fn get() -> String {
    render(&load())
}

/// Set the register for one app (`app`), or the default when `app` is absent or null. The body
/// is `{"formality": "match|casual|polite|formal", "app": "bundle.id"?}`. Returns the document.
pub fn set(body: &str) -> Result<String, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| "invalid translate register request".to_string())?;
    let formality = value
        .get("formality")
        .and_then(serde_json::Value::as_str)
        .map(|f| f.trim().to_ascii_lowercase())
        .filter(|f| REGISTERS.contains(&f.as_str()))
        .ok_or_else(|| "formality must be one of match, casual, polite or formal".to_string())?;
    let app = match value.get("app") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(id)) if !id.trim().is_empty() => Some(id.trim().to_string()),
        _ => return Err("app must be a bundle id or null".to_string()),
    };
    let document = update(|document| {
        apply(document, app.as_deref(), &formality);
        Ok(())
    })?;
    Ok(render(&document))
}

/// The languages there are prompts for (mirrors `shogun_core::translate::Language`).
pub const PAIR_LANGUAGES: &[&str] = &["en", "ja"];

/// `en-US`, `ja_JP`, ` JA ` → `en` / `ja`; anything else is `None`.
fn language_tag(tag: &str) -> Option<&'static str> {
    let head = tag.trim().split(['-', '_']).next().unwrap_or("").to_ascii_lowercase();
    PAIR_LANGUAGES.iter().copied().find(|l| *l == head)
}

/// The stored pair, when there is one and it is still a valid pair (mirrors
/// `shogun_core::translate_prefs::TranslatePrefs::stored_pair`).
fn stored_pair(document: &serde_json::Value) -> Option<(&'static str, &'static str)> {
    let pair = document.get("pair")?;
    let mine = language_tag(pair.get("mine")?.as_str()?)?;
    let theirs = language_tag(pair.get("theirs")?.as_str()?)?;
    (mine != theirs).then_some((mine, theirs))
}

fn render_pair(document: &serde_json::Value) -> String {
    let pair = stored_pair(document).map(|(mine, theirs)| serde_json::json!({ "mine": mine, "theirs": theirs }));
    serde_json::json!({
        "follows_system": pair.is_none(),
        "pair": pair,
        "languages": PAIR_LANGUAGES,
    })
    .to_string()
}

/// The language pair (FR-TL-03): `{"pair": {"mine", "theirs"} | null, "follows_system", "languages"}`.
/// `null` means the desktop derives it from the system language, which only it can read.
pub fn pair_get() -> String {
    render_pair(&load())
}

/// Choose the pair — `{"mine": "ja", "theirs"?: "en"}` (`theirs` defaults to the other language)
/// — or go back to following the system language with `{"mine": null}` or `{"reset": true}`.
/// The same `translate.json` the desktop's Settings row writes; local and reversible (L1).
pub fn pair_set(body: &str) -> Result<String, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| "invalid translate pair request".to_string())?;
    let reset = value.get("reset").and_then(serde_json::Value::as_bool).unwrap_or(false)
        || matches!(value.get("mine"), Some(serde_json::Value::Null));
    let chosen = if reset {
        None
    } else {
        let mine = value
            .get("mine")
            .and_then(serde_json::Value::as_str)
            .and_then(language_tag)
            .ok_or_else(|| "mine must be en or ja (or null to follow the system language)".to_string())?;
        let theirs = match value.get("theirs") {
            None | Some(serde_json::Value::Null) => PAIR_LANGUAGES
                .iter()
                .copied()
                .find(|l| *l != mine)
                .ok_or_else(|| "no other language".to_string())?,
            Some(serde_json::Value::String(tag)) => {
                language_tag(tag).ok_or_else(|| "theirs must be en or ja".to_string())?
            }
            Some(_) => return Err("theirs must be en or ja".to_string()),
        };
        if mine == theirs {
            return Err("mine and theirs must be different languages".to_string());
        }
        Some((mine, theirs))
    };
    let document = update(|document| {
        apply_pair(document, chosen);
        Ok(())
    })?;
    Ok(render_pair(&document))
}

fn apply_pair(document: &mut serde_json::Value, pair: Option<(&str, &str)>) {
    if !document.is_object() {
        *document = serde_json::json!({});
    }
    let Some(object) = document.as_object_mut() else {
        return;
    };
    match pair {
        Some((mine, theirs)) => {
            object.insert("pair".into(), serde_json::json!({ "mine": mine, "theirs": theirs }));
        }
        None => {
            object.remove("pair");
        }
    }
}

/// The document's two rules (mirrors `shogun_core::translate_prefs::TranslatePrefs::remember`).
fn apply(document: &mut serde_json::Value, app: Option<&str>, formality: &str) {
    if !document.is_object() {
        *document = serde_json::json!({});
    }
    let Some(object) = document.as_object_mut() else {
        return;
    };
    match app {
        None => {
            object.insert("default_formality".into(), serde_json::Value::String(formality.into()));
        }
        Some(app) => {
            let per_app = object
                .entry("per_app")
                .or_insert_with(|| serde_json::json!({}));
            if !per_app.is_object() {
                *per_app = serde_json::json!({});
            }
            let Some(per_app) = per_app.as_object_mut() else {
                return;
            };
            if formality == "match" {
                per_app.remove(app);
            } else {
                per_app.insert(app.into(), serde_json::Value::String(formality.into()));
            }
        }
    }
}

fn render(document: &serde_json::Value) -> String {
    let default_formality = document
        .get("default_formality")
        .and_then(serde_json::Value::as_str)
        .filter(|f| REGISTERS.contains(f))
        .unwrap_or("match");
    let per_app: serde_json::Map<String, serde_json::Value> = document
        .get("per_app")
        .and_then(serde_json::Value::as_object)
        .map(|m| {
            m.iter()
                .filter(|(_, v)| v.as_str().is_some_and(|f| REGISTERS.contains(&f)))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    serde_json::json!({ "default_formality": default_formality, "per_app": per_app }).to_string()
}

fn load() -> serde_json::Value {
    translate_json_path()
        .map(|path| load_at(&path))
        .unwrap_or_else(|| serde_json::json!({}))
}

fn load_at(path: &Path) -> serde_json::Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|json| serde_json::from_str(&json).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}))
}

/// Read-patch-write under `translate.json.lock`, the flock protocol the desktop's register
/// chips also take, so neither face clobbers the other's change.
fn update(
    patch: impl FnOnce(&mut serde_json::Value) -> Result<(), String>,
) -> Result<serde_json::Value, String> {
    let path = translate_json_path().ok_or("translate settings unavailable")?;
    let _process = PROCESS_WRITE_LOCK
        .lock()
        .map_err(|_| "translate settings busy".to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("translate settings unavailable: {e}"))?;
    }
    let _file = acquire_file_lock(&path)?;
    let mut document = load_at(&path);
    patch(&mut document)?;
    let json = serde_json::to_string_pretty(&document).map_err(|e| e.to_string())?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = path.with_extension(format!("json.tmp.{}.{}", std::process::id(), sequence));
    std::fs::write(&temporary, json).map_err(|e| format!("save failed: {e}"))?;
    if let Err(e) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(temporary);
        return Err(format!("save failed: {e}"));
    }
    Ok(document)
}

fn acquire_file_lock(path: &Path) -> Result<SettingsFileLock, String> {
    let lock_path = path.with_extension("json.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .map_err(|error| format!("translate settings unavailable: {error}"))?;
    #[cfg(unix)]
    for _ in 0..LOCK_RETRIES {
        if unsafe {
            libc::flock(
                std::os::fd::AsRawFd::as_raw_fd(&file),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        } == 0
        {
            return Ok(SettingsFileLock(file));
        }
        std::thread::sleep(LOCK_RETRY_DELAY);
    }
    #[cfg(not(unix))]
    return Ok(SettingsFileLock(file));
    #[cfg(unix)]
    Err("translate settings busy".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_app_entry_wins_and_match_forgets_it() {
        let mut d = serde_json::json!({});
        apply(&mut d, None, "polite");
        apply(&mut d, Some("com.tinyspeck.slackmacgap"), "casual");
        assert_eq!(
            render(&d),
            r#"{"default_formality":"polite","per_app":{"com.tinyspeck.slackmacgap":"casual"}}"#
        );
        apply(&mut d, Some("com.tinyspeck.slackmacgap"), "match");
        assert_eq!(render(&d), r#"{"default_formality":"polite","per_app":{}}"#);
    }

    #[test]
    fn unknown_registers_are_dropped_on_read_not_passed_through() {
        let d = serde_json::json!({ "default_formality": "shouty", "per_app": { "a": "formal", "b": 3 } });
        assert_eq!(render(&d), r#"{"default_formality":"match","per_app":{"a":"formal"}}"#);
    }

    #[test]
    fn the_pair_is_written_beside_the_register_and_reset_removes_only_it() {
        let mut d = serde_json::json!({ "default_formality": "polite" });
        assert_eq!(
            render_pair(&d),
            r#"{"follows_system":true,"languages":["en","ja"],"pair":null}"#
        );
        apply_pair(&mut d, Some(("en", "ja")));
        assert_eq!(stored_pair(&d), Some(("en", "ja")));
        assert!(render_pair(&d).contains(r#""follows_system":false"#));
        // The register still reads the same, and a register write keeps the pair.
        apply(&mut d, Some("com.apple.mail"), "formal");
        assert_eq!(stored_pair(&d), Some(("en", "ja")));
        assert!(render(&d).contains(r#""default_formality":"polite""#));
        apply_pair(&mut d, None);
        assert_eq!(stored_pair(&d), None);
        assert!(render(&d).contains("com.apple.mail"));
    }

    #[test]
    fn a_bad_stored_pair_reads_as_following_the_system() {
        for bad in [
            serde_json::json!({ "pair": { "mine": "ja", "theirs": "ja" } }),
            serde_json::json!({ "pair": { "mine": "fr", "theirs": "en" } }),
            serde_json::json!({ "pair": "ja" }),
        ] {
            assert_eq!(stored_pair(&bad), None, "{bad}");
        }
        assert_eq!(language_tag(" ja_JP "), Some("ja"));
        assert_eq!(language_tag("EN-us"), Some("en"));
    }

    #[test]
    fn pair_set_rejects_what_it_cannot_store() {
        assert!(pair_set(r#"{"mine":"fr"}"#).is_err());
        assert!(pair_set(r#"{"mine":"ja","theirs":"ja"}"#).is_err());
        assert!(pair_set(r#"{"mine":"ja","theirs":3}"#).is_err());
        assert!(pair_set(r#"{}"#).is_err());
        assert!(pair_set("nonsense").is_err());
    }

    #[test]
    fn set_rejects_a_register_outside_the_four() {
        assert!(set(r#"{"formality":"shouty"}"#).is_err());
        assert!(set(r#"{"formality":"polite","app":7}"#).is_err());
        assert!(set("nonsense").is_err());
    }
}
