//! `memory.fs` on the store (plan E6-T2): the memory tool's six commands over the `/memories`
//! projection ([`crate::memfs`]) and the caller's scratch files (V100).
//!
//! Who is calling is the call's writer ([`current_write`]): the faces set it per request (the
//! multi-principal server from the agent key, the single-principal faces as the owner). Its
//! scratch directory is `/memories/scratch/<actor id>/`, backed by the `agent_scratch:<actor id>`
//! namespace; nothing under another agent's directory is visible or writable.
//!
//! Writes outside that directory never edit a file: `create` / `str_replace` / `insert` on a
//! world path become one `memory.ingest` event (title = the path, thread `memfs:<path>`, an
//! idempotency key derived from the command, so a retried call adds nothing) and the review
//! decides what becomes state; `delete` / `rename` there are refused.

use sha2::{Digest, Sha256};
use shogun_mcp::ingest_api::{IngestItem, IngestRequest, Namespace as WireNamespace};
use shogun_mcp::memfs_tool::{self, FsCommand, FsReply};
use shogun_memory::actor::{current_write, Namespace, WriteContext};
use shogun_memory::scratch_files::{self, ScratchError};

use super::Db;
use crate::memfs;

/// Longest thread key `memory.ingest` accepts (`ingest_api::MAX_THREAD_KEY_CHARS`).
const MAX_THREAD_CHARS: usize = 256;

fn scratch_error(path: &str, error: ScratchError) -> FsReply {
    FsReply::error(match error {
        ScratchError::NotFound => memfs_tool::missing(path),
        ScratchError::Exists => format!("Error: The destination {path} already exists"),
        ScratchError::InvalidPath => format!("Error: invalid path {path}."),
        ScratchError::TooLarge => format!("Error: the file is larger than {} characters.", scratch_files::MAX_CONTENT_CHARS),
        ScratchError::TooManyFiles => format!("Error: your scratch directory already holds {} files; delete some first.", scratch_files::MAX_FILES_PER_NAMESPACE),
        ScratchError::NotScratch => "Error: this caller has no scratch directory.".to_string(),
        ScratchError::Denied => format!("Error: no write access to {path}; the principal has not granted it."),
        ScratchError::Db => "Error: the memory store could not complete the operation; retry.".to_string(),
    })
}

/// Which part of the tree a path is in, for the calling agent.
enum Place<'a> {
    /// `/memories` itself.
    Root,
    /// Inside the caller's own scratch directory (the relative path), or the directory itself.
    OwnScratch(Option<&'a str>),
    /// Inside the scratch tree but not the caller's: invisible.
    OtherScratch,
    /// Anything else: a projection of the world model.
    World,
}

fn place<'a>(path: &'a str, scratch: &Namespace) -> Place<'a> {
    if path == memfs::ROOT {
        return Place::Root;
    }
    if let Some(dir) = memfs::scratch_dir(scratch) {
        if path == dir {
            return Place::OwnScratch(None);
        }
        if let Some(rel) = memfs::own_scratch_relative(path, scratch) {
            return Place::OwnScratch(Some(rel));
        }
    }
    if memfs::is_scratch_path(path) && path != memfs::SCRATCH_DIR {
        return Place::OtherScratch;
    }
    Place::World
}

