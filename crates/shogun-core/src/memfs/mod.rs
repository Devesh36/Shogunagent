//! The `/memories` virtual filesystem (plan E6-T1): the world model projected onto the file shape
//! agents already know (Anthropic's `memory_20250818` tool, Claude Code's `MEMORY.md`).
//!
//! ```text
//! /memories/INDEX.md                    ≤ 200 lines and ≤ 25 KB, always
//! /memories/commitments.md              open commitments, band and proving quote
//! /memories/open_loops.md               open loops, band and proving quote
//! /memories/people/<slug>.md            one per listed person
//! /memories/projects/<slug>.md          one per listed project
//! /memories/scratch/<agent_id>/**       the calling agent's own files (V100, stored)
//! ```
//!
//! Everything outside `scratch/` is **generated** from the state tables on every read: nothing is
//! stored and nothing is edited in place (a write there becomes a `memory.ingest` event; the
//! engine decides what becomes state). Only the world namespace is projected, and Low-confidence
//! rows are left out entirely: Medium ones are written `possibly:`, High ones as facts — the same
//! band rule every other face applies ([`shogun_fusion::confidence`]).
//!
//! Slugs ([`slug`]) are the lower-cased name with every run of other characters folded to `-`,
//! then `-<id>`: stable for a row whatever else is added or removed, and unique because the id is.

use rusqlite::{params, Connection, OptionalExtension};
use shogun_fusion::confidence::{band, Band, POSSIBLY_PREFIX};
use shogun_memory::actor::{Namespace, ReadScope};
use shogun_memory::state::{self, StateTable};

/// The mount point every path starts with.
pub const ROOT: &str = "/memories";
/// The index file.
pub const INDEX: &str = "/memories/INDEX.md";
/// The two state-list files.
pub const COMMITMENTS: &str = "/memories/commitments.md";
pub const OPEN_LOOPS: &str = "/memories/open_loops.md";
/// Directory prefixes.
pub const PEOPLE_DIR: &str = "/memories/people";
pub const PROJECTS_DIR: &str = "/memories/projects";
pub const SCRATCH_DIR: &str = "/memories/scratch";
/// `INDEX.md` limits (Claude Code loads the first 200 lines or 25 KB of `MEMORY.md`).
pub const INDEX_MAX_LINES: usize = 200;
pub const INDEX_MAX_BYTES: usize = 25_000;
/// Most people / projects projected as files.
pub const MAX_ENTITY_FILES: usize = 500;
/// Most rows a list file carries.
pub const MAX_LIST_ROWS: usize = 300;
/// Longest slug stem before the `-<id>` suffix.
const SLUG_STEM_CHARS: usize = 60;

/// One file the tree holds, with its size in bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub bytes: usize,
}

/// The stable file stem of a state row: lower-cased name, other characters folded to `-`, then
/// `-<id>`. A name with nothing usable in it becomes `<kind>-<id>`.
pub fn slug(name: &str, id: i64, kind: &str) -> String {
    let mut stem = String::new();
    let mut dash = false;
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            if dash && !stem.is_empty() {
                stem.push('-');
            }
            dash = false;
            stem.push(c);
        } else {
            dash = true;
        }
        if stem.chars().count() >= SLUG_STEM_CHARS {
            break;
        }
    }
    if stem.is_empty() {
        stem.push_str(kind);
    }
    format!("{stem}-{id}")
}

/// The id a slug ends in.
pub fn slug_id(slug: &str) -> Option<i64> {
    slug.rsplit('-').next().and_then(|id| id.parse().ok())
}

/// The scratch directory of an agent: `/memories/scratch/<agent_id>`.
pub fn scratch_dir(namespace: &Namespace) -> Option<String> {
    match namespace {
        Namespace::AgentScratch(id) => Some(format!("{SCRATCH_DIR}/{id}")),
        Namespace::World => None,
    }
}

/// Whether `path` is under the scratch tree (any agent's).
pub fn is_scratch_path(path: &str) -> bool {
    path == SCRATCH_DIR || path.starts_with(&format!("{SCRATCH_DIR}/"))
}

