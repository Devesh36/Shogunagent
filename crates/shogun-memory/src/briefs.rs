//! The persisted nightly Morning Brief repository (§6.8 / FR-MB-01..06).
//!
//! One row per local calendar day (`date` is the primary key): the Dream Cycle's MorningBrief job
//! assembles the brief at night and upserts it here, so the morning display is a read — immediate
//! and offline-stable — rather than a live assembly. Like `meeting_recaps`, this is a document,
//! not a log: regenerating a day's brief replaces the row.
//!
//! The brief's *content* type lives here too ([`BriefPayload`]): the assembler (shogun-fusion,
//! driven from shogun-core) serializes into it and the desktop layer deserializes out of it, but
//! storage itself only ever sees the JSON string — the same already-serialized-columns pattern as
//! `meeting_recaps`.
//!
//! `prev_digest` carries the digest of the payload the current row replaced. That makes the
//! FR-MB-06 "Updated" mark derivable without a second table: a brief is `updated` when its payload
//! digest differs from `prev_digest`. An upsert with an unchanged payload touches neither the
//! payload nor `prev_digest`, so re-running the nightly job (crash-resume, FR-DC-04) is idempotent.

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

/// One line in the brief's "Today" section (FR-MB-01): an event from the synced calendar
/// (`calendar_occurrences`). Dated commitments are not schedule lines; they stay in
/// `commitments_due`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BriefScheduleLine {
    pub start_ms: i64,
    pub title: String,
    /// A date, not an instant: shown without a time. Absent in payloads saved before it existed.
    #[serde(default)]
    pub all_day: bool,
    /// FR-MB-06: changed after the brief was generated (shown with an "Updated" mark).
    pub updated: bool,
}

/// One confidence-gated brief line (commitments due / open loops). `possibly` is the
/// medium-confidence hedge (FR-MB-05); provenance travels with every item (FR-ST-02).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BriefLine {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance_event_id: Option<i64>,
    /// Edited context keeps authored wording without asserting that an old source proves it.
    /// Missing on older payloads; absence must never imply accepted intent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_kind: Option<crate::state::EvidenceKind>,
    pub possibly: bool,
}

/// One suggested action attached to the brief (FR-MB-01: ≤3, each permission-level-tagged).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BriefActionLine {
    pub label: String,
    /// The action's permission level as a string ("L1" / "L2" / "L3").
    pub level: String,
}

/// The persisted brief content — the JSON shape stored in `briefs.payload`. Mirrors the fusion
/// assembler's `MorningBrief` sections; this crate must not depend on shogun-fusion, so the shape
/// is declared here and the core layer converts into it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BriefPayload {
    /// The local calendar day this brief is for ('YYYY-MM-DD'), same value as the row key.
    pub date: String,
    /// "Today": the synced calendar's events for the day, all-day first, then time-ordered.
    pub today: Vec<BriefScheduleLine>,
    /// "Commitments due": overdue first, then soonest.
    pub commitments_due: Vec<BriefLine>,
    /// "Open loops": top-N by staleness.
    pub open_loops: Vec<BriefLine>,
    /// "What happened" summary lines (≤5). Extractive unless the Batch lane wrote prose.
    pub what_happened: Vec<String>,
    /// Local meeting recaps remain separate from the five overall highlights.
    #[serde(default)]
    pub meetings: Vec<String>,
    /// Suggested actions (≤3), permission-level-tagged.
    pub suggested_actions: Vec<BriefActionLine>,
    /// One Charm line for the morning greeting (issue #10): written nightly by the MorningBrief
    /// job from the user's `# Charm` directives, on the Batch lane. `None` when no directives
    /// exist or the night degraded — the card simply omits the line. `serde(default)` keeps every
    /// already-stored payload readable (JSON-internal field, no migration).
    #[serde(default)]
    pub charm_line: Option<String>,
    /// One Work Patterns line (FR-PAT-06b / design §5d-5e): a noticed moment the first morning a
    /// new pattern crosses the threshold, else the week's strongest trend ("Translation is up 2×
    /// this week."). `None` when the profile has nothing worth a line. `serde(default)` keeps
    /// every already-stored payload readable (JSON-internal field, no migration).
    #[serde(default)]
    pub pattern_line: Option<String>,
}

