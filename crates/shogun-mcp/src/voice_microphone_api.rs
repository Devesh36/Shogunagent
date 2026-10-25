//! Shared desktop/MCP/REST/CLI helpers for the dictation microphone setting (issue #246).
//!
//! `voice.json` holds more than this one field (`enabled`, the personal-dictionary egress
//! consent), and four faces write it. So every write here is a **partial merge**: the file is
//! loaded as raw JSON and only `microphone` is replaced. Deserializing into a typed struct and
//! writing it back would silently erase whatever key that struct does not know about — which is
//! exactly how an API-face write would clobber the desktop's voice settings.
//!
//! `null` is the explicit "follow the macOS default input" reset. A blank, unavailable, or
//! ambiguous name is rejected before persistence on every face. Live discovery lives in the
//! lightweight `shogun-microphone` crate so MCP/REST/CLI share desktop capture's exact-name rules
//! without pulling CPAL or ALSA into Linux builds; since E25 that crate is only in the `desktop`
//! build, and the server answers a named selection as unavailable.

#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::Path;

use crate::plan_source::voice_json_path;

const KEY: &str = "microphone";

/// Run a read-merge-write of `voice.json` under the cross-process advisory lock: reload the file,
/// apply `mutate` to its root object, atomically replace it, and return the merged document.
///
/// Every mutation of the file — desktop, MCP, REST, CLI — must go through here. An unlocked
/// read-merge-write lets one process resurrect its stale snapshot over another's update (the
/// desktop LANE mutex cannot see the other processes).
pub fn update(
    mutate: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let path = voice_json_path().ok_or("voice settings unavailable")?;
    update_at(&path, mutate)
}

/// The persisted selection, or `None` when the default input is in use.
pub fn selected() -> Option<String> {
    voice_json_path().as_deref().and_then(selected_at)
}

/// `GET` body for every face: the current selection.
pub fn get() -> String {
    serde_json::json!({ KEY: selected() }).to_string()
}

/// Apply a request body (`{"microphone": "Studio Mic"}` or `{"microphone": null}`).
pub fn set(body: &str) -> Result<String, String> {
    store(parse(body)?)
}

/// Persist a selection, leaving every other voice setting untouched. `None` resets to the macOS
/// default input.
pub fn store(microphone: Option<String>) -> Result<String, String> {
    let microphone = validated(microphone)?;
    let path = voice_json_path().ok_or("voice settings unavailable")?;
    store_validated_at(&path, microphone, validate_selection)
}

/// Check a selection against the live input list. The Mac app's build (`desktop`) asks CoreAudio
/// through `shogun-microphone`; the server build carries no device discovery (E25) and answers
/// what that crate answers off macOS: the default input is fine, a named device cannot be
/// confirmed.
#[cfg(feature = "desktop")]
fn validate_selection(selected: Option<&str>) -> Result<(), String> {
    shogun_microphone::validate_selection(selected)
}

#[cfg(not(feature = "desktop"))]
fn validate_selection(selected: Option<&str>) -> Result<(), String> {
    match selected {
        None => Ok(()),
        Some(_) => Err(UNAVAILABLE_OFF_MACOS.to_string()),
    }
}

/// Same text as `shogun_microphone::UNAVAILABLE_OFF_MACOS`, so a server and a Linux desktop build
/// fail a named selection identically.
#[cfg(not(feature = "desktop"))]
const UNAVAILABLE_OFF_MACOS: &str = "microphone device validation is available only on macOS";

/// Parse a request body into a selection. Anything other than a string or `null` is a hard error
/// on every face.
pub fn parse(body: &str) -> Result<Option<String>, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| "invalid voice microphone request".to_string())?;
    match value.get(KEY) {
        Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(name)) => Ok(Some(name.clone())),
        _ => Err("voice microphone must be a device name or null".to_string()),
    }
}

