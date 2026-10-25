//! File exports into user-chosen folders (issue #397): the queue and the executor every face
//! shares. The desktop's "Save to folder" and an API caller's `export.write_file` both become a
//! `local_writes` row; only a confirmed row is written, and only by [`Db::local_write_run`].

use std::path::{Path, PathBuf};

use super::*;
use crate::file_export::{self, ExportError};
use shogun_agents::permission::{Action, ExportFormat, Level, LocalAction};
use shogun_memory::local_writes::{self, LocalWrite, WriteRequest};

/// The only export source today: a meeting's Recap.
pub const SOURCE_MEETING_RECAP: &str = "meeting_recap";
/// The file stem used when a meeting has no usable title.
const FALLBACK_STEM: &str = "Meeting recap";

/// A new export, as a face asks for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportAsk {
    pub session_id: i64,
    pub include_transcript: bool,
    pub format: ExportFormat,
    /// The allowed folder (or a folder beneath one).
    pub folder: String,
    /// The file stem; `None` → derived from the meeting title.
    pub name: Option<String>,
    /// `ui` (the click is the confirmation) or `api` (waits for the user).
    pub origin: &'static str,
}

impl Db {
    /// The document a Recap export writes. Markdown is exactly the `meeting.recap.export` text
    /// followed by the provenance footer; CSV is one row per decision / next action.
    pub fn export_render(
        &self,
        session_id: i64,
        include_transcript: bool,
        format: ExportFormat,
    ) -> Option<String> {
        match format {
            ExportFormat::Markdown => self
                .meeting_recap_markdown(session_id, include_transcript)
                .map(|md| md + &crate::meeting::export::recap_provenance_footer(session_id)),
            ExportFormat::Csv => {
                let recap = self.meeting_recap(session_id)?;
                let minutes = self.meeting_recap_full(session_id).map(|stored| {
                    crate::meeting::minutes::MeetingMinutes {
                        summary: stored.summary,
                        decisions: serde_json::from_str(&stored.decisions_json).unwrap_or_default(),
                        next_actions: serde_json::from_str(&stored.next_actions_json)
                            .unwrap_or_default(),
                        evidence: Vec::new(),
                        sections: Vec::new(),
                    }
                });
                Some(crate::meeting::export::recap_csv(&recap.title, session_id, minutes.as_ref()))
            }
        }
    }

    /// Record an export request after checking it can land: the session exists, the name is
    /// valid, and the target folder is inside an allowed folder — so a request that could never
    /// be written is refused now, not after the user confirmed it. A `ui` request is written at
    /// once (the click was the one tap); an `api` request waits in `awaiting_confirm`.
    ///
    /// The folder is checked first, so every face (the desktop's Save to folder and the API's
    /// `export.write_file`) refuses an outside folder with the same error whether or not the
    /// meeting exists.
    pub fn local_write_request(&self, ask: &ExportAsk, roots: &[PathBuf]) -> Result<LocalWrite, String> {
        let folder = file_export::resolve_folder(roots, &ask.folder).map_err(|e| e.as_str().to_string())?;
        let recap = self
            .meeting_recap(ask.session_id)
            .ok_or_else(|| "meeting not found".to_string())?;
        // Both stems are separator-free (`checked_stem` refuses one, `file_stem` replaces it), so
        // the file lands directly in the folder resolved above.
        let stem = match ask.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
            Some(name) => file_export::checked_stem(name, ask.format).map_err(|e| e.as_str().to_string())?,
            None => file_export::file_stem(&recap.title, FALLBACK_STEM),
        };
        let folder = folder.display().to_string();
        let now = self.now_ms();
        let row = self
            .with_conn("local_writes.request", |c| {
                local_writes::request(
                    c,
                    &WriteRequest {
                        source: SOURCE_MEETING_RECAP,
                        source_id: ask.session_id,
                        include_transcript: ask.include_transcript,
                        format: ask.format.extension(),
                        folder: &folder,
                        name: &stem,
                        origin: ask.origin,
                    },
                    now,
                )
            })
            .map_err(|fault| format!("export request failed: {}", fault.as_str()))?;
        if row.status == "writing" {
            return Ok(self.local_write_run(row, roots));
        }
        Ok(row)
    }

    /// The user confirms or cancels a request an AI client made. A confirmed request is written
    /// now; the returned row says how it ended. `None` when it was not waiting.
    pub fn local_write_decide(
        &self,
        id: i64,
        confirm: bool,
        roots: &[PathBuf],
    ) -> Result<Option<LocalWrite>, String> {
        let now = self.now_ms();
        let claimed = self
            .with_conn("local_writes.decide", |c| local_writes::decide(c, id, confirm, now))
            .map_err(|fault| format!("export decision failed: {}", fault.as_str()))?;
        match claimed {
            Some(row) => Ok(Some(self.local_write_run(row, roots))),
            None if !confirm => Ok(self.local_write(id)),
            None => Ok(None),
        }
    }

    /// Perform a confirmed (`writing`) row: render, run `LocalAction::WriteFile` at L2 inside the
    /// allowed folders, record path + bytes + digest (or the fixed failure reason).
    fn local_write_run(&self, row: LocalWrite, roots: &[PathBuf]) -> LocalWrite {
        let result = self.local_write_perform(&row, roots);
        let now = self.now_ms();
        let _ = self.with_conn("local_writes.finish", |c| match &result {
            Ok(w) => local_writes::finish(
                c,
                row.id,
                Ok((&w.path.display().to_string(), w.bytes as i64, &w.digest)),
                now,
            ),
            Err(e) => local_writes::finish(c, row.id, Err(e.as_str()), now),
        });
        self.local_write(row.id).unwrap_or(row)
    }

    fn local_write_perform(
        &self,
        row: &LocalWrite,
        roots: &[PathBuf],
    ) -> Result<file_export::Written, ExportError> {
        let format = ExportFormat::parse(&row.format).ok_or(ExportError::BadName)?;
        let body = self
            .export_render(row.source_id, row.include_transcript, format)
            .ok_or(ExportError::WriteFailed)?;
        let action = Action::Local(LocalAction::WriteFile {
            path_in_allowed_root: file_export::target_path(Path::new(&row.folder), &row.name, format)
                .display()
                .to_string(),
            format,
            body,
        });
        // The row reached `writing` only through a UI click or the user's confirm: that is the L2
        // one tap the action requires.
        file_export::execute(roots, &action, Level::L2)
    }

    pub fn local_write(&self, id: i64) -> Option<LocalWrite> {
        self.with_conn("local_writes.get", |c| local_writes::get(c, id)).ok().flatten()
    }

    pub fn local_writes_recent(&self, limit: usize) -> Vec<LocalWrite> {
        self.with_conn("local_writes.recent", |c| local_writes::recent(c, limit))
            .unwrap_or_default()
    }
}