/// A `briefs` row as stored, plus the derived `updated` flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredBrief {
    pub date: String,
    /// The BriefPayload JSON, verbatim.
    pub payload: String,
    /// Whether generated prose was attached (false = extractive honest degradation, FR-MB-04).
    pub generated: bool,
    pub built_at: i64,
    /// Digest of the payload this row replaced (`None` until a regeneration changes it).
    pub prev_digest: Option<String>,
    /// FR-MB-06: the brief's content changed after it was first built for this day.
    pub updated: bool,
}

/// Stable content digest for change detection (the FR-MB-06 `updated` mark): FNV-1a 64-bit over
/// the payload's UTF-8 bytes, 16 lower-hex chars. Implemented inline — this crate carries no hash
/// dependency, and the digest is a change marker, never a security boundary. Deliberately the same
/// digest-not-content posture as `traceability`: only a digest of the payload's predecessor
/// persists, and the payload itself is local-only data anyway.
pub fn digest(payload: &str) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0100_0000_01b3;
    let mut h = OFFSET;
    for b in payload.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(PRIME);
    }
    format!("{h:016x}")
}

fn derive_updated(prev_digest: Option<&str>, payload_digest: &str) -> bool {
    prev_digest.is_some_and(|p| p != payload_digest)
}

/// Insert or replace the brief for `date` (FR-DC-04 idempotent upsert). Returns the resulting
/// `updated` flag.
///
/// - First write of the day: row inserted, `prev_digest` NULL → `updated = false`.
/// - Re-run with the *same* payload (crash-resume): payload and `prev_digest` untouched — only
///   `generated`/`built_at` refresh — so `updated` keeps its value (false unless an earlier
///   regeneration changed the content).
/// - Regeneration with a *different* payload: `prev_digest` becomes the replaced payload's
///   digest → `updated = true` (FR-MB-06).
pub fn upsert_brief(
    conn: &Connection,
    date: &str,
    payload_json: &str,
    generated: bool,
    built_at: i64,
) -> Result<bool, rusqlite::Error> {
    if conn.is_autocommit() {
        let tx = conn.unchecked_transaction()?;
        let updated = upsert_brief_in_transaction(&tx, date, payload_json, generated, built_at)?;
        tx.commit()?;
        Ok(updated)
    } else {
        upsert_brief_in_transaction(conn, date, payload_json, generated, built_at)
    }
}

/// Write a brief while the caller owns the surrounding transaction. Unlike [`upsert_brief`],
/// this does not begin or commit a transaction, so a generation CAS, payload replacement, and
/// search projection can become durable together.
pub fn upsert_brief_in_transaction(
    conn: &Connection,
    date: &str,
    payload_json: &str,
    generated: bool,
    built_at: i64,
) -> Result<bool, rusqlite::Error> {
    let existing: Option<(String, Option<String>, bool)> = conn
        .query_row(
            "SELECT payload, prev_digest, generated FROM briefs WHERE date = ?1",
            [date],
            |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0)),
        )
        .optional()?;
    let invalidated: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM search_document_invalidations
         WHERE source_table='briefs' AND source_id=?1 AND invalidation_kind='evidence_deleted')",
        [date],
        |row| row.get(0),
    )?;
    let mut generated = generated;
    let mut merged_payload = None;
    if let Some((old_payload, _, was_generated)) = existing.as_ref().filter(|_| !invalidated) {
        if let (Ok(previous), Ok(mut incoming)) = (
            serde_json::from_str::<BriefPayload>(old_payload),
            serde_json::from_str::<BriefPayload>(payload_json),
        ) {
            if *was_generated && !generated {
                incoming.what_happened = previous.what_happened;
                incoming.charm_line = previous.charm_line;
                generated = true;
            }
            if previous.pattern_line.is_some() {
                incoming.pattern_line = previous.pattern_line;
            }
            merged_payload = Some(
                serde_json::to_string(&incoming)
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?,
            );
        }
    }
    let payload = merged_payload.as_deref().unwrap_or(payload_json);
    let new_digest = digest(payload);
    let updated = match existing {
        None => {
            conn.execute(
                "INSERT INTO briefs (date, payload, generated, built_at, prev_digest)
                 VALUES (?1, ?2, ?3, ?4, NULL)",
                params![date, payload, i64::from(generated), built_at],
            )?;
            false
        }
        Some((old_payload, old_prev, _)) => {
            if digest(&old_payload) == new_digest {
                // Same content: a re-run must not manufacture an "Updated" mark or lose one.
                conn.execute(
                    "UPDATE briefs SET generated = ?2, built_at = ?3 WHERE date = ?1",
                    params![date, i64::from(generated), built_at],
                )?;
                derive_updated(old_prev.as_deref(), &new_digest)
            } else {
                conn.execute(
                    "UPDATE briefs SET payload = ?2, generated = ?3, built_at = ?4, prev_digest = ?5
                     WHERE date = ?1",
                    params![date, payload, i64::from(generated), built_at, digest(&old_payload)],
                )?;
                true
            }
        }
    };
    let _ = crate::search_documents::project_source_after_write(
        conn,
        &crate::search_documents::SourceRef::Brief(date.to_owned()),
        built_at,
    );
    Ok(updated)
}

