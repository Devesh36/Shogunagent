//! Rewrite learning loop storage seams (#1066): outcomes, style notes, prompt context. All text
//! stays in the encrypted store; nothing here logs.
use super::*;
use shogun_memory::rewrite_loop as rl;

/// The volatile prompt blocks for one rewrite plus the notes shown in them.
#[derive(Debug, Clone, Default)]
pub struct RewritePromptContext {
    pub text: String,
    pub note_ids: Vec<i64>,
}

/// Who the writer is, from settings (never guessed from captured names).
#[derive(Debug, Clone, Default)]
pub struct WriterIdentity {
    pub name: Option<String>,
    pub role: Option<String>,
    pub languages: Option<String>,
}

impl Db {
    /// Assemble the always-on blocks. Few-shot raw text rides with the writing-style setting (on by default, V79;
    /// the user can switch it off); the notes, length prior and recipient history are derived
    /// and always on. Fails soft: an unreadable store yields an empty context.
    pub fn rewrite_prompt_context(
        &self,
        app: &str,
        partner: Option<&str>,
        who: &WriterIdentity,
        already_present: &str,
    ) -> RewritePromptContext {
        self.with_conn("rewrite_loop.context", |conn| {
            let shots_ok = shogun_memory::writing_voice::enabled(conn).unwrap_or(false);
            let notes = rl::active_notes(conn, app, partner, 6)?;
            let shots = if shots_ok { rl::few_shot(conn, app, partner, 3)? } else { vec![] };
            let person = match partner {
                Some(p) => rl::person_facts(conn, p).unwrap_or(None),
                None => None,
            };
            let history = match partner {
                Some(p) => rl::partner_facts(conn, app, p)?,
                None => None,
            };
            let inputs = crate::rewrite_context::Inputs {
                owner_name: who.name.as_deref(),
                owner_role: who.role.as_deref(),
                languages: who.languages.as_deref(),
                partner_label: partner.map(crate::writing_recipient::partner_label),
                person,
                history,
                prior: rl::length_prior(conn, app, partner)?,
                notes: &notes,
                shots: &shots,
                already_present,
            };
            Ok::<_, rusqlite::Error>(RewritePromptContext {
                text: crate::rewrite_context::render(&inputs),
                note_ids: notes.iter().map(|n| n.id).collect(),
            })
        })
        .unwrap_or_default()
    }

    /// Record a delivered rewrite; the id feeds the post-insert watch.
    pub fn rewrite_record_outcome(&self, o: &rl::NewOutcome<'_>) -> Option<i64> {
        self.with_conn_mut("rewrite_loop.record", |conn| rl::record_outcome(conn, o, (self.clock)())).ok().flatten()
    }

    pub fn rewrite_apply_observation(&self, id: i64, verdict: rl::Verdict, score: f64, text: &str) {
        let _ = self.with_conn("rewrite_loop.observe", |conn| rl::apply_observation(conn, id, verdict, score, text, (self.clock)()));
    }

    pub fn rewrite_apply_sent(&self, id: i64, sent: &str) {
        let _ = self.with_conn("rewrite_loop.sent", |conn| rl::apply_sent(conn, id, sent, (self.clock)()));
    }

    pub fn rewrite_style_notes(&self) -> Result<Vec<rl::Note>, String> {
        self.with_conn_reported("rewrite_loop.notes", |conn| rl::list_notes(conn).map_err(|_| "Could not load style notes".to_string()))
    }

    pub fn rewrite_summary(&self) -> Result<rl::Summary, String> {
        self.with_conn_reported("rewrite_loop.summary", |conn| rl::summary(conn).map_err(|_| "Could not load rewrite summary".to_string()))
    }

    pub fn rewrite_delete_note(&self, id: i64) -> Result<bool, String> {
        self.with_conn_reported("rewrite_loop.delete_note", |conn| rl::delete_note(conn, id).map_err(|_| "Could not delete style note".to_string()))
    }

    pub fn rewrite_rate_note(&self, id: i64, confirm: bool) -> Result<bool, String> {
        self.with_conn_reported("rewrite_loop.rate_note", |conn| {
            rl::rate_note(conn, id, confirm).map_err(|_| "Could not rate style note".to_string())
        })
    }

    pub(crate) fn rewrite_unmined(&self, cap: usize) -> Vec<rl::MiningRow> {
        self.with_conn("rewrite_loop.unmined", |conn| {
            rl::promote_repeated_instructions(conn, (self.clock)())?;
            rl::unmined(conn, cap)
        })
        .unwrap_or_default()
    }

    pub(crate) fn rewrite_unmined_backlog(&self) -> i64 {
        self.with_conn("rewrite_loop.backlog", rl::unmined_backlog).unwrap_or(-1)
    }

    /// Move history saved under a pre-normalisation `thread:` key ("thread:(2) x - google chrome –
    /// anant") to today's key ("thread:x"). Idempotent; no-op once every key is current.
    pub(crate) fn rewrite_rekey_partners(&self) {
        let _ = self.with_conn("rewrite_loop.rekey", |conn| {
            for old in rl::thread_partner_keys(conn)? {
                let new = crate::writing_recipient::partner_key("", "", old.strip_prefix("thread:"));
                if let Some(new) = new.filter(|n| *n != old) {
                    rl::rename_partner(conn, &old, &new)?;
                }
            }
            Ok::<_, rusqlite::Error>(())
        });
    }

    pub(crate) fn rewrite_active_notes_for_mining(&self) -> Vec<rl::Note> {
        self.with_conn("rewrite_loop.notes_for_mining", rl::list_notes).unwrap_or_default()
    }

    pub(crate) fn rewrite_apply_mining(&self, ids: &[i64], notes: &[crate::rewrite_mining::MinedNote], drop: &[i64]) {
        let now = (self.clock)();
        let _ = self.with_conn("rewrite_loop.apply_mining", |conn| {
            for n in notes {
                // Learned again from new cases: corroborate the note on file.
                if rl::add_note(conn, &n.scope, &n.app, &n.partner, "learned", &n.text, ids, now)?.is_none() {
                    rl::corroborate_note(conn, &n.scope, &n.app, &n.partner, &n.text)?;
                }
            }
            for id in drop {
                rl::supersede_note(conn, *id, now)?;
            }
            rl::mark_mined(conn, ids, now)
        });
    }

    pub(crate) fn rewrite_mining_failed(&self, ids: &[i64]) {
        let _ = self.with_conn("rewrite_loop.mining_failed", |conn| rl::bump_mine_attempts(conn, ids));
    }
}