impl Db {
    /// One `memory.fs` command; see the module docs.
    pub fn memory_fs(&self, command: &FsCommand) -> Result<FsReply, String> {
        let writer = current_write();
        let scratch_ctx = WriteContext::scratch(writer.actor.clone()).map_err(|_| "memory_fs_unavailable".to_string())?;
        let scratch = scratch_ctx.namespace.clone();
        let now = self.now_ms();
        match command {
            FsCommand::View { path, view_range, offset } => self.memfs_view(path, *view_range, *offset, &scratch),
            FsCommand::Create { path, file_text } => match place(path, &scratch) {
                Place::OwnScratch(Some(rel)) => self.scratch_write(path, |conn| {
                    scratch_files::write(conn, &scratch_ctx, rel, file_text, now)
                        .map(|_| FsReply::ok(format!("File created successfully at: {path}")))
                }),
                Place::Root | Place::OwnScratch(None) => Ok(FsReply::error(format!("Error: {path} is a directory; name a file under it."))),
                Place::OtherScratch => self.granted_scratch_write(&writer, path, file_text, now, "File created successfully at: "),
                Place::World => self.world_write(&writer, command, path, file_text),
            },
            FsCommand::StrReplace { path, old_str, new_str } => {
                let current = match self.memfs_read(path, &scratch)? {
                    Some(text) => text,
                    None => return Ok(FsReply::error(memfs_tool::missing(path))),
                };
                let edited = match memfs_tool::apply_str_replace(path, &current, old_str, new_str) {
                    Ok(edited) => edited,
                    Err(message) => return Ok(FsReply::error(message)),
                };
                match place(path, &scratch) {
                    Place::OwnScratch(Some(rel)) => self.scratch_write(path, |conn| {
                        scratch_files::write(conn, &scratch_ctx, rel, &edited, now)
                            .map(|_| FsReply::ok("The memory file has been edited."))
                    }),
                    _ => {
                        let text = if new_str.trim().is_empty() {
                            format!("No longer true: {old_str}")
                        } else {
                            new_str.clone()
                        };
                        self.world_write(&writer, command, path, &text)
                    }
                }
            }
            FsCommand::Insert { path, insert_line, insert_text } => {
                let current = match self.memfs_read(path, &scratch)? {
                    Some(text) => text,
                    None => return Ok(FsReply::error(memfs_tool::missing(path))),
                };
                let edited = match memfs_tool::apply_insert(&current, *insert_line, insert_text) {
                    Ok(edited) => edited,
                    Err(message) => return Ok(FsReply::error(message)),
                };
                match place(path, &scratch) {
                    Place::OwnScratch(Some(rel)) => self.scratch_write(path, |conn| {
                        scratch_files::write(conn, &scratch_ctx, rel, &edited, now)
                            .map(|_| FsReply::ok(format!("The file {path} has been edited.")))
                    }),
                    _ => self.world_write(&writer, command, path, insert_text),
                }
            }
            FsCommand::Delete { path } => match place(path, &scratch) {
                Place::Root => Ok(FsReply::error("Error: the /memories root cannot be deleted.")),
                Place::OwnScratch(Some(rel)) => self.scratch_write(path, |conn| {
                    scratch_files::delete(conn, &scratch, rel).map(|_| FsReply::ok(format!("Successfully deleted {path}")))
                }),
                Place::OwnScratch(None) => self.scratch_write(path, |conn| {
                    let files = scratch_files::list(conn, &scratch)?;
                    for file in &files {
                        scratch_files::delete(conn, &scratch, &file.path)?;
                    }
                    Ok(FsReply::ok(format!("Successfully deleted {path}")))
                }),
                Place::OtherScratch => Ok(FsReply::error(memfs_tool::missing(path))),
                Place::World => Ok(FsReply::error(format!(
                    "Error: {path} is generated from the world model and cannot be deleted. Correct a fact with state.update or state.delete, or write what changed to a file and let the review apply it."
                ))),
            },
            FsCommand::Rename { old_path, new_path } => match (place(old_path, &scratch), place(new_path, &scratch)) {
                (Place::OwnScratch(Some(from)), Place::OwnScratch(Some(to))) => self.scratch_write(old_path, |conn| {
                    scratch_files::rename(conn, &scratch_ctx, from, to, now)
                        .map(|_| FsReply::ok(format!("Successfully renamed {old_path} to {new_path}")))
                })
                .map(|reply| match reply.output.starts_with("Error: The destination") {
                    true => FsReply::error(format!("Error: The destination {new_path} already exists")),
                    false => reply,
                }),
                (Place::Root, _) | (_, Place::Root) => Ok(FsReply::error("Error: the /memories root cannot be renamed.")),
                (Place::OtherScratch, _) => Ok(FsReply::error(memfs_tool::missing(old_path))),
                _ => Ok(FsReply::error(format!(
                    "Error: only files in your scratch directory ({}) can be renamed; {old_path} or {new_path} is outside it.",
                    memfs::scratch_dir(&scratch).unwrap_or_default()
                ))),
            },
        }
    }