/// The calling agent's relative path for a scratch path, or `None` when the path is outside
/// that agent's own scratch directory.
pub fn own_scratch_relative<'a>(path: &'a str, namespace: &Namespace) -> Option<&'a str> {
    let dir = scratch_dir(namespace)?;
    path.strip_prefix(&dir).and_then(|rest| rest.strip_prefix('/')).filter(|rest| !rest.is_empty())
}

/// Any agent's scratch file path: its namespace and the relative path
/// (`/memories/scratch/<id>/<rel>`). Whether the caller may see it is the data layer's call
/// (E18 scratch ACL, [`shogun_memory::scratch_files`]).
pub fn scratch_target(path: &str) -> Option<(Namespace, &str)> {
    let rest = path.strip_prefix(SCRATCH_DIR)?.strip_prefix('/')?;
    let (id, rel) = rest.split_once('/')?;
    if rel.is_empty() {
        return None;
    }
    Some((Namespace::scratch(id).ok()?, rel))
}

fn band_prefix(confidence: f64) -> Option<&'static str> {
    match band(confidence) {
        Band::High => Some(""),
        Band::Medium => Some(POSSIBLY_PREFIX),
        Band::Low => None,
    }
}

fn band_name(confidence: f64) -> &'static str {
    match band(confidence) {
        Band::High => "high",
        Band::Medium => "medium",
        Band::Low => "low",
    }
}

