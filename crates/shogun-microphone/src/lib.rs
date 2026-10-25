//! Shared microphone discovery and exact-name selection rules.
//!
//! ShogunAI persists microphone names because CPAL exposes no stable cross-platform device ID.
//! Every selection surface must therefore reject missing and duplicate names before persistence;
//! accepting either would report success for a selection capture cannot resolve later.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

/// One selectable input name and whether more than one live device exposes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputDeviceChoice {
    /// Exact display name persisted in `voice.json`.
    pub name: String,
    /// Whether multiple live inputs expose this same display name.
    pub ambiguous: bool,
}

/// Return the live macOS input list, collapsing duplicate names into one ambiguous choice.
pub fn input_device_choices() -> Result<Vec<InputDeviceChoice>, String> {
    Ok(choices_from_names(platform_input_device_names()?))
}

/// Return display names for live macOS inputs.
pub fn input_device_names() -> Result<Vec<String>, String> {
    Ok(input_device_choices()?
        .into_iter()
        .map(|choice| choice.name)
        .collect())
}

/// Validate a persisted selection against the live macOS input list.
///
/// `None` follows the current system default and requires no named device. A selected name must
/// resolve to exactly one live input.
pub fn validate_selection(selected: Option<&str>) -> Result<(), String> {
    let Some(selected) = selected else {
        return Ok(());
    };
    validate_against_choices(selected, &input_device_choices()?)
}

/// Resolve `selected` to exactly one matching device.
///
/// Missing and duplicate names are errors; neither may silently fall back to another input.
pub fn find_named_device<D>(
    devices: impl Iterator<Item = (String, D)>,
    selected: &str,
) -> Result<D, String> {
    let mut matches = devices
        .filter(|(name, _)| name == selected)
        .map(|(_, device)| device);
    let Some(device) = matches.next() else {
        return Err(unavailable(selected));
    };
    if matches.next().is_some() {
        return Err(ambiguous(selected));
    }
    Ok(device)
}

fn choices_from_names(mut names: Vec<String>) -> Vec<InputDeviceChoice> {
    names.sort_unstable();
    let mut choices: Vec<InputDeviceChoice> = Vec::new();
    for name in names {
        if let Some(previous) = choices.last_mut() {
            if previous.name == name {
                previous.ambiguous = true;
                continue;
            }
        }
        choices.push(InputDeviceChoice {
            name,
            ambiguous: false,
        });
    }
    choices
}

fn validate_against_choices(selected: &str, choices: &[InputDeviceChoice]) -> Result<(), String> {
    let Some(choice) = choices.iter().find(|choice| choice.name == selected) else {
        return Err(unavailable(selected));
    };
    if choice.ambiguous {
        return Err(ambiguous(selected));
    }
    Ok(())
}

fn unavailable(selected: &str) -> String {
    format!("selected microphone is unavailable: {selected}")
}

fn ambiguous(selected: &str) -> String {
    format!("selected microphone name is ambiguous: {selected}; choose a different input")
}

#[cfg(feature = "desktop")]
mod platform;

#[cfg(feature = "desktop")]
use platform::platform_input_device_names;

/// Without the `desktop` feature there is no device list to read: the same answer the Mac-only
/// reader gives off macOS.
#[cfg(not(feature = "desktop"))]
fn platform_input_device_names() -> Result<Vec<String>, String> {
    Err(UNAVAILABLE_OFF_MACOS.to_string())
}

/// What every face answers when it cannot list inputs (a server, Linux, a non-desktop build).
pub const UNAVAILABLE_OFF_MACOS: &str = "microphone device validation is available only on macOS";

#[cfg(test)]
mod tests {
    use super::*;

    fn choices(names: &[&str]) -> Vec<InputDeviceChoice> {
        choices_from_names(names.iter().map(|name| (*name).to_string()).collect())
    }

    #[test]
    fn exact_selection_is_accepted() {
        let choices = choices(&["Built-in Microphone", "Studio Mic"]);

        assert_eq!(validate_against_choices("Studio Mic", &choices), Ok(()));
    }

    #[test]
    fn unavailable_selection_is_rejected() {
        let choices = choices(&["Built-in Microphone"]);

        let error = validate_against_choices("Studio Mic", &choices)
            .expect_err("missing selection must fail before persistence");
        assert!(error.contains("unavailable"), "unexpected error: {error}");
    }

    #[test]
    fn duplicate_selection_is_rejected() {
        let choices = choices(&["Studio Mic", "Studio Mic"]);

        let error = validate_against_choices("Studio Mic", &choices)
            .expect_err("ambiguous selection must fail before persistence");
        assert!(error.contains("ambiguous"), "unexpected error: {error}");
    }

    #[test]
    fn duplicate_names_collapse_into_one_ambiguous_choice() {
        assert_eq!(
            choices(&["Studio Mic", "Built-in Microphone", "Studio Mic"]),
            vec![
                InputDeviceChoice {
                    name: "Built-in Microphone".to_string(),
                    ambiguous: false,
                },
                InputDeviceChoice {
                    name: "Studio Mic".to_string(),
                    ambiguous: true,
                },
            ]
        );
    }

    #[test]
    fn default_selection_needs_no_platform_device_list() {
        assert_eq!(validate_selection(None), Ok(()));
    }
}
