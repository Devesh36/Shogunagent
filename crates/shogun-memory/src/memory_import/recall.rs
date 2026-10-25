//! Retrieval over committed imported claims (#391 follow-up): the lexical lane local search and
//! context assembly read, so an imported claim can be *found* — always as an imported, capped,
//! `possibly` claim, never as observed fact.
//!
//! There is no projection or FTS table to keep in sync: this reads `memory_import_items` directly,
//! so `memory.import.delete` (which removes the rows) takes the claims out of search and context in
//! the same transaction. The table is bounded by what the user chose to import (each request is at
//! most [`super::MAX_REQUEST_BYTES`]), and the scan is one indexed-free pass of `instr` per term —
//! cheap next to the event FTS lane. `tests/imported_search_bounds.rs` pins the bounded work on
//! every `cargo test`; the wall-clock SLO gate is the explicit release run in `tests/search_slo.rs`.
//!
//! Superseded claims are not recalled: the package itself says they no longer hold. Items below
//! `min_confidence` are not recalled either — the same fact floor derived search documents use, so
//! a Low (0.3) imported claim stays listable in the import history but never reaches an answer.

use rusqlite::{params_from_iter, types::Value, Connection};

use super::package::Platform;

/// One imported claim that matched a query.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedMatch {
    /// `memory_import_items.id`.
    pub item_id: i64,
    /// `memory_imports.id` — the provenance record.
    pub import_id: i64,
    pub platform: Platform,
    pub section: String,
    pub kind: Option<String>,
    pub name: String,
    pub target: Option<String>,
    pub summary: Option<String>,
    pub status: String,
    /// Stored confidence; never above [`super::CONFIDENCE_MAX`].
    pub confidence: f64,
    /// How many of the query terms the claim contains.
    pub matched_terms: usize,
}

impl ImportedMatch {
    /// The searchable body: what the claim says, with its section and status so a reader sees
    /// "goal … (active)" rather than a bare phrase.
    pub fn body(&self) -> String {
        let mut out = String::new();
        out.push_str(&self.section.replace('_', " "));
        if let Some(kind) = self.kind.as_deref().filter(|k| !k.is_empty()) {
            out.push_str(" (");
            out.push_str(kind);
            out.push(')');
        }
        out.push_str(": ");
        out.push_str(&self.name);
        if let Some(target) = self.target.as_deref().filter(|t| !t.is_empty()) {
            out.push_str(" → ");
            out.push_str(target);
        }
        if let Some(summary) = self.summary.as_deref().filter(|s| !s.is_empty() && *s != self.name) {
            out.push_str(" — ");
            out.push_str(summary);
        }
        out.push_str(" [");
        out.push_str(&self.status);
        out.push(']');
        out
    }
}

