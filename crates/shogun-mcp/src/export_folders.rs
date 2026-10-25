//! The folders the user allowed SHOGUN to save exports into (issue #397).
//!
//! A preference, not a secret: `export_folders.json` in app-data holds `{"folders": [...]}` —
//! absolute, canonical directory paths the user picked in Settings. No folder, no file writes:
//! the feature is off until the user adds one. Every face (desktop / MCP / REST / CLI) reads the
//! same document; this crate owns it because every face can reach it (shogun-core depends on
//! this crate, never the other way).
//!
//! Adding a folder widens where files may appear, so only the desktop's folder picker adds one
//! (the pick *is* the consent). Listing and removing are symmetric on every face — removing only
//! narrows the boundary.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::plan_source::export_folders_json_path;

/// At most this many folders — the list is a boundary the user reads, not a catalogue.
pub const MAX_FOLDERS: usize = 16;

static PROCESS_WRITE_LOCK: Mutex<()> = Mutex::new(());
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Why a folder could not be added or removed. Fixed strings: never echo the path back into a
/// log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderError {
    Unavailable,
    NotAbsolute,
    NotADirectory,
    TooBroad,
    TooMany,
    NotFound,
    SaveFailed,
}

impl FolderError {
    pub fn as_str(self) -> &'static str {
        match self {
            FolderError::Unavailable => "export folders are unavailable on this machine",
            FolderError::NotAbsolute => "folder must be an absolute path",
            FolderError::NotADirectory => "folder does not exist or is not a directory",
            FolderError::TooBroad => "choose a folder, not the whole disk",
            FolderError::TooMany => "too many export folders",
            FolderError::NotFound => "that folder is not an export folder",
            FolderError::SaveFailed => "couldn't save export folders",
        }
    }

    /// A stable code for the desktop, which words it in the UI language (#778). The API faces
    /// keep [`FolderError::as_str`].
    pub fn code(self) -> &'static str {
        match self {
            FolderError::Unavailable => "folders_unavailable",
            FolderError::NotAbsolute => "folder_not_absolute",
            FolderError::NotADirectory => "folder_not_a_directory",
            FolderError::TooBroad => "folder_too_broad",
            FolderError::TooMany => "folder_too_many",
            FolderError::NotFound => "folder_not_found",
            FolderError::SaveFailed => "folder_save_failed",
        }
    }
}

/// The allowed folders, as stored (canonical absolute paths). Empty when unset or unreadable.
pub fn list() -> Vec<PathBuf> {
    export_folders_json_path().map(|p| list_at(&p)).unwrap_or_default()
}

/// Add a folder (desktop only — see the module docs). Returns the stored (canonical) path.
pub fn add(folder: &str) -> Result<PathBuf, FolderError> {
    let path = export_folders_json_path().ok_or(FolderError::Unavailable)?;
    add_at(&path, folder)
}

/// Remove a folder. Matching is on the stored path or on its canonical form.
pub fn remove(folder: &str) -> Result<(), FolderError> {
    let path = export_folders_json_path().ok_or(FolderError::Unavailable)?;
    remove_at(&path, folder)
}

/// The list as JSON for the API faces: `{"folders": [...]}`.
pub fn list_json() -> String {
    let folders: Vec<String> = list().iter().map(|p| p.display().to_string()).collect();
    serde_json::json!({ "folders": folders }).to_string()
}

/// The stored list as one value, for the seal (audit 2026-10-20 M6).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FolderList(pub Vec<PathBuf>);

/// The seal on `export_folders.json`: installed by `shogun_core::settings_seal`. A folder added
/// outside the app is not an export folder; a folder removed outside it is gone.
pub static SEAL: crate::settings_seal_hook::SealSlot<FolderList> =
    crate::settings_seal_hook::SealSlot::new();

/// The allowed folders stored at `settings`, checked against its seal.
pub fn list_at(settings: &Path) -> Vec<PathBuf> {
    let file: Vec<PathBuf> = std::fs::read_to_string(settings)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("folders").and_then(|f| f.as_array()).cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|v| v.as_str())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .take(MAX_FOLDERS)
        .collect();
    SEAL.trusted(settings, FolderList(file)).0
}