/// The persisted brief for `date`, with the derived `updated` flag. `None` means the nightly job
/// has not written one — a normal state (the caller falls back to the degraded live assembly).
pub fn get_brief(conn: &Connection, date: &str) -> Result<Option<StoredBrief>, rusqlite::Error> {
    conn.query_row(
        "SELECT date, payload, generated, built_at, prev_digest FROM briefs WHERE date = ?1",
        [date],
        |r| {
            let payload: String = r.get(1)?;
            let prev_digest: Option<String> = r.get(4)?;
            let updated = derive_updated(prev_digest.as_deref(), &digest(&payload));
            Ok(StoredBrief {
                date: r.get(0)?,
                payload,
                generated: r.get::<_, i64>(2)? != 0,
                built_at: r.get(3)?,
                prev_digest,
                updated,
            })
        },
    )
    .optional()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_brief_lines_keep_source_ids_without_inventing_edited_context(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let legacy = r#"{"text":"old wording","provenance_event_id":42,"possibly":true}"#;
        let line: BriefLine = serde_json::from_str(legacy)?;
        assert_eq!(line.provenance_event_id, Some(42));
        assert_eq!(line.evidence_kind, None);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&serde_json::to_string(&line)?)?,
            serde_json::from_str::<serde_json::Value>(legacy)?
        );
        Ok(())
    }

    #[test]
    fn a_late_local_write_cannot_downgrade_generated_content() -> Result<(), Box<dyn std::error::Error>> {
        let conn = crate::open_in_memory()?;
        let mut original: BriefPayload = serde_json::from_str(&payload("original"))?;
        original.what_happened = vec!["Exact generated prose — 日本語".into()];
        original.charm_line = Some("Saved greeting".into());
        let mut local = original.clone();
        local.what_happened = vec!["Local extract".into()];
        local.charm_line = None;
        local.today = vec![BriefScheduleLine { start_ms: 200, title: "Fresh calendar item".into(), all_day: false, updated: false }];
        local.commitments_due[0].text = "Fresh due commitment".into();
        local.open_loops = vec![BriefLine {
            text: "Fresh open loop".into(),
            provenance_event_id: Some(2),
            evidence_kind: None,
            possibly: false,
        }];
        local.meetings = vec!["Fresh meeting recap".into()];
        upsert_brief(&conn, "2026-09-14", &serde_json::to_string(&original)?, true, 100)?;
        upsert_brief(&conn, "2026-09-14", &serde_json::to_string(&local)?, false, 200)?;
        let row = get_brief(&conn, "2026-09-14")?.ok_or("missing brief")?;
        let saved: BriefPayload = serde_json::from_str(&row.payload)?;
        assert!(row.generated);
        assert!(row.updated);
        assert_eq!(saved.what_happened, original.what_happened);
        assert_eq!(saved.charm_line, original.charm_line);
        assert_eq!((saved.today, saved.commitments_due, saved.open_loops, saved.meetings), (local.today, local.commitments_due, local.open_loops, local.meetings));
        assert_eq!(row.built_at, 200);
        Ok(())
    }

    #[test]
    fn local_regeneration_after_evidence_deletion_does_not_restore_generated_text() -> Result<(), Box<dyn std::error::Error>> {
        let conn = crate::open_in_memory()?;
        let mut original: BriefPayload = serde_json::from_str(&payload("original"))?;
        original.what_happened = vec!["Deleted source prose".into()];
        upsert_brief(&conn, "2026-09-14", &serde_json::to_string(&original)?, true, 100)?;
        conn.execute("INSERT INTO search_document_invalidations (source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms) VALUES ('briefs','2026-09-14','evidence_deleted',100,101) ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind='evidence_deleted',required_owner_updated_after_ms=100", [])?;
        original.what_happened = vec!["Safe local replacement".into()];
        upsert_brief(&conn, "2026-09-14", &serde_json::to_string(&original)?, false, 200)?;
        let row = get_brief(&conn, "2026-09-14")?.ok_or("missing brief")?;
        assert!(!row.generated);
        assert!(!row.payload.contains("Deleted source prose"));
        Ok(())
    }

    fn payload(marker: &str) -> String {
        // A minimal valid BriefPayload JSON; `marker` varies the content.
        serde_json::to_string(&BriefPayload {
            date: "2026-08-09".into(),
            today: vec![],
            commitments_due: vec![BriefLine {
                text: marker.into(),
                provenance_event_id: Some(1),
                evidence_kind: None,
                possibly: false,
            }],
            open_loops: vec![],
            what_happened: vec![],
            suggested_actions: vec![],
            charm_line: None,
            pattern_line: None,
            meetings: vec![],
        })
        .unwrap()
    }

    fn payload_with_highlight(marker: &str, highlight: &str) -> Result<String, serde_json::Error> {
        serde_json::to_string(&BriefPayload {
            date: "2026-08-09".into(),
            today: vec![],
            commitments_due: vec![BriefLine {
                text: marker.into(),
                provenance_event_id: Some(1),
                evidence_kind: None,
                possibly: false,
            }],
            open_loops: vec![],
            what_happened: vec![highlight.into()],
            meetings: vec![],
            suggested_actions: vec![],
            charm_line: None,
            pattern_line: None,
        })
    }

    #[test]
    fn a_day_with_no_brief_reads_none() {
        let conn = crate::open_in_memory().unwrap();
        assert_eq!(get_brief(&conn, "2026-08-09").unwrap(), None);
    }

    #[test]
    fn first_write_is_not_marked_updated() {
        let conn = crate::open_in_memory().unwrap();
        let updated = upsert_brief(&conn, "2026-08-09", &payload("a"), false, 100).unwrap();
        assert!(!updated, "the day's first brief has nothing to differ from");

        let row = get_brief(&conn, "2026-08-09").unwrap().unwrap();
        assert_eq!(row.payload, payload("a"));
        assert!(!row.generated);
        assert_eq!(row.built_at, 100);
        assert_eq!(row.prev_digest, None);
        assert!(!row.updated);
    }

    #[test]
    fn rewriting_the_same_payload_is_idempotent_and_stays_not_updated() {
        let conn = crate::open_in_memory().unwrap();
        upsert_brief(&conn, "2026-08-09", &payload("a"), false, 100).unwrap();
        let updated = upsert_brief(&conn, "2026-08-09", &payload("a"), false, 200).unwrap();
        assert!(!updated, "an unchanged payload must not manufacture an Updated mark");

        let row = get_brief(&conn, "2026-08-09").unwrap().unwrap();
        assert!(!row.updated);
        assert_eq!(row.built_at, 200, "the re-run's timestamp still lands");
        let n: i64 =
            conn.query_row("SELECT count(*) FROM briefs", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "one row per day, not an append log");
    }

    #[test]
    fn a_changed_payload_flips_updated_and_records_the_replaced_digest() {
        let conn = crate::open_in_memory().unwrap();
        upsert_brief(&conn, "2026-08-09", &payload("a"), false, 100).unwrap();
        let updated = upsert_brief(&conn, "2026-08-09", &payload("b"), false, 200).unwrap();
        assert!(updated, "changed content is the FR-MB-06 Updated case");

        let row = get_brief(&conn, "2026-08-09").unwrap().unwrap();
        assert!(row.updated);
        assert_eq!(row.payload, payload("b"));
        assert_eq!(row.prev_digest.as_deref(), Some(digest(&payload("a")).as_str()));
    }

    #[test]
    fn a_rerun_after_a_change_keeps_the_updated_mark() {
        let conn = crate::open_in_memory().unwrap();
        upsert_brief(&conn, "2026-08-09", &payload("a"), false, 100).unwrap();
        upsert_brief(&conn, "2026-08-09", &payload("b"), false, 200).unwrap();
        // crash-resume re-runs the job with the same (changed) payload
        let updated = upsert_brief(&conn, "2026-08-09", &payload("b"), false, 300).unwrap();
        assert!(updated, "an idempotent re-run must not erase the day's Updated mark");
        assert!(get_brief(&conn, "2026-08-09").unwrap().unwrap().updated);
    }

    #[test]
    fn briefs_are_keyed_per_day() {
        let conn = crate::open_in_memory().unwrap();
        upsert_brief(&conn, "2026-08-08", &payload("yesterday"), true, 100).unwrap();
        upsert_brief(&conn, "2026-08-09", &payload("today"), false, 200).unwrap();

        let yesterday = get_brief(&conn, "2026-08-08").unwrap().unwrap();
        let today = get_brief(&conn, "2026-08-09").unwrap().unwrap();
        assert!(yesterday.generated);
        assert!(!today.generated);
        assert_ne!(yesterday.payload, today.payload);
        assert!(!today.updated, "a new day starts unmarked even when yesterday's differs");
    }

    #[test]
    fn digest_is_deterministic_hex_and_content_sensitive() {
        assert_eq!(digest("x"), digest("x"));
        assert_ne!(digest("x"), digest("y"));
        let d = digest("payload");
        assert_eq!(d.len(), 16);
        assert!(d.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn payload_round_trips_through_json() {
        let p = BriefPayload {
            date: "2026-08-09".into(),
            today: vec![BriefScheduleLine {
                start_ms: 9,
                title: "standup".into(),
                all_day: false,
                updated: false,
            }],
            commitments_due: vec![BriefLine {
                text: "send the deck".into(),
                provenance_event_id: Some(4),
                evidence_kind: None,
                possibly: true,
            }],
            open_loops: vec![],
            what_happened: vec!["shipped the report".into()],
            suggested_actions: vec![BriefActionLine { label: "prep the standup".into(), level: "L2".into() }],
            charm_line: Some("your knack for order is the lever today".into()),
            pattern_line: Some("⚔ Noticed: you do open_app a lot.".into()),
            meetings: vec![],
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: BriefPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn payloads_stored_before_charm_existed_still_parse() {
        // Every brief row written before issue #10 lacks the field; reading one back must not
        // become a parse error that silently downgrades the morning to the degraded brief.
        let old = r#"{"date":"2026-08-01","today":[],"commitments_due":[],"open_loops":[],
                      "what_happened":[],"suggested_actions":[]}"#;
        let p: BriefPayload = serde_json::from_str(old).unwrap();
        assert_eq!(p.charm_line, None);
    }
    #[test]
    fn local_refresh_keeps_generated_highlights_until_a_source_changes()
    -> Result<(), Box<dyn std::error::Error>> {
        let conn = crate::open_in_memory()?;
        let event_id = {
            conn.execute(
                "INSERT INTO event_log (ts, source, kind, content, content_hash, last_seen_at)
                 VALUES (100, 'capture', 'text', 'original source', 'brief-source', 100)",
                [],
            )?;
            conn.last_insert_rowid()
        };
        upsert_brief(
            &conn,
            "2026-08-09",
            &payload_with_highlight("model section", "Generated highlight")?,
            true,
            200,
        )?;
        upsert_brief(
            &conn,
            "2026-08-09",
            &payload_with_highlight("local section", "Local fallback")?,
            false,
            300,
        )?;

        let stored = get_brief(&conn, "2026-08-09")?
            .ok_or_else(|| std::io::Error::other("missing generated brief"))?;
        let payload: BriefPayload = serde_json::from_str(&stored.payload)?;
        assert!(stored.generated);
        assert_eq!(payload.what_happened, ["Generated highlight"]);

        let sources = serde_json::json!([[event_id, "brief-source-digest"]]).to_string();
        conn.execute(
            "INSERT INTO brief_generation
                 (date, input_from_ts, input_to_ts, output_language, sources_json, state, created_at, updated_at)
             VALUES ('2026-08-09', 0, 200, 'en', ?1, 'pending', 200, 200)",
            [sources],
        )?;
        conn.execute(
            "UPDATE event_log SET content = 'changed source', content_hash = 'brief-source-v2'
             WHERE id = ?1",
            [event_id],
        )?;

        let invalidated = get_brief(&conn, "2026-08-09")?
            .ok_or_else(|| std::io::Error::other("missing invalidated brief"))?;
        let invalidated_payload: BriefPayload = serde_json::from_str(&invalidated.payload)?;
        let state: String = conn.query_row(
            "SELECT state FROM brief_generation WHERE date = '2026-08-09'",
            [],
            |row| row.get(0),
        )?;
        assert!(!invalidated.generated);
        assert!(invalidated_payload.what_happened.is_empty());
        assert_eq!(state, "expired");

        upsert_brief(
            &conn,
            "2026-08-09",
            &payload_with_highlight("fresh local section", "Fresh local fallback")?,
            false,
            400,
        )?;
        let refreshed = get_brief(&conn, "2026-08-09")?
            .ok_or_else(|| std::io::Error::other("missing refreshed brief"))?;
        let refreshed_payload: BriefPayload = serde_json::from_str(&refreshed.payload)?;
        assert!(!refreshed.generated);
        assert_eq!(refreshed_payload.what_happened, ["Fresh local fallback"]);
        Ok(())
    }

    #[test]
    fn source_delete_expires_its_matching_generation_ledger()
    -> Result<(), Box<dyn std::error::Error>> {
        let conn = crate::open_in_memory()?;
        conn.execute(
            "INSERT INTO event_log (ts, source, kind, content, content_hash, last_seen_at)
             VALUES (100, 'capture', 'text', 'deletable source', 'delete-source', 100)",
            [],
        )?;
        let event_id = conn.last_insert_rowid();
        upsert_brief(
            &conn,
            "2026-08-09",
            &payload_with_highlight("model section", "Generated highlight")?,
            true,
            200,
        )?;
        conn.execute(
            "INSERT INTO brief_generation
                 (date, input_from_ts, input_to_ts, output_language, sources_json, state, created_at, updated_at)
             VALUES ('2026-08-09', 0, 200, 'en', ?1, 'pending', 200, 200)",
            [serde_json::json!([[event_id, "delete-source-digest"]]).to_string()],
        )?;

        conn.execute("DELETE FROM event_log WHERE id = ?1", [event_id])?;

        let generated: i64 = conn.query_row(
            "SELECT generated FROM briefs WHERE date = '2026-08-09'",
            [],
            |row| row.get(0),
        )?;
        let state: String = conn.query_row(
            "SELECT state FROM brief_generation WHERE date = '2026-08-09'",
            [],
            |row| row.get(0),
        )?;
        let payload: String = conn.query_row(
            "SELECT payload FROM briefs WHERE date = '2026-08-09'",
            [],
            |row| row.get(0),
        )?;
        let payload: BriefPayload = serde_json::from_str(&payload)?;
        assert_eq!(generated, 0);
        assert!(payload.what_happened.is_empty());
        assert_eq!(state, "expired");
        Ok(())
    }

    #[test]
    fn late_cloud_source_preserves_a_completed_brief() -> Result<(), Box<dyn std::error::Error>> {
        let conn = crate::open_in_memory()?;
        upsert_brief(
            &conn,
            "2026-08-09",
            &payload_with_highlight("model section", "Generated highlight")?,
            true,
            200,
        )?;
        conn.execute(
            "INSERT INTO brief_generation
                 (date, input_from_ts, input_to_ts, output_language, sources_json, state, created_at, updated_at)
             VALUES ('2026-08-09', 0, 200, 'en', '[]', 'complete', 200, 200)",
            [],
        )?;

        conn.execute(
            "INSERT INTO event_log (ts, source, kind, content, content_hash, last_seen_at)
             VALUES (100, 'capture', 'text', 'late source', 'late-source', 100)",
            [],
        )?;

        let stored = get_brief(&conn, "2026-08-09")?
            .ok_or_else(|| std::io::Error::other("missing completed brief"))?;
        let payload: BriefPayload = serde_json::from_str(&stored.payload)?;
        let state: String = conn.query_row(
            "SELECT state FROM brief_generation WHERE date = '2026-08-09'",
            [],
            |row| row.get(0),
        )?;
        assert!(stored.generated);
        assert_eq!(payload.what_happened, ["Generated highlight"]);
        assert_eq!(state, "complete");
        Ok(())
    }

    #[test]
    fn late_cloud_source_keeps_pending_local_fallback() -> Result<(), Box<dyn std::error::Error>> {
        let conn = crate::open_in_memory()?;
        upsert_brief(
            &conn,
            "2026-08-09",
            &payload_with_highlight("local section", "Local fallback")?,
            false,
            200,
        )?;
        conn.execute(
            "INSERT INTO brief_generation
                 (date, input_from_ts, input_to_ts, output_language, sources_json, state, created_at, updated_at)
             VALUES ('2026-08-09', 0, 200, 'en', '[]', 'pending', 200, 200)",
            [],
        )?;

        conn.execute(
            "INSERT INTO event_log (ts, source, kind, content, content_hash, last_seen_at)
             VALUES (100, 'capture', 'text', 'late source', 'late-local-source', 100)",
            [],
        )?;

        let stored = get_brief(&conn, "2026-08-09")?
            .ok_or_else(|| std::io::Error::other("missing local brief"))?;
        let payload: BriefPayload = serde_json::from_str(&stored.payload)?;
        let state: String = conn.query_row(
            "SELECT state FROM brief_generation WHERE date = '2026-08-09'",
            [],
            |row| row.get(0),
        )?;
        assert!(!stored.generated);
        assert_eq!(payload.what_happened, ["Local fallback"]);
        assert_eq!(state, "pending");
        Ok(())
    }

    #[test]
    fn generation_ledger_defaults_generation_and_request_id_with_explicit_state()
    -> Result<(), Box<dyn std::error::Error>> {
        let conn = crate::open_in_memory()?;
        conn.execute(
            "INSERT INTO brief_generation
                 (date, input_from_ts, input_to_ts, output_language, sources_json, state, created_at, updated_at)
             VALUES ('2026-08-09', 0, 100, 'en', '[]', 'pending', 100, 100)",
            [],
        )?;
        conn.execute(
            "INSERT INTO brief_generation
                 (date, input_from_ts, input_to_ts, output_language, sources_json, state, created_at, updated_at)
             VALUES ('2026-08-10', 100, 200, 'en', '[]', 'pending', 200, 200)",
            [],
        )?;
        let (state, generation, request_id): (String, i64, String) = conn.query_row(
            "SELECT state, generation, request_id FROM brief_generation WHERE date = '2026-08-09'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let next_request_id: String = conn.query_row(
            "SELECT request_id FROM brief_generation WHERE date = '2026-08-10'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(state, "pending");
        assert_eq!(generation, 0);
        assert_eq!(request_id.len(), 32);
        assert_ne!(request_id, next_request_id);
        assert!(conn
            .execute(
                "INSERT INTO brief_generation
                     (date, input_from_ts, input_to_ts, output_language, sources_json, state, created_at, updated_at)
                 VALUES ('2026-08-11', 200, 300, 'en', '[]', 'unknown', 300, 300)",
                [],
            )
            .is_err());
        Ok(())
    }
}
