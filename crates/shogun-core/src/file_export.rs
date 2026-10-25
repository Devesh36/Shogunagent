//! `LocalAction::WriteFile` (issue #397): put an export into a folder the user chose.
//!
//! The rules, all enforced here before a byte is written:
//! - the target's folder must resolve (symlinks followed, `..` refused) to one of the allowed
//!   folders or a folder beneath one — a path outside, a relative path, or a symlink that points
//!   out is refused;
//! - a file is only ever **created**: an existing name is never opened for writing, the next free
//!   `name (2).md`, `name (3).md`, … is used instead (`create_new` — no overwrite, no truncate,
//!   and a symlink planted at the target name is not followed);
//! - nothing here deletes anything;
//! - the action is L2: the caller must hold the one-tap confirmation.
//!
//! The only thing that persists about the write beyond the file itself is a digest and a byte
//! count (see `shogun_memory::local_writes`) — never the content, never in a log.

use std::io::Write;
use std::path::{Component, Path, PathBuf};

use shogun_agents::permission::{Action, ExportFormat, Level, LocalAction};

/// Longest file stem we produce or accept (characters).
pub const MAX_STEM_CHARS: usize = 120;
/// How many ` (n)` suffixes to try before giving up.
const MAX_SUFFIX: u32 = 99;

/// Why a write was refused. Fixed strings — safe for an API response and a log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportError {
    /// No allowed folder is set (the feature is off).
    NoFolders,
    /// The target is outside every allowed folder (or not an absolute, `..`-free path).
    OutsideAllowedFolders,
    /// The file name is empty, hidden, has a separator / control character, or the wrong extension.
    BadName,
    /// The action is not a file write.
    NotAFileWrite,
    /// The write was not confirmed at L2.
    NotConfirmed,
    /// Every candidate name up to ` (99)` is taken.
    NoFreeName,
    /// The disk refused the write.
    WriteFailed,
}

impl ExportError {
    pub fn as_str(self) -> &'static str {
        match self {
            ExportError::NoFolders => "no export folder is set",
            ExportError::OutsideAllowedFolders => "that folder is not an export folder",
            ExportError::BadName => "invalid file name",
            ExportError::NotAFileWrite => "not a file export",
            ExportError::NotConfirmed => "file exports need a one-tap confirmation",
            ExportError::NoFreeName => "too many files with that name",
            ExportError::WriteFailed => "couldn't write the file",
        }
    }
}

/// What a completed write produced: where it landed, how big it is, and its digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    pub path: PathBuf,
    pub bytes: usize,
    /// xxh64 of the content, 16 lower-hex chars — the same digest the traceability log uses.
    pub digest: String,
}

/// A file stem from a title: separators, reserved and control characters become `-`, leading
/// dots and surrounding whitespace go, length is capped. Never empty.
pub fn file_stem(title: &str, fallback: &str) -> String {
    let cleaned: String = title
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_start_matches('.').trim();
    let capped: String = trimmed.chars().take(MAX_STEM_CHARS).collect();
    let capped = capped.trim_end_matches(['.', ' ']).to_string();
    if capped.is_empty() {
        fallback.to_string()
    } else {
        capped
    }
}

/// Validate a caller-chosen name (a stem, or a stem plus this format's extension). Unlike
/// [`file_stem`] this refuses rather than repairs: an API caller asked for exactly this name.
pub fn checked_stem(name: &str, format: ExportFormat) -> Result<String, ExportError> {
    let name = name.trim();
    let suffix = format!(".{}", format.extension());
    let stem = if name.to_ascii_lowercase().ends_with(&suffix) {
        &name[..name.len() - suffix.len()]
    } else {
        name
    };
    let bad = stem.is_empty()
        || stem.starts_with('.')
        || stem.chars().count() > MAX_STEM_CHARS
        || stem.chars().any(|c| c == '/' || c == '\\' || c == ':' || c.is_control());
    if bad {
        Err(ExportError::BadName)
    } else {
        Ok(stem.to_string())
    }
}

/// The intended path for `stem` in `folder`.
pub fn target_path(folder: &Path, stem: &str, format: ExportFormat) -> PathBuf {
    folder.join(format!("{stem}.{}", format.extension()))
}

/// The allowed folders, canonicalized. `NoFolders` when none are set or all are unreadable.
fn canonical_roots(roots: &[PathBuf]) -> Result<Vec<PathBuf>, ExportError> {
    let roots: Vec<PathBuf> = roots.iter().filter_map(|r| r.canonicalize().ok()).collect();
    if roots.is_empty() {
        return Err(ExportError::NoFolders);
    }
    Ok(roots)
}

