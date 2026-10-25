//! Shared persistence for meeting settings.
//!
//! The desktop owns capture, but MCP/CLI/REST need to change the same user setting. Keeping the
//! file location and atomic write here prevents those faces from silently diverging.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use super::settings::Settings;

const DESKTOP_IDENTIFIER: &str = "com.syogun.shogunai";
const SETTINGS_FILE: &str = "meeting.json";
const LOCK_RETRIES: usize = 200;
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(10);

static PROCESS_WRITE_LOCK: Mutex<()> = Mutex::new(());
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct SettingsFileLock {
    file: File,
}

impl Drop for SettingsFileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::flock(std::os::fd::AsRawFd::as_raw_fd(&self.file), libc::LOCK_UN);
        }
    }
}

/// The desktop app-data file, with a debug/test override for isolated tests.
pub fn settings_path() -> Option<PathBuf> {
    #[cfg(debug_assertions)]
    if let Ok(path) = std::env::var("SHOGUN_MEETING_SETTINGS_JSON") {
        if !path.trim().is_empty() {
            return Some(PathBuf::from(path));
        }
    }

    #[cfg(not(debug_assertions))]
    let _ = SETTINGS_FILE;

    if cfg!(target_os = "macos") {
        let home = std::env::var_os("HOME")?;
        return Some(
            PathBuf::from(home)
                .join("Library/Application Support")
                .join(DESKTOP_IDENTIFIER)
                .join(SETTINGS_FILE),
        );
    }
    None
}

/// Read a settings file. Bad or absent files read as the shipped default (on, 2026-10-20).
pub fn load() -> Settings {
    settings_path().map_or_else(Settings::default, |path| load_at(&path))
}

/// Only a missing file takes the shipped default (on). A file that exists but cannot be read or
/// parsed reads as off, so a saved Off never fails open into listening.
fn load_at(path: &Path) -> Settings {
    let off = || Settings { enabled: false, allow_mic_only_detect: false, ..Settings::default() };
    match std::fs::read_to_string(path) {
        Ok(json) => serde_json::from_str(&json).unwrap_or_else(|_| off()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Settings::default(),
        Err(_) => off(),
    }
}

/// Persist settings atomically so another API face never observes half-written JSON.
pub fn save(settings: &Settings) -> Result<(), String> {
    let path = settings_path().ok_or("meeting settings unavailable")?;
    save_at(&path, settings)
}

pub fn save_at(path: &Path, settings: &Settings) -> Result<(), String> {
    let _process = PROCESS_WRITE_LOCK
        .lock()
        .map_err(|_| "meeting settings busy".to_string())?;
    prepare_parent(path)?;
    let _file = acquire_file_lock(path)?;
    write_at_locked(path, settings)
}

/// Atomically read, patch, and replace settings while holding both process and cross-process locks.
/// Callers changing one field must use this instead of writing a stale full snapshot.
pub fn update(patch: impl FnOnce(&mut Settings)) -> Result<Settings, String> {
    let path = settings_path().ok_or("meeting settings unavailable")?;
    update_at(&path, patch)
}

pub fn update_at(path: &Path, patch: impl FnOnce(&mut Settings)) -> Result<Settings, String> {
    let _process = PROCESS_WRITE_LOCK
        .lock()
        .map_err(|_| "meeting settings busy".to_string())?;
    prepare_parent(path)?;
    let _file = acquire_file_lock(path)?;
    let mut settings = load_at(path);
    patch(&mut settings);
    write_at_locked(path, &settings)?;
    Ok(settings)
}

fn prepare_parent(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("meeting settings unavailable: {e}"))?;
    }
    Ok(())
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
            return Ok(SettingsFileLock { file });
        }
        std::thread::sleep(LOCK_RETRY_DELAY);
    }
    #[cfg(not(unix))]
    return Ok(SettingsFileLock { file });
    #[cfg(unix)]
    Err("meeting settings busy".to_string())
}

fn write_at_locked(path: &Path, settings: &Settings) -> Result<(), String> {
    let json = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
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
    use std::sync::{Arc, Barrier};

    use super::*;
    use crate::meeting::settings::MeetingMode;

    #[test]
    fn a_missing_file_is_on_and_a_corrupt_one_is_off() {
        let path = std::env::temp_dir().join(format!(
            "shogun-meeting-settings-corrupt-{}-{}.json",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        assert!(load_at(&path).enabled);
        std::fs::write(&path, "{\"enabled\":fal").unwrap();
        let corrupt = load_at(&path);
        assert!(!corrupt.enabled);
        assert!(!corrupt.allow_mic_only_detect);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn captions_choice_survives_reload_and_unrelated_settings_updates() -> Result<(), String> {
        let path = std::env::temp_dir().join(format!(
            "shogun-captions-settings-{}-{}.json",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        assert!(!load_at(&path).captions_hidden);
        update_at(&path, |settings| settings.captions_hidden = true)?;
        update_at(&path, |settings| settings.enabled = true)?;
        assert!(load_at(&path).captions_hidden);
        update_at(&path, |settings| settings.captions_hidden = false)?;
        assert!(!load_at(&path).captions_hidden);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("json.lock"));
        Ok(())
    }

    #[test]
    fn concurrent_field_updates_preserve_both_changes() -> Result<(), String> {
        let path = std::env::temp_dir().join(format!(
            "shogun-meeting-settings-race-{}-{}.json",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        save_at(&path, &Settings::default())?;
        let barrier = Arc::new(Barrier::new(3));
        let enabled_path = path.clone();
        let enabled_barrier = Arc::clone(&barrier);
        let enabled = std::thread::spawn(move || {
            enabled_barrier.wait();
            update_at(&enabled_path, |settings| settings.enabled = true)
        });
        let mode_path = path.clone();
        let mode_barrier = Arc::clone(&barrier);
        let mode = std::thread::spawn(move || {
            mode_barrier.wait();
            update_at(&mode_path, |settings| {
                settings.meeting_mode = MeetingMode::TwoWay;
            })
        });
        barrier.wait();
        enabled
            .join()
            .map_err(|_| "enabled settings worker panicked".to_string())??;
        mode.join()
            .map_err(|_| "mode settings worker panicked".to_string())??;
        let stored = load_at(&path);
        let lock_path = path.with_extension("json.lock");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(lock_path);
        if !stored.enabled || stored.meeting_mode != MeetingMode::TwoWay {
            return Err("concurrent settings updates lost a field".to_string());
        }
        Ok(())
    }
}
