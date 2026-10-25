//! `Shougun.md` のパース・注入基盤（純粋ロジック）。

pub mod directives;
pub mod model;
pub mod parse;
pub mod sample;

pub use directives::{
    render_directives, render_directives_with_lessons, render_learned_section, LearnedLesson,
};
pub use model::{
    Charm, ParseReport, Profile, RawSection, SectionError, ShougunConfig, Style, Workflow,
};
pub use parse::parse_shougun;
pub use sample::sample_markdown;

use std::path::{Path, PathBuf};

/// 既定のファイルパス（ホーム直下 `~/Shougun.md`）。
pub fn default_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join("Shougun.md"))
}

/// ファイルを読み、無ければサンプルを書き出す。
/// 戻り値: (パース済み設定, 新規作成したか)。
pub fn load_or_create(path: &Path) -> std::io::Result<(ShougunConfig, bool)> {
    if path.exists() {
        let mut content = std::fs::read_to_string(path)?;
        if content == sample::LEGACY_JAPANESE_SAMPLE {
            content = sample_markdown();
            std::fs::write(path, &content)?;
        }
        let (cfg, _report) = parse_shougun(&content);
        Ok((cfg, false))
    } else {
        let sample = sample_markdown();
        std::fs::write(path, &sample)?;
        let (cfg, _report) = parse_shougun(&sample);
        Ok((cfg, true))
    }
}

/// The `n`th place [`replace_with_sample`] may keep a replaced file, beside it:
/// `<stem>.backup.md`, then `<stem>.backup-2.md`, `<stem>.backup-3.md`, …
/// (`~/Shougun.backup.md` first for the default path).
pub fn backup_path(path: &Path, n: u32) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Shougun".to_string());
    if n <= 1 {
        path.with_file_name(format!("{stem}.backup.md"))
    } else {
        path.with_file_name(format!("{stem}.backup-{n}.md"))
    }
}

/// How many backup names [`replace_with_sample`] tries before refusing.
const MAX_BACKUPS: u32 = 1000;

