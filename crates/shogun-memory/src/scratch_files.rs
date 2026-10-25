//! An agent's scratch files (V100): the stored half of the `/memories` virtual filesystem
//! (ShogunAgent E6). Every row belongs to one `agent_scratch:<agent_id>` namespace and records its
//! last writer; the world half (`/memories/people/…`, `commitments.md`, …) is a projection of the
//! state tables and never lands here. Paths are relative to `/memories/scratch/<agent_id>/`,
//! `/`-separated, with no empty, `.` or `..` segment ([`clean_path`]).

use rusqlite::{params, Connection, OptionalExtension};

use crate::actor::{Namespace, WriteContext};

/// Longest relative path accepted.
pub const MAX_PATH_CHARS: usize = 512;
/// Longest file content accepted (chars).
pub const MAX_CONTENT_CHARS: usize = 64_000;
/// Most files one scratch namespace holds.
pub const MAX_FILES_PER_NAMESPACE: usize = 2_000;

/// One scratch file as listed or read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchFile {
    pub path: String,
    pub content: String,
    pub actor: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Why a scratch operation was refused. Never carries content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScratchError {
    InvalidPath,
    TooLarge,
    TooManyFiles,
    /// The target of a rename already exists.
    Exists,
    NotFound,
    /// The namespace is not an agent's scratch namespace.
    NotScratch,
    Db,
}

impl From<rusqlite::Error> for ScratchError {
    fn from(_: rusqlite::Error) -> Self {
        ScratchError::Db
    }
}

/// Validate and normalise a relative scratch path: `/`-separated segments, none empty, `.` or
/// `..`, no control characters or backslashes, at most [`MAX_PATH_CHARS`]. Leading and trailing
/// slashes are dropped. `None` when the path is unusable.
pub fn clean_path(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_matches('/');
    if trimmed.is_empty() || trimmed.chars().count() > MAX_PATH_CHARS {
        return None;
    }
    if trimmed.chars().any(|c| c.is_control() || c == '\\') {
        return None;
    }
    let segments: Vec<&str> = trimmed.split('/').collect();
    if segments.iter().any(|s| s.is_empty() || *s == "." || *s == "..") {
        return None;
    }
    Some(segments.join("/"))
}