    /// A write into another agent's scratch file (E18): allowed only with a `write` grant, which
    /// the data layer checks. Without one the path answers as missing, so a caller learns nothing
    /// about another agent's files.
    fn granted_scratch_write(&self, writer: &WriteContext, path: &str, text: &str, now: i64, done: &str) -> Result<FsReply, String> {
        let Some((namespace, rel)) = memfs::scratch_target(path) else {
            return Ok(FsReply::error(memfs_tool::missing(path)));
        };
        let ctx = WriteContext { actor: writer.actor.clone(), namespace };
        let reply = self.scratch_write(path, |conn| {
            scratch_files::write(conn, &ctx, rel, text, now).map(|_| FsReply::ok(format!("{done}{path}")))
        })?;
        Ok(if reply.is_error { FsReply::error(memfs_tool::missing(path)) } else { reply })
    }

    fn memfs_read(&self, path: &str, scratch: &Namespace) -> Result<Option<String>, String> {
        self.with_conn("memory_fs.read", |conn| memfs::read(conn, path, Some(scratch)))
            .map_err(|_| "memory_fs_unavailable".to_string())
    }

    fn memfs_view(&self, path: &str, view_range: Option<(i64, i64)>, offset: usize, scratch: &Namespace) -> Result<FsReply, String> {
        if let Some(text) = self.memfs_read(path, scratch)? {
            return Ok(match memfs_tool::render_file(path, &text, view_range, offset) {
                Ok(view) => FsReply::ok(view),
                Err(message) => FsReply::error(message),
            });
        }
        let files = self
            .with_conn("memory_fs.list", |conn| memfs::files(conn, Some(scratch)))
            .map_err(|_| "memory_fs_unavailable".to_string())?;
        let prefix = format!("{path}/");
        let listed: Vec<(String, usize)> = files.into_iter().map(|entry| (entry.path, entry.bytes)).collect();
        let is_dir = path == memfs::ROOT
            || memfs::scratch_dir(scratch).is_some_and(|dir| dir == path || dir.starts_with(&prefix))
            || listed.iter().any(|(file, _)| file.starts_with(&prefix));
        if !is_dir || matches!(place(path, scratch), Place::OtherScratch) {
            return Ok(FsReply::error(memfs_tool::missing(path)));
        }
        Ok(FsReply::ok(memfs_tool::render_dir(path, &listed, offset)))
    }

    fn scratch_write(
        &self,
        path: &str,
        f: impl FnOnce(&rusqlite::Connection) -> Result<FsReply, ScratchError>,
    ) -> Result<FsReply, String> {
        match self.with_conn("memory_fs.scratch", |conn| Ok::<_, rusqlite::Error>(f(conn))) {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(error)) => Ok(scratch_error(path, error)),
            Err(_) => Err("memory_fs_unavailable".to_string()),
        }
    }

    /// A write to a world path: one `memory.ingest` event the review turns into state or not.
    fn world_write(&self, writer: &WriteContext, command: &FsCommand, path: &str, text: &str) -> Result<FsReply, String> {
        if text.trim().is_empty() {
            return Ok(FsReply::error(format!("Error: nothing to record for {path}: the text is empty.")));
        }
        let digest = Sha256::digest(format!("{}\u{0}{path}\u{0}{text}", command.name()).as_bytes());
        let key: String = digest.iter().take(24).map(|b| format!("{b:02x}")).collect();
        let thread: String = format!("memfs:{path}").chars().take(MAX_THREAD_CHARS).collect();
        let request = IngestRequest {
            items: vec![IngestItem { ts_ms: None, kind: "memory_fs".to_string(), title: Some(path.to_string()), content: text.to_string() }],
            turns: false,
            source: "memory.fs".to_string(),
            actor: writer.actor.as_str().to_string(),
            thread_key: Some(thread),
            metadata: None,
            idempotency_key: format!("memfs-{key}"),
            namespace: WireNamespace::World,
        };
        let outcome = self.agent_ingest(&request)?;
        if outcome.rejected > 0 {
            return Ok(FsReply::error(format!(
                "Error: not recorded. The text for {path} reads as an instruction to the assistant, which memory does not store."
            )));
        }
        Ok(FsReply {
            output: format!(
                "Recorded for review as event {}. {path} is generated from the world model and was not edited in place; the hourly review decides what becomes state.",
                outcome.event_ids.iter().map(i64::to_string).collect::<Vec<_>>().join(", ")
            ),
            is_error: false,
            event_ids: if outcome.replayed { Vec::new() } else { outcome.event_ids },
        })
    }
}

#[cfg(test)]
mod tests;