/// Replace the settings file with the sample without losing the user's own (#907).
///
/// When the file exists and differs from the sample, its exact bytes are kept first. A backup is
/// never overwritten: the first free name from [`backup_path`] is used, unless a backup already
/// holds these exact bytes, which is then reused. So edits made after an earlier reset, or a file
/// that is the sample of an older version, get a backup of their own beside the first one. A
/// file that already is the current sample needs none. A file that exists but cannot be read is
/// refused, not replaced.
///
/// Both writes go to a temporary file that is renamed into place, so a crash leaves the old
/// content or the new, never half of one. The settings file is written through a symlink (the
/// link stays and its target is replaced), and keeps its permissions; a backup gets the same
/// permissions as the file it keeps. Returns the backup's path when one was made or reused.
pub fn replace_with_sample(path: &Path) -> std::io::Result<Option<PathBuf>> {
    let sample = sample_markdown();
    let backup = match std::fs::read(path) {
        Ok(current) if current != sample.as_bytes() => {
            let permissions = std::fs::metadata(path)?.permissions();
            Some(keep_backup(path, &current, &permissions)?)
        }
        Ok(_) => None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    // Through a symlink: replace what it points at, so the link (and whatever syncs it) stays.
    let target = match std::fs::canonicalize(path) {
        Ok(target) => target,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => path.to_path_buf(),
        Err(error) => return Err(error),
    };
    let permissions = std::fs::metadata(&target).ok().map(|m| m.permissions());
    write_atomically(&target, sample.as_bytes(), permissions.as_ref())?;
    Ok(backup)
}

/// Save `bytes` at the first free backup name, or reuse a backup that already holds them.
fn keep_backup(
    path: &Path,
    bytes: &[u8],
    permissions: &std::fs::Permissions,
) -> std::io::Result<PathBuf> {
    for n in 1..=MAX_BACKUPS {
        let candidate = backup_path(path, n);
        match std::fs::read(&candidate) {
            Ok(existing) if existing == bytes => return Ok(candidate),
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                write_atomically(&candidate, bytes, Some(permissions))?;
                return Ok(candidate);
            }
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::other("too many Shougun.md backups; remove some old ones"))
}

/// Write `bytes` to a temporary file beside `path`, flush it to disk, and rename it over `path`.
/// The temporary file gets `permissions` before any byte is written, so the user's text is never
/// readable more widely than the file it replaces or keeps, not even for a moment.
fn write_atomically(
    path: &Path,
    bytes: &[u8],
    permissions: Option<&std::fs::Permissions>,
) -> std::io::Result<()> {
    use std::io::Write;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let result = std::fs::File::create(&temp)
        .and_then(|mut file| {
            if let Some(permissions) = permissions {
                file.set_permissions(permissions.clone())?;
            }
            file.write_all(bytes)?;
            file.sync_all()
        })
        .and_then(|()| std::fs::rename(&temp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// ファイルを読んでパース結果とレポートを返す（存在しなければ空＋ok）。
pub fn load_report(path: &Path) -> std::io::Result<(ShougunConfig, ParseReport)> {
    if path.exists() {
        let content = std::fs::read_to_string(path)?;
        Ok(parse_shougun(&content))
    } else {
        Ok((ShougunConfig::default(), ParseReport { ok: true, section_errors: Vec::new() }))
    }
}

#[cfg(test)]
mod io_tests {
    use super::*;

    fn fresh_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "shougun_{name}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn replacing_with_the_sample_keeps_the_users_file_as_a_backup() {
        let dir = fresh_dir("replace");
        let path = dir.join("Shougun.md");
        let mine = "# Profile\nI write short replies.\n\u{00e9}\n";
        std::fs::write(&path, mine).unwrap();

        let backup = replace_with_sample(&path).unwrap();
        assert_eq!(backup, Some(dir.join("Shougun.backup.md")));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), sample_markdown());
        assert_eq!(std::fs::read_to_string(dir.join("Shougun.backup.md")).unwrap(), mine);

        // Pressing it again: the file is already the sample, so no backup is made or touched.
        assert_eq!(replace_with_sample(&path).unwrap(), None);
        assert_eq!(std::fs::read_to_string(dir.join("Shougun.backup.md")).unwrap(), mine);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A backup is never written over. Content that differs from every existing
    /// backup gets the next free name; content a backup already holds reuses it.
    #[test]
    fn an_existing_backup_is_kept() {
        let dir = fresh_dir("kept");
        let path = dir.join("Shougun.md");
        let mine = "# Profile\nYears of my own notes.\n";
        std::fs::write(&path, mine).unwrap();
        assert_eq!(replace_with_sample(&path).unwrap(), Some(dir.join("Shougun.backup.md")));

        // Edited again, then replaced again: the newer edits get a backup of their own, and the
        // first one is kept.
        std::fs::write(&path, "newer edits").unwrap();
        assert_eq!(replace_with_sample(&path).unwrap(), Some(dir.join("Shougun.backup-2.md")));
        assert_eq!(std::fs::read_to_string(dir.join("Shougun.backup.md")).unwrap(), mine);
        assert_eq!(std::fs::read_to_string(dir.join("Shougun.backup-2.md")).unwrap(), "newer edits");

        // A file that is the sample of an older version is not the current sample: it is kept
        // too, never written over an earlier backup.
        std::fs::write(&path, "an older sample").unwrap();
        assert_eq!(replace_with_sample(&path).unwrap(), Some(dir.join("Shougun.backup-3.md")));
        assert_eq!(std::fs::read_to_string(dir.join("Shougun.backup.md")).unwrap(), mine);

        // Content already kept is not duplicated: the existing backup is reused.
        std::fs::write(&path, mine).unwrap();
        assert_eq!(replace_with_sample(&path).unwrap(), Some(dir.join("Shougun.backup.md")));
        assert!(!dir.join("Shougun.backup-4.md").exists());

        // No temporary file is left behind.
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().all(|name| !name.ends_with(".tmp")), "{names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn replacing_writes_through_a_symlink_and_keeps_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = fresh_dir("symlink");
        let real = dir.join("dotfiles-Shougun.md");
        std::fs::write(&real, "mine").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        let path = dir.join("Shougun.md");
        std::os::unix::fs::symlink(&real, &path).unwrap();

        let backup = replace_with_sample(&path).unwrap().unwrap();
        assert!(std::fs::symlink_metadata(&path).unwrap().file_type().is_symlink(), "the link stays");
        assert_eq!(std::fs::read_to_string(&real).unwrap(), sample_markdown(), "its target got the sample");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&real), 0o600);
        assert_eq!(mode(&backup), 0o600, "the backup is no more readable than the file it keeps");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "mine");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_gets_the_sample_and_no_backup() {
        let dir = fresh_dir("missing");
        let path = dir.join("Shougun.md");
        assert_eq!(replace_with_sample(&path).unwrap(), None);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), sample_markdown());
        assert!(!dir.join("Shougun.backup.md").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_cannot_be_read_is_refused_not_replaced() {
        let dir = fresh_dir("unreadable");
        // A directory where the file should be: reading it fails with something other than
        // "not found", and nothing may be written over it.
        let path = dir.join("Shougun.md");
        std::fs::create_dir_all(&path).unwrap();
        assert!(replace_with_sample(&path).is_err());
        assert!(path.is_dir());
        assert!(!dir.join("Shougun.backup.md").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_or_create_writes_sample_when_missing() {
        let dir = std::env::temp_dir().join(format!("shougun_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("Shougun.md");
        let _ = std::fs::remove_file(&path);

        let (cfg, created) = load_or_create(&path).expect("load_or_create");
        assert!(created, "missing file should be created");
        assert!(path.exists());
        // 2回目は作成しない
        let (_cfg2, created2) = load_or_create(&path).expect("second load");
        assert!(!created2);
        let _ = cfg; // 使用済み扱い
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_or_create_migrates_only_the_untouched_japanese_template() {
        let dir = std::env::temp_dir().join(format!("shougun_migrate_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("Shougun.md");
        std::fs::write(&path, sample::LEGACY_JAPANESE_SAMPLE).expect("write legacy sample");

        let (cfg, created) = load_or_create(&path).expect("migrate legacy sample");

        assert!(!created);
        assert_eq!(cfg.style.tone, "Friendly and direct");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read migrated sample"),
            sample_markdown()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
