//! The import preview: what committing each item would do. Reads the database, never writes.
//!
//! Dedupe rules (the design note has the table):
//!
//! * Keys are [`super::normalize_key`] of the name — plus every alias for an entity. Two items
//!   collide only inside the same *bucket*: the same entity type (a person "Atlas" is not the
//!   project "Atlas"), the same profile section, the same relation, or open loops.
//! * **Inside the package**, the first item wins; a later colliding item is `duplicate_skip` of it.
//! * **Already imported** — a collision with an item a previous import stored — is
//!   `duplicate_skip` too, so re-pasting the same package is a no-op.
//! * **Existing records** — people (name, aliases, addresses, handles), projects (name), extracted
//!   organizations / tools / topics (name), open loops (description) — give `merge_into`: on
//!   commit the imported claim is stored *linked* to that record. The observed record itself is
//!   never overwritten; its own confidence and provenance stay exactly as they were.
//! * Otherwise `new`.

use std::collections::HashMap;

use rusqlite::Connection;
use serde::Serialize;

use super::package::{Correction, ImportItem, ImportPackage, ItemError, Platform, Section};
use super::{normalize_key, FORMAT};

/// What committing one item would do.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProposedAction {
    /// Stored as a new imported claim.
    New,
    /// Stored linked to an existing record, which is left untouched. `label` is that record's
    /// name, so the reviewer can see what it would attach to.
    MergeInto { table: &'static str, id: i64, label: String },
    /// Not stored by default: `of` is the earlier package item (`entities/0`) or the earlier
    /// import (`import:3`) that already carries it.
    DuplicateSkip { reason: &'static str, of: String },
}

/// One valid item and what would happen to it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProposedItem {
    #[serde(flatten)]
    pub item: ImportItem,
    /// The confidence it would be stored at (capped; never High).
    pub confidence: f64,
    /// Always true: nothing imported reaches the fact band, so every read surface hedges it.
    pub possibly: bool,
    /// The package claimed `high` and the cap lowered it.
    pub capped: bool,
    pub action: ProposedAction,
    /// Whether "import the defaults" would include it (not a duplicate, not superseded).
    pub selected_by_default: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PreviewCounts {
    pub new: usize,
    pub merge_into: usize,
    pub duplicate_skip: usize,
    pub rejected: usize,
}

/// The whole preview — what the Import memory screen and every API face show before commit.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ImportPreview {
    pub format: &'static str,
    pub platform: Platform,
    pub provenance: &'static str,
    pub generated_at: Option<String>,
    pub coverage_note: Option<String>,
    pub package_bytes: usize,
    pub package_xxh64: String,
    pub items: Vec<ProposedItem>,
    pub rejected: Vec<ItemError>,
    pub corrections_needed: Vec<Correction>,
    pub counts: PreviewCounts,
}

/// The collision bucket of an item: entity type / profile section / relation / open loop.
fn bucket(item: &ImportItem) -> String {
    match item.section {
        Section::Entity | Section::Relationship => {
            format!("{}:{}", item.section.as_str(), item.kind.as_deref().unwrap_or(""))
        }
        other => other.as_str().to_string(),
    }
}

type Index = HashMap<(String, String), (&'static str, i64, String)>;

/// Existing records keyed by (bucket, normalised key) → (table, id, label).
fn existing_index(conn: &Connection) -> Result<Index, rusqlite::Error> {
    let mut index = Index::new();
    let mut put = |bucket: &str, raw: &str, table: &'static str, id: i64, label: &str| {
        let key = normalize_key(raw);
        if !key.is_empty() {
            index
                .entry((bucket.to_string(), key))
                .or_insert_with(|| (table, id, label.to_string()));
        }
    };
    {
        let mut stmt = conn.prepare(
            "SELECT id, display_name, aliases, emails, handles FROM people ORDER BY id",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let id: i64 = r.get(0)?;
            let name: String = r.get(1)?;
            put("entity:person", &name, "people", id, &name);
            for col in 2..=4 {
                let json: Option<String> = r.get(col)?;
                for alias in json_strings(json.as_deref()) {
                    put("entity:person", &alias, "people", id, &name);
                }
            }
        }
    }
    {
        let mut stmt = conn.prepare("SELECT id, name FROM projects ORDER BY id")?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let (id, name): (i64, String) = (r.get(0)?, r.get(1)?);
            put("entity:project", &name, "projects", id, &name);
        }
    }
    {
        let mut stmt = conn.prepare(
            "SELECT id, kind, name FROM knowledge_entities
              WHERE kind IN ('organization', 'tool', 'topic') ORDER BY id",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let (id, kind, name): (i64, String, String) = (r.get(0)?, r.get(1)?, r.get(2)?);
            put(&format!("entity:{kind}"), &name, "knowledge_entities", id, &name);
        }
    }
    {
        let mut stmt = conn.prepare("SELECT id, description FROM open_loops ORDER BY id")?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let (id, description): (i64, String) = (r.get(0)?, r.get(1)?);
            put("open_loop", &description, "open_loops", id, &description);
        }
    }
    Ok(index)
}

