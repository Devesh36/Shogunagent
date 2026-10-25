//! What the standalone servers print on stderr when they refuse to start, or start degraded
//! (issue #744).
//!
//! `shogun-mcp` is launched by an MCP client from an entry in that client's config, so the only
//! place a user or a support thread can see why it failed is the client's server log. A refusal
//! is never one fixed sentence: a launcher pointing at an old folder, a missing settings file and
//! the switch really being off must not all read "Memory API is disabled", nor every token problem
//! "must match". These builders name the file that was checked and the specific cause, and
//! never echo a token.
//!
//! Pure: the binaries gather the facts (one file read, the environment) and print the lines. The
//! exit decisions stay exactly where they were.

use std::path::{Path, PathBuf};

use crate::client_config::TOKEN_PLACEHOLDER;
use crate::client_install::{display_path, looks_like_token};
use crate::plan_source::PlanLock;

/// The pre-rename app-data folder of development builds. A launcher still pointing here is the
/// stale setup issue #744 found.
pub const LEGACY_DATA_FOLDER: &str = "dev.shogun.spike";

/// What `memory_api.json` looks like to this process. [`SettingsState::Enabled`] is exactly when
/// [`crate::memory_api_settings::require_enabled`] accepts the file (pinned by tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsState {
    Enabled,
    /// The file exists and the switch is off.
    Disabled,
    /// No file at the resolved path.
    Missing,
    /// The file exists but could not be read.
    Unreadable,
    /// Read, but not settings JSON.
    Malformed,
}

/// Classify the settings file at `path` with one read.
pub fn settings_state(path: &Path) -> SettingsState {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<crate::memory_api_settings::Settings>(&text) {
            Ok(settings) if settings.enabled => SettingsState::Enabled,
            Ok(_) => SettingsState::Disabled,
            Err(_) => SettingsState::Malformed,
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => SettingsState::Missing,
        Err(_) => SettingsState::Unreadable,
    }
}

/// Where the database path came from and whether it looks like the app's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbPathFacts {
    /// `SHOGUN_DB_PATH` was set (otherwise the default `./shogun.db` in the working directory).
    pub env_set: bool,
    /// A component of the path is the legacy development folder.
    pub legacy_folder: bool,
    /// Whether the path is inside the app's data folder; `None` when that folder is unknown here.
    pub in_app_data: Option<bool>,
    /// The path starts with `~` or names a variable (`$HOME`, `${HOME}`) that nothing expanded.
    /// Hand-edited client configs often do this; the process then reads it literally.
    pub unexpanded: bool,
}

impl DbPathFacts {
    pub fn of(db_path: &Path, env_set: bool, app_data_dir: Option<&Path>) -> Self {
        let raw = db_path.to_string_lossy();
        Self {
            env_set,
            legacy_folder: db_path
                .components()
                .any(|c| c.as_os_str() == LEGACY_DATA_FOLDER),
            in_app_data: app_data_dir.map(|dir| db_path.starts_with(dir)),
            unexpanded: env_set && (raw.starts_with('~') || names_a_variable(&raw)),
        }
    }
}

