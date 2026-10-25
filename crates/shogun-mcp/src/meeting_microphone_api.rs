//! Shared MCP/REST helpers for the meeting microphone setting.

use std::fs::{File, OpenOptions};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use crate::plan_source::meeting_json_path;

const LOCK_RETRIES: usize = 200;
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(10);
static PROCESS_WRITE_LOCK: Mutex<()> = Mutex::new(());
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct SettingsFileLock(File);

impl Drop for SettingsFileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::flock(std::os::fd::AsRawFd::as_raw_fd(&self.0), libc::LOCK_UN);
        }
    }
}

fn normalize(microphone: Option<String>) -> Option<String> {
    microphone.filter(|name| !name.trim().is_empty())
}

pub fn get() -> String {
    let microphone = load()
        .get("microphone")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    serde_json::json!({ "microphone": microphone }).to_string()
}

pub fn set(body: &str) -> Result<String, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| "invalid meeting microphone request".to_string())?;
    let microphone = match value.get("microphone") {
        Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(name)) => Some(name.clone()),
        _ => return Err("meeting microphone must be a string or null".to_string()),
    };
    let microphone = normalize(microphone);
    update(|settings| {
        let object = settings
            .as_object_mut()
            .ok_or_else(|| "meeting settings are malformed".to_string())?;
        object.insert(
            "microphone".to_string(),
            microphone
                .clone()
                .map(serde_json::Value::String)
                .unwrap_or(serde_json::Value::Null),
        );
        Ok(())
    })?;
    Ok(serde_json::json!({ "microphone": microphone }).to_string())
}

pub(crate) fn load() -> serde_json::Value {
    meeting_json_path()
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

/// Read-patch-write under the same lock-file protocol as shogun-core's desktop settings store.
/// This process cannot depend on shogun-core (the dependency points the other way), but both
/// faces coordinate through `meeting.json.lock` and unique temporary paths.
pub(crate) fn update(patch: impl FnOnce(&mut serde_json::Value) -> Result<(), String>) -> Result<(), String> {
    let path = meeting_json_path().ok_or("meeting settings unavailable")?;
    let _process = PROCESS_WRITE_LOCK
        .lock()
        .map_err(|_| "meeting settings busy".to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("meeting settings unavailable: {e}"))?;
    }
    let _file = acquire_file_lock(&path)?;
    let mut settings = load_at(&path);
    patch(&mut settings)?;
    let json = serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = path.with_extension(format!("json.tmp.{}.{}", std::process::id(), sequence));
    std::fs::write(&temporary, json).map_err(|e| format!("save failed: {e}"))?;
    let result = std::fs::rename(&temporary, path).map_err(|e| format!("save failed: {e}"));
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

fn acquire_file_lock(path: &Path) -> Result<SettingsFileLock, String> {
    let lock_path = path.with_extension("json.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .map_err(|error| format!("meeting settings unavailable: {error}"))?;
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
    Err("meeting settings busy".to_string())
}