/// `folder` checked against canonical `roots`: absolute, `..`-free, a real directory inside one.
fn resolve_folder_in(roots: &[PathBuf], folder: &Path) -> Result<PathBuf, ExportError> {
    if !folder.is_absolute()
        || folder
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(ExportError::OutsideAllowedFolders);
    }
    let folder = folder
        .canonicalize()
        .ok()
        .filter(|p| p.is_dir())
        .ok_or(ExportError::OutsideAllowedFolders)?;
    if !roots.iter().any(|root| folder.starts_with(root)) {
        return Err(ExportError::OutsideAllowedFolders);
    }
    Ok(folder)
}

/// The folder half of [`resolve`], on its own — refuses an unauthorized `folder` up front, before
/// the caller's other inputs (the meeting, the file name) are even looked at.
pub fn resolve_folder(roots: &[PathBuf], folder: &str) -> Result<PathBuf, ExportError> {
    resolve_folder_in(&canonical_roots(roots)?, Path::new(folder.trim()))
}

/// Where a write may land: the canonical folder and the file name. Refuses (before any write)
/// anything that is not an absolute, `..`-free path whose folder resolves inside an allowed one.
pub fn resolve(
    roots: &[PathBuf],
    path_in_allowed_root: &str,
    format: ExportFormat,
) -> Result<(PathBuf, String), ExportError> {
    let roots = canonical_roots(roots)?;
    let path = Path::new(path_in_allowed_root);
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(ExportError::OutsideAllowedFolders);
    }
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or(ExportError::BadName)?;
    let stem = checked_stem(file_name, format)?;
    if file_name != format!("{stem}.{}", format.extension()) {
        return Err(ExportError::BadName);
    }
    let folder = resolve_folder_in(
        &roots,
        path.parent().ok_or(ExportError::OutsideAllowedFolders)?,
    )?;
    Ok((folder, file_name.to_string()))
}

/// Create `file_name` in `folder` — or the first free ` (n)` variant — and write `body`. Never
/// opens an existing file.
pub fn write_new(folder: &Path, file_name: &str, body: &str) -> Result<PathBuf, ExportError> {
    let (stem, ext) = match file_name.rsplit_once('.') {
        Some((stem, ext)) => (stem, format!(".{ext}")),
        None => (file_name, String::new()),
    };
    for n in 1..=MAX_SUFFIX {
        let name = if n == 1 {
            file_name.to_string()
        } else {
            format!("{stem} ({n}){ext}")
        };
        let path = folder.join(&name);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(body.as_bytes()).map_err(|_| ExportError::WriteFailed)?;
                file.sync_all().map_err(|_| ExportError::WriteFailed)?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(ExportError::WriteFailed),
        }
    }
    Err(ExportError::NoFreeName)
}