/// `$NAME` or `${…}` anywhere in the value.
fn names_a_variable(value: &str) -> bool {
    value.contains("${")
        || value
            .split('$')
            .skip(1)
            .any(|rest| rest.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_'))
}

fn shown(path: &Path, home: Option<&Path>) -> String {
    match home {
        Some(home) => display_path(path, home),
        None => path.display().to_string(),
    }
}

/// The refusal printed when the settings gate closes. `state` is never [`SettingsState::Enabled`]
/// here. The switch-off wording is kept for the plain cases: the switch really off, or no file yet
/// next to a database path that looks right (the switch was never turned on, so nothing was
/// written). The database path is blamed only when it is suspect: unset, unexpanded, the legacy
/// folder, or outside the app's data folder.
pub fn settings_refusal(
    face: &str,
    state: SettingsState,
    settings_path: &Path,
    db: &DbPathFacts,
    home: Option<&Path>,
) -> String {
    let at = shown(settings_path, home);
    let path_suspect =
        !db.env_set || db.unexpanded || db.legacy_folder || db.in_app_data == Some(false);
    let mut lines = vec![match state {
        SettingsState::Missing if path_suspect => format!(
            "{face}: no Memory API settings at {at}. They sit next to ShogunAI's memory.db, so SHOGUN_DB_PATH must point at that file."
        ),
        SettingsState::Missing => format!(
            "{face}: Memory API is disabled. Enable it in ShogunAI Settings > Memory API. (no settings yet at {at})"
        ),
        SettingsState::Enabled | SettingsState::Disabled => format!(
            "{face}: Memory API is disabled. Enable it in ShogunAI Settings > Memory API. (checked {at})"
        ),
        SettingsState::Unreadable => format!("{face}: could not read the Memory API settings at {at}."),
        SettingsState::Malformed => format!(
            "{face}: the Memory API settings at {at} are not valid. Move that file aside, then turn on the Memory API in ShogunAI Settings > Memory API to write a new one."
        ),
    }];
    if matches!(state, SettingsState::Missing) && !path_suspect && db.in_app_data.is_none() {
        // The app's data folder is unknown here, so the path cannot be vouched for either.
        lines.push(format!(
            "{face}: if it is already on, SHOGUN_DB_PATH may point at a different memory.db than ShogunAI uses."
        ));
    }
    if !db.env_set {
        lines.push(format!(
            "{face}: SHOGUN_DB_PATH is not set, so this process looked in its working directory."
        ));
    }
    if db.unexpanded {
        // Checked first: an unexpanded `~/Library/...` is never inside the data folder, and saying
        // "outside" would send the user away from the right folder.
        lines.push(format!(
            "{face}: SHOGUN_DB_PATH starts with ~ or names a variable such as $HOME, and nothing expanded it, so this process read it literally. Write the full path; the entry from ShogunAI Settings > Memory API > Add to an app has it."
        ));
    } else if db.legacy_folder {
        lines.push(format!(
            "{face}: SHOGUN_DB_PATH points into an old {LEGACY_DATA_FOLDER} folder. This app's entry is out of date: replace it from ShogunAI Settings > Memory API > Add to an app."
        ));
    } else if db.env_set && db.in_app_data == Some(false) {
        lines.push(format!(
            "{face}: SHOGUN_DB_PATH is outside ShogunAI's data folder. Use the entry from ShogunAI Settings > Memory API > Add to an app."
        ));
    }
    lines.join("\n")
}

/// How `SHOGUN_API_TOKEN` arrived, without its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenEnv {
    Absent,
    Empty,
    /// A variable reference the client did not expand, e.g. `${SHOGUN_API_TOKEN}`.
    Unexpanded,
    /// The documentation placeholder, never replaced.
    Placeholder,
    /// Present but not the shape SHOGUN issues.
    NotTokenShaped,
    /// Shaped like a SHOGUN token.
    TokenShaped,
}

pub fn classify_token_env(value: Option<&str>) -> TokenEnv {
    let Some(value) = value else {
        return TokenEnv::Absent;
    };
    let value = value.trim();
    if value.is_empty() {
        TokenEnv::Empty
    } else if value.contains("${") || value.contains("$(") || value.starts_with('$') {
        TokenEnv::Unexpanded
    } else if value == TOKEN_PLACEHOLDER {
        TokenEnv::Placeholder
    } else if looks_like_token(value) {
        TokenEnv::TokenShaped
    } else {
        TokenEnv::NotTokenShaped
    }
}

/// The refusal printed when an entry names no app and its token matches no issued token (or it
/// passes none). Since audit 2026-10-20 M5 this applies with no token issued too.
pub fn token_refusal(face: &str, env: TokenEnv) -> String {
    let why = match env {
        TokenEnv::Absent => "This entry names no app and passes no token, and every entry must do one or the other: in ShogunAI Settings > Memory API, add ShogunAI to this app (the entry it writes names the app).",
        TokenEnv::Empty => "SHOGUN_API_TOKEN is empty. If this app takes it from your login environment, it was not there when the app started (launchctl setenv lasts until you log out).",
        TokenEnv::Unexpanded => "SHOGUN_API_TOKEN holds an unexpanded variable reference. This app does not expand variables in that place; set the variable in the environment the app starts with.",
        TokenEnv::Placeholder => "SHOGUN_API_TOKEN is still the placeholder. Put the token ShogunAI issued for this app there.",
        TokenEnv::NotTokenShaped => "SHOGUN_API_TOKEN does not look like a ShogunAI token. Copy the whole token again from ShogunAI Settings > Memory API.",
        TokenEnv::TokenShaped => "SHOGUN_API_TOKEN matches no token issued on this Mac; it may have been revoked. Issue a new one in ShogunAI Settings > Memory API.",
    };
    format!("{face}: SHOGUN_API_TOKEN must match an issued Memory API token.\n{face}: {why}")
}

