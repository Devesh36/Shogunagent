//! Bounded local correction observation and exact Undo.
use crate::daemon::Db;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use shogun_memory::voice_terms::{self, NewVoiceTerm, VoiceTermScope};

use crate::dictionary_learn::Learned;

const RETENTION_MS: i64 = 86_400_000;
/// Undo stops relearning a pair for 30 days (owner decision, 2026-10-19).
const SUPPRESSION_MS: i64 = 30 * RETENTION_MS;
/// v2: one fix is enough and the dictated text may be checked by the relay model. On by default
/// (2026-10-19 owner decision) until the user turns it off; only an in-app action turns it on.
const CONSENT_VERSION: i64 = 2;
const MAX_ROWS: i64 = 64;

#[derive(Clone, Debug, Serialize)]
pub struct AutoAdded {
    pub id: i64,
    pub original: String,
    pub corrected: String,
    pub created_at_ms: i64,
    /// The dictionary term this addition created or extended (None once Undo detached it).
    pub term_id: Option<i64>,
    /// `local` (on-device rule) or `model` (the relay picked it).
    pub source: String,
    /// Additions from one dictation share one notch notice and one Undo.
    pub group_id: Option<i64>,
}

#[derive(Serialize)]
pub struct AutoAddView {
    pub enabled: bool,
    pub added: Vec<AutoAdded>,
    /// Ids of live terms that auto-add created or extended (drives the Dictionary "Learned" tab).
    pub learned_term_ids: Vec<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum AutoAddAction {
    Enable { consent: bool },
    Disable,
    Undo { id: i64 },
    UndoGroup { group_id: i64 },
}

#[derive(Default)]
pub struct ObservationResult {
    pub active: bool,
    pub resolved: bool,
    pub added: Option<AutoAdded>,
}

fn generation(conn: &Connection) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT generation FROM dictionary_autoadd_settings
         WHERE id = 1 AND (user_set = 0 OR (enabled = 1 AND consent_version = ?1))",
        [CONSENT_VERSION],
        |r| r.get(0),
    )
    .optional()
}

pub(crate) fn prune(conn: &Connection, now: i64) -> rusqlite::Result<usize> {
    let cutoff = now.saturating_sub(RETENTION_MS);
    let mut stmt = conn.prepare(
        "SELECT original, corrected, created_at_ms FROM dictionary_autoadd_corrections
         WHERE state = 'undone' AND created_at_ms <= ?1",
    )?;
    let expired = stmt
        .query_map([cutoff], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    for (original, corrected, created_at) in expired {
        suppress_pair(conn, &original, &corrected, created_at, "undo")?;
    }
    conn.execute(
        "DELETE FROM dictionary_autoadd_suppression WHERE created_at_ms <= ?1",
        [now.saturating_sub(SUPPRESSION_MS)],
    )?;
    conn.execute(
        "DELETE FROM dictionary_autoadd_corrections WHERE created_at_ms <= ?1",
        [cutoff],
    )
}

fn pair_digest(conn: &Connection, original: &str, corrected: &str) -> rusqlite::Result<Vec<u8>> {
    let key: Vec<u8> = conn.query_row(
        "SELECT key FROM dictionary_autoadd_secret WHERE id = 1",
        [],
        |r| r.get(0),
    )?;
    let mut digest = Sha256::new();
    digest.update(key);
    digest.update((original.len() as u64).to_le_bytes());
    digest.update(original.as_bytes());
    digest.update(corrected.as_bytes());
    Ok(digest.finalize().to_vec())
}

fn suppress_pair(
    conn: &Connection,
    original: &str,
    corrected: &str,
    created_at: i64,
    reason: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO dictionary_autoadd_suppression (pair_digest, created_at_ms, reason) VALUES (?1, ?2, ?3)",
        params![pair_digest(conn, original, corrected)?, created_at, reason],
    )?;
    Ok(())
}

fn find_alias_id(tx: &Connection, term_id: i64, alias: &str) -> rusqlite::Result<Option<i64>> {
    tx.query_row(
        "SELECT id FROM voice_term_aliases WHERE term_id = ?1 AND alias = ?2",
        params![term_id, alias],
        |r| r.get(0),
    )
    .optional()
}

fn terminal_capacity(error: &rusqlite::Error) -> bool {
    matches!(error, rusqlite::Error::InvalidParameterName(message) if message.contains("capacity reached"))
}

fn finish_observation<H>(
    tx: rusqlite::Transaction<'_>,
    expected_generation: i64,
    before_commit: &mut impl FnMut() -> Option<H>,
    result: ObservationResult,
) -> rusqlite::Result<ObservationResult> {
    let Some(_guard) = before_commit() else {
        return Ok(ObservationResult::default());
    };
    if generation(&tx)? != Some(expected_generation) {
        return Ok(ObservationResult::default());
    }
    tx.commit()?;
    Ok(result)
}

fn conflicting_alias(
    tx: &Connection,
    spelling: &str,
    except_id: Option<i64>,
) -> rusqlite::Result<bool> {
    tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM voice_term_aliases a JOIN voice_terms t ON t.id = a.term_id
         WHERE a.alias_normalized = ?1 AND t.id != ?2)",
        params![
            voice_terms::normalized_alias(spelling),
            except_id.unwrap_or(-1)
        ],
        |r| r.get(0),
    )
}

fn mark_suppressed(tx: &Connection, id: i64) -> rusqlite::Result<()> {
    tx.execute(
        "UPDATE dictionary_autoadd_corrections SET state = 'suppressed' WHERE id = ?1",
        [id],
    )?;
    Ok(())
}