fn quote(conn: &Connection, table: StateTable, id: i64) -> Result<Option<(String, i64)>, rusqlite::Error> {
    conn.query_row(
        "SELECT quote, event_id FROM state_provenance
         WHERE state_table = ?1 AND state_id = ?2 AND quote IS NOT NULL
         ORDER BY batch_reviewed DESC, id LIMIT 1",
        params![table.as_str(), id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
}

fn day(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let d = shogun_memory::due::civil_from_days(days);
    format!("{:04}-{:02}-{:02}", d.year, d.month, d.day)
}

/// A one-line quote, safe inside a Markdown blockquote.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The people the tree projects: visible, world namespace, not Low.
fn people(conn: &Connection) -> Result<Vec<state::PersonRow>, rusqlite::Error> {
    let page = i64::try_from(MAX_ENTITY_FILES).unwrap_or(i64::MAX);
    Ok(state::list_people_page_scoped(conn, page, 0, ReadScope::WORLD_ONLY)?
        .into_iter()
        .filter(|row| band_prefix(row.confidence).is_some())
        .collect())
}

fn projects(conn: &Connection) -> Result<Vec<state::ProjectRow>, rusqlite::Error> {
    let page = i64::try_from(MAX_ENTITY_FILES).unwrap_or(i64::MAX);
    Ok(state::list_projects_page_scoped(conn, page, 0, ReadScope::WORLD_ONLY)?
        .into_iter()
        .filter(|row| band_prefix(row.confidence).is_some())
        .collect())
}

fn open_commitments(conn: &Connection) -> Result<Vec<state::CommitmentRow>, rusqlite::Error> {
    Ok(state::list_commitments_scoped(conn, ReadScope::WORLD_ONLY)?
        .into_iter()
        .filter(|row| row.status != "done" && row.status != "cancelled" && band_prefix(row.confidence).is_some())
        .take(MAX_LIST_ROWS)
        .collect())
}

fn open_loops(conn: &Connection) -> Result<Vec<state::OpenLoopRow>, rusqlite::Error> {
    Ok(state::list_open_loops_scoped(conn, ReadScope::WORLD_ONLY)?
        .into_iter()
        .filter(|row| row.status == "open" && band_prefix(row.confidence).is_some())
        .take(MAX_LIST_ROWS)
        .collect())
}

/// `/memories/commitments.md`.
pub fn commitments_md(conn: &Connection) -> Result<String, rusqlite::Error> {
    let mut out = String::from("# Commitments\n\nOpen commitments. Lines starting `possibly:` are Medium confidence: verify before acting.\n");
    let rows = open_commitments(conn)?;
    if rows.is_empty() {
        out.push_str("\nNone open.\n");
    }
    for row in rows {
        let prefix = band_prefix(row.confidence).unwrap_or_default();
        out.push_str(&format!("\n- {prefix}{} (id {}, {}", one_line(&row.description), row.id, band_name(row.confidence)));
        if let Some(due) = row.due_at {
            out.push_str(&format!(", due {}", day(due)));
        }
        out.push_str(")\n");
        if let Some((text, event_id)) = quote(conn, StateTable::Commitments, row.id)? {
            out.push_str(&format!("  > {} (event {event_id})\n", one_line(&text)));
        }
    }
    Ok(out)
}

/// `/memories/open_loops.md`.
pub fn open_loops_md(conn: &Connection) -> Result<String, rusqlite::Error> {
    let mut out = String::from("# Open loops\n\nWhat is waiting or unresolved. Lines starting `possibly:` are Medium confidence.\n");
    let rows = open_loops(conn)?;
    if rows.is_empty() {
        out.push_str("\nNone open.\n");
    }
    for row in rows {
        let prefix = band_prefix(row.confidence).unwrap_or_default();
        out.push_str(&format!(
            "\n- {prefix}{} ({}, id {}, {})\n",
            one_line(&row.description),
            row.kind,
            row.id,
            band_name(row.confidence)
        ));
        if let Some((text, event_id)) = quote(conn, StateTable::OpenLoops, row.id)? {
            out.push_str(&format!("  > {} (event {event_id})\n", one_line(&text)));
        }
    }
    Ok(out)
}

/// The commitments and open loops a person or project is party to, as list lines.
fn related(conn: &Connection, column: &str, id: i64) -> Result<Vec<String>, rusqlite::Error> {
    let mut lines = Vec::new();
    for (table, label, open) in [("commitments", "commitment", "status NOT IN ('done', 'cancelled')"), ("open_loops", "open loop", "status = 'open'")] {
        let mut stmt = conn.prepare(&format!(
            "SELECT description, confidence FROM {table}
             WHERE {column} = ?1 AND {open} AND namespace = 'world' AND merged_into IS NULL
             ORDER BY updated_at DESC LIMIT 20"
        ))?;
        let rows = stmt.query_map([id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)))?;
        for row in rows {
            let (description, confidence) = row?;
            if let Some(prefix) = band_prefix(confidence) {
                lines.push(format!("- {label}: {prefix}{}", one_line(&description)));
            }
        }
    }
    Ok(lines)
}

fn entity_md(conn: &Connection, table: StateTable, id: i64) -> Result<Option<String>, rusqlite::Error> {
    let Some(row) = shogun_memory::state_edit::export_entity(conn, table, id)? else {
        return Ok(None);
    };
    let Some(prefix) = band_prefix(row.confidence) else {
        return Ok(None);
    };
    let mut out = format!("# {}\n\n", one_line(&row.title));
    out.push_str(&format!("Confidence: {} ({:.2}).", band_name(row.confidence), row.confidence));
    if let Some(status) = &row.status {
        out.push_str(&format!(" Status: {status}."));
    }
    out.push('\n');
    if let Some(person) = &row.person {
        if !person.aliases.is_empty() {
            out.push_str(&format!("Also called: {}.\n", person.aliases.join(", ")));
        }
    }
    if let Some(detail) = row.detail.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        out.push_str(&format!("\n{prefix}{detail}\n"));
    }
    let column = if table == StateTable::People { "counterparty_id" } else { "project_id" };
    let related = related(conn, column, id)?;
    if !related.is_empty() {
        out.push_str("\n## Open items\n\n");
        out.push_str(&related.join("\n"));
        out.push('\n');
    }
    let mut stmt = conn.prepare(
        "SELECT quote, event_id FROM state_provenance
         WHERE state_table = ?1 AND state_id = ?2 AND quote IS NOT NULL ORDER BY id DESC LIMIT 5",
    )?;
    let quotes = stmt
        .query_map(params![table.as_str(), id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    if !quotes.is_empty() || !row.provenance.is_empty() {
        out.push_str("\n## Evidence\n\n");
        for (text, event_id) in quotes {
            out.push_str(&format!("> {} (event {event_id})\n", one_line(&text)));
        }
        let ids: Vec<String> = row.provenance.iter().take(10).map(i64::to_string).collect();
        if !ids.is_empty() {
            out.push_str(&format!("Source events: {}.\n", ids.join(", ")));
        }
    }
    Ok(Some(out))
}

/// `/memories/INDEX.md`: what the tree holds, always within [`INDEX_MAX_LINES`] and
/// [`INDEX_MAX_BYTES`]. When the lists do not fit, the tail of each is replaced by one line
/// saying how many more there are and where to look.
pub fn index_md(conn: &Connection, scratch: Option<&Namespace>) -> Result<String, rusqlite::Error> {
    let commitments = open_commitments(conn)?.len();
    let loops = open_loops(conn)?.len();
    let people = people(conn)?;
    let projects = projects(conn)?;
    let mut head = vec![
        "# Memory index".to_string(),
        String::new(),
        "Generated from the world model; files outside scratch/ are read-only projections.".to_string(),
        "Writing to them records an event the engine reviews; it does not edit the file.".to_string(),
        String::new(),
        format!("- [Commitments](commitments.md): {commitments} open"),
        format!("- [Open loops](open_loops.md): {loops} open"),
    ];
    if let Some(dir) = scratch.and_then(scratch_dir) {
        let rel = dir.trim_start_matches(&format!("{ROOT}/")).to_string();
        head.push(format!("- [Your scratch]({rel}/): your own files, only you see them"));
    }
    let person_lines: Vec<String> = people
        .iter()
        .map(|p| format!("- [{}](people/{}.md)", one_line(&p.display_name), slug(&p.display_name, p.id, "person")))
        .collect();
    let project_lines: Vec<String> = projects
        .iter()
        .map(|p| format!("- [{}](projects/{}.md) {}", one_line(&p.name), slug(&p.name, p.id, "project"), p.status))
        .collect();
    Ok(fit_index(head, &[("People", "people/", person_lines), ("Projects", "projects/", project_lines)]))
}

/// Lay out the head and the sections within the index limits. Each section gets an equal share
/// of what the head leaves; a section that does not fit ends with a `… N more` line.
fn fit_index(head: Vec<String>, sections: &[(&str, &str, Vec<String>)]) -> String {
    let mut lines = head;
    let fits = |lines: &[String]| lines.len() <= INDEX_MAX_LINES && lines.iter().map(|l| l.len() + 1).sum::<usize>() <= INDEX_MAX_BYTES;
    let per_section_lines = (INDEX_MAX_LINES.saturating_sub(lines.len()) / sections.len().max(1)).saturating_sub(3);
    let per_section_bytes = (INDEX_MAX_BYTES.saturating_sub(lines.iter().map(|l| l.len() + 1).sum())) / sections.len().max(1);
    for (title, dir, entries) in sections {
        let mut section = vec![String::new(), format!("## {title}")];
        let mut bytes: usize = section.iter().map(|l| l.len() + 1).sum();
        let mut shown = 0;
        // Reserve the `… more` line's room.
        let reserve = 80;
        for entry in entries {
            if shown >= per_section_lines || bytes + entry.len() + 1 + reserve > per_section_bytes {
                break;
            }
            bytes += entry.len() + 1;
            section.push(entry.clone());
            shown += 1;
        }
        if shown < entries.len() {
            section.push(format!("- … {} more in {dir} (view /memories/{dir})", entries.len() - shown));
        } else if entries.is_empty() {
            section.push("- none yet".to_string());
        }
        lines.extend(section);
    }
    // A last guard so the limits hold whatever the inputs: drop lines from the end.
    while !fits(&lines) {
        lines.pop();
    }
    let mut out = lines.join("\n");
    out.push('\n');
    if out.len() > INDEX_MAX_BYTES {
        out.truncate(INDEX_MAX_BYTES);
    }
    out
}

/// Every file of the tree for `scratch` (the calling agent's namespace; `None` lists the world
/// files only). Directories are implied by the paths.
pub fn files(conn: &Connection, scratch: Option<&Namespace>) -> Result<Vec<Entry>, rusqlite::Error> {
    let mut out = vec![
        Entry { path: INDEX.to_string(), bytes: index_md(conn, scratch)?.len() },
        Entry { path: COMMITMENTS.to_string(), bytes: commitments_md(conn)?.len() },
        Entry { path: OPEN_LOOPS.to_string(), bytes: open_loops_md(conn)?.len() },
    ];
    for p in people(conn)? {
        let path = format!("{PEOPLE_DIR}/{}.md", slug(&p.display_name, p.id, "person"));
        let bytes = entity_md(conn, StateTable::People, p.id)?.map_or(0, |t| t.len());
        out.push(Entry { path, bytes });
    }
    for p in projects(conn)? {
        let path = format!("{PROJECTS_DIR}/{}.md", slug(&p.name, p.id, "project"));
        let bytes = entity_md(conn, StateTable::Projects, p.id)?.map_or(0, |t| t.len());
        out.push(Entry { path, bytes });
    }
    if let (Some(namespace), Some(dir)) = (scratch, scratch.and_then(scratch_dir)) {
        for file in shogun_memory::scratch_files::list(conn, namespace).map_err(|_| rusqlite::Error::InvalidQuery)? {
            out.push(Entry { path: format!("{dir}/{}", file.path), bytes: file.content.len() });
        }
    }
    Ok(out)
}

/// Read one file of the tree. `None` when the path names no file (a directory, or nothing).
/// A scratch path outside the calling agent's own directory reads as nothing.
pub fn read(conn: &Connection, path: &str, scratch: Option<&Namespace>) -> Result<Option<String>, rusqlite::Error> {
    match path {
        INDEX => return index_md(conn, scratch).map(Some),
        COMMITMENTS => return commitments_md(conn).map(Some),
        OPEN_LOOPS => return open_loops_md(conn).map(Some),
        _ => {}
    }
    for (dir, table) in [(PEOPLE_DIR, StateTable::People), (PROJECTS_DIR, StateTable::Projects)] {
        if let Some(file) = path.strip_prefix(dir).and_then(|rest| rest.strip_prefix('/')) {
            let Some(id) = file.strip_suffix(".md").and_then(slug_id) else {
                return Ok(None);
            };
            // The slug must be the row's current one: a stale or invented name does not resolve.
            let current = match table {
                StateTable::People => people(conn)?.into_iter().find(|p| p.id == id).map(|p| slug(&p.display_name, p.id, "person")),
                _ => projects(conn)?.into_iter().find(|p| p.id == id).map(|p| slug(&p.name, p.id, "project")),
            };
            if current.as_deref() != file.strip_suffix(".md") {
                return Ok(None);
            }
            return entity_md(conn, table, id);
        }
    }
    if let Some(namespace) = scratch {
        if let Some(rel) = own_scratch_relative(path, namespace) {
            return shogun_memory::scratch_files::read(conn, namespace, rel)
                .map(|file| file.map(|f| f.content))
                .map_err(|_| rusqlite::Error::InvalidQuery);
        }
    }
    // Another agent's file: readable only with a grant (E18); the data layer answers `None`
    // otherwise, exactly as for a file that does not exist.
    if let Some((namespace, rel)) = scratch_target(path) {
        // The caller is the agent `scratch` names; without one (a world-only listing) another
        // agent's file is nothing.
        let Some(Namespace::AgentScratch(own)) = scratch else {
            return Ok(None);
        };
        let Ok(caller) = shogun_memory::actor::Actor::agent(own) else {
            return Ok(None);
        };
        if !shogun_memory::acl::may_read(conn, &caller, &namespace.as_str()).unwrap_or(false) {
            return Ok(None);
        }
        return shogun_memory::scratch_files::read(conn, &namespace, rel)
            .map(|file| file.map(|f| f.content))
            .map_err(|_| rusqlite::Error::InvalidQuery);
    }
    Ok(None)
}

#[cfg(test)]
mod tests;