/// The refusal printed when the entry names an app (`--client <id>`, issue #963) that is not
/// connected: never added, removed in Settings, or a misspelt id.
pub fn connection_refusal(face: &str, client: &str) -> String {
    let label = crate::memory_api_settings::grant_label(client);
    format!(
        "{face}: ShogunAI is not connected to {label}.\n{face}: in ShogunAI Settings > Memory API, choose Add to {label}. Removing it there disconnects it."
    )
}

/// The refusal printed when an app's connection is checked by code signature (issue #979) and
/// the program that started `shogun-mcp` is not that app. Names no process and no path: only the
/// connection, the cause and the way back.
pub fn launcher_refusal(
    face: &str,
    client: &str,
    refusal: crate::launcher_check::LauncherRefusal,
) -> String {
    use crate::launcher_check::LauncherRefusal;
    let label = crate::memory_api_settings::grant_label(client);
    let why = match refusal {
        LauncherRefusal::WrongSigner => format!("the program that started it is not {label}."),
        LauncherRefusal::NoAppLauncher => {
            format!("it was not started by {label} (for example, it was run from Terminal).")
        }
        LauncherRefusal::Unreadable => {
            "macOS could not confirm which app started it.".to_string()
        }
        LauncherRefusal::Unsupported => "this build cannot check which app started it.".to_string(),
    };
    format!(
        "{face}: refused: {why} Only {label} itself may use its connection.\n{face}: if {label} started it, update {label} and ShogunAI to their latest versions, then in ShogunAI Settings > Memory API choose Remove from {label}, Add to {label} again, and restart {label}."
    )
}

/// The refusal printed when the token list cannot be read. `why` is the loader's own reason
/// (a Keychain status, never secret material).
pub fn tokens_unavailable(face: &str, why: &str) -> String {
    let mut line = format!("{face}: could not load Memory API tokens securely");
    if !why.trim().is_empty() {
        line.push_str(&format!(" ({})", why.trim()));
    }
    line.push_str(&format!(
        ".\n{face}: if macOS asked whether {face} may use your keychain, choose Always Allow and restart this app. The server re-reads its tokens while it runs, so a one-time Allow asks again."
    ));
    line
}

/// Printed once at start-up when the plan locks the Memory API. The server still starts; every
/// tool call answers `plan_required` with the same reason.
pub fn plan_lock_note(face: &str, lock: PlanLock) -> String {
    format!(
        "{face}: the Memory API is locked on this Mac ({}). {} Tool calls answer plan_required until then.",
        lock.code(),
        lock.fix()
    )
}