/// A device name must actually name something. Blank input is a malformed request, not a request
/// for the default input — that one is spelled `null`.
fn validated(microphone: Option<String>) -> Result<Option<String>, String> {
    match microphone {
        Some(name) if name.trim().is_empty() => {
            Err("voice microphone name is empty; use null for the default input".to_string())
        }
        other => Ok(other),
    }
}

fn selected_at(path: &Path) -> Option<String> {
    load_at(path)
        .ok()?
        .get(KEY)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn store_at(path: &Path, microphone: Option<String>) -> Result<String, String> {
    let value = microphone
        .clone()
        .map(serde_json::Value::String)
        .unwrap_or(serde_json::Value::Null);
    update_at(path, |settings| {
        settings.insert(KEY.to_string(), value);
    })?;
    Ok(serde_json::json!({ KEY: microphone }).to_string())
}

fn store_validated_at(
    path: &Path,
    microphone: Option<String>,
    validate: impl FnOnce(Option<&str>) -> Result<(), String>,
) -> Result<String, String> {
    validate(microphone.as_deref())?;
    store_at(path, microphone)
}

fn update_at(
    path: &Path,
    mutate: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let _lock = lock_at(path)?;
    let mut settings = load_at(path)?;
    mutate(&mut settings);
    save_at(path, &settings)?;
    Ok(settings)
}

/// The held `voice.json` lock: the flock'd file on unix. `flock` does not exist off unix
/// (Windows dev builds must still compile — warnings are denied), so there a process-wide mutex
/// keeps in-process read-merge-writes atomic while the held file handle is the best-effort
/// cross-process signal — the same convention as `approval_store::StoreLock`.
struct VoiceSettingsLock {
    _file: std::fs::File,
    #[cfg(not(unix))]
    _guard: std::sync::MutexGuard<'static, ()>,
}

/// Take the exclusive advisory lock for `voice.json` mutations; released when the returned lock
/// drops. The lock lives on a stable sibling (`voice.json.lock`) rather than the settings file
/// itself, because the atomic-rename replace swaps the settings inode out from under any waiter.
fn lock_at(path: &Path) -> Result<VoiceSettingsLock, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("voice settings unavailable: {e}"))?;
    }
    #[cfg(not(unix))]
    let guard = {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    };
    let lock = std::fs::File::create(path.with_extension("json.lock"))
        .map_err(|e| format!("voice settings unavailable: {e}"))?;
    #[cfg(unix)]
    while unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(format!("voice settings lock failed: {error}"));
        }
    }
    Ok(VoiceSettingsLock {
        _file: lock,
        #[cfg(not(unix))]
        _guard: guard,
    })
}

/// The settings document to merge into. Only a missing file is a fresh start; every other failure
/// (unreadable, unparseable, non-object root) must abort the write — folding those into `{}`
/// would let the next save replace whatever the user had with a microphone-only object.
fn load_at(path: &Path) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let json = match std::fs::read_to_string(path) {
        Ok(json) => json,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(serde_json::Map::new())
        }
        Err(error) => return Err(format!("voice settings unreadable: {error}")),
    };
    match serde_json::from_str(&json) {
        Ok(serde_json::Value::Object(settings)) => Ok(settings),
        Ok(_) => Err("voice settings are malformed: not a JSON object".to_string()),
        Err(error) => Err(format!("voice settings are malformed: {error}")),
    }
}

