//! The extra columns the Notch action row matches on (Context Fusion, FR-CF-01/03), read in three
//! indexed queries beside the regular `state::list_*` reads: a commitment's direction and
//! counterparty, an open loop's counterparty, and a person's aliases and addresses.
//!
//! Read-only and schema-neutral: no migration, nothing written. Visibility stays with the
//! `state::list_*` reads (rows without supported provenance are hidden there); this module only
//! decorates rows those reads already returned, keyed by id.

use std::collections::HashMap;

use rusqlite::Connection;

/// A commitment's direction (`theirs` = owed to me) and counterparty person id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CommitmentLink {
    pub theirs: bool,
    pub counterparty_id: Option<i64>,
}

/// A person's match terms: aliases and email addresses (JSON arrays in `people`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PersonTerms {
    pub aliases: Vec<String>,
    pub emails: Vec<String>,
}

/// Direction and counterparty for every unresolved commitment, by id.
pub fn commitment_links(conn: &Connection) -> Result<HashMap<i64, CommitmentLink>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, direction, counterparty_id FROM commitments WHERE status IN ('open', 'overdue')",
    )?;
    let rows = stmt.query_map([], |r| {
        let direction: String = r.get(1)?;
        Ok((r.get::<_, i64>(0)?, CommitmentLink { theirs: direction == "theirs", counterparty_id: r.get(2)? }))
    })?;
    rows.collect()
}

/// Counterparty person id for every open loop, by id.
pub fn open_loop_counterparties(conn: &Connection) -> Result<HashMap<i64, i64>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, counterparty_id FROM open_loops WHERE status = 'open' AND counterparty_id IS NOT NULL",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
    rows.collect()
}

/// A JSON array of strings, or nothing. Malformed values read as empty rather than failing the
/// whole row — the row still shows, it just matches on its display name only.
fn string_array(raw: Option<String>) -> Vec<String> {
    raw.and_then(|s| serde_json::from_str::<Vec<serde_json::Value>>(&s).ok())
        .map(|v| {
            v.into_iter()
                .filter_map(|x| x.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Aliases and addresses for every person that has any, by id.
pub fn person_terms(conn: &Connection) -> Result<HashMap<i64, PersonTerms>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, aliases, emails FROM people WHERE aliases IS NOT NULL OR emails IS NOT NULL",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            PersonTerms { aliases: string_array(r.get(1)?), emails: string_array(r.get(2)?) },
        ))
    })?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        insert_commitment, insert_open_loop, insert_person, CommitmentDirection, CommitmentStatus,
        NewCommitment, NewOpenLoop, NewPerson, OpenLoopKind, Provenance,
    };

    fn event(conn: &Connection) -> i64 {
        crate::event_log::insert(
            conn,
            &crate::event_log::NewEvent {
                ts: 1,
                source: "capture",
                kind: "text",
                app_bundle_id: None,
                window_title: None,
                content: "evidence",
                content_hash: "h1",
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn links_and_terms_are_read_by_id() {
        let mut conn = crate::open_in_memory().unwrap();
        let e = event(&conn);
        let p = insert_person(
            &mut conn,
            &NewPerson {
                display_name: "Dana Reyes",
                aliases_json: Some(r#"["dreyes", "", 7]"#),
                emails_json: Some(r#"["dana@client.example"]"#),
                handles_json: None,
                relationship_summary: None,
                confidence: 0.9,
                now: 1,
            },
            &[Provenance::new(e)],
        )
        .unwrap();
        let bad = insert_person(
            &mut conn,
            &NewPerson {
                display_name: "Broken",
                aliases_json: Some("not json"),
                emails_json: None,
                handles_json: None,
                relationship_summary: None,
                confidence: 0.9,
                now: 1,
            },
            &[Provenance::new(e)],
        )
        .unwrap();
        let c = insert_commitment(
            &mut conn,
            &NewCommitment {
                direction: CommitmentDirection::Theirs,
                counterparty_id: Some(p),
                description: "Dana owes the NDA",
                due_at: None,
                status: CommitmentStatus::Open,
                project_id: None,
                confidence: 0.9,
                now: 1,
            },
            &[Provenance::new(e)],
        )
        .unwrap();
        let l = insert_open_loop(
            &mut conn,
            &NewOpenLoop {
                kind: OpenLoopKind::ReplyNeeded,
                description: "Reply to Dana",
                counterparty_id: Some(p),
                project_id: None,
                opened_at: 1,
                confidence: 0.9,
                now: 1,
            },
            &[Provenance::new(e)],
        )
        .unwrap();

        let links = commitment_links(&conn).unwrap();
        assert_eq!(links.get(&c), Some(&CommitmentLink { theirs: true, counterparty_id: Some(p) }));
        assert_eq!(open_loop_counterparties(&conn).unwrap().get(&l), Some(&p));
        let terms = person_terms(&conn).unwrap();
        assert_eq!(terms[&p].aliases, vec!["dreyes".to_string()], "blank and non-string entries dropped");
        assert_eq!(terms[&p].emails, vec!["dana@client.example".to_string()]);
        assert!(terms[&bad].aliases.is_empty(), "malformed JSON reads as empty");
    }
}
