//! Keeping one row per real person or project (the model half).
//!
//! After an hourly review has applied its entities, the stored people and projects are shown to
//! the model once more, whole, with the question "which of these records are the same one?". Its
//! groups are merged by `shogun_memory::entity_dedupe` (survivor rules, provenance union, nothing
//! user-edited is overwritten). The deterministic merge of case and alias collisions needs no model
//! and runs in local maintenance; this catches "Toru" / "toru tano" / the GitHub handle.
//!
//! Best effort and Batch lane only (invariant 5): a failure changes nothing and is retried after
//! the next review. It is only asked again when the people or projects tables changed — after a
//! failure too, so a relay outage is not re-asked every tick — and at most every
//! [`MIN_INTERVAL_MS`] (two calls per ask; the owner's budget is tens of calls a day).

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use crate::daemon::Db;
use crate::llm::anthropic::{BatchItem, BatchLane};

/// The relay purpose: server-side shape `{"groups":[{"ids":[..],"canonical_name":".."}]}`.
pub const DEDUPE_PURPOSE: &str = "entity_dedupe";

const DEDUPE_INSTRUCTION: &str = "\
    You clean up a personal memory. Each line of the data is one stored record of a person or a \
    project: id | name | also known as | about. Decide which records are the SAME real person or \
    project and should be one record.\n\
    The same: a first name and the full name, a handle and a name, a nickname, a misspelling, a \
    case or spacing variant, a name with a title or a company attached. Different people who only \
    share a first name are NOT the same, but a handle or username that starts with or contains another record\'s name (Torutesu and Toru Tano) is the same person unless the descriptions clearly say otherwise: group records only when the names, the aliases or the \
    descriptions point to one entity.\n\
    Return ONLY a JSON object (no prose, no code fence): \
    {\"groups\":[{\"ids\":[the ids of the records that are one entity, at least two],\
    \"canonical_name\":\"the fullest proper-cased form of the name, e.g. Toru Tano\"}]}. \
    Use only ids that appear in the data, each id in at most one group. If nothing is duplicated \
    return {\"groups\":[]}.";

/// The fingerprint of the people and projects tables at the last ask, answered or not (0 = never).
static LAST_ASKED: AtomicU64 = AtomicU64::new(0);
/// When the last ask went out (0 = never this process).
static LAST_ASKED_MS: AtomicI64 = AtomicI64::new(0);
/// The shortest gap between two asks.
pub const MIN_INTERVAL_MS: i64 = 6 * 3_600_000;

/// One review item per table that has records worth asking about. All items share one `system`.
pub fn dedupe_items(inputs: &[(&'static str, String)]) -> Vec<BatchItem> {
    let system: std::sync::Arc<str> = crate::llm::fence_instruction(DEDUPE_INSTRUCTION).into();
    inputs
        .iter()
        .map(|(label, text)| BatchItem {
            custom_id: (*label).to_string(),
            purpose: DEDUPE_PURPOSE.to_string(),
            chunk: crate::llm::fence_data(&shogun_redact::redact_egress(text)),
            system: Some(system.clone()),
        })
        .collect()
}

/// Whether the tables changed since the last ask. Cheap: checked before any client is built.
pub fn dedupe_due(db: &Db) -> bool {
    is_due(db.entity_fingerprint(), db.now_ms())
}

fn is_due(fingerprint: u64, now_ms: i64) -> bool {
    let last_ms = LAST_ASKED_MS.load(Ordering::Relaxed);
    (fingerprint == 0 || fingerprint != LAST_ASKED.load(Ordering::Relaxed))
        && (last_ms == 0 || now_ms - last_ms >= MIN_INTERVAL_MS)
}

fn stamp(fingerprint: u64, now_ms: i64) {
    LAST_ASKED.store(fingerprint, Ordering::Relaxed);
    LAST_ASKED_MS.store(now_ms, Ordering::Relaxed);
}

/// Ask the model which stored people / projects are the same and merge what it names. Returns the
/// rows absorbed. Never fails the caller.
pub async fn dedupe_entities<B: BatchLane>(db: &Db, lane: &B) -> usize {
    let fingerprint = db.entity_fingerprint();
    if !is_due(fingerprint, db.now_ms()) {
        return 0;
    }
    let inputs = db.entity_dedupe_inputs();
    if inputs.is_empty() {
        LAST_ASKED.store(fingerprint, Ordering::Relaxed);
        return 0;
    }
    stamp(fingerprint, db.now_ms());
    let Ok(results) = lane.review(&dedupe_items(&inputs)).await else {
        // Not asked again until the tables change and the interval has passed, not every tick.
        return 0;
    };
    let mut merged = 0;
    for result in &results {
        if let Some(text) = result.text.as_deref() {
            merged += db.apply_entity_groups(&result.custom_id, text);
        }
    }
    stamp(db.entity_fingerprint(), db.now_ms());
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asks_only_when_the_tables_changed_and_the_interval_passed() {
        stamp(7, 1_000);
        assert!(!is_due(7, 1_000 + MIN_INTERVAL_MS), "nothing changed");
        assert!(!is_due(8, 1_000 + MIN_INTERVAL_MS - 1), "changed, but asked too recently");
        assert!(is_due(8, 1_000 + MIN_INTERVAL_MS));
    }

    #[test]
    fn items_share_one_system_and_carry_the_table_label() {
        let items = dedupe_items(&[("people", "1 | Toru | | ".into()), ("projects", "2 | Kestrel | | ".into())]);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].custom_id, "people");
        assert_eq!(items[1].custom_id, "projects");
        assert!(items.iter().all(|i| i.purpose == DEDUPE_PURPOSE));
        assert_eq!(items[0].system, items[1].system);
        assert!(items[0].system.as_deref().is_some_and(|s| s.contains("canonical_name")));
        assert!(items[0].chunk.contains("Toru"));
    }
}