/// Items earlier imports stored, keyed by (bucket, key) → import id.
fn imported_index(conn: &Connection) -> Result<HashMap<(String, String), i64>, rusqlite::Error> {
    let mut index = HashMap::new();
    let mut stmt = conn.prepare(
        "SELECT import_id, section, kind, match_key, aliases FROM memory_import_items ORDER BY id",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let import_id: i64 = r.get(0)?;
        let section: String = r.get(1)?;
        let kind: Option<String> = r.get(2)?;
        let key: String = r.get(3)?;
        let aliases: Option<String> = r.get(4)?;
        let bucket = match (section.as_str(), kind) {
            ("entity" | "relationship", Some(kind)) => format!("{section}:{kind}"),
            _ => section.clone(),
        };
        index.entry((bucket.clone(), key)).or_insert(import_id);
        for alias in json_strings(aliases.as_deref()) {
            let k = normalize_key(&alias);
            if !k.is_empty() {
                index.entry((bucket.clone(), k)).or_insert(import_id);
            }
        }
    }
    Ok(index)
}

/// Strings in a JSON array column: plain strings, or `{"value": …}` objects. Anything else
/// (NULL, malformed JSON) is no aliases rather than an error.
fn json_strings(json: Option<&str>) -> Vec<String> {
    let Some(json) = json else { return Vec::new() };
    let Ok(serde_json::Value::Array(list)) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|v| v.as_str().or_else(|| v.get("value").and_then(|x| x.as_str())))
        .map(str::to_string)
        .collect()
}