pub fn add_at(settings: &Path, folder: &str) -> Result<PathBuf, FolderError> {
    let candidate = PathBuf::from(folder.trim());
    if !candidate.is_absolute() {
        return Err(FolderError::NotAbsolute);
    }
    let canonical = candidate.canonicalize().map_err(|_| FolderError::NotADirectory)?;
    if !canonical.is_dir() {
        return Err(FolderError::NotADirectory);
    }
    if canonical.parent().is_none() {
        return Err(FolderError::TooBroad);
    }
    let _guard = PROCESS_WRITE_LOCK.lock().map_err(|_| FolderError::SaveFailed)?;
    let mut folders = list_at(settings);
    if folders.iter().any(|f| f == &canonical) {
        return Ok(canonical);
    }
    if folders.len() >= MAX_FOLDERS {
        return Err(FolderError::TooMany);
    }
    folders.push(canonical.clone());
    save_at(settings, &folders)?;
    Ok(canonical)
}

pub fn remove_at(settings: &Path, folder: &str) -> Result<(), FolderError> {
    let wanted = PathBuf::from(folder.trim());
    let canonical = wanted.canonicalize().ok();
    let _guard = PROCESS_WRITE_LOCK.lock().map_err(|_| FolderError::SaveFailed)?;
    let mut folders = list_at(settings);
    let before = folders.len();
    folders.retain(|f| f != &wanted && Some(f) != canonical.as_ref());
    if folders.len() == before {
        return Err(FolderError::NotFound);
    }
    save_at(settings, &folders)
}

fn save_at(settings: &Path, folders: &[PathBuf]) -> Result<(), FolderError> {
    if let Some(parent) = settings.parent() {
        std::fs::create_dir_all(parent).map_err(|_| FolderError::SaveFailed)?;
    }
    let list: Vec<String> = folders.iter().map(|p| p.display().to_string()).collect();
    let json = serde_json::to_string_pretty(&serde_json::json!({ "folders": list }))
        .map_err(|_| FolderError::SaveFailed)?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = settings.with_extension(format!("json.tmp.{}.{}", std::process::id(), sequence));
    std::fs::write(&temporary, json).map_err(|_| FolderError::SaveFailed)?;
    let result = std::fs::rename(&temporary, settings).map_err(|_| FolderError::SaveFailed);
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    } else {
        // The app's pick, or a removal from any face: seal what is now in force (M6).
        SEAL.seal(settings, &FolderList(folders.to_vec()));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_codes_are_stable_and_distinct() {
        let all = [
            FolderError::Unavailable,
            FolderError::NotAbsolute,
            FolderError::NotADirectory,
            FolderError::TooBroad,
            FolderError::TooMany,
            FolderError::NotFound,
            FolderError::SaveFailed,
        ];
        let codes: std::collections::HashSet<&str> = all.iter().map(|e| e.code()).collect();
        assert_eq!(codes.len(), all.len());
        for code in codes {
            assert!(code.chars().all(|c| c.is_ascii_lowercase() || c == '_'), "{code}");
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "shogun-export-folders-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn unset_means_no_folders_and_the_feature_is_off() {
        let dir = scratch("unset");
        assert!(list_at(&dir.join("export_folders.json")).is_empty());
    }

    #[test]
    fn add_stores_the_canonical_directory_once_and_remove_narrows() {
        let dir = scratch("add");
        let settings = dir.join("export_folders.json");
        let target = dir.join("Notes");
        std::fs::create_dir_all(&target).unwrap();
        let dotted = format!("{}/../Notes", target.display());
        let stored = add_at(&settings, &dotted).unwrap();
        assert_eq!(stored, target.canonicalize().unwrap());
        // Adding it again (spelled differently) is idempotent.
        add_at(&settings, target.to_str().unwrap()).unwrap();
        assert_eq!(list_at(&settings), vec![stored.clone()]);
        remove_at(&settings, stored.to_str().unwrap()).unwrap();
        assert!(list_at(&settings).is_empty());
        assert_eq!(remove_at(&settings, stored.to_str().unwrap()), Err(FolderError::NotFound));
    }

    #[test]
    fn relative_missing_and_root_folders_are_refused() {
        let dir = scratch("refuse");
        let settings = dir.join("export_folders.json");
        assert_eq!(add_at(&settings, "Notes"), Err(FolderError::NotAbsolute));
        assert_eq!(
            add_at(&settings, dir.join("missing").to_str().unwrap()),
            Err(FolderError::NotADirectory)
        );
        let file = dir.join("file.txt");
        std::fs::write(&file, "x").unwrap();
        assert_eq!(add_at(&settings, file.to_str().unwrap()), Err(FolderError::NotADirectory));
        assert_eq!(add_at(&settings, "/"), Err(FolderError::TooBroad));
        assert!(list_at(&settings).is_empty());
    }
}