/// Put one correction row's pair into the dictionary and mark the row `added`. `None` when it
/// conflicts with the user's terms or capacity (the row is then `suppressed`). Shared by the
/// two-sighting rule and single-fix learning.
fn add_pair(
    tx: &rusqlite::Transaction<'_>,
    id: i64,
    original: &str,
    corrected: &str,
    now: i64,
    observation_id: i64,
) -> rusqlite::Result<Option<i64>> {
    let existing: Option<ExistingTerm> = tx.query_row(
        "SELECT id, canonical, locale, scope, scope_ref, enabled FROM voice_terms
         WHERE canonical = ?1 COLLATE NOCASE
         ORDER BY CASE WHEN locale IS NULL AND scope = 'surface' AND scope_ref = 'voice_dictation' THEN 0 ELSE 1 END, id LIMIT 1",
        [corrected], |r| Ok(ExistingTerm {
            id: r.get(0)?, canonical: r.get(1)?, locale: r.get(2)?,
            scope: r.get(3)?, scope_ref: r.get(4)?, enabled: r.get(5)?,
        }),
    ).optional()?;
    let target_id = existing.as_ref().map(|term| term.id);
    let other_canonical: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM voice_terms WHERE canonical = ?1 COLLATE NOCASE AND id != ?2)",
        params![corrected, target_id.unwrap_or(-1)], |r| r.get(0),
    )?;
    if other_canonical || conflicting_alias(tx, original, target_id)?
        || conflicting_alias(tx, corrected, target_id)? {
        mark_suppressed(tx, id)?;
        return Ok(None);
    }
    let mut created_term = false;
    let term_id;
    let alias_id;
    let canonical_alias_id;
    if let Some(existing) = existing {
        if existing.canonical != corrected || existing.locale.is_some() || existing.scope != "surface"
            || existing.scope_ref.as_deref() != Some("voice_dictation") || !existing.enabled {
            mark_suppressed(tx, id)?;
            return Ok(None);
        }
        term_id = existing.id;
        alias_id = match voice_terms::add_voice_term_alias_in_transaction(tx, existing.id, original, now) {
            Ok(id) => id,
            Err(error) if terminal_capacity(&error) => {
                mark_suppressed(tx, id)?;
                suppress_pair(tx, original, corrected, now, "capacity")?;
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        canonical_alias_id = None;
    } else {
        let term = match voice_terms::create_voice_term_in_transaction(tx, &NewVoiceTerm {
            canonical: corrected.to_string(),
            // Keyterm-only terms (original == corrected) store just the canonical row.
            aliases: if original == corrected { Vec::new() } else { vec![original.to_string()] },
            locale: None, scope: Some(VoiceTermScope::Surface),
            scope_ref: Some("voice_dictation".into()), priority: None, enabled: true,
        }, now) {
            Ok(term) => term,
            Err(error) if terminal_capacity(&error) => {
                mark_suppressed(tx, id)?;
                suppress_pair(tx, original, corrected, now, "capacity")?;
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        term_id = term.id;
        alias_id = find_alias_id(tx, term.id, original)?;
        canonical_alias_id = find_alias_id(tx, term.id, corrected)?;
        created_term = true;
    }
    let Some(alias_id) = alias_id else {
        if created_term {
            tx.execute("DELETE FROM voice_terms WHERE id = ?1", [term_id])?;
        }
        mark_suppressed(tx, id)?;
        return Ok(None);
    };
    tx.execute(
        "INSERT INTO dictionary_autoadd_artifacts
         (correction_id, term_id, alias_id, canonical_alias_id, created_term, created_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![id, term_id, alias_id, canonical_alias_id, created_term, now],
    )?;
    tx.execute(
        "UPDATE dictionary_autoadd_corrections SET observations = 2, state = 'added', created_at_ms = ?5,
         last_observation_id = ?7,
         term_id = ?2, alias_id = ?3, canonical_alias_id = ?4,
         term_updated_at_ms = ?5, created_term = ?6 WHERE id = ?1",
        params![id, term_id, alias_id, canonical_alias_id, now, created_term, observation_id],
    )?;
    Ok(Some(term_id))
}

struct UndoRecord {
    term_id: i64,
    alias_id: i64,
    original: String,
    corrected: String,
    created_at: i64,
}

struct ExistingTerm {
    id: i64,
    canonical: String,
    locale: Option<String>,
    scope: String,
    scope_ref: Option<String>,
    enabled: bool,
}

fn undo(tx: &Connection, id: i64) -> rusqlite::Result<bool> {
    let record: Option<UndoRecord> = tx.query_row(
        "SELECT a.term_id, a.alias_id, c.original, c.corrected, c.created_at_ms
         FROM dictionary_autoadd_corrections c JOIN dictionary_autoadd_artifacts a ON a.correction_id = c.id
         WHERE c.id = ?1 AND c.state = 'added' AND a.alias_id IS NOT NULL",
        [id], |r| Ok(UndoRecord {
            term_id: r.get(0)?, alias_id: r.get(1)?, original: r.get(2)?, corrected: r.get(3)?, created_at: r.get(4)?,
        }),
    ).optional()?;
    let Some(UndoRecord {
        term_id,
        alias_id,
        original,
        corrected,
        created_at,
    }) = record
    else {
        return Ok(false);
    };
    let alias_matches: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM voice_term_aliases WHERE id = ?1 AND term_id = ?2 AND alias = ?3)",
        params![alias_id, term_id, original], |r| r.get(0),
    )?;
    if !alias_matches {
        return Ok(false);
    }
    let unchanged: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM voice_terms WHERE id = ?1 AND canonical = ?2
          AND locale IS NULL AND scope = 'surface' AND scope_ref = 'voice_dictation' AND enabled = 1)",
        params![term_id, corrected], |r| r.get(0),
    )?;
    if !unchanged {
        return Ok(false);
    }
    if original == corrected {
        // Keyterm-only: its one alias row is the canonical. Remove the term only while it is still
        // just that row; a user who added aliases has made it theirs.
        let remaining: i64 = tx.query_row(
            "SELECT count(*) FROM voice_term_aliases WHERE term_id = ?1",
            [term_id],
            |r| r.get(0),
        )?;
        if remaining != 1 {
            return Ok(false);
        }
        tx.execute("DELETE FROM voice_terms WHERE id = ?1 AND priority = 0", [term_id])?;
        tx.execute("DELETE FROM dictionary_autoadd_artifacts WHERE term_id = ?1", [term_id])?;
    } else {
        tx.execute(
            "DELETE FROM voice_term_aliases WHERE id = ?1 AND term_id = ?2",
            params![alias_id, term_id],
        )?;
        tx.execute(
            "UPDATE dictionary_autoadd_artifacts SET alias_id = NULL WHERE correction_id = ?1",
            [id],
        )?;
        let canonical_id: Option<i64> = tx.query_row(
            "SELECT canonical_alias_id FROM dictionary_autoadd_artifacts WHERE term_id = ?1 AND created_term = 1 LIMIT 1",
            [term_id], |r| r.get(0),
        ).optional()?.flatten();
        if let Some(canonical_id) = canonical_id {
            // A manual update replaces aliases and clears ownership; only the original canonical row
            // may remain when deleting an automatically created term.
            let remaining: i64 = tx.query_row(
                "SELECT count(*) FROM voice_term_aliases WHERE term_id = ?1",
                [term_id],
                |r| r.get(0),
            )?;
            let canonical_matches: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM voice_term_aliases WHERE id = ?1 AND term_id = ?2 AND alias = ?3)",
                params![canonical_id, term_id, corrected], |r| r.get(0),
            )?;
            if remaining == 1 && canonical_matches {
                tx.execute(
                    "DELETE FROM voice_terms WHERE id = ?1 AND priority = 0",
                    [term_id],
                )?;
                tx.execute(
                    "DELETE FROM dictionary_autoadd_artifacts WHERE term_id = ?1",
                    [term_id],
                )?;
            }
        }
    }
    suppress_pair(tx, &original, &corrected, created_at, "undo")?;
    tx.execute(
        "UPDATE dictionary_autoadd_corrections SET state = 'undone', term_id = NULL,
         alias_id = NULL, canonical_alias_id = NULL, term_updated_at_ms = NULL WHERE id = ?1",
        [id],
    )?;
    Ok(true)
}

