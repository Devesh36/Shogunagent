//! The data-layer half of the meetings / screen recall restriction for connections the app made
//! on its own (owner decision 2026-10-20, `shogun_mcp::source_policy`). The face sets the call's
//! access; the reads here leave out every row whose event source is a meeting transcript or the
//! screen text Visual recall keeps, before anything is returned.

use std::collections::HashSet;

use rusqlite::Connection;
use shogun_memory::search::{CanonicalEvidenceId, FederatedHit, FederatedSourceId, FederatedSourceKind};
use shogun_memory::search_documents::SearchDocumentKind;
use shogun_mcp::source_policy::SENSITIVE_EVENT_SOURCES;

/// The ids among `ids` whose event is a meeting transcript or kept screen text.
pub(crate) fn sensitive_event_ids(conn: &Connection, ids: &[i64]) -> rusqlite::Result<HashSet<i64>> {
    let mut out = HashSet::new();
    let sources = SENSITIVE_EVENT_SOURCES
        .iter()
        .map(|source| format!("'{source}'"))
        .collect::<Vec<_>>()
        .join(",");
    // Chunked well below SQLite's bind-variable limit.
    for chunk in ids.chunks(500) {
        let marks = vec!["?"; chunk.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT id FROM event_log WHERE id IN ({marks}) AND source IN ({sources})"
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| row.get::<_, i64>(0))?;
        for id in rows {
            out.insert(id?);
        }
    }
    Ok(out)
}

/// Every event id a hit rests on: its own, its collapsed sources', and its evidence.
pub(crate) fn hit_event_ids(hit: &FederatedHit) -> impl Iterator<Item = i64> + '_ {
    let own = match hit.source_id {
        FederatedSourceId::Event(id) => Some(id),
        _ => None,
    };
    let sources = hit.sources.iter().filter_map(|source| match source.id {
        FederatedSourceId::Event(id) => Some(id),
        _ => None,
    });
    let evidence = hit.evidence_ids.iter().filter_map(|id| match id {
        CanonicalEvidenceId::Event(id) => Some(*id),
        _ => None,
    });
    own.into_iter().chain(sources).chain(evidence)
}

fn is_meeting_kind(kind: &FederatedSourceKind) -> bool {
    matches!(
        kind,
        FederatedSourceKind::Meeting
            | FederatedSourceKind::Derived(
                SearchDocumentKind::SessionSummary
                    | SearchDocumentKind::MeetingRecap
                    | SearchDocumentKind::MeetingNote
            )
    )
}

fn is_meeting_evidence(id: &CanonicalEvidenceId) -> bool {
    matches!(
        id,
        CanonicalEvidenceId::Session(_)
            | CanonicalEvidenceId::TranscriptSegment(_)
            | CanonicalEvidenceId::SessionNote(_)
            | CanonicalEvidenceId::MeetingRecap(_)
    )
}

/// Leave out the hits a withheld connection may not see, and the evidence pointers into them:
///
/// - a meeting hit (transcripts, minutes, session summaries, meeting notes) is dropped;
/// - a hit whose own text is a sensitive event, or that collapsed one in, is dropped;
/// - any other hit (a person, a commitment, a period) stays, without its evidence ids that point
///   at a sensitive event or a meeting, so neither `provenance` nor `open` leads back to one.
pub(crate) fn withhold_hits(hits: Vec<FederatedHit>, sensitive: &HashSet<i64>) -> Vec<FederatedHit> {
    hits.into_iter()
        .filter(|hit| {
            !is_meeting_kind(&hit.source_kind)
                && !hit.sources.iter().any(|source| is_meeting_kind(&source.kind))
                && !matches!(hit.source_id, FederatedSourceId::Event(id) if sensitive.contains(&id))
                && !hit.sources.iter().any(
                    |source| matches!(source.id, FederatedSourceId::Event(id) if sensitive.contains(&id)),
                )
        })
        .map(|mut hit| {
            hit.evidence_ids.retain(|id| {
                !is_meeting_evidence(id)
                    && !matches!(id, CanonicalEvidenceId::Event(event) if sensitive.contains(event))
            });
            hit
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use shogun_memory::search::FederatedSource;

    fn hit(kind: FederatedSourceKind, id: FederatedSourceId, evidence: Vec<CanonicalEvidenceId>) -> FederatedHit {
        FederatedHit {
            sources: vec![FederatedSource { kind: kind.clone(), id: id.clone() }],
            source_kind: kind,
            source_id: id,
            evidence_ids: evidence,
            title: String::new(),
            excerpt: "text".into(),
            confidence: 1.0,
            period_start_ms: None,
            period_end_ms: None,
            component_ranks: Default::default(),
            fused_score: 1.0,
            tiers: Default::default(),
            answer_eligible: true,
            match_quality: Default::default(),
        }
    }

    #[test]
    fn meeting_and_screen_text_hits_are_dropped_and_their_pointers_stripped() {
        let sensitive: HashSet<i64> = [2, 3].into_iter().collect();
        let hits = vec![
            // A captured event: kept.
            hit(FederatedSourceKind::Event, FederatedSourceId::Event(1), vec![CanonicalEvidenceId::Event(1)]),
            // A screen-text event and a meeting transcript event: dropped.
            hit(FederatedSourceKind::Event, FederatedSourceId::Event(2), vec![]),
            hit(FederatedSourceKind::Event, FederatedSourceId::Event(3), vec![]),
            // A meeting search row: dropped.
            hit(FederatedSourceKind::Meeting, FederatedSourceId::Meeting(9), vec![CanonicalEvidenceId::TranscriptSegment(4)]),
            hit(
                FederatedSourceKind::Derived(SearchDocumentKind::MeetingRecap),
                FederatedSourceId::Event(10),
                vec![],
            ),
            // A user note: kept.
            hit(FederatedSourceKind::UserNote, FederatedSourceId::UserNote(5), vec![CanonicalEvidenceId::UserNote(5)]),
        ];
        let mut person = hit(
            FederatedSourceKind::Derived(SearchDocumentKind::Person),
            FederatedSourceId::Event(11),
            vec![
                CanonicalEvidenceId::Event(1),
                CanonicalEvidenceId::Event(2),
                CanonicalEvidenceId::TranscriptSegment(7),
                CanonicalEvidenceId::Event(12),
            ],
        );
        person.source_id = FederatedSourceId::UserNote(99);
        person.sources.clear();
        let mut all = hits;
        all.push(person);
        let kept = withhold_hits(all, &sensitive);
        let ids: Vec<_> = kept.iter().map(|h| h.source_id.clone()).collect();
        assert_eq!(
            ids,
            [FederatedSourceId::Event(1), FederatedSourceId::UserNote(5), FederatedSourceId::UserNote(99)]
        );
        // The person stays, without the evidence into screen text or a transcript.
        assert_eq!(kept[2].evidence_ids, [CanonicalEvidenceId::Event(1), CanonicalEvidenceId::Event(12)]);
    }

    #[test]
    fn sensitive_ids_come_from_the_event_source() -> rusqlite::Result<()> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE TABLE event_log (id INTEGER PRIMARY KEY, source TEXT NOT NULL);
             INSERT INTO event_log VALUES (1,'capture'),(2,'screen_text'),(3,'meeting'),(4,'gmail'),(5,'screen_ocr');",
        )?;
        let found = sensitive_event_ids(&conn, &[1, 2, 3, 4, 5, 6])?;
        let mut found: Vec<i64> = found.into_iter().collect();
        found.sort_unstable();
        assert_eq!(found, [2, 3, 5]);
        assert!(sensitive_event_ids(&conn, &[])?.is_empty());
        Ok(())
    }
}