fn namespace_sql(namespace: &Namespace) -> Result<String, ScratchError> {
    match namespace {
        Namespace::AgentScratch(_) => Ok(namespace.as_str().into_owned()),
        Namespace::World => Err(ScratchError::NotScratch),
    }
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ScratchFile> {
    Ok(ScratchFile { path: r.get(0)?, content: r.get(1)?, actor: r.get(2)?, created_at: r.get(3)?, updated_at: r.get(4)? })
}

/// Every file of `namespace`, in path order.
pub fn list(conn: &Connection, namespace: &Namespace) -> Result<Vec<ScratchFile>, ScratchError> {
    let ns = namespace_sql(namespace)?;
    let mut stmt = conn.prepare(
        "SELECT path, content, actor, created_at, updated_at FROM scratch_files WHERE namespace = ?1 ORDER BY path",
    )?;
    let rows = stmt.query_map([ns], row)?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// One file, or `None`.
pub fn read(conn: &Connection, namespace: &Namespace, path: &str) -> Result<Option<ScratchFile>, ScratchError> {
    let ns = namespace_sql(namespace)?;
    let found = conn
        .query_row(
            "SELECT path, content, actor, created_at, updated_at FROM scratch_files WHERE namespace = ?1 AND path = ?2",
            params![ns, path],
            row,
        )
        .optional()?;
    Ok(found)
}

/// Create or overwrite `path` with `content` as `ctx` (whose namespace must be a scratch one).
/// Returns whether the file existed before.
pub fn write(conn: &Connection, ctx: &WriteContext, path: &str, content: &str, now: i64) -> Result<bool, ScratchError> {
    let ns = namespace_sql(&ctx.namespace)?;
    let path = clean_path(path).ok_or(ScratchError::InvalidPath)?;
    if content.chars().count() > MAX_CONTENT_CHARS {
        return Err(ScratchError::TooLarge);
    }
    let existed = read(conn, &ctx.namespace, &path)?.is_some();
    if !existed {
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM scratch_files WHERE namespace = ?1", [&ns], |r| r.get(0))?;
        if usize::try_from(count).unwrap_or(usize::MAX) >= MAX_FILES_PER_NAMESPACE {
            return Err(ScratchError::TooManyFiles);
        }
    }
    conn.execute(
        "INSERT INTO scratch_files (namespace, path, content, actor, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)
         ON CONFLICT (namespace, path) DO UPDATE SET content = excluded.content, actor = excluded.actor,
             updated_at = excluded.updated_at",
        params![ns, path, content, ctx.actor_sql(), now],
    )?;
    Ok(existed)
}

/// The rows a path names: the file itself, or every file under it as a directory.
const UNDER: &str = "(path = ?2 OR substr(path, 1, length(?2) + 1) = ?2 || '/')";

/// Delete one file, or every file under `path` when it names a directory. Returns how many went.
pub fn delete(conn: &Connection, namespace: &Namespace, path: &str) -> Result<usize, ScratchError> {
    let ns = namespace_sql(namespace)?;
    let path = clean_path(path).ok_or(ScratchError::InvalidPath)?;
    let removed = conn.execute(&format!("DELETE FROM scratch_files WHERE namespace = ?1 AND {UNDER}"), params![ns, path])?;
    if removed == 0 {
        return Err(ScratchError::NotFound);
    }
    Ok(removed)
}

/// Rename one file, or every file under a directory, never overwriting. Returns how many moved.
pub fn rename(conn: &Connection, ctx: &WriteContext, from: &str, to: &str, now: i64) -> Result<usize, ScratchError> {
    let ns = namespace_sql(&ctx.namespace)?;
    let from = clean_path(from).ok_or(ScratchError::InvalidPath)?;
    let to = clean_path(to).ok_or(ScratchError::InvalidPath)?;
    if from == to || to.starts_with(&format!("{from}/")) {
        return Err(ScratchError::Exists);
    }
    let moving: Vec<String> = {
        let mut stmt = conn.prepare(&format!("SELECT path FROM scratch_files WHERE namespace = ?1 AND {UNDER} ORDER BY path"))?;
        let rows = stmt.query_map(params![ns, from], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        rows
    };
    if moving.is_empty() {
        return Err(ScratchError::NotFound);
    }
    let targets: Vec<(String, String)> = moving
        .iter()
        .map(|old| (old.clone(), format!("{to}{}", old.strip_prefix(&from).unwrap_or(""))))
        .collect();
    for (_, target) in &targets {
        let exists: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM scratch_files WHERE namespace = ?1 AND {UNDER}"),
            params![ns, target],
            |r| r.get(0),
        )?;
        if exists > 0 {
            return Err(ScratchError::Exists);
        }
    }
    for (old, target) in &targets {
        conn.execute(
            "UPDATE scratch_files SET path = ?3, actor = ?4, updated_at = ?5 WHERE namespace = ?1 AND path = ?2",
            params![ns, old, target, ctx.actor_sql(), now],
        )?;
    }
    Ok(targets.len())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::actor::Actor;

    fn scratch_ctx(id: &str) -> WriteContext {
        WriteContext::scratch(Actor::agent(id).unwrap()).unwrap()
    }

    #[test]
    fn paths_are_cleaned_and_traversal_is_refused() {
        assert_eq!(clean_path("/notes/plan.md/"), Some("notes/plan.md".into()));
        assert_eq!(clean_path("notes//plan.md"), None);
        assert_eq!(clean_path("../etc/passwd"), None);
        assert_eq!(clean_path("a/./b"), None);
        assert_eq!(clean_path(""), None);
        assert_eq!(clean_path("a\\b"), None);
    }

    #[test]
    fn files_live_in_one_namespace_with_their_writer_and_rename_never_overwrites() {
        let conn = crate::open_in_memory().unwrap();
        let ctx = scratch_ctx("codex");
        assert!(!write(&conn, &ctx, "progress.md", "# Progress\n", 10).unwrap());
        assert!(write(&conn, &ctx, "progress.md", "# Progress\n- step 1\n", 11).unwrap(), "overwrite reports it existed");
        write(&conn, &ctx, "notes/a.md", "a", 12).unwrap();
        write(&conn, &ctx, "notes/b.md", "b", 13).unwrap();
        let all = list(&conn, &ctx.namespace).unwrap();
        assert_eq!(all.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["notes/a.md", "notes/b.md", "progress.md"]);
        assert!(all.iter().all(|f| f.actor == "agent:codex"));
        assert_eq!(read(&conn, &ctx.namespace, "progress.md").unwrap().unwrap().content, "# Progress\n- step 1\n");
        // Another agent's namespace sees nothing; the world is never a scratch namespace.
        assert!(list(&conn, &scratch_ctx("claude").namespace).unwrap().is_empty());
        assert_eq!(write(&conn, &WriteContext::owner(), "x.md", "x", 1), Err(ScratchError::NotScratch));
        // Rename a directory; refuse to land on an existing file.
        assert_eq!(rename(&conn, &ctx, "notes", "archive", 20).unwrap(), 2);
        assert!(read(&conn, &ctx.namespace, "archive/a.md").unwrap().is_some());
        assert_eq!(rename(&conn, &ctx, "archive/a.md", "archive/b.md", 21), Err(ScratchError::Exists));
        assert_eq!(rename(&conn, &ctx, "missing.md", "x.md", 21), Err(ScratchError::NotFound));
        // Delete a directory recursively, then the file.
        assert_eq!(delete(&conn, &ctx.namespace, "archive").unwrap(), 2);
        assert_eq!(delete(&conn, &ctx.namespace, "archive"), Err(ScratchError::NotFound));
        assert_eq!(delete(&conn, &ctx.namespace, "progress.md").unwrap(), 1);
        assert!(list(&conn, &ctx.namespace).unwrap().is_empty());
    }

    #[test]
    fn content_and_paths_are_bounded() {
        let conn = crate::open_in_memory().unwrap();
        let ctx = scratch_ctx("codex");
        let huge = "x".repeat(MAX_CONTENT_CHARS + 1);
        assert_eq!(write(&conn, &ctx, "big.md", &huge, 1), Err(ScratchError::TooLarge));
        assert_eq!(write(&conn, &ctx, "../x.md", "x", 1), Err(ScratchError::InvalidPath));
    }
}