impl Db {
    pub fn dictionary_autoadd_view(&self) -> Result<AutoAddView, String> {
        self.with_conn("dictionary_autoadd.view", |conn| {
            let enabled = generation(conn)?.is_some();
            // Newest addition first. The id is assigned while a correction is still pending, so a
            // word added after a newer one has the lower id; `created_at_ms` is set when it is added.
            let mut stmt = conn.prepare(
                "SELECT id, original, corrected, created_at_ms, term_id, source, group_id FROM dictionary_autoadd_corrections
                 WHERE state = 'added' AND created_at_ms > ?1 ORDER BY created_at_ms DESC, id DESC",
            )?;
            let added = stmt
                .query_map([self.now_ms().saturating_sub(RETENTION_MS)], |r| {
                    Ok(AutoAdded {
                        id: r.get(0)?,
                        original: r.get(1)?,
                        corrected: r.get(2)?,
                        created_at_ms: r.get(3)?,
                        term_id: r.get(4)?,
                        source: r.get(5)?,
                        group_id: r.get(6)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let learned_term_ids = conn
                .prepare(
                    "SELECT DISTINCT a.term_id FROM dictionary_autoadd_artifacts a
                     JOIN voice_terms t ON t.id = a.term_id
                     WHERE a.created_term = 1 OR a.alias_id IS NOT NULL ORDER BY a.term_id",
                )?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<i64>>>()?;
            Ok::<_, rusqlite::Error>(AutoAddView { enabled, added, learned_term_ids })
        })
        .map_err(|_| "Could not load dictionary auto-add state".into())
    }

    /// Only a direct desktop action or a confirmed shared approval may call this.
    pub fn enable_dictionary_autoadd_after_approval(&self) -> Result<(), String> {
        self.with_conn_mut("dictionary_autoadd.enable", |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute(
                "UPDATE dictionary_autoadd_settings SET enabled = 1, consent_version = ?1, user_set = 1,
                 generation = generation + 1 WHERE id = 1",
                [CONSENT_VERSION],
            )?;
            tx.execute(
                "DELETE FROM dictionary_autoadd_corrections WHERE state = 'pending'",
                [],
            )?;
            tx.commit()
        })
        .map_err(|_| "Could not enable dictionary auto-add".to_string())?;
        crate::panel_events::notify(crate::panel_events::PanelPart::DictionaryAutoAdd);
        Ok(())
    }

    /// Generic API write path refuses enable even with caller-supplied consent.
    pub fn dictionary_autoadd_action(&self, action: AutoAddAction) -> Result<(), String> {
        match action {
            AutoAddAction::Enable { .. } => {
                Err("in-app approval required to enable dictionary auto-add".into())
            }
            AutoAddAction::Disable => {
                self.with_conn_mut("dictionary_autoadd.disable", |conn| {
                    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    tx.execute("UPDATE dictionary_autoadd_settings SET enabled = 0, user_set = 1, generation = generation + 1 WHERE id = 1", [])?;
                    tx.execute("DELETE FROM dictionary_autoadd_corrections WHERE state = 'pending'", [])?;
                    tx.commit()
                }).map_err(|_| "Could not disable dictionary auto-add".to_string())?;
                crate::panel_events::notify(crate::panel_events::PanelPart::DictionaryAutoAdd);
                Ok(())
            }
            AutoAddAction::Undo { .. } | AutoAddAction::UndoGroup { .. } => {
                let restored = self
                    .with_conn_mut("dictionary_autoadd.undo", |conn| {
                        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                        prune(&tx, self.now_ms())?;
                        let ids = match action {
                            AutoAddAction::UndoGroup { group_id } => tx
                                .prepare("SELECT id FROM dictionary_autoadd_corrections WHERE group_id = ?1 AND state = 'added'")?
                                .query_map([group_id], |r| r.get(0))?
                                .collect::<rusqlite::Result<Vec<i64>>>()?,
                            AutoAddAction::Undo { id } => vec![id],
                            _ => Vec::new(),
                        };
                        let mut restored = false;
                        for id in ids {
                            restored |= undo(&tx, id)?;
                        }
                        tx.commit()?;
                        Ok::<_, rusqlite::Error>(restored)
                    })
                    .map_err(|_| "Could not undo dictionary auto-add".to_string())?;
                if !restored {
                    return Err(
                        "Dictionary term changed or auto-add not found; review it in Settings"
                            .into(),
                    );
                }
                crate::panel_events::notify(crate::panel_events::PanelPart::DictionaryAutoAdd);
                Ok(())
            }
        }
    }

    pub fn dictionary_observation_generation(&self) -> Option<i64> {
        self.with_conn("dictionary_autoadd.generation", generation)
            .ok()
            .flatten()
    }

    /// The retired two-sighting rule (no production caller since single-fix local learning); kept
    /// only so the pending-row, undo and suppression tests keep exercising those paths.
    #[cfg(test)]
    pub fn observe_dictionary_correction_guarded<G>(
        &self,
        expected_generation: i64,
        observation_id: u64,
        read: impl FnOnce() -> Option<(String, String, G)>,
    ) -> ObservationResult {
        self.observe_dictionary_correction_with_commit(
            expected_generation,
            observation_id,
            read,
            || Some(()),
        )
    }

    /// Caller keeps capture guard alive and validates session ownership under its commit fence.
    #[cfg(test)]
    pub fn observe_dictionary_correction_with_commit<G, H>(
        &self,
        expected_generation: i64,
        observation_id: u64,
        read: impl FnOnce() -> Option<(String, String, G)>,
        mut before_commit: impl FnMut() -> Option<H>,
    ) -> ObservationResult {
        let Ok(observation_id) = i64::try_from(observation_id) else {
            return ObservationResult::default();
        };
        match self.with_conn("dictionary_autoadd.generation", generation) {
            Ok(Some(current)) if current == expected_generation => {}
            Ok(_) => return ObservationResult::default(),
            Err(_) => {
                return ObservationResult {
                    active: true,
                    ..Default::default()
                }
            }
        }
        let Some((before, after, guard)) = read() else {
            return ObservationResult {
                active: true,
                ..Default::default()
            };
        };
        let Some((original, corrected)) = spelling_change(&before, &after) else {
            return ObservationResult {
                active: true,
                ..Default::default()
            };
        };
        let result = self.with_conn_mut("dictionary_autoadd.observe", |conn| -> rusqlite::Result<ObservationResult> {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if generation(&tx)? != Some(expected_generation) {
                return Ok(ObservationResult::default());
            }
            let now = self.now_ms();
            prune(&tx, now)?;
            let suppressed: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM dictionary_autoadd_suppression WHERE pair_digest = ?1)",
                [pair_digest(&tx, &original, &corrected)?], |r| r.get(0),
            )?;
            if suppressed {
                return finish_observation(tx, expected_generation, &mut before_commit, ObservationResult { active: true, resolved: true, added: None });
            }
            let previous: Option<(i64, i64, String, i64)> = tx.query_row(
                "SELECT id, observations, state, last_observation_id FROM dictionary_autoadd_corrections
                 WHERE original = ?1 AND corrected = ?2",
                params![original, corrected], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            ).optional()?;
            let id = if let Some((id, count, state, last_observation_id)) = previous {
                if state != "pending" { return finish_observation(tx, expected_generation, &mut before_commit, ObservationResult { active: true, resolved: true, added: None }); }
                if count != 1 { return finish_observation(tx, expected_generation, &mut before_commit, ObservationResult { active: true, resolved: true, added: None }); }
                if last_observation_id == observation_id {
                    return finish_observation(tx, expected_generation, &mut before_commit, ObservationResult { active: true, resolved: true, added: None });
                }
                id
            } else {
                let pending: i64 = tx.query_row("SELECT count(*) FROM dictionary_autoadd_corrections WHERE state = 'pending'", [], |r| r.get(0))?;
                let total: i64 = tx.query_row("SELECT count(*) FROM dictionary_autoadd_corrections", [], |r| r.get(0))?;
                if pending >= 8 || total >= 64 {
                    return finish_observation(tx, expected_generation, &mut before_commit, ObservationResult { active: true, resolved: true, added: None });
                }
                tx.execute(
                    "INSERT INTO dictionary_autoadd_corrections (original, corrected, created_at_ms, last_observation_id)
                     VALUES (?1, ?2, ?3, ?4)", params![original, corrected, now, observation_id],
                )?;
                return finish_observation(tx, expected_generation, &mut before_commit, ObservationResult { active: true, resolved: true, added: None });
            };
            let Some(term_id) = add_pair(&tx, id, &original, &corrected, now, observation_id)? else {
                return finish_observation(tx, expected_generation, &mut before_commit, ObservationResult { active: true, resolved: true, added: None });
            };
            finish_observation(tx, expected_generation, &mut before_commit, ObservationResult {
                active: true, resolved: true,
                added: Some(AutoAdded {
                    id, original, corrected, created_at_ms: now, term_id: Some(term_id),
                    source: "local".into(), group_id: None,
                }),
            })
        }).unwrap_or(ObservationResult { active: true, ..Default::default() });
        drop(guard);
        if result.added.is_some() {
            crate::panel_events::notify(crate::panel_events::PanelPart::DictionaryAutoAdd);
        }
        result
    }

    /// Single-fix learning (v2): add each validated term in one transaction under the caller's
    /// commit fence. The additions form one group: one notch notice, one Undo.
    pub fn commit_learned_terms<H>(
        &self,
        expected_generation: i64,
        observation_id: u64,
        terms: &[(Learned, &'static str)],
        mut before_commit: impl FnMut() -> Option<H>,
    ) -> Vec<AutoAdded> {
        let Ok(observation_id) = i64::try_from(observation_id) else {
            return Vec::new();
        };
        let added = self
            .with_conn_mut("dictionary_autoadd.learn", |conn| -> rusqlite::Result<Vec<AutoAdded>> {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                if generation(&tx)? != Some(expected_generation) {
                    return Ok(Vec::new());
                }
                let now = self.now_ms();
                prune(&tx, now)?;
                let mut added: Vec<AutoAdded> = Vec::new();
                for (learned, source) in terms {
                    let corrected = learned.term.as_str();
                    // An alias that normalises to the term is no alias (the dictionary would drop
                    // it and Undo could not find it): learn it keyterm-only.
                    let original = learned
                        .heard
                        .as_deref()
                        .filter(|heard| !crate::dictionary_learn::same_spelling(heard, corrected))
                        .unwrap_or(corrected);
                    if ![original, corrected].iter().all(|s| (2..=60).contains(&s.chars().count())) {
                        continue;
                    }
                    let suppressed: bool = tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM dictionary_autoadd_suppression WHERE pair_digest = ?1)",
                        [pair_digest(&tx, original, corrected)?],
                        |r| r.get(0),
                    )?;
                    if suppressed {
                        continue;
                    }
                    if original == corrected {
                        // A keyterm-only term the dictionary already knows adds nothing.
                        let known: bool = tx.query_row(
                            "SELECT EXISTS(SELECT 1 FROM voice_terms WHERE canonical = ?1 COLLATE NOCASE)",
                            [corrected],
                            |r| r.get(0),
                        )?;
                        if known || conflicting_alias(&tx, corrected, None)? {
                            continue;
                        }
                    }
                    let previous: Option<(i64, String)> = tx
                        .query_row(
                            "SELECT id, state FROM dictionary_autoadd_corrections WHERE original = ?1 AND corrected = ?2",
                            params![original, corrected],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        )
                        .optional()?;
                    let id = match previous {
                        Some((id, state)) if state == "pending" => {
                            tx.execute(
                                "UPDATE dictionary_autoadd_corrections SET source = ?2 WHERE id = ?1",
                                params![id, *source],
                            )?;
                            id
                        }
                        Some(_) => continue,
                        None => {
                            let total: i64 = tx.query_row(
                                "SELECT count(*) FROM dictionary_autoadd_corrections",
                                [],
                                |r| r.get(0),
                            )?;
                            if total >= MAX_ROWS {
                                break;
                            }
                            tx.execute(
                                "INSERT INTO dictionary_autoadd_corrections
                                 (original, corrected, created_at_ms, last_observation_id, source)
                                 VALUES (?1, ?2, ?3, ?4, ?5)",
                                params![original, corrected, now, observation_id, *source],
                            )?;
                            tx.last_insert_rowid()
                        }
                    };
                    if let Some(term_id) = add_pair(&tx, id, original, corrected, now, observation_id)? {
                        added.push(AutoAdded {
                            id,
                            original: original.to_string(),
                            corrected: corrected.to_string(),
                            created_at_ms: now,
                            term_id: Some(term_id),
                            source: (*source).to_string(),
                            group_id: None,
                        });
                    }
                }
                let group_id = added.first().map(|a| a.id);
                for addition in &mut added {
                    addition.group_id = group_id;
                    tx.execute(
                        "UPDATE dictionary_autoadd_corrections SET group_id = ?2 WHERE id = ?1",
                        params![addition.id, group_id],
                    )?;
                }
                let done = ObservationResult { active: true, resolved: true, added: None };
                let committed = finish_observation(tx, expected_generation, &mut before_commit, done)?;
                Ok(if committed.active { added } else { Vec::new() })
            })
            .unwrap_or_default();
        if !added.is_empty() {
            crate::panel_events::notify(crate::panel_events::PanelPart::DictionaryAutoAdd);
            eprintln!("[dictionary] notice pushed ({})", added.len());
        }
        added
    }

    /// SHOGUN's own people and project names, most recently mentioned first: local recognition
    /// hints for learning. Never sent as Deepgram keyterms (they biased ordinary speech).
    pub fn dictionary_recognition_names(&self, limit: i64) -> Vec<String> {
        use shogun_memory::{entity_pages::visible_sql, state::StateTable};
        self.with_conn("dictionary_autoadd.names", |conn| {
            // The rows the lists show: no singletons, non-entities or task labels (#1340).
            conn.prepare(&format!(
                "SELECT name FROM (
                   SELECT display_name AS name, COALESCE(last_mentioned_at, last_evidence_at) AS seen
                   FROM people WHERE {} OR COALESCE(person_class, '') = 'self'
                   UNION ALL
                   SELECT name, COALESCE(last_mentioned_at, last_evidence_at) FROM projects
                   WHERE status != 'done' AND {}
                 ) ORDER BY seen DESC LIMIT ?1",
                visible_sql(StateTable::People, "people"),
                visible_sql(StateTable::Projects, "projects")
            ))?
            .query_map([limit], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()
        })
        .unwrap_or_default()
    }
}

/// Only one changed word (or up to three words joined into one). Surrounding text must match.
/// ponytail: conservative spelling heuristic misses distant phonetic errors; expand from real cases.
pub fn spelling_change(before: &str, after: &str) -> Option<(String, String)> {
    if before.len() > 16_384 || after.len() > 16_384 || before == after {
        return None;
    }
    let old: Vec<&str> = before.split_whitespace().collect();
    let new: Vec<&str> = after.split_whitespace().collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let old = &old[prefix..old.len() - suffix];
    let new = &new[prefix..new.len() - suffix];
    if !(1..=3).contains(&old.len()) || new.len() != 1 {
        return None;
    }
    // Strip only sentence punctuation, never path/email/code separators.
    let trim = |s: &str| {
        s.trim_matches(['.', ',', '!', '?', ';', ':', '"', '(', ')'])
            .to_string()
    };
    let original = trim(&old.join(" "));
    let corrected = trim(new[0]);
    if original == corrected
        || original.to_lowercase() == corrected.to_lowercase()
        || !(2..=60).contains(&corrected.chars().count())
        || !(2..=60).contains(&original.chars().count())
        || !original.chars().all(|c| c.is_alphabetic() || c == ' ')
        || !corrected.chars().all(char::is_alphabetic)
    {
        return None;
    }
    if ordinary_word_pair(&original, &corrected) {
        return None;
    }
    let a: Vec<char> = original
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    let b: Vec<char> = corrected.chars().flat_map(char::to_lowercase).collect();
    if a.first() != b.first() || a.len().abs_diff(b.len()) > 2 {
        return None;
    }
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ac) in a.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, bc) in b.iter().enumerate() {
            let previous = row[j + 1];
            row[j + 1] = (row[j] + 1)
                .min(previous + 1)
                .min(diagonal + usize::from(ac != bc));
            diagonal = previous;
        }
    }
    // Adjacent transpositions are common spelling corrections, even in short names.
    let swapped = a
        .iter()
        .zip(&b)
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    let distance = match swapped.as_slice() {
        [i, j] if a.len() == b.len() && *j == *i + 1 && a[*i] == b[*j] && a[*j] == b[*i] => 1,
        _ => *row.last()?,
    };
    if distance > if a.len().max(b.len()) >= 6 { 2 } else { 1 } {
        return None;
    }
    Some((original, corrected))
}