/// `$HOME`, for showing paths as `~/...`.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_api_settings::require_enabled;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "shogun-startup-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn enabled_is_exactly_what_the_gate_accepts() {
        let dir = temp("gate");
        let path = dir.join("memory_api.json");
        for (text, want) in [
            (None, SettingsState::Missing),
            (Some(r#"{"enabled":true}"#), SettingsState::Enabled),
            (Some(r#"{"enabled":false}"#), SettingsState::Disabled),
            (Some("{}"), SettingsState::Disabled),
            (Some("not json"), SettingsState::Malformed),
            (Some(""), SettingsState::Malformed),
        ] {
            let _ = std::fs::remove_file(&path);
            if let Some(text) = text {
                std::fs::write(&path, text).unwrap();
            }
            let got = settings_state(&path);
            assert_eq!(got, want, "{text:?}");
            assert_eq!(
                got == SettingsState::Enabled,
                require_enabled(&path).is_ok(),
                "{text:?}: same answer as the gate"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_legacy_or_foreign_db_path_is_named_and_the_default_is_explained() {
        let app = Path::new("/Users/me/Library/Application Support/com.syogun.shogunai");
        let home = Path::new("/Users/me");
        let settings =
            Path::new("/Users/me/Library/Application Support/dev.shogun.spike/memory_api.json");

        let legacy = DbPathFacts::of(
            Path::new("/Users/me/Library/Application Support/dev.shogun.spike/memory.db"),
            true,
            Some(app),
        );
        assert!(legacy.legacy_folder);
        let text = settings_refusal(
            "shogun-mcp",
            SettingsState::Missing,
            settings,
            &legacy,
            Some(home),
        );
        assert!(
            text.contains("~/Library/Application Support/dev.shogun.spike/memory_api.json"),
            "{text}"
        );
        assert!(text.contains("old dev.shogun.spike folder"), "{text}");
        assert!(
            !text.contains("/Users/me/"),
            "the user name stays out: {text}"
        );

        let unset = DbPathFacts::of(Path::new("./shogun.db"), false, Some(app));
        let text = settings_refusal(
            "shogun-mcp",
            SettingsState::Missing,
            Path::new("./memory_api.json"),
            &unset,
            Some(home),
        );
        assert!(text.contains("SHOGUN_DB_PATH is not set"), "{text}");

        let foreign = DbPathFacts::of(Path::new("/tmp/other/memory.db"), true, Some(app));
        assert_eq!(foreign.in_app_data, Some(false));
        let text = settings_refusal(
            "shogun-mcp",
            SettingsState::Missing,
            Path::new("/tmp/other/memory_api.json"),
            &foreign,
            Some(home),
        );
        assert!(text.contains("outside ShogunAI's data folder"), "{text}");

        let current = DbPathFacts::of(&app.join("memory.db"), true, Some(app));
        assert_eq!(
            (current.legacy_folder, current.in_app_data, current.unexpanded),
            (false, Some(true), false)
        );
    }

    #[test]
    fn an_unexpanded_db_path_is_named_instead_of_the_switch_or_the_folder() {
        let app = Path::new("/Users/me/Library/Application Support/com.syogun.shogunai");
        for raw in [
            "~/Library/Application Support/com.syogun.shogunai/memory.db",
            "$HOME/Library/Application Support/com.syogun.shogunai/memory.db",
            "${HOME}/Library/Application Support/com.syogun.shogunai/memory.db",
        ] {
            let facts = DbPathFacts::of(Path::new(raw), true, Some(app));
            assert!(facts.unexpanded, "{raw}");
            let settings = Path::new(raw).with_file_name("memory_api.json");
            // Missing: the literal path has no file, and the enabled one sits at the real path.
            let text = settings_refusal("shogun-mcp", SettingsState::Missing, &settings, &facts, None);
            assert!(text.contains("nothing expanded it"), "{text}");
            assert!(!text.contains("Memory API is disabled"), "not the switch: {text}");
            assert!(!text.contains("outside ShogunAI's data folder"), "not the folder: {text}");
        }
        for plain in ["/Users/me/memory.db", "/Users/me/$5 budget/memory.db", "./a~b/memory.db"] {
            assert!(!DbPathFacts::of(Path::new(plain), true, Some(app)).unexpanded, "{plain}");
        }
        // The default path is not the user's doing; it keeps its own line.
        assert!(!DbPathFacts::of(Path::new("~/shogun.db"), false, Some(app)).unexpanded);
    }

    #[test]
    fn the_switch_off_case_keeps_its_wording_and_names_the_file() {
        let facts = DbPathFacts::of(Path::new("/a/memory.db"), true, None);
        let text = settings_refusal(
            "shogun-mcp",
            SettingsState::Disabled,
            Path::new("/a/memory_api.json"),
            &facts,
            None,
        );
        assert!(
            text.starts_with(
                "shogun-mcp: Memory API is disabled. Enable it in ShogunAI Settings > Memory API."
            ),
            "{text}"
        );
        assert!(text.contains("/a/memory_api.json"), "{text}");
        assert_eq!(
            text.lines().count(),
            1,
            "no path hints when the file was found: {text}"
        );
    }

    #[test]
    fn token_problems_are_told_apart_without_echoing_the_value() {
        let real = "shogun_0123456789abcdef0123";
        assert_eq!(classify_token_env(None), TokenEnv::Absent);
        assert_eq!(classify_token_env(Some("  ")), TokenEnv::Empty);
        assert_eq!(
            classify_token_env(Some("${SHOGUN_API_TOKEN}")),
            TokenEnv::Unexpanded
        );
        assert_eq!(
            classify_token_env(Some("${env:SHOGUN_API_TOKEN}")),
            TokenEnv::Unexpanded
        );
        assert_eq!(
            classify_token_env(Some("$SHOGUN_API_TOKEN")),
            TokenEnv::Unexpanded
        );
        assert_eq!(
            classify_token_env(Some(TOKEN_PLACEHOLDER)),
            TokenEnv::Placeholder
        );
        assert_eq!(
            classify_token_env(Some("hunter2")),
            TokenEnv::NotTokenShaped
        );
        assert_eq!(classify_token_env(Some(real)), TokenEnv::TokenShaped);

        for env in [
            TokenEnv::Absent,
            TokenEnv::Empty,
            TokenEnv::Unexpanded,
            TokenEnv::Placeholder,
            TokenEnv::NotTokenShaped,
            TokenEnv::TokenShaped,
        ] {
            let text = token_refusal("shogun-mcp", env);
            assert!(text.starts_with(
                "shogun-mcp: SHOGUN_API_TOKEN must match an issued Memory API token."
            ));
            assert!(
                !looks_like_token(&text) && !text.contains("hunter2"),
                "{text}"
            );
        }
    }

    #[test]
    fn a_missing_file_next_to_the_right_database_means_the_switch_was_never_turned_on() {
        // The app writes memory_api.json only when the switch (or the profile) is saved, and
        // "Add to an app" works before that: the entry is right, the API is just not on.
        let app = Path::new("/Users/me/Library/Application Support/com.syogun.shogunai");
        let right = DbPathFacts::of(&app.join("memory.db"), true, Some(app));
        let text = settings_refusal(
            "shogun-mcp",
            SettingsState::Missing,
            &app.join("memory_api.json"),
            &right,
            Some(Path::new("/Users/me")),
        );
        assert!(text.starts_with("shogun-mcp: Memory API is disabled. Enable it in ShogunAI Settings > Memory API."), "{text}");
        assert!(!text.contains("must point at"), "the path is not blamed: {text}");
        assert_eq!(text.lines().count(), 1, "{text}");

        // Where the app's folder is unknown the path cannot be vouched for: say both.
        let unknown = DbPathFacts::of(Path::new("/srv/shogun/memory.db"), true, None);
        let text = settings_refusal("shogun-mcp", SettingsState::Missing, Path::new("/srv/shogun/memory_api.json"), &unknown, None);
        assert!(text.contains("Memory API is disabled") && text.contains("may point at a different memory.db"), "{text}");
    }

    #[test]
    fn an_out_of_date_path_is_named_whatever_the_file_says() {
        // A legacy folder holding a switched-off or broken file: turning the switch on in the app
        // writes a different file, so the path hint must still appear.
        let legacy = DbPathFacts::of(Path::new("/Users/me/Library/Application Support/dev.shogun.spike/memory.db"), true, None);
        for state in [SettingsState::Disabled, SettingsState::Malformed, SettingsState::Unreadable] {
            let text = settings_refusal("shogun-mcp", state, Path::new("/x/memory_api.json"), &legacy, None);
            assert!(text.contains("old dev.shogun.spike folder"), "{state:?}: {text}");
        }
        let text = settings_refusal("shogun-mcp", SettingsState::Malformed, Path::new("/x/memory_api.json"), &DbPathFacts::of(Path::new("/x/memory.db"), true, None), None);
        assert!(text.contains("Move that file aside"), "{text}");
    }

    #[test]
    fn keychain_and_plan_notes_carry_the_reason() {
        let text = tokens_unavailable("shogun-mcp", "Keychain status -128");
        assert!(
            text.contains("(Keychain status -128)") && text.contains("Always Allow"),
            "{text}"
        );
        assert!(tokens_unavailable("shogun-mcp", "")
            .starts_with("shogun-mcp: could not load Memory API tokens securely."));

        let text = plan_lock_note("shogun-mcp", PlanLock::TrialEnded);
        assert!(
            text.contains("(trial_ended)") && text.contains(PlanLock::TrialEnded.fix()),
            "{text}"
        );
    }

    #[test]
    fn launcher_refusal_names_the_app_the_cause_and_the_way_back() {
        use crate::launcher_check::LauncherRefusal;
        for (refusal, cause) in [
            (LauncherRefusal::WrongSigner, "the program that started it is not Cursor."),
            (LauncherRefusal::NoAppLauncher, "it was not started by Cursor"),
            (LauncherRefusal::Unreadable, "macOS could not confirm which app started it."),
            (LauncherRefusal::Unsupported, "this build cannot check which app started it."),
        ] {
            let text = launcher_refusal("shogun-mcp", "cursor", refusal);
            assert!(text.starts_with("shogun-mcp: refused: "), "{text}");
            assert!(text.contains(cause), "{refusal:?}: {text}");
            assert!(text.contains("Only Cursor itself may use its connection."), "{text}");
            assert!(text.contains("choose Remove from Cursor, Add to Cursor again"), "{text}");
            assert_eq!(text.lines().count(), 2, "{text}");
        }
    }
}
