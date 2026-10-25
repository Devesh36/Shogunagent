//! Shared MCP/REST helpers for the Recap preferences: output language and the custom minutes
//! instruction (docs/meeting-gap-analysis-2026-09.md B3 / B5, invariant 6).
//!
//! Same file, same lock protocol as the microphone setting (`meeting.json`, read-patch-write
//! under `meeting.json.lock`), so a preference set from the CLI is the one the desktop's minutes
//! generator reads on the next Recap. Keys are the `Settings` serde names in shogun-core
//! (`recap_language`, `minutes_instruction`); this crate cannot depend on shogun-core, so the
//! accepted values and the length ceiling are restated here and pinned by tests on both sides.

use crate::meeting_microphone_api::{load, update};

/// The wire values of `RecapLanguage` (shogun-core `meeting::settings`), lowercase like serde.
pub const RECAP_LANGUAGES: &[&str] = &["auto", "english", "japanese"];
/// Mirrors `shogun_core::meeting::minutes::MAX_INSTRUCTION_CHARS`.
pub const MAX_INSTRUCTION_CHARS: usize = 2_000;

fn render(settings: &serde_json::Value) -> String {
    let recap_language = settings
        .get("recap_language")
        .and_then(serde_json::Value::as_str)
        .filter(|v| RECAP_LANGUAGES.contains(v))
        .unwrap_or("auto");
    let minutes_instruction = settings
        .get("minutes_instruction")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.chars().count() <= MAX_INSTRUCTION_CHARS)
        .map(str::to_string);
    serde_json::json!({
        "recap_language": recap_language,
        "minutes_instruction": minutes_instruction,
    })
    .to_string()
}

/// `meeting.recap_prefs.get`: `{ recap_language, minutes_instruction }`. Unset or unreadable
/// values read as the shipped defaults (`auto`, `null`), never as an error.
pub fn get() -> String {
    render(&load())
}

/// `meeting.recap_prefs.set`: patch either field. `recap_language` must be one of
/// [`RECAP_LANGUAGES`]; `minutes_instruction` is a string (trimmed; blank clears) or `null`
/// (clears), at most [`MAX_INSTRUCTION_CHARS`] characters. A request that names neither is
/// refused rather than silently succeeding.
pub fn set(body: &str) -> Result<String, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| "invalid recap preferences request".to_string())?;
    let language = match value.get("recap_language") {
        None => None,
        Some(serde_json::Value::String(raw)) => {
            let lower = raw.trim().to_ascii_lowercase();
            if !RECAP_LANGUAGES.contains(&lower.as_str()) {
                return Err(format!(
                    "recap_language must be one of {}",
                    RECAP_LANGUAGES.join(" / ")
                ));
            }
            Some(lower)
        }
        Some(_) => return Err("recap_language must be a string".to_string()),
    };
    let instruction = match value.get("minutes_instruction") {
        None => None,
        Some(serde_json::Value::Null) => Some(None),
        Some(serde_json::Value::String(raw)) => {
            let trimmed = raw.trim();
            if trimmed.chars().count() > MAX_INSTRUCTION_CHARS {
                return Err(format!(
                    "minutes_instruction is too long (max {MAX_INSTRUCTION_CHARS} characters)"
                ));
            }
            Some((!trimmed.is_empty()).then(|| trimmed.to_string()))
        }
        Some(_) => return Err("minutes_instruction must be a string or null".to_string()),
    };
    if language.is_none() && instruction.is_none() {
        return Err("nothing to set: give recap_language and/or minutes_instruction".to_string());
    }
    let mut after = serde_json::Value::Null;
    update(|settings| {
        let object = settings
            .as_object_mut()
            .ok_or_else(|| "meeting settings are malformed".to_string())?;
        if let Some(language) = &language {
            object.insert(
                "recap_language".to_string(),
                serde_json::Value::String(language.clone()),
            );
        }
        if let Some(instruction) = &instruction {
            object.insert(
                "minutes_instruction".to_string(),
                instruction
                    .clone()
                    .map(serde_json::Value::String)
                    .unwrap_or(serde_json::Value::Null),
            );
        }
        after = settings.clone();
        Ok(())
    })?;
    Ok(render(&after))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_and_junk_values_read_as_the_defaults() {
        let v: serde_json::Value = serde_json::from_str(&render(&serde_json::json!({}))).unwrap();
        assert_eq!(v["recap_language"], "auto");
        assert!(v["minutes_instruction"].is_null());
        let junk = serde_json::json!({ "recap_language": "klingon", "minutes_instruction": "   " });
        let v: serde_json::Value = serde_json::from_str(&render(&junk)).unwrap();
        assert_eq!(v["recap_language"], "auto");
        assert!(v["minutes_instruction"].is_null());
    }

    #[test]
    fn a_set_request_is_validated_before_any_file_is_touched() {
        // Each of these fails at validation, before `update` (which needs the app-data path).
        assert!(set(r#"{"recap_language":"klingon"}"#).is_err());
        assert!(set(r#"{"recap_language":7}"#).is_err());
        assert!(set(r#"{"minutes_instruction":7}"#).is_err());
        assert!(
            set("{}").is_err(),
            "nothing to set must not read as success"
        );
        let huge = format!(
            r#"{{"minutes_instruction":"{}"}}"#,
            "x".repeat(MAX_INSTRUCTION_CHARS + 1)
        );
        assert!(set(&huge).unwrap_err().contains("too long"));
        assert!(set("not json").is_err());
    }
}