/// Propose an action for every valid item of `package` against what `conn` already holds.
pub fn preview(conn: &Connection, package: &ImportPackage) -> Result<ImportPreview, rusqlite::Error> {
    let existing = existing_index(conn)?;
    let imported = imported_index(conn)?;
    let mut seen: HashMap<(String, String), String> = HashMap::new();
    let mut counts = PreviewCounts { rejected: package.rejected.len(), ..PreviewCounts::default() };
    let mut items = Vec::with_capacity(package.items.len());

    for item in &package.items {
        let bucket = bucket(item);
        let keys = item.keys();
        let find = |map_has: &dyn Fn(&(String, String)) -> bool| {
            keys.iter().map(|k| (bucket.clone(), k.clone())).find(|probe| map_has(probe))
        };
        let action = if let Some(probe) = find(&|p| seen.contains_key(p)) {
            ProposedAction::DuplicateSkip { reason: "in_package", of: seen[&probe].clone() }
        } else if let Some(probe) = find(&|p| imported.contains_key(p)) {
            ProposedAction::DuplicateSkip { reason: "already_imported", of: format!("import:{}", imported[&probe]) }
        } else if let Some(probe) = find(&|p| existing.contains_key(p)) {
            let (table, id, label) = existing[&probe].clone();
            ProposedAction::MergeInto { table, id, label }
        } else {
            ProposedAction::New
        };
        match action {
            ProposedAction::New => counts.new += 1,
            ProposedAction::MergeInto { .. } => counts.merge_into += 1,
            ProposedAction::DuplicateSkip { .. } => counts.duplicate_skip += 1,
        }
        let duplicate = matches!(action, ProposedAction::DuplicateSkip { .. });
        if !duplicate {
            for key in keys {
                seen.entry((bucket.clone(), key)).or_insert_with(|| item.id.clone());
            }
        }
        let confidence = item.stored_confidence();
        items.push(ProposedItem {
            confidence,
            possibly: true,
            capped: item.claimed == super::package::Claimed::High,
            selected_by_default: !duplicate && item.status != "superseded",
            action,
            item: item.clone(),
        });
    }

    Ok(ImportPreview {
        format: FORMAT,
        platform: package.platform,
        provenance: package.platform.provenance(),
        generated_at: package.generated_at.clone(),
        coverage_note: package.coverage_note.clone(),
        package_bytes: package.package_bytes,
        package_xxh64: package.package_xxh64.clone(),
        items,
        rejected: package.rejected.clone(),
        corrections_needed: package.corrections_needed.clone(),
        counts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_import::parse_package;

    fn package(body: &str) -> ImportPackage {
        parse_package(&format!(
            r#"{{"format":"shogun-memory-import/v1","source":{{"platform":"chatgpt"}},{body}}}"#
        ))
        .unwrap()
    }

    fn entity(name: &str, kind: &str, aliases: &str) -> String {
        format!(
            r#"{{"name":"{name}","type":"{kind}","aliases":{aliases},"confidence":"high","evidence":"e","status":"active"}}"#
        )
    }

    #[test]
    fn dedupe_inside_the_package_against_records_and_by_type() {
        let mut conn = crate::open_in_memory().unwrap();
        let ev = crate::event_log::insert(
            &conn,
            &crate::event_log::NewEvent {
                ts: 1,
                source: "capture",
                kind: "text",
                app_bundle_id: None,
                window_title: None,
                content: "x",
                content_hash: "h",
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap();
        let alex = crate::state::insert_person(
            &mut conn,
            &crate::state::NewPerson {
                display_name: "Alex Morgan",
                aliases_json: Some(r#"["Al"]"#),
                emails_json: Some(r#"["alex@acme.com"]"#),
                confidence: 0.9,
                now: 1,
                ..Default::default()
            },
            &[crate::state::Provenance::new(ev)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO open_loops (kind, description, opened_at, status, confidence, created_at, updated_at, last_evidence_at)
             VALUES ('other', 'Send the Atlas deck', 1, 'open', 0.4, 1, 1, 1)",
            [],
        )
        .unwrap();

        let p = package(&format!(
            r#""entities":[{},{},{},{},{}],
               "open_loops":[{{"summary":"send the atlas deck.","status":"open","confidence":"low","evidence":null}}],
               "profile":{{"preferences":[{{"value":"Short","confidence":"low","evidence":null,"status":"superseded"}}]}}"#,
            entity("alex@acme.com", "person", "[]"),
            entity("Atlas", "project", r#"["Project Atlas"]"#),
            entity("atlas", "topic", "[]"),
            entity("Project  ATLAS", "project", "[]"),
            entity("Rust", "tool", "[]"),
        ));
        let pv = preview(&conn, &p).unwrap();
        let item = |id: &str| pv.items.iter().find(|i| i.item.id == id).unwrap();
        assert_eq!(
            item("entities/0").action,
            ProposedAction::MergeInto { table: "people", id: alex, label: "Alex Morgan".into() },
            "an address on the person matches"
        );
        assert_eq!(item("entities/1").action, ProposedAction::New);
        assert_eq!(item("entities/2").action, ProposedAction::New, "a topic is not the project");
        assert_eq!(
            item("entities/3").action,
            ProposedAction::DuplicateSkip { reason: "in_package", of: "entities/1".into() },
            "matches the earlier project's alias"
        );
        assert!(matches!(item("open_loops/0").action, ProposedAction::MergeInto { table: "open_loops", .. }));
        assert_eq!(pv.counts, PreviewCounts { new: 4, merge_into: 2, duplicate_skip: 1, rejected: 0 });
        assert!(pv.items.iter().all(|i| i.confidence <= crate::memory_import::CONFIDENCE_MAX && i.possibly));
        assert!(item("entities/0").capped);
        assert!(!item("preferences/0").selected_by_default, "superseded");
        assert!(!item("entities/3").selected_by_default);
        assert!(item("entities/4").selected_by_default);
        assert_eq!(pv.provenance, "imported:chatgpt");

        // The preview wrote nothing.
        let n: i64 = conn.query_row("SELECT count(*) FROM memory_import_items", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }
}