fn ordinary_word_pair(original: &str, corrected: &str) -> bool {
    if !original.bytes().all(|byte| byte.is_ascii_alphabetic())
        || !corrected.bytes().all(|byte| byte.is_ascii_alphabetic())
    {
        return false;
    }
    // macOS ships broad local word and proper-name lists. Without them, suppress lowercase
    // pairs; keep capitalized names eligible rather than silently disable name learning.
    let words = crate::dictionary_learn::word_lists();
    words.map_or_else(|| {
        original.bytes().all(|byte| byte.is_ascii_lowercase())
            && corrected.bytes().all(|byte| byte.is_ascii_lowercase())
    }, |(common, proper)| {
        if proper.contains(original) && proper.contains(corrected) { return false; }
        common.contains(&original.to_ascii_lowercase()) && common.contains(&corrected.to_ascii_lowercase())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::Arc;

    #[test]
    fn two_insertions_add_once_and_undo_suppresses_relearning(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(Arc::new(|| 100))?;
        assert!(db.dictionary_observation_generation().is_some(), "on by default");
        assert!(db
            .dictionary_autoadd_action(AutoAddAction::Enable { consent: true })
            .is_err());
        db.enable_dictionary_autoadd_after_approval()?;
        let generation = db
            .dictionary_observation_generation()
            .ok_or("no generation")?;
        let observe = |session| {
            db.observe_dictionary_correction_guarded(generation, session, || {
                Some(("Hi Jon".into(), "Hi John".into(), ()))
            })
        };
        assert!(observe(1).resolved);
        assert!(
            observe(1).added.is_none(),
            "two polls of one insertion cannot learn"
        );
        assert!(db.list_voice_terms()?.is_empty());
        let added = observe(2).added.ok_or("second insertion did not add")?;
        assert_eq!(added.corrected, "John");
        assert_eq!(db.list_voice_terms()?.len(), 1);
        db.dictionary_autoadd_action(AutoAddAction::Undo { id: added.id })?;
        assert!(db.list_voice_terms()?.is_empty());
        assert!(
            observe(3).added.is_none(),
            "Undo must suppress immediate relearning"
        );
        assert!(db.dictionary_autoadd_view()?.added.is_empty());
        Ok(())
    }

    #[test]
    fn preexisting_term_is_learned_only_while_its_auto_alias_exists(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(Arc::new(|| 100))?;
        let term = db.create_voice_term(&NewVoiceTerm {
            canonical: "John".into(),
            aliases: vec!["Johnny".into()],
            locale: None,
            scope: Some(VoiceTermScope::Surface),
            scope_ref: Some("voice_dictation".into()),
            priority: Some(7),
            enabled: true,
        })?;
        db.enable_dictionary_autoadd_after_approval()?;
        let generation = db.dictionary_observation_generation().ok_or("no generation")?;
        for session in 1..=2 {
            db.observe_dictionary_correction_guarded(generation, session, || {
                Some(("Hi Jon".into(), "Hi John".into(), ()))
            });
        }
        let view = db.dictionary_autoadd_view()?;
        assert_eq!(view.learned_term_ids, [term.id]);
        db.dictionary_autoadd_action(AutoAddAction::Undo { id: view.added[0].id })?;
        assert_eq!(db.list_voice_terms()?.len(), 1, "the user's term survives");
        assert!(db.dictionary_autoadd_view()?.learned_term_ids.is_empty());
        Ok(())
    }

    #[test]
    fn undo_removes_only_its_alias_from_preexisting_term() -> Result<(), Box<dyn std::error::Error>>
    {
        let db = Db::open_in_memory(Arc::new(|| 100))?;
        let term = db.create_voice_term(&NewVoiceTerm {
            canonical: "John".into(),
            aliases: vec!["Johnny".into()],
            locale: None,
            scope: Some(VoiceTermScope::Surface),
            scope_ref: Some("voice_dictation".into()),
            priority: Some(7),
            enabled: true,
        })?;
        db.enable_dictionary_autoadd_after_approval()?;
        let generation = db
            .dictionary_observation_generation()
            .ok_or("no generation")?;
        for session in 1..=2 {
            let result = db.observe_dictionary_correction_guarded(generation, session, || {
                Some(("Hi Jon".into(), "Hi John".into(), ()))
            });
            assert!(result.resolved);
        }
        let id = db.dictionary_autoadd_view()?.added[0].id;
        db.dictionary_autoadd_action(AutoAddAction::Undo { id })?;
        let after = db.list_voice_terms()?;
        assert_eq!(after[0].id, term.id);
        assert_eq!(after[0].priority, 7);
        assert_eq!(after[0].aliases, vec!["Johnny", "John"]);
        Ok(())
    }

    #[test]
    fn existing_alias_is_never_claimed_or_removed_by_autoadd(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(Arc::new(|| 100))?;
        let term = db.create_voice_term(&NewVoiceTerm {
            canonical: "John".into(),
            aliases: vec!["Jon".into()],
            locale: None,
            scope: Some(VoiceTermScope::Surface),
            scope_ref: Some("voice_dictation".into()),
            priority: None,
            enabled: true,
        })?;
        db.enable_dictionary_autoadd_after_approval()?;
        let generation = db
            .dictionary_observation_generation()
            .ok_or("no generation")?;
        for session in 1..=2 {
            db.observe_dictionary_correction_guarded(generation, session, || {
                Some(("Hi Jon".into(), "Hi John".into(), ()))
            });
        }
        assert!(db.dictionary_autoadd_view()?.added.is_empty());
        let after = db.list_voice_terms()?;
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].id, term.id);
        assert!(after[0].aliases.iter().any(|alias| alias == "Jon"));
        Ok(())
    }

    #[test]
    fn expired_autoadd_history_is_not_presented_as_permanent_history(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let clock = Arc::new(AtomicI64::new(100));
        let db = Db::open_in_memory(Arc::new({
            let clock = Arc::clone(&clock);
            move || clock.load(Ordering::Relaxed)
        }))?;
        db.enable_dictionary_autoadd_after_approval()?;
        let generation = db
            .dictionary_observation_generation()
            .ok_or("no generation")?;
        for session in 1..=2 {
            db.observe_dictionary_correction_guarded(generation, session, || {
                Some(("Hi Jon".into(), "Hi John".into(), ()))
            });
        }
        let first_id = db.dictionary_autoadd_view()?.added[0].id;
        clock.store(RETENTION_MS + 101, Ordering::Relaxed);
        assert!(db.dictionary_autoadd_view()?.added.is_empty());
        assert_eq!(db.list_voice_terms()?.len(), 1);
        for session in 3..=4 {
            db.observe_dictionary_correction_guarded(generation, session, || {
                Some(("Hi Jan".into(), "Hi Jane".into(), ()))
            });
        }
        let second_id = db.dictionary_autoadd_view()?.added[0].id;
        assert!(
            second_id > first_id,
            "a fresh notice/Undo id must never be reused"
        );
        assert_eq!(db.list_voice_terms()?.len(), 2);
        Ok(())
    }

    #[test]
    fn view_lists_the_latest_addition_first_even_with_an_older_id(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let clock = Arc::new(AtomicI64::new(100));
        let db = Db::open_in_memory(Arc::new({
            let clock = Arc::clone(&clock);
            move || clock.load(Ordering::Relaxed)
        }))?;
        db.enable_dictionary_autoadd_after_approval()?;
        let generation = db
            .dictionary_observation_generation()
            .ok_or("no generation")?;
        let observe = |session, before: &str, after: &str| {
            db.observe_dictionary_correction_guarded(generation, session, || {
                Some((before.to_string(), after.to_string(), ()))
            })
        };
        // John is seen first, so it gets the lower id, but Jane is added first.
        observe(1, "Hi Jon", "Hi John");
        clock.store(200, Ordering::Relaxed);
        observe(2, "Hi Jan", "Hi Jane");
        let jane = observe(3, "Hi Jan", "Hi Jane")
            .added
            .ok_or("Jane was not added")?;
        clock.store(300, Ordering::Relaxed);
        let john = observe(4, "Hi Jon", "Hi John")
            .added
            .ok_or("John was not added")?;
        assert!(john.id < jane.id, "the id is assigned while pending");
        let added = db.dictionary_autoadd_view()?.added;
        let words: Vec<_> = added.iter().map(|entry| entry.corrected.as_str()).collect();
        assert_eq!(words, ["John", "Jane"], "the latest addition comes first");
        assert_eq!(added[0].id, john.id);
        assert_eq!(added[0].created_at_ms, 300);
        Ok(())
    }

    #[test]
    fn view_lists_learned_term_ids_until_the_term_is_gone() -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(Arc::new(|| 100))?;
        db.enable_dictionary_autoadd_after_approval()?;
        let generation = db.dictionary_observation_generation().ok_or("no generation")?;
        let observe = |session| {
            db.observe_dictionary_correction_guarded(generation, session, || {
                Some(("Hi Jon".into(), "Hi John".into(), ()))
            })
        };
        observe(1);
        let added = observe(2).added.ok_or("not added")?;
        let term_id = db.list_voice_terms()?.first().ok_or("no term")?.id;
        let view = db.dictionary_autoadd_view()?;
        assert_eq!(view.learned_term_ids, [term_id]);
        assert_eq!(added.term_id, Some(term_id));
        assert_eq!(view.added[0].term_id, Some(term_id));
        db.dictionary_autoadd_action(AutoAddAction::Undo { id: added.id })?;
        assert!(db.dictionary_autoadd_view()?.learned_term_ids.is_empty());
        Ok(())
    }

    #[test]
    fn toggle_clears_pending_but_keeps_exact_undo_and_undone_suppression(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(Arc::new(|| 100))?;
        db.enable_dictionary_autoadd_after_approval()?;
        let generation = db
            .dictionary_observation_generation()
            .ok_or("no generation")?;
        for session in 1..=2 {
            db.observe_dictionary_correction_guarded(generation, session, || {
                Some(("Hi Jon".into(), "Hi John".into(), ()))
            });
        }
        let id = db.dictionary_autoadd_view()?.added[0].id;
        db.observe_dictionary_correction_guarded(generation, 3, || {
            Some(("Hi Jan".into(), "Hi Jane".into(), ()))
        });
        db.dictionary_autoadd_action(AutoAddAction::Disable)?;
        assert!(!db.dictionary_autoadd_view()?.enabled);
        assert_eq!(db.dictionary_autoadd_view()?.added[0].id, id);
        db.dictionary_autoadd_action(AutoAddAction::Undo { id })?;
        assert!(db.list_voice_terms()?.is_empty());
        db.enable_dictionary_autoadd_after_approval()?;
        let new_generation = db
            .dictionary_observation_generation()
            .ok_or("no generation")?;
        assert_ne!(generation, new_generation);
        let first_after_toggle =
            db.observe_dictionary_correction_guarded(new_generation, 4, || {
                Some(("Hi Jan".into(), "Hi Jane".into(), ()))
            });
        assert!(
            first_after_toggle.added.is_none(),
            "old pending observation must be cleared"
        );
        for session in 5..=6 {
            db.observe_dictionary_correction_guarded(new_generation, session, || {
                Some(("Hi Jon".into(), "Hi John".into(), ()))
            });
        }
        assert!(
            db.list_voice_terms()?.is_empty(),
            "Undo must remain suppressed after toggling"
        );
        assert!(db
            .observe_dictionary_correction_guarded(new_generation, 7, || {
                Some(("Hi Jan".into(), "Hi Jane".into(), ()))
            })
            .added
            .is_some());
        Ok(())
    }

    #[test]
    fn changed_term_cannot_be_erased_by_undo_and_case_only_is_ignored(
    ) -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(spelling_change("Hi john", "Hi John"), None);
        let db = Db::open_in_memory(Arc::new(|| 100))?;
        db.enable_dictionary_autoadd_after_approval()?;
        let generation = db
            .dictionary_observation_generation()
            .ok_or("no generation")?;
        for session in 1..=2 {
            db.observe_dictionary_correction_guarded(generation, session, || {
                Some(("Hi Jon".into(), "Hi John".into(), ()))
            });
        }
        let added = db.dictionary_autoadd_view()?.added[0].clone();
        let term = db.list_voice_terms()?[0].clone();
        db.update_voice_term(
            term.id,
            &NewVoiceTerm {
                canonical: "John".into(),
                aliases: vec!["Jon", "My alias"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                locale: None,
                scope: Some(VoiceTermScope::Surface),
                scope_ref: Some("voice_dictation".into()),
                priority: None,
                enabled: true,
            },
        )?;
        assert!(db
            .dictionary_autoadd_action(AutoAddAction::Undo { id: added.id })
            .is_err());
        assert!(db.list_voice_terms()?[0]
            .aliases
            .iter()
            .any(|alias| alias == "My alias"));
        Ok(())
    }

    #[test]
    fn revocation_during_read_discards_correction() -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(Arc::new(|| 100))?;
        db.enable_dictionary_autoadd_after_approval()?;
        let generation = db
            .dictionary_observation_generation()
            .ok_or("no generation")?;
        let result = db.observe_dictionary_correction_guarded(generation, 1, || {
            db.dictionary_autoadd_action(AutoAddAction::Disable).ok()?;
            Some(("Hi Jon".into(), "Hi John".into(), ()))
        });
        assert!(!result.active);
        assert!(!db.dictionary_autoadd_view()?.enabled);
        assert!(db.dictionary_autoadd_view()?.added.is_empty());
        Ok(())
    }

    #[test]
    fn conflicting_manual_alias_in_any_scope_is_not_reassigned() -> Result<(), Box<dyn std::error::Error>> {
        for (scope, scope_ref) in [
            (VoiceTermScope::Global, None),
            (VoiceTermScope::Surface, Some("voice_dictation")),
            (VoiceTermScope::Bundle, Some("com.example.editor")),
        ] {
            let db = Db::open_in_memory(Arc::new(|| 100))?;
            db.create_voice_term(&NewVoiceTerm {
                canonical: "Jonathan".into(), aliases: vec!["Jon".into()], locale: None,
                scope: Some(scope), scope_ref: scope_ref.map(str::to_string),
                priority: None, enabled: true,
            })?;
            db.enable_dictionary_autoadd_after_approval()?;
            let generation = db.dictionary_observation_generation().ok_or("no generation")?;
            for session in 1..=2 {
                db.observe_dictionary_correction_guarded(generation, session, ||
                    Some(("Hi Jon".into(), "Hi John".into(), ())));
            }
            assert!(db.dictionary_autoadd_view()?.added.is_empty());
            assert_eq!(db.list_voice_terms()?.len(), 1);
            assert_eq!(db.list_voice_terms()?[0].canonical, "Jonathan");
            let resolved = db.voice_dictionary().correct("Jon", &crate::voice_dictionary::DictionaryContext {
                locale: None, bundle_id: Some("com.example.editor".into()),
                surface: Some("voice_dictation".into()),
            });
            assert_eq!(resolved.text, "Jonathan");
        }
        Ok(())
    }

    #[test]
    fn independent_undo_survives_later_auto_alias() -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(Arc::new(|| 100))?;
        db.enable_dictionary_autoadd_after_approval()?;
        let generation = db.dictionary_observation_generation().ok_or("no generation")?;
        for session in 1..=2 {
            db.observe_dictionary_correction_guarded(generation, session, ||
                Some(("Hi Jon".into(), "Hi John".into(), ())));
        }
        let first = db.dictionary_autoadd_view()?.added[0].id;
        for session in 3..=4 {
            db.observe_dictionary_correction_guarded(generation, session, ||
                Some(("Hi Jhon".into(), "Hi John".into(), ())));
        }
        let second = db.dictionary_autoadd_view()?.added[0].id;
        db.dictionary_autoadd_action(AutoAddAction::Undo { id: second })?;
        db.dictionary_autoadd_action(AutoAddAction::Undo { id: first })?;
        assert!(db.list_voice_terms()?.is_empty());
        Ok(())
    }

    #[test]
    fn undone_pair_stays_suppressed_after_text_expires() -> Result<(), Box<dyn std::error::Error>> {
        let clock = Arc::new(AtomicI64::new(100));
        let db = Db::open_in_memory(Arc::new({
            let clock = Arc::clone(&clock);
            move || clock.load(Ordering::Relaxed)
        }))?;
        db.enable_dictionary_autoadd_after_approval()?;
        let generation = db.dictionary_observation_generation().ok_or("no generation")?;
        for session in 1..=2 {
            db.observe_dictionary_correction_guarded(generation, session, ||
                Some(("Hi Jon".into(), "Hi John".into(), ())));
        }
        let id = db.dictionary_autoadd_view()?.added[0].id;
        db.dictionary_autoadd_action(AutoAddAction::Undo { id })?;
        clock.store(RETENTION_MS + 101, Ordering::Relaxed);
        for session in 3..=4 {
            assert!(db.observe_dictionary_correction_guarded(generation, session, ||
                Some(("Hi Jon".into(), "Hi John".into(), ()))).resolved);
        }
        assert!(db.list_voice_terms()?.is_empty());
        let (raw, suppressed) = db.with_conn("dictionary_autoadd.test", |conn| {
            let raw: i64 = conn.query_row("SELECT count(*) FROM dictionary_autoadd_corrections", [], |r| r.get(0))?;
            let suppressed: i64 = conn.query_row("SELECT count(*) FROM dictionary_autoadd_suppression", [], |r| r.get(0))?;
            Ok::<_, rusqlite::Error>((raw, suppressed))
        }).map_err(|error| std::io::Error::other(format!("{error:?}")))?;
        assert_eq!((raw, suppressed), (0, 1));
        Ok(())
    }

    #[test]
    fn continued_typing_and_whitespace_are_not_corrections() {
        // Dictation learning feeds the whole text after the insertion when the field ends there.
        assert_eq!(spelling_change("Hi Jon", "Hi Jon. Next sentence"), None);
        assert_eq!(spelling_change("Hi  Jon\n", "Hi Jon"), None);
    }

    #[test]
    fn ordinary_words_are_not_learned() {
        assert_eq!(spelling_change("then", "than"), None);
        assert_eq!(spelling_change("form", "from"), None);
        if std::path::Path::new("/usr/share/dict/words").exists() {
            assert_eq!(spelling_change("Then it", "Than it"), None);
        }
        assert_eq!(spelling_change("Hi Jon", "Hi John"), Some(("Jon".into(), "John".into())));
    }

    #[test]
    fn alias_capacity_is_terminal_for_the_observed_pair() -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(Arc::new(|| 100))?;
        db.create_voice_term(&NewVoiceTerm {
            canonical: "John".into(),
            aliases: (0..15).map(|n| format!("variant{n}")).collect(),
            locale: None, scope: Some(VoiceTermScope::Surface),
            scope_ref: Some("voice_dictation".into()), priority: None, enabled: true,
        })?;
        db.enable_dictionary_autoadd_after_approval()?;
        let generation = db.dictionary_observation_generation().ok_or("no generation")?;
        for session in 1..=3 {
            assert!(db.observe_dictionary_correction_guarded(generation, session, ||
                Some(("Hi Jon".into(), "Hi John".into(), ()))).resolved);
        }
        let state: String = db.with_conn("dictionary_autoadd.test", |conn| conn.query_row(
            "SELECT state FROM dictionary_autoadd_corrections WHERE original = 'Jon' AND corrected = 'John'",
            [], |r| r.get(0),
        )).map_err(|error| std::io::Error::other(format!("{error:?}")))?;
        assert_eq!(state, "suppressed");
        assert!(db.dictionary_autoadd_view()?.added.is_empty());
        Ok(())
    }

    #[test]
    fn lost_session_before_commit_rolls_back_observation() -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(Arc::new(|| 100))?;
        db.enable_dictionary_autoadd_after_approval()?;
        let generation = db.dictionary_observation_generation().ok_or("no generation")?;
        let result = db.observe_dictionary_correction_with_commit(generation, 1, ||
            Some(("Hi Jon".into(), "Hi John".into(), ())), || None::<()>);
        assert!(!result.active);
        let raw: i64 = db.with_conn("dictionary_autoadd.test", |conn| conn.query_row(
            "SELECT count(*) FROM dictionary_autoadd_corrections", [], |r| r.get(0),
        )).map_err(|error| std::io::Error::other(format!("{error:?}")))?;
        assert_eq!(raw, 0);
        Ok(())
    }

    fn learned(heard: Option<&str>, term: &str) -> (Learned, &'static str) {
        (Learned { heard: heard.map(str::to_string), term: term.to_string() }, "model")
    }

    #[test]
    fn one_fix_adds_a_group_and_group_undo_suppresses_for_thirty_days(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let clock = Arc::new(AtomicI64::new(100));
        let db = Db::open_in_memory(Arc::new({
            let clock = Arc::clone(&clock);
            move || clock.load(Ordering::Relaxed)
        }))?;
        db.enable_dictionary_autoadd_after_approval()?;
        let generation = db.dictionary_observation_generation().ok_or("no generation")?;
        let terms = [learned(Some("tore tessu"), "Torutesu"), learned(Some("shogun eye"), "ShogunAI")];
        let added = db.commit_learned_terms(generation, 1, &terms, || Some(()));
        assert_eq!(added.len(), 2, "one fix is enough");
        let group = added[0].group_id.ok_or("no group")?;
        assert!(added.iter().all(|a| a.group_id == Some(group) && a.source == "model"));
        assert_eq!(db.list_voice_terms()?.len(), 2);
        assert_eq!(db.dictionary_autoadd_view()?.added[0].group_id, Some(group));

        db.dictionary_autoadd_action(AutoAddAction::UndoGroup { group_id: group })?;
        assert!(db.list_voice_terms()?.is_empty());
        assert!(db.commit_learned_terms(generation, 2, &terms, || Some(())).is_empty(), "suppressed");
        clock.store(31 * RETENTION_MS, Ordering::Relaxed);
        assert_eq!(db.commit_learned_terms(generation, 3, &terms, || Some(())).len(), 2, "suppression expired");
        Ok(())
    }

    #[test]
    fn learning_respects_the_commit_fence_and_generation() -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(Arc::new(|| 100))?;
        db.enable_dictionary_autoadd_after_approval()?;
        let generation = db.dictionary_observation_generation().ok_or("no generation")?;
        let terms = [learned(Some("tore tessu"), "Torutesu")];
        assert!(db.commit_learned_terms(generation, 1, &terms, || None::<()>).is_empty());
        assert!(db.list_voice_terms()?.is_empty(), "refused fence writes nothing");
        db.dictionary_autoadd_action(AutoAddAction::Disable)?;
        assert!(db.commit_learned_terms(generation, 1, &terms, || Some(())).is_empty());
        assert!(db.list_voice_terms()?.is_empty());
        Ok(())
    }

    #[test]
    fn keyterm_only_term_is_added_once_and_undo_removes_it() -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(Arc::new(|| 100))?;
        db.enable_dictionary_autoadd_after_approval()?;
        let generation = db.dictionary_observation_generation().ok_or("no generation")?;
        let added = db.commit_learned_terms(generation, 1, &[learned(None, "Shogun")], || Some(()));
        assert_eq!(added.len(), 1);
        let terms = db.list_voice_terms()?;
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].canonical, "Shogun");
        assert!(db.commit_learned_terms(generation, 2, &[learned(None, "shogun")], || Some(())).is_empty());
        assert_eq!(db.dictionary_autoadd_view()?.learned_term_ids, [terms[0].id]);
        db.dictionary_autoadd_action(AutoAddAction::Undo { id: added[0].id })?;
        assert!(db.list_voice_terms()?.is_empty());
        Ok(())
    }

    #[test]
    fn case_only_heard_is_learned_keyterm_only_and_undoes_cleanly() -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(Arc::new(|| 100))?;
        let generation = db.dictionary_observation_generation().ok_or("no generation")?;
        let added = db.commit_learned_terms(generation, 1, &[learned(Some("shogun"), "Shogun")], || Some(()));
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].original, "Shogun");
        db.dictionary_autoadd_action(AutoAddAction::Undo { id: added[0].id })?;
        assert!(db.list_voice_terms()?.is_empty(), "Undo removes it");
        assert!(db.dictionary_autoadd_view()?.learned_term_ids.is_empty());
        Ok(())
    }

    #[test]
    fn learning_adds_a_misheard_alias_to_an_existing_term() -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(Arc::new(|| 100))?;
        db.create_voice_term(&NewVoiceTerm {
            canonical: "Torutesu".into(),
            aliases: vec![],
            locale: None,
            scope: Some(VoiceTermScope::Surface),
            scope_ref: Some("voice_dictation".into()),
            priority: None,
            enabled: true,
        })?;
        db.enable_dictionary_autoadd_after_approval()?;
        let generation = db.dictionary_observation_generation().ok_or("no generation")?;
        let added = db.commit_learned_terms(generation, 1, &[learned(Some("tore tessu"), "Torutesu")], || Some(()));
        assert_eq!(added.len(), 1);
        let terms = db.list_voice_terms()?;
        assert_eq!(terms.len(), 1);
        assert!(terms[0].aliases.iter().any(|alias| alias == "tore tessu"));
        Ok(())
    }

    #[test]
    fn on_by_default_until_the_user_turns_it_off() -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(Arc::new(|| 100))?;
        assert!(db.dictionary_autoadd_view()?.enabled, "never touched: on");
        db.dictionary_autoadd_action(AutoAddAction::Disable)?;
        assert!(!db.dictionary_autoadd_view()?.enabled, "explicit off stays off");
        assert!(db.dictionary_observation_generation().is_none());
        assert!(db.dictionary_autoadd_action(AutoAddAction::Enable { consent: true }).is_err(), "API cannot turn it on");
        db.enable_dictionary_autoadd_after_approval()?;
        assert!(db.dictionary_autoadd_view()?.enabled);
        db.with_conn_mut("test", |conn| {
            conn.execute("UPDATE dictionary_autoadd_settings SET enabled = 1, consent_version = 1", [])
        })
        .map_err(|error| std::io::Error::other(format!("{error:?}")))?;
        assert!(db.dictionary_observation_generation().is_none(), "a v1 opt-in the user set needs v2 consent");
        Ok(())
    }
}
