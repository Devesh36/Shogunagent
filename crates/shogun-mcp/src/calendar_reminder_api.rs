//! Shared calendar reminder preference for desktop, MCP, REST, and CLI.
use crate::meeting_microphone_api::{load, update};

pub fn minutes_from(value: &serde_json::Value) -> u8 {
    match value.get("calendar_reminder_minutes") {
        None => 1,
        Some(value) => value
            .as_u64()
            .filter(|minutes| matches!(minutes, 0 | 1 | 5 | 10))
            .map_or(0, |minutes| minutes as u8),
    }
}

pub fn get() -> String {
    serde_json::json!({"minutes": minutes_from(&load())}).to_string()
}

pub fn parse_minutes(body: &str) -> Result<u8, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| "invalid reminder settings")?;
    value
        .get("minutes")
        .and_then(serde_json::Value::as_u64)
        .filter(|minutes| matches!(minutes, 0 | 1 | 5 | 10))
        .map(|minutes| minutes as u8)
        .ok_or_else(|| "minutes must be 0, 1, 5, or 10".into())
}

pub fn set(body: &str) -> Result<String, String> {
    let minutes = parse_minutes(body)?;
    update(|settings| {
        settings
            .as_object_mut()
            .ok_or("meeting settings unavailable")?
            .insert("calendar_reminder_minutes".into(), minutes.into());
        Ok(())
    })?;
    Ok(serde_json::json!({"minutes": minutes}).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_and_valid_options_are_shared() {
        assert_eq!(minutes_from(&serde_json::json!({})), 1);
        for minutes in [0, 1, 5, 10] {
            assert_eq!(
                parse_minutes(&serde_json::json!({"minutes":minutes}).to_string()).unwrap(),
                minutes
            );
            assert_eq!(
                minutes_from(&serde_json::json!({"calendar_reminder_minutes":minutes})),
                minutes
            );
        }
        for body in [
            "{}",
            "null",
            "{\"minutes\":2}",
            "{\"minutes\":-1}",
            "{\"minutes\":\"5\"}",
        ] {
            assert!(parse_minutes(body).is_err());
        }
        assert_eq!(
            minutes_from(&serde_json::json!({"calendar_reminder_minutes":2})),
            0
        );
    }
}