/// Imported claims containing at least one of `terms`, best term coverage first, newest first on
/// ties. `terms` are matched case-insensitively for ASCII (SQLite `lower`), exactly otherwise —
/// the same bytes the trigram FTS lanes see for CJK.
pub fn search_imported(
    conn: &Connection,
    terms: &[String],
    min_confidence: f64,
    limit: usize,
) -> Result<Vec<ImportedMatch>, rusqlite::Error> {
    let terms: Vec<String> = terms
        .iter()
        .map(|t| t.to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    if terms.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    // `instr(hay, ?n) > 0` per term, summed: the number of distinct terms the claim contains.
    let matched = (0..terms.len())
        .map(|i| format!("(instr(hay, ?{}) > 0)", i + 3))
        .collect::<Vec<_>>()
        .join(" + ");
    let sql = format!(
        "WITH c AS (
             SELECT i.id, i.import_id, m.origin, i.section, i.kind, i.name, i.target, i.summary,
                    i.status, i.confidence,
                    lower(i.name || ' ' || coalesce(i.summary, '') || ' ' || coalesce(i.target, '')
                          || ' ' || coalesce(i.aliases, '') || ' ' || coalesce(i.kind, '')) AS hay
               FROM memory_import_items i JOIN memory_imports m ON m.id = i.import_id
              WHERE i.confidence >= ?1 AND i.status <> 'superseded'
         )
         SELECT id, import_id, origin, section, kind, name, target, summary, status, confidence,
                ({matched}) AS matched
           FROM c
          WHERE matched > 0
          ORDER BY matched DESC, id DESC
          LIMIT ?2"
    );
    let mut values: Vec<Value> = Vec::with_capacity(terms.len() + 2);
    values.push(Value::Real(min_confidence));
    values.push(Value::Integer(i64::try_from(limit).unwrap_or(i64::MAX)));
    values.extend(terms.into_iter().map(Value::Text));
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(values), |r| {
        let origin: String = r.get(2)?;
        Ok((
            origin,
            ImportedMatch {
                item_id: r.get(0)?,
                import_id: r.get(1)?,
                platform: Platform::Other,
                section: r.get(3)?,
                kind: r.get(4)?,
                name: r.get(5)?,
                target: r.get(6)?,
                summary: r.get(7)?,
                status: r.get(8)?,
                confidence: r.get(9)?,
                matched_terms: usize::try_from(r.get::<_, i64>(10)?).unwrap_or(0),
            },
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (origin, mut item) = row?;
        // The V52 CHECK admits only known origins; an unknown one (a newer binary's row) is
        // still an import, labelled `other` rather than dropped.
        item.platform = Platform::parse(&origin).unwrap_or(Platform::Other);
        out.push(item);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_import::{commit, delete_import, parse_request, Via};

    fn import(conn: &mut Connection, pkg: &str) -> i64 {
        let req = parse_request(&format!(r#"{{"package":{pkg},"select":"default"}}"#)).unwrap();
        commit(conn, &req, Via::Api, 7).unwrap().import_id
    }

    const PKG: &str = r#"{"format":"shogun-memory-import/v1","source":{"platform":"computer_history"},
        "profile":{"goals":[{"value":"Ship the Orchid beta","confidence":"high","evidence":"e","status":"active"},
                            {"value":"Old Orchid plan","confidence":"high","evidence":"e","status":"superseded"}],
                   "preferences":[{"value":"Orchid standups short","confidence":"low","evidence":"e","status":"active"}]},
        "entities":[{"name":"Orchid","type":"project","summary":"Mobile banking app","aliases":["Project Orchid"],"confidence":"medium","evidence":"e","status":"active"}]}"#;

    fn terms(t: &[&str]) -> Vec<String> {
        t.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn finds_medium_claims_by_term_coverage_and_skips_low_and_superseded() {
        let mut conn = crate::open_in_memory().unwrap();
        let id = import(&mut conn, PKG);
        // The default selection skips superseded; select it explicitly so the filter is tested.
        let req = parse_request(&format!(r#"{{"package":{PKG},"select":["goals/1"]}}"#)).unwrap();
        commit(&mut conn, &req, Via::Api, 8).unwrap();

        let hits = search_imported(&conn, &terms(&["orchid", "banking"]), 0.5, 10).unwrap();
        let names: Vec<&str> = hits.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["Orchid", "Ship the Orchid beta"], "coverage first; low + superseded out");
        assert_eq!(hits[0].matched_terms, 2);
        assert_eq!(hits[0].import_id, id);
        assert_eq!(hits[0].platform, Platform::ComputerHistory);
        assert!(hits.iter().all(|h| h.confidence <= crate::memory_import::CONFIDENCE_MAX));
        assert_eq!(hits[0].body(), "entity (project): Orchid — Mobile banking app [active]");

        // ASCII case-folds; aliases are searchable.
        let alias = search_imported(&conn, &terms(&["PROJECT ORCHID"]), 0.5, 10).unwrap();
        assert_eq!(alias.len(), 1);
        // A floor at 0.0 lets the Low claim through (the history/listing view, not search).
        let all = search_imported(&conn, &terms(&["standups"]), 0.0, 10).unwrap();
        assert_eq!(all.len(), 1);
        assert!(search_imported(&conn, &terms(&["standups"]), 0.5, 10).unwrap().is_empty());
        assert!(search_imported(&conn, &[], 0.5, 10).unwrap().is_empty());
        assert!(search_imported(&conn, &terms(&["orchid"]), 0.5, 0).unwrap().is_empty());
    }

    #[test]
    fn deleting_the_import_removes_its_claims_from_recall() {
        let mut conn = crate::open_in_memory().unwrap();
        let id = import(&mut conn, PKG);
        assert!(!search_imported(&conn, &terms(&["orchid"]), 0.5, 10).unwrap().is_empty());
        assert!(delete_import(&mut conn, id).unwrap());
        assert!(search_imported(&conn, &terms(&["orchid"]), 0.0, 10).unwrap().is_empty());
    }

    #[test]
    fn cjk_terms_match_as_stored() {
        let mut conn = crate::open_in_memory().unwrap();
        import(
            &mut conn,
            r#"{"format":"shogun-memory-import/v1","source":{"platform":"claude"},
            "profile":{"goals":[{"value":"決算資料を金曜までに仕上げる","confidence":"medium","evidence":"e","status":"active"}]}}"#,
        );
        let hits = search_imported(&conn, &terms(&["決算資"]), 0.5, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].platform, Platform::Claude);
    }
}