fn save_at(path: &Path, settings: &serde_json::Map<String, serde_json::Value>) -> Result<(), String> {
    let json = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    // The lock serializes writers going through update_at; the pid-unique name keeps a writer
    // that predates the lock from clobbering or renaming someone else's temporary file.
    let temporary = path.with_extension(format!("json.tmp.{}", std::process::id()));
    std::fs::write(&temporary, json).map_err(|e| format!("save failed: {e}"))?;
    std::fs::rename(&temporary, path).map_err(|e| format!("save failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("shogun-voice-mic-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{name}.json"));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn api_write_preserves_the_desktop_only_voice_settings() {
        let path = scratch("preserve");
        std::fs::write(
            &path,
            r#"{"enabled":true,"microphone":"Old","share_personal_dictionary_with_speech_provider":true}"#,
        )
        .expect("seed");

        store_at(&path, Some("Studio Mic".into())).expect("store");

        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        assert_eq!(written["microphone"], "Studio Mic");
        assert_eq!(written["enabled"], true);
        assert_eq!(
            written["share_personal_dictionary_with_speech_provider"],
            true
        );
        assert_eq!(selected_at(&path).as_deref(), Some("Studio Mic"));
    }

    #[test]
    fn null_resets_to_the_default_input_without_dropping_other_keys() {
        let path = scratch("reset");
        std::fs::write(&path, r#"{"enabled":true,"microphone":"Studio Mic"}"#).expect("seed");

        store_at(&path, None).expect("store");

        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        assert!(written["microphone"].is_null());
        assert_eq!(written["enabled"], true);
        assert_eq!(selected_at(&path), None);
    }

    #[test]
    fn a_blank_name_is_rejected_rather_than_falling_back_to_the_default() {
        assert!(validated(Some("   ".into())).is_err());
        assert!(validated(Some(String::new())).is_err());
        assert_eq!(validated(None), Ok(None));
        assert_eq!(validated(Some("Mic".into())), Ok(Some("Mic".into())));
    }

    #[test]
    fn an_unavailable_device_is_rejected_before_persistence() {
        let path = scratch("unavailable");
        let original = r#"{"enabled":true,"microphone":null}"#;
        std::fs::write(&path, original).expect("seed");

        let error = store_validated_at(&path, Some("Missing Mic".into()), |_| {
            Err("selected microphone is unavailable: Missing Mic".to_string())
        })
        .expect_err("unavailable device must fail before persistence");

        assert!(error.contains("unavailable"), "unexpected error: {error}");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), original);
    }

    #[test]
    fn malformed_requests_fail_visibly() {
        assert!(parse("not json").is_err());
        assert!(parse("{}").is_err());
        assert!(parse(r#"{"microphone":7}"#).is_err());
        assert!(parse(r#"{"microphone":["a"]}"#).is_err());
        assert_eq!(parse(r#"{"microphone":null}"#), Ok(None));
        assert_eq!(
            parse(r#"{"microphone":"Studio Mic"}"#),
            Ok(Some("Studio Mic".into()))
        );
    }

    #[test]
    fn concurrent_writers_do_not_lose_each_others_updates() {
        // The reported race: the desktop saves `enabled` from a stale snapshot while the CLI
        // stores a microphone. flock makes each read-merge-write atomic, so both keys survive.
        let path = scratch("race");
        let writer = |key: &'static str| {
            let path = path.clone();
            std::thread::spawn(move || {
                for i in 0..50 {
                    update_at(&path, |settings| {
                        settings.insert(key.to_string(), serde_json::Value::from(i));
                    })
                    .expect("update");
                }
            })
        };
        let desktop = writer("enabled");
        let cli = writer("microphone");
        desktop.join().expect("join");
        cli.join().expect("join");
        let settings = load_at(&path).expect("load");
        assert_eq!(settings["enabled"], 49);
        assert_eq!(settings["microphone"], 49);
    }

    #[test]
    fn a_malformed_file_fails_the_write_and_is_left_untouched() {
        for seed in ["not json", "[1,2]", "\"a string\""] {
            let path = scratch("malformed");
            std::fs::write(&path, seed).expect("seed");
            assert!(store_at(&path, Some("Studio Mic".into())).is_err());
            assert_eq!(std::fs::read_to_string(&path).expect("read"), seed);
            // The read path degrades to the default input instead of erroring.
            assert_eq!(selected_at(&path), None);
        }
    }

    #[test]
    fn a_missing_file_reads_as_the_default_input_and_writes_cleanly() {
        let path = scratch("fresh");
        assert_eq!(selected_at(&path), None);
        store_at(&path, Some("Studio Mic".into())).expect("store");
        assert_eq!(selected_at(&path).as_deref(), Some("Studio Mic"));
    }
}