/// Run a confirmed [`LocalAction::WriteFile`]. `approval` is the level the user confirmed at —
/// L1 (no confirmation) is refused, since the action requires L2.
pub fn execute(roots: &[PathBuf], action: &Action, approval: Level) -> Result<Written, ExportError> {
    let Action::Local(LocalAction::WriteFile { path_in_allowed_root, format, body }) = action else {
        return Err(ExportError::NotAFileWrite);
    };
    if approval == Level::L1 || action.required_level() == Level::L1 {
        return Err(ExportError::NotConfirmed);
    }
    let (folder, file_name) = resolve(roots, path_in_allowed_root, *format)?;
    let path = write_new(&folder, &file_name, body)?;
    Ok(Written {
        path,
        bytes: body.len(),
        digest: crate::llm::traceability::digest(body),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn scratch(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "shogun-file-export-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn write(path: &Path, body: &str) -> Action {
        Action::Local(LocalAction::WriteFile {
            path_in_allowed_root: path.display().to_string(),
            format: ExportFormat::Markdown,
            body: body.into(),
        })
    }

    #[test]
    fn writes_inside_an_allowed_folder_and_digests_the_content() {
        let root = scratch("ok");
        let out = execute(std::slice::from_ref(&root), &write(&root.join("Recap.md"), "# Hi\n"), Level::L2)
            .unwrap();
        assert_eq!(out.path, root.join("Recap.md"));
        assert_eq!(std::fs::read_to_string(&out.path).unwrap(), "# Hi\n");
        assert_eq!(out.bytes, 5);
        assert_eq!(out.digest, crate::llm::traceability::digest("# Hi\n"));
        // A subfolder of an allowed folder is inside it.
        std::fs::create_dir_all(root.join("Meetings")).unwrap();
        assert!(execute(std::slice::from_ref(&root), &write(&root.join("Meetings/A.md"), "a"), Level::L2).is_ok());
    }

    #[test]
    fn never_overwrites_an_existing_file() {
        let root = scratch("clobber");
        std::fs::write(root.join("Recap.md"), "mine").unwrap();
        std::fs::write(root.join("Recap (2).md"), "mine too").unwrap();
        let out = execute(std::slice::from_ref(&root), &write(&root.join("Recap.md"), "new"), Level::L2).unwrap();
        assert_eq!(out.path, root.join("Recap (3).md"));
        assert_eq!(std::fs::read_to_string(root.join("Recap.md")).unwrap(), "mine");
        assert_eq!(std::fs::read_to_string(root.join("Recap (2).md")).unwrap(), "mine too");
        assert_eq!(std::fs::read_to_string(out.path).unwrap(), "new");
    }

    #[test]
    fn paths_outside_the_allowed_folders_are_refused_before_any_write() {
        let root = scratch("root");
        let outside = scratch("outside");
        let refused = |target: &str| {
            let action = Action::Local(LocalAction::WriteFile {
                path_in_allowed_root: target.to_string(),
                format: ExportFormat::Markdown,
                body: "x".into(),
            });
            execute(std::slice::from_ref(&root), &action, Level::L2)
        };
        // Absolute path in another folder.
        assert_eq!(
            refused(&outside.join("a.md").display().to_string()),
            Err(ExportError::OutsideAllowedFolders)
        );
        // `..` climbing out.
        assert_eq!(
            refused(&format!("{}/../{}/a.md", root.display(), outside.file_name().unwrap().to_str().unwrap())),
            Err(ExportError::OutsideAllowedFolders)
        );
        // Relative path.
        assert_eq!(refused("a.md"), Err(ExportError::OutsideAllowedFolders));
        // A symlink inside the root that points out.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
            assert_eq!(
                refused(&root.join("escape/a.md").display().to_string()),
                Err(ExportError::OutsideAllowedFolders)
            );
            // A symlink planted at the target name is not followed (create_new refuses it) —
            // the write moves to the next free name instead.
            std::os::unix::fs::symlink(outside.join("victim.md"), root.join("b.md")).unwrap();
            let out = refused(&root.join("b.md").display().to_string()).unwrap();
            assert_eq!(out.path, root.join("b (2).md"));
            assert!(!outside.join("victim.md").exists());
        }
        // Hidden / wrong-extension names.
        assert_eq!(refused(&root.join(".hidden.md").display().to_string()), Err(ExportError::BadName));
        assert_eq!(refused(&root.join("a.txt").display().to_string()), Err(ExportError::BadName));
        // Nothing was written outside, ever.
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    }

    #[test]
    fn no_folders_means_off_and_l1_is_not_a_confirmation() {
        let root = scratch("off");
        let action = write(&root.join("a.md"), "x");
        assert_eq!(execute(&[], &action, Level::L2), Err(ExportError::NoFolders));
        assert_eq!(execute(std::slice::from_ref(&root), &action, Level::L1), Err(ExportError::NotConfirmed));
        assert!(!root.join("a.md").exists());
        let not_a_write = Action::Local(LocalAction::LocalSearch { query: "q".into() });
        assert_eq!(execute(&[root], &not_a_write, Level::L2), Err(ExportError::NotAFileWrite));
    }

    #[test]
    fn stems_are_made_safe_and_caller_names_are_checked() {
        assert_eq!(file_stem("Q3 / Q4: plan?", "Recap"), "Q3 - Q4- plan-");
        assert_eq!(file_stem("  ..  ", "Recap"), "Recap");
        assert_eq!(file_stem("../../etc/passwd", "Recap"), "-..-etc-passwd");
        assert_eq!(checked_stem("Weekly.md", ExportFormat::Markdown), Ok("Weekly".into()));
        assert_eq!(checked_stem("Weekly", ExportFormat::Csv), Ok("Weekly".into()));
        assert_eq!(checked_stem("a/b", ExportFormat::Markdown), Err(ExportError::BadName));
        assert_eq!(checked_stem("..", ExportFormat::Markdown), Err(ExportError::BadName));
        assert_eq!(checked_stem("", ExportFormat::Markdown), Err(ExportError::BadName));
    }
}
