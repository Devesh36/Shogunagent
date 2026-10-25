//! People and projects as pages (owner decision 2026-10-14, Goldfish parity).
//!
//! * **Mentions.** The hourly review links an entity to the events that name it. `mention_count`
//!   is the number of distinct sessions among those events — one app in one clock hour, the unit
//!   the review reads ([`note_mention`]) — so a name seen fifty times in one Slack hour is one
//!   mention, the same name in Slack and Mail that hour is two, and a replayed or backfilled hour
//!   never counts twice.
//!   Every new link marks the page dirty and moves `last_mentioned_at` (never `last_evidence_at`,
//!   which drives confidence age: repeated sightings must not refresh a memory).
//! * **Pages.** A dirty row named in two or more sessions gets its page rewritten by a model call
//!   (`shogun_core::dreamcycle::pages`): a one-line summary and at most ten claims, each tagged
//!   with how it is known and the events it rests on. [`apply_page`] keeps the claims with their
//!   event ids and renders the description column from them, so every face that reads the
//!   description (dashboard, Wiki, MCP / REST / CLI, chat grounding, search) shows the page.
//!   A row the user edited is never rewritten.
//! * **Singletons.** A row the review created that was named in only one session has no page and is
//!   left off every list unless the user edited it ([`visible_sql`]); after [`PRUNE_AFTER_MS`] it
//!   is removed ([`prune`]) if it still has one mention and no page (owner decision 2026-10-19,
//!   Goldfish parity: 60 days), unless the user edited it or a commitment / open loop points at it. Rows other paths made
//!   (imports, meeting speakers, connectors, the user) are not singletons by this rule.
//! * **Facts.** A listed person or project reads as a fact: passing the list rule (which also keeps
//!   unreviewed guesses below 0.5 off) stands in for confidence, so no face hedges it with
//!   "possibly" ([`listed_confidence_sql`]). Hidden rows are
//!   hidden from lists, Wiki, neighbours and related rows; search and the World model (the correction
//!   surface) still find one, as does a read by id.
//! * **Ranking.** Lists order by engagement: the page's claims about what the user did or said,
//!   newer ones weighing more ([`engagement`]); then mention count, then last seen. Public figures
//!   and watched media come after everyone and everything the user deals with ([`rank_sql`]).
//!
//! Provenance is not trimmed: `state_provenance` also backs evidence, visibility and related-row
//! reads. A rewrite reads only the newest [`READ_LINKS`] links of a row, so the page's input is
//! bounded the same way.

use std::collections::HashMap;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::state::StateTable;

/// Pages rewritten after one hourly review.
pub const PAGES_PER_RUN: usize = 4;
/// Pages rewritten after one review while more than [`BACKLOG_DIRTY`] wait (the first backlog).
pub const BACKLOG_PAGES_PER_RUN: usize = 16;
pub const BACKLOG_DIRTY: usize = 10;

/// How many of `dirty` waiting pages one run rewrites: [`BACKLOG_PAGES_PER_RUN`] while a backlog
/// of more than [`BACKLOG_DIRTY`] drains, [`PAGES_PER_RUN`] after.
pub fn pages_per_run(dirty: usize) -> usize {
    if dirty > BACKLOG_DIRTY {
        BACKLOG_PAGES_PER_RUN
    } else {
        PAGES_PER_RUN
    }
}
/// Sessions a row must be named in before it gets a page and shows on lists.
pub const MIN_MENTIONS: i64 = 2;
pub const MAX_CLAIMS: usize = 10;
pub const SUMMARY_MAX_CHARS: usize = 140;
const CLAIM_MAX_CHARS: usize = 240;
/// Newest provenance links one rewrite reads (the validation set for cited event ids).
pub const READ_LINKS: usize = 200;
/// Related page names one rewrite may link.
const RELATED_NAMES: usize = 8;
/// Excerpts one rewrite sends, and how long each may be.
const EXCERPTS_PER_PAGE: usize = 12;
const EXCERPT_CHARS: usize = 600;
/// Consecutive failures before a page waits; the wait doubles from an hour, up to 64 hours.
const FAILURES_BEFORE_BLOCK: i64 = 3;
const HOUR_MS: i64 = 3_600_000;
/// A singleton nobody touched is removed this long after it was last named (owner 2026-10-19).
pub const PRUNE_AFTER_MS: i64 = 60 * 24 * HOUR_MS;
/// Engagement is Σ 2^((t − EPOCH) / HALF_LIFE) over the user's own claims. Ordering by it equals
/// ordering by Σ 2^((t − now) / HALF_LIFE) for any `now` (the factor 2^((EPOCH − now)/HALF_LIFE) is
/// shared), so the stored score never needs recomputing as time passes.
/// ponytail: f64 overflows ~85 years past the epoch; re-base the epoch long before that.
const ENGAGEMENT_EPOCH_MS: i64 = 1_767_225_600_000; // 2026-01-01T00:00Z
const ENGAGEMENT_HALF_LIFE_MS: f64 = 30.0 * 24.0 * 3_600_000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    UserAction,
    UserSaid,
    OnScreen,
    Inferred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Weight {
    Core,
    Detail,
    Noise,
}

/// One stored claim. `at` is the newest cited event's time (ms), the claim's date.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Claim {
    pub text: String,
    pub basis: Basis,
    pub weight: Weight,
    pub event_ids: Vec<i64>,
    pub at: i64,
}

/// The `page` column. `seen` is the newest provenance link (row id) the page was written from;
/// `v` the [`PAGE_VERSION`] whose instruction wrote it (0 = before versions).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Page {
    pub summary: String,
    pub claims: Vec<Claim>,
    #[serde(default)]
    pub seen: i64,
    #[serde(default)]
    pub v: u32,
}

/// The page instruction's version. 1: task labels and generic single words are not projects
/// (#1340); [`requeue_stale_user_projects`] gives older pages one rewrite under it.
pub const PAGE_VERSION: u32 = 1;

fn cols(table: StateTable) -> Option<(&'static str, &'static str, &'static str)> {
    match table {
        StateTable::People => Some(("people", "display_name", "relationship_summary")),
        StateTable::Projects => Some(("projects", "name", "summary")),
        _ => None,
    }
}

/// Distinct sessions (app or source × clock hour, the review's own unit) among a row's provenance
/// events (`t` is the table name, used qualified).
fn hours_sql(t: &str) -> String {
    format!(
        "(SELECT COUNT(DISTINCT COALESCE(e.app_bundle_id, e.source) || ':' || (e.ts / {HOUR_MS}))
           FROM state_provenance s JOIN event_log e ON e.id = s.event_id
           WHERE s.state_table = '{t}' AND s.state_id = {t}.id)"
    )
}

/// Record that row `id` was linked to an event from `evidence_ts`: recount its sessions, mark the page
/// dirty, move `last_mentioned_at`. `class` is the review's person or project class: it fills an
/// unknown class, `contact` always wins (the user deals with them), and the owner's own row
/// (`self`) keeps its class. A project's class is only filled: the page rewrite, which reads all
/// its evidence, decides it after that (one hour's `user_project` must not undo `media_or_public`).
pub(crate) fn note_mention(
    conn: &Connection,
    table: StateTable,
    id: i64,
    evidence_ts: i64,
    class: Option<&str>,
) -> Result<(), rusqlite::Error> {
    let Some((t, _, _)) = cols(table) else { return Ok(()) };
    conn.execute(
        &format!(
            "UPDATE {t} SET mention_count = {}, page_dirty = 1,
                    last_mentioned_at = MAX(COALESCE(last_mentioned_at, 0), ?2) WHERE id = ?1",
            hours_sql(t)
        ),
        params![id, evidence_ts],
    )?;
    match (table, class) {
        (StateTable::People, Some(class)) => {
            conn.execute(
                "UPDATE people SET person_class = ?2 WHERE id = ?1
                   AND (person_class IS NULL OR (?2 = 'contact' AND person_class <> 'self'))",
                params![id, class],
            )?;
        }
        (StateTable::Projects, Some(class)) => {
            conn.execute(
                "UPDATE projects SET project_class = ?2 WHERE id = ?1 AND project_class IS NULL",
                params![id, class],
            )?;
        }
        _ => {}
    }
    Ok(())
}

/// Recount a row's sessions after its provenance changed hands (a merge) and mark its page dirty.
pub(crate) fn recount(conn: &Connection, table: StateTable, id: i64) -> Result<(), rusqlite::Error> {
    let Some((t, _, _)) = cols(table) else { return Ok(()) };
    conn.execute(
        &format!("UPDATE {t} SET mention_count = {}, page_dirty = 1 WHERE id = ?1", hours_sql(t)),
        [id],
    )?;
    Ok(())
}

/// The user touched the row.
fn locked_sql(a: &str) -> String {
    format!("({a}.user_pinned != 0 OR {a}.user_edited_at IS NOT NULL OR {a}.wording_edited != 0)")
}

/// A commitment / open loop (or, for a project, a thread) points at the row.
fn linked_sql(table: StateTable, a: &str) -> String {
    let linked = match table {
        StateTable::People => format!(
            "EXISTS(SELECT 1 FROM commitments c WHERE c.counterparty_id = {a}.id)
             OR EXISTS(SELECT 1 FROM open_loops o WHERE o.counterparty_id = {a}.id)"
        ),
        _ => format!(
            "EXISTS(SELECT 1 FROM commitments c WHERE c.project_id = {a}.id)
             OR EXISTS(SELECT 1 FROM open_loops o WHERE o.project_id = {a}.id)
             OR EXISTS(SELECT 1 FROM threads h WHERE h.project_id = {a}.id)"
        ),
    };
    format!("({linked})")
}

/// A row the hourly review created (its key is `person:` / `project:`, see `knowledge::persist`)
/// that is not worth a page: named in fewer than [`MIN_MENTIONS`] sessions, or (people) not a person.
fn singleton_sql(table: StateTable, a: &str) -> String {
    match table {
        StateTable::People => format!(
            "(COALESCE({a}.extraction_key, '') GLOB 'person:*'
              AND ({a}.mention_count < {MIN_MENTIONS} OR COALESCE({a}.person_class, '') = 'not_a_person'))"
        ),
        _ => format!("(COALESCE({a}.extraction_key, '') GLOB 'project:*' AND {a}.mention_count < {MIN_MENTIONS})"),
    }
}

/// Leading words that make a project name a task label ("Fix Visual Recall …"), not a project.
const TASK_VERBS: [&str; 12] = [
    "fix", "improve", "add", "update", "implement", "refactor", "rewrite", "design", "remove", "investigate", "debug",
    "migrate",
];

/// A class or name that keeps a row off the lists unless the user touched it: not a person or
/// project, or (#1340) a project name shaped like a task: a leading task verb or seven+ words.
/// Public figures and watched media stay listed, ranked last ([`rank_sql`], Goldfish parity).
/// ponytail: fixed verb list; a real project named "Fix…" needs a user edit to show.
fn hidden_class_sql(table: StateTable, a: &str) -> String {
    match table {
        StateTable::People => format!("(COALESCE({a}.person_class, '') = 'not_a_person')"),
        _ => {
            let name = format!("lower(trim({a}.name))");
            let verbs: Vec<String> = TASK_VERBS.iter().map(|v| format!("{name} GLOB '{v} *'")).collect();
            format!(
                "(COALESCE({a}.project_class, '') = 'not_a_project'
                  OR length({name}) - length(replace({name}, ' ', '')) >= 6 OR {})",
                verbs.join(" OR ")
            )
        }
    }
}

/// The owner's own row (people only), never listed as a person, whatever the user did to it.
fn self_sql(table: StateTable, a: &str) -> String {
    match table {
        StateTable::People => format!("(COALESCE({a}.person_class, '') = 'self')"),
        _ => "0".to_string(),
    }
}

/// The list rule for `people` / `projects` aliased `a`: shown when the user touched it, or when it
/// is not a singleton, not classed as no person / no project, and not an unreviewed guess (below
/// 0.5, the 2026-10-02 rule: e.g. a name a connector saw once). The owner's own row never shows.
/// Used by every face; a read by id ignores it. It reads only the row, so a fault in another table
/// never empties the people list.
pub fn visible_sql(table: StateTable, a: &str) -> String {
    format!(
        "(NOT {} AND ({} OR ({a}.confidence >= 0.5 AND NOT {} AND NOT {})))",
        self_sql(table, a),
        locked_sql(a),
        singleton_sql(table, a),
        hidden_class_sql(table, a)
    )
}

/// The confidence a listed person or project is shown with: at least the reviewed-fact level.
pub const LISTED_CONFIDENCE: f64 = 0.85;

/// `a.confidence` as every face reads it: raised to [`LISTED_CONFIDENCE`] for a row the list rule
/// shows, the stored value for a hidden one (only reachable by id).
pub fn listed_confidence_sql(table: StateTable, a: &str) -> String {
    format!("CASE WHEN {} THEN MAX({a}.confidence, {LISTED_CONFIDENCE:.2}) ELSE {a}.confidence END", visible_sql(table, a))
}

/// Whether `table` row `id` passes the list rule. `false` when the row is gone.
pub fn is_listed(conn: &Connection, table: StateTable, id: i64) -> Result<bool, rusqlite::Error> {
    let Some((t, _, _)) = cols(table) else { return Ok(true) };
    Ok(conn
        .query_row(&format!("SELECT 1 FROM {t} a WHERE a.id = ?1 AND {}", visible_sql(table, "a")), [id], |_| Ok(()))
        .optional()?
        .is_some())
}

/// The list order for `people` / `projects` aliased `a`: public figures and media / public
/// projects last, then engagement, mention count, last seen.
pub fn rank_sql(table: StateTable, a: &str) -> String {
    let figure = if table == StateTable::People {
        format!("(COALESCE({a}.person_class, '') = 'public_figure'), ")
    } else {
        format!("(COALESCE({a}.project_class, '') = 'media_or_public'), ")
    };
    format!(
        "{figure}{a}.engagement DESC, {a}.mention_count DESC, COALESCE({a}.last_mentioned_at, {a}.last_evidence_at) DESC, {a}.id"
    )
}

/// The ranking score of a page's claims: what the user did or said, newer counting more.
pub fn engagement(claims: &[Claim]) -> f64 {
    claims
        .iter()
        .filter(|c| matches!(c.basis, Basis::UserAction | Basis::UserSaid))
        .map(|c| ((c.at - ENGAGEMENT_EPOCH_MS) as f64 / ENGAGEMENT_HALF_LIFE_MS).exp2())
        .sum()
}

/// The repo part of a repository path ("owner/repo" → "repo"): the project it is evidence for when
/// a project of that name is known (`knowledge::persist`, `entity_dedupe::dedupe_exact`).
pub fn repo_path_project(name: &str) -> Option<&str> {
    let n = name.trim();
    let (owner, repo) = n.split_once('/')?;
    let part = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'));
    (part(owner) && part(repo) && !repo.contains('/')).then_some(repo.trim_end_matches(['-', '_', '.'])).filter(|r| !r.is_empty())
}

/// A project name that is a work item, not a project: a PR / issue number, a commit title, a
/// branch, a file. At most evidence for a parent project; never a project row of its own.
pub fn looks_like_artifact(name: &str) -> bool {
    let n = name.trim();
    let lower = n.to_lowercase();
    let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    let number = lower
        .trim_start_matches("pull request")
        .trim_start_matches("pr")
        .trim_start_matches("issue")
        .trim()
        .trim_start_matches('#')
        .trim();
    if digits(number) {
        return true;
    }
    const PREFIXES: [&str; 11] = ["feat", "fix", "perf", "docs", "chore", "refactor", "test", "build", "ci", "style", "revert"];
    let head: String = lower.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    if PREFIXES.contains(&head.as_str()) {
        let rest = &lower[head.len()..];
        // `feat: …`, `fix(desktop): …`, `feat!: …` (a commit title) or `feat/branch-name`.
        if rest.starts_with(':') || rest.starts_with('(') || rest.starts_with("!:") || (rest.starts_with('/') && !n.contains(' ')) {
            return true;
        }
    }
    // Not `js` / `go`: "Node.js" is a product name.
    const FILE_EXTS: [&str; 16] = [
        "rs", "ts", "tsx", "jsx", "py", "md", "json", "toml", "yml", "yaml", "swift", "sql", "css", "html", "sh", "lock",
    ];
    !n.contains(' ') && n.rsplit_once('.').is_some_and(|(stem, ext)| !stem.is_empty() && FILE_EXTS.contains(&ext.to_lowercase().as_str()))
}

/// One dated excerpt the rewrite may cite.
#[derive(Debug, Clone, PartialEq)]
pub struct Excerpt {
    pub event_id: i64,
    pub ts: i64,
    pub app: Option<String>,
    pub text: String,
}

/// What one page rewrite reads.
#[derive(Debug, Clone)]
pub struct PageTask {
    pub table: StateTable,
    pub id: i64,
    pub name: String,
    pub aliases: Vec<String>,
    /// `person_class` of a person, `project_class` of a project.
    pub class: Option<String>,
    /// The current description when there is no page yet (the review's one-liner).
    pub description: String,
    pub page: Option<Page>,
    /// New evidence, oldest first.
    pub excerpts: Vec<Excerpt>,
    /// Event ids the answer may cite (the excerpts and the old claims still linked) → their time.
    pub citable: HashMap<i64, i64>,
    /// Listed people and projects sharing evidence with this row: the names a claim may link as
    /// `[[Name]]` (the app opens that page; [`render`] prints the bare name).
    pub related: Vec<String>,
    /// Newest provenance link (row id) when the task was built.
    pub seen: i64,
}

/// The dirty pages to rewrite now, newest mention first, across people and projects.
pub fn dirty_pages(conn: &Connection, now: i64, limit: usize) -> Result<Vec<(StateTable, i64)>, rusqlite::Error> {
    let arm = |table: StateTable| {
        let t = cols(table).map_or("", |c| c.0);
        format!(
            "SELECT '{t}', a.id, COALESCE(a.last_mentioned_at, a.last_evidence_at) AS seen FROM {t} a
              WHERE a.page_dirty = 1 AND a.mention_count >= {MIN_MENTIONS}
                AND a.user_pinned = 0 AND a.user_edited_at IS NULL AND a.wording_edited = 0
                AND (a.page_blocked_until IS NULL OR a.page_blocked_until <= ?1){}",
            if table == StateTable::People {
                " AND COALESCE(a.person_class, '') NOT IN ('not_a_person', 'self')"
            } else {
                " AND COALESCE(a.project_class, '') <> 'not_a_project'"
            }
        )
    };
    let sql = format!(
        "{} UNION ALL {} ORDER BY seen DESC, 2 DESC LIMIT ?2",
        arm(StateTable::People),
        arm(StateTable::Projects)
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![now, limit as i64], |r| {
        let t: String = r.get(0)?;
        Ok((if t == "people" { StateTable::People } else { StateTable::Projects }, r.get::<_, i64>(1)?))
    })?;
    rows.collect()
}

/// A window of `text` around `quote` (or its head), at most [`EXCERPT_CHARS`], on one line.
fn excerpt_around(text: &str, quote: Option<&str>) -> String {
    let start = quote
        .filter(|q| !q.trim().is_empty())
        .and_then(|q| text.find(q))
        .map_or(0, |at| text[..at].chars().count().saturating_sub(EXCERPT_CHARS / 4));
    let window: String = text.chars().skip(start).take(EXCERPT_CHARS).collect();
    window.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Gather what the rewrite of `table` row `id` reads. Event text comes only through
/// [`crate::event_log::batch_event_text`], the Batch lane's source policy (meeting text never
/// leaves the device). `None` when the row is gone.
pub fn page_task(conn: &Connection, table: StateTable, id: i64) -> Result<Option<PageTask>, rusqlite::Error> {
    let Some((t, name_col, desc_col)) = cols(table) else { return Ok(None) };
    let class_col = if table == StateTable::People { "person_class" } else { "project_class" };
    type RowText = (String, Option<String>, Option<String>, Option<String>, Option<String>);
    let row: Option<RowText> = conn
        .query_row(
            &format!("SELECT {name_col}, aliases, {desc_col}, page, {class_col} FROM {t} WHERE id = ?1"),
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((name, aliases, description, page, class)) = row else { return Ok(None) };
    let page: Option<Page> = page.and_then(|p| serde_json::from_str(&p).ok());
    let mut stmt = conn.prepare(
        "SELECT p.id, p.event_id, e.ts, e.app_bundle_id, p.quote FROM state_provenance p JOIN event_log e ON e.id = p.event_id
          WHERE p.state_table = ?1 AND p.state_id = ?2 ORDER BY e.ts DESC, e.id DESC LIMIT ?3",
    )?;
    /// (link id, event id, event ts, app, quote)
    type Link = (i64, i64, i64, Option<String>, Option<String>);
    let links: Vec<Link> = stmt
        .query_map(params![t, id, READ_LINKS as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
        .collect::<Result<_, _>>()?;
    let linked: HashMap<i64, i64> = links.iter().map(|l| (l.1, l.2)).collect();
    // Over every link, not just the newest by time: a backfilled old event has a new link id.
    let seen: i64 = conn.query_row(
        "SELECT COALESCE(MAX(id), 0) FROM state_provenance WHERE state_table = ?1 AND state_id = ?2",
        params![t, id],
        |r| r.get(0),
    )?;
    let since = page.as_ref().map_or(0, |p| p.seen);
    // New links since the page was written; a merge moves old links in, so fall back to the newest.
    let mut fresh: Vec<_> = links.iter().filter(|l| l.0 > since).collect();
    if fresh.is_empty() {
        fresh = links.iter().collect();
    }
    let mut excerpts = Vec::new();
    for (_, event_id, ts, app, quote) in fresh {
        if excerpts.len() == EXCERPTS_PER_PAGE {
            break;
        }
        let Some(cleared) = crate::event_log::batch_event_text(conn, *event_id, usize::MAX)? else { continue };
        let text = excerpt_around(&cleared.content, quote.as_deref());
        if !text.is_empty() {
            excerpts.push(Excerpt { event_id: *event_id, ts: *ts, app: app.clone(), text });
        }
    }
    excerpts.reverse();
    let mut citable: HashMap<i64, i64> = excerpts.iter().map(|e| (e.event_id, e.ts)).collect();
    for claim in page.iter().flat_map(|p| &p.claims) {
        for id in &claim.event_ids {
            if let Some(ts) = linked.get(id) {
                citable.insert(*id, *ts);
            }
        }
    }
    let key = format!("{}-{id}", if table == StateTable::People { "person" } else { "project" });
    let related: Vec<String> = crate::knowledge::neighbors(conn, &key)?
        .into_iter()
        .filter(|n| matches!(n.kind.as_str(), "person" | "project"))
        .map(|n| n.name)
        .take(RELATED_NAMES)
        .collect();
    Ok(Some(PageTask {
        table,
        id,
        related,
        name,
        aliases: crate::entity_names::parse_aliases(aliases.as_deref()),
        class,
        description: description.unwrap_or_default(),
        page,
        excerpts,
        citable,
        seen,
    }))
}

fn one_line(s: &str, max: usize) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(max).collect()
}

fn basis_str(b: Basis) -> &'static str {
    match b {
        Basis::UserAction => "user_action",
        Basis::UserSaid => "user_said",
        Basis::OnScreen => "on_screen",
        Basis::Inferred => "inferred",
    }
}

fn day(ts: i64) -> String {
    let d = crate::due::local_date(ts);
    format!("{:04}-{:02}-{:02}", d.year, d.month, d.day)
}

impl PageTask {
    /// The rewrite's data: the entity, its current page and the new dated excerpts. Captured
    /// text; the caller redacts and fences it.
    pub fn data(&self) -> String {
        let mut out = format!(
            "Type: {}\nName: {}\n",
            if self.table == StateTable::People { "person" } else { "project" },
            one_line(&self.name, 120)
        );
        if !self.aliases.is_empty() {
            let aliases: Vec<String> = self.aliases.iter().take(8).map(|a| one_line(a, 60)).collect();
            out.push_str(&format!("Also known as: {}\n", aliases.join(", ")));
        }
        if let Some(class) = &self.class {
            out.push_str(&format!("Class: {class}\n"));
        }
        if !self.related.is_empty() {
            let related: Vec<String> = self.related.iter().map(|n| one_line(n, 60)).collect();
            out.push_str(&format!("Related pages: {}\n", related.join(", ")));
        }
        match &self.page {
            Some(page) => {
                out.push_str(&format!("Current page:\nSummary: {}\n", one_line(&page.summary, SUMMARY_MAX_CHARS)));
                for c in &page.claims {
                    let ids: Vec<String> = c.event_ids.iter().filter(|id| self.citable.contains_key(id)).map(i64::to_string).collect();
                    out.push_str(&format!("- [{}] {} (events {})\n", basis_str(c.basis), one_line(&c.text, CLAIM_MAX_CHARS), ids.join(", ")));
                }
            }
            None if !self.description.trim().is_empty() => {
                out.push_str(&format!("Current description: {}\n", one_line(&self.description, 400)));
            }
            None => {}
        }
        out.push_str("New evidence (oldest first):\n");
        for e in &self.excerpts {
            out.push_str(&format!("[event {}] {} {}: {}\n", e.event_id, day(e.ts), e.app.as_deref().unwrap_or("capture"), e.text));
        }
        out
    }
}

#[derive(Deserialize)]
struct WireClaim {
    text: String,
    basis: Basis,
    weight: Weight,
    #[serde(default)]
    event_ids: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct WirePage {
    summary: String,
    #[serde(default)]
    claims: Vec<serde_json::Value>,
    #[serde(default)]
    person_class: Option<String>,
    #[serde(default)]
    project_class: Option<String>,
}

/// A rewritten page that passed the checks.
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    pub page: Page,
    /// The page's reclassification: `person_class` of a person, `project_class` of a project.
    pub class: Option<String>,
}

/// `text` cut to at most `max` characters at a word, never inside a `[[link]]`.
fn cut_at_word(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max).collect();
    let mut head = cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head);
    if let Some(open) = head.rfind("[[").filter(move |&open| !head[open..].contains("]]")) {
        head = &head[..open];
    }
    head.trim_end().trim_end_matches([',', ';', ':']).to_string()
}

/// Generated text fit to store: one line, bounded (an over-long line is cut at a word, not
/// thrown away), not instruction-shaped.
fn clean(raw: &str, max: usize) -> Option<String> {
    let text = cut_at_word(&raw.split_whitespace().collect::<Vec<_>>().join(" "), max);
    if text.is_empty() {
        return None;
    }
    crate::sanitize::persist_generated(&text).is_some_and(|c| c.text == text).then_some(text)
}

/// Read the model's page for `task`. Noise claims are dropped, every cited id must be one the task
/// offered (anything else is dropped from the claim, and a claim left with none is dropped), at
/// most [`MAX_CLAIMS`] survive. `None` when the answer is unreadable, the summary unusable or no
/// claim survives: the old page stays.
pub fn parse_draft(task: &PageTask, json: &str) -> Option<Draft> {
    let wire: WirePage = serde_json::from_str(json).ok()?;
    let summary = clean(&wire.summary, SUMMARY_MAX_CHARS)?;
    let mut claims: Vec<Claim> = Vec::new();
    for value in wire.claims {
        let Ok(c) = serde_json::from_value::<WireClaim>(value) else { continue };
        if c.weight == Weight::Noise {
            continue;
        }
        let Some(text) = clean(&link_related(&c.text, &task.related), CLAIM_MAX_CHARS) else { continue };
        let mut ids: Vec<i64> = c
            .event_ids
            .iter()
            .filter_map(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())))
            .filter(|id| task.citable.contains_key(id))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        let Some(at) = ids.iter().filter_map(|id| task.citable.get(id)).max().copied() else { continue };
        // A claim is a dated paraphrase, never the message itself: a long run of a cited excerpt drops it.
        if task.excerpts.iter().any(|e| ids.contains(&e.event_id) && crate::sanitize::copies_words(&text, &e.text)) {
            continue;
        }
        if claims.iter().any(|k| k.text.eq_ignore_ascii_case(&text)) {
            continue;
        }
        claims.push(Claim { text, basis: c.basis, weight: c.weight, event_ids: ids, at });
        if claims.len() == MAX_CLAIMS {
            break;
        }
    }
    if claims.is_empty() {
        return None;
    }
    // Core claims first, each group in the model's order.
    claims.sort_by_key(|c| c.weight != Weight::Core);
    let class = if task.table == StateTable::People {
        wire.person_class.filter(|c| matches!(c.as_str(), "contact" | "public_figure" | "not_a_person"))
    } else {
        wire.project_class.filter(|c| matches!(c.as_str(), "user_project" | "media_or_public" | "not_a_project"))
    };
    Some(Draft { page: Page { summary, claims, seen: task.seen, v: PAGE_VERSION }, class })
}

/// `text` with each `[[Name]]` kept only when `Name` is one of `related` (any case, written as
/// listed); any other bracketed name loses its brackets.
fn link_related(text: &str, related: &[String]) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(open) = rest.find("[[") {
        let Some(close) = rest[open + 2..].find("]]") else { break };
        let inner = &rest[open + 2..open + 2 + close];
        out.push_str(&rest[..open]);
        match related.iter().find(|r| r.to_lowercase() == inner.trim().to_lowercase()) {
            Some(name) if !inner.contains('[') => out.push_str(&format!("[[{name}]]")),
            _ => out.push_str(inner),
        }
        rest = &rest[open + 2 + close + 2..];
    }
    out.push_str(rest);
    out
}

/// A claim as plain text (`[[Name]]` → `Name`), for every face that is not the page view.
fn plain(text: &str) -> String {
    text.replace("[[", "").replace("]]", "")
}

/// The description a page renders to: the summary, then one dated bullet per claim.
pub fn render(page: &Page) -> String {
    let mut out = plain(&page.summary);
    for c in &page.claims {
        out.push_str(&format!("\n- {} ({})", plain(&c.text), day(c.at)));
    }
    out
}

/// Store the model's answer for `task`. Returns whether a page was written; an unusable answer
/// counts as a failure ([`page_failed`]) and keeps the old page. The user's wording is never
/// overwritten: a row they edited since the task was built is left alone.
pub fn apply_page(conn: &Connection, task: &PageTask, json: &str, now: i64) -> Result<bool, rusqlite::Error> {
    let Some((t, _, desc_col)) = cols(task.table) else { return Ok(false) };
    let Some(draft) = parse_draft(task, json) else {
        page_failed(conn, task.table, task.id, now)?;
        return Ok(false);
    };
    let page_json = serde_json::to_string(&draft.page).unwrap_or_default();
    let written = conn.execute(
        &format!(
            "UPDATE {t} SET page = ?2, {desc_col} = ?3, engagement = ?4, page_failures = 0, page_blocked_until = NULL,
                    page_dirty = CASE WHEN (SELECT MAX(id) FROM state_provenance WHERE state_table = '{t}' AND state_id = ?1) > ?5
                                      THEN 1 ELSE 0 END
              WHERE id = ?1 AND user_pinned = 0 AND user_edited_at IS NULL AND wording_edited = 0"
        ),
        params![task.id, page_json, render(&draft.page), engagement(&draft.page.claims), task.seen],
    )?;
    if written == 0 {
        return Ok(false);
    }
    if let Some(class) = &draft.class {
        let col = if task.table == StateTable::People { "person_class" } else { "project_class" };
        conn.execute(&format!("UPDATE {t} SET {col} = ?2 WHERE id = ?1"), params![task.id, class])?;
    }
    let _ = crate::search_documents::project_source_after_write(conn, &crate::state_edit::source_ref(task.table, task.id), now);
    Ok(true)
}

/// Nothing the Batch lane may see backs this page (e.g. only meeting text): no call, no failure.
/// The page waits clean until a new mention marks it again.
pub fn page_skipped(conn: &Connection, task: &PageTask) -> Result<(), rusqlite::Error> {
    let Some((t, _, _)) = cols(task.table) else { return Ok(()) };
    conn.execute(
        &format!(
            "UPDATE {t} SET page_dirty = 0 WHERE id = ?1
               AND COALESCE((SELECT MAX(id) FROM state_provenance WHERE state_table = '{t}' AND state_id = ?1), 0) <= ?2"
        ),
        params![task.id, task.seen],
    )?;
    Ok(())
}

/// A rewrite failed (the call, or an unusable answer): count it, and after
/// [`FAILURES_BEFORE_BLOCK`] in a row hold the page back, an hour and doubling, at most 64 hours.
/// A success resets both ([`apply_page`]).
pub fn page_failed(conn: &Connection, table: StateTable, id: i64, now: i64) -> Result<(), rusqlite::Error> {
    let Some((t, _, _)) = cols(table) else { return Ok(()) };
    conn.execute(
        &format!(
            "UPDATE {t} SET page_failures = page_failures + 1,
                    page_blocked_until = CASE WHEN page_failures + 1 >= {FAILURES_BEFORE_BLOCK}
                        THEN ?2 + ({HOUR_MS} << MIN(page_failures + 1 - {FAILURES_BEFORE_BLOCK}, 6))
                        ELSE page_blocked_until END
              WHERE id = ?1"
        ),
        params![id, now],
    )?;
    Ok(())
}

/// Mentions below which a `user_project` page written by an older instruction is re-queued.
const REQUEUE_BELOW_MENTIONS: i64 = 5;

/// Give each lightly mentioned `user_project` page an older [`PAGE_VERSION`] wrote one rewrite
/// under the current instruction (#1340): rows like "dream" or "portfolio" were labelled before
/// the instruction named generic words, and nothing re-queues a page until the row is seen again.
/// A rewritten page carries the current version, so this is a no-op once done. Never a row the
/// user touched. Returns the rows re-queued.
pub fn requeue_stale_user_projects(conn: &Connection) -> Result<usize, rusqlite::Error> {
    conn.execute(
        &format!(
            "UPDATE projects SET page_dirty = 1
              WHERE page_dirty = 0 AND page IS NOT NULL AND COALESCE(json_extract(page, '$.v'), 0) < ?1
                AND project_class = 'user_project' AND mention_count < {REQUEUE_BELOW_MENTIONS}
                AND NOT {}",
            locked_sql("projects")
        ),
        [PAGE_VERSION],
    )
}

/// Remove singletons with one mention and no page last named more than [`PRUNE_AFTER_MS`] ago
/// (a row hidden only for its class is kept), and project rows that are work
/// items ([`looks_like_artifact`]) whatever their age; never a row the user touched or one a
/// commitment / open loop points at. No tombstone: a new mention may bring the name back.
/// Returns the rows removed.
pub fn prune(conn: &mut Connection, now: i64) -> Result<usize, rusqlite::Error> {
    let mut doomed: Vec<(StateTable, i64)> = Vec::new();
    for table in [StateTable::People, StateTable::Projects] {
        let Some((t, name_col, _)) = cols(table) else { continue };
        let mut stmt = conn.prepare(&format!(
            "SELECT a.id, a.{name_col}, ({} AND a.mention_count < {MIN_MENTIONS} AND a.page IS NULL
                                        AND COALESCE(a.last_mentioned_at, a.last_evidence_at) < ?1)
               FROM {t} a WHERE NOT {} AND NOT {}",
            singleton_sql(table, "a"),
            locked_sql("a"),
            linked_sql(table, "a")
        ))?;
        let rows = stmt.query_map([now - PRUNE_AFTER_MS], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, bool>(2)?)))?;
        for row in rows {
            let (id, name, stale) = row?;
            if stale || (table == StateTable::Projects && looks_like_artifact(&name)) {
                doomed.push((table, id));
            }
        }
    }
    let tx = conn.savepoint()?;
    for (table, id) in &doomed {
        crate::state_edit::remove_state_row(&tx, *table, *id)?;
    }
    tx.commit()?;
    for (table, id) in &doomed {
        let _ = crate::search_documents::project_source_after_write(conn, &crate::state_edit::source_ref(*table, *id), now);
    }
    Ok(doomed.len())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const H: i64 = HOUR_MS;

    fn event(conn: &Connection, ts: i64, text: &str) -> i64 {
        crate::event_log::insert(
            conn,
            &crate::event_log::NewEvent {
                ts,
                source: "capture",
                kind: "text",
                app_bundle_id: Some("app"),
                window_title: None,
                content: text,
                content_hash: &format!("h{ts}{text}"),
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap()
    }

    fn person(conn: &Connection, name: &str) -> i64 {
        conn.execute(
            "INSERT INTO people(display_name,extraction_key,confidence,base_confidence,created_at,updated_at,last_evidence_at)
             VALUES(?1,'person:'||?1,0.85,0.85,1,1,1)",
            [name],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn link(conn: &Connection, id: i64, event_id: i64, ts: i64) {
        conn.execute("INSERT INTO state_provenance(state_table,state_id,event_id,weight) VALUES('people',?1,?2,1.0)", params![id, event_id]).unwrap();
        note_mention(conn, StateTable::People, id, ts, Some("contact")).unwrap();
    }

    fn get(conn: &Connection, id: i64, col: &str) -> i64 {
        conn.query_row(&format!("SELECT {col} FROM people WHERE id=?1"), [id], |r| r.get(0)).unwrap()
    }

    #[test]
    fn a_mention_counts_once_per_session() {
        let conn = crate::open_in_memory().unwrap();
        let dana = person(&conn, "Dana");
        for ts in [10 * H + 5, 10 * H + 900, 10 * H + 3_000_000] {
            link(&conn, dana, event(&conn, ts, "Dana"), ts);
        }
        assert_eq!(get(&conn, dana, "mention_count"), 1, "three sightings in one hour are one mention");
        link(&conn, dana, event(&conn, 11 * H + 1, "Dana again"), 11 * H + 1);
        assert_eq!(get(&conn, dana, "mention_count"), 2);
        assert_eq!(get(&conn, dana, "page_dirty"), 1);
        assert_eq!(get(&conn, dana, "last_mentioned_at"), 11 * H + 1);
        assert_eq!(get(&conn, dana, "last_evidence_at"), 1, "a sighting never refreshes confidence age");
        // The same hour in another app is another session.
        let mail = crate::event_log::insert(&conn, &crate::event_log::NewEvent {
            ts: 11 * H + 2, source: "capture", kind: "text", app_bundle_id: Some("mail"), window_title: None,
            content: "Dana in mail", content_hash: "h-mail", dwell_ms: 0, display_id: None, window_bounds: None,
        }).unwrap();
        link(&conn, dana, mail, 11 * H + 2);
        assert_eq!(get(&conn, dana, "mention_count"), 3, "two apps in one hour are two sessions");
    }

    #[test]
    fn dirty_selection_takes_the_newest_eligible_pages_up_to_the_limit() {
        let conn = crate::open_in_memory().unwrap();
        let mut ids = Vec::new();
        for (n, name) in ["A", "B", "C", "D", "E", "F"].iter().enumerate() {
            let id = person(&conn, name);
            let base = (n as i64 + 1) * 10 * H;
            link(&conn, id, event(&conn, base, name), base);
            link(&conn, id, event(&conn, base + H, name), base + H);
            ids.push(id);
        }
        let single = person(&conn, "Solo");
        link(&conn, single, event(&conn, 500 * H, "Solo"), 500 * H);
        conn.execute("UPDATE people SET wording_edited=1 WHERE id=?1", [ids[5]]).unwrap();
        conn.execute("UPDATE people SET page_blocked_until=?2 WHERE id=?1", params![ids[4], 1_000 * H]).unwrap();
        assert_eq!((pages_per_run(BACKLOG_DIRTY + 1), pages_per_run(BACKLOG_DIRTY)), (16, 4), "a backlog drains faster");
        let picked: Vec<i64> = dirty_pages(&conn, 900 * H, PAGES_PER_RUN).unwrap().into_iter().map(|(_, id)| id).collect();
        // Newest first; the singleton, the user-edited row and the blocked one are skipped.
        assert_eq!(picked, vec![ids[3], ids[2], ids[1], ids[0]]);
    }

    fn task(citable: &[(i64, i64)]) -> PageTask {
        PageTask {
            table: StateTable::People,
            id: 1,
            name: "Dana".into(),
            aliases: vec![],
            class: None,
            description: String::new(),
            page: None,
            excerpts: vec![],
            citable: citable.iter().copied().collect(),
            related: vec!["Acme Launch".into()],
            seen: 9,
        }
    }

    #[test]
    fn claims_keep_their_events_and_noise_or_foreign_citations_are_dropped() {
        let t = task(&[(4, 4 * H), (7, 7 * H)]);
        let json = r#"{"summary":"Cofounder at Acme; you plan launches together.","person_class":"contact","claims":[
            {"text":"Asked you to review the launch plan","basis":"user_said","weight":"detail","event_ids":[7,"4",99]},
            {"text":"Shared a meme","basis":"on_screen","weight":"noise","event_ids":[4]},
            {"text":"Works at a company you never saw","basis":"inferred","weight":"core","event_ids":[99]},
            {"text":"You reviewed the pitch deck with them","basis":"user_action","weight":"core","event_ids":[4]}]}"#;
        let draft = parse_draft(&t, json).unwrap();
        let texts: Vec<&str> = draft.page.claims.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, ["You reviewed the pitch deck with them", "Asked you to review the launch plan"], "core first; noise and uncited dropped");
        assert_eq!(draft.page.claims[1].event_ids, vec![4, 7]);
        assert_eq!(draft.page.claims[1].at, 7 * H, "a claim is dated by its newest event");
        assert_eq!(draft.class.as_deref(), Some("contact"));
        assert_eq!(draft.page.seen, 9);
        assert_eq!(draft.page.v, PAGE_VERSION, "a rewrite records the instruction version");
        // Nothing survives → no draft: the old page stays.
        assert!(parse_draft(&t, r#"{"summary":"x y z","claims":[{"text":"t","basis":"inferred","weight":"noise","event_ids":[4]}]}"#).is_none());
        assert!(parse_draft(&t, "not json").is_none());
        // An over-long claim is cut at a word, not dropped.
        let long = format!("Leads [[Acme Launch]] {}", "planning ".repeat(40));
        let json = format!(r#"{{"summary":"Cofounder.","claims":[{{"text":"{long}","basis":"on_screen","weight":"core","event_ids":[4]}}]}}"#);
        let draft = parse_draft(&t, &json).unwrap();
        let text = &draft.page.claims[0].text;
        assert!(text.starts_with("Leads [[Acme Launch]] planning") && text.ends_with("planning"), "{text}");
        assert!(text.chars().count() <= CLAIM_MAX_CHARS);
    }

    #[test]
    fn a_claim_copying_its_message_is_dropped() {
        let mut t = task(&[(4, 4 * H)]);
        t.excerpts = vec![Excerpt {
            event_id: 4,
            ts: 4 * H,
            app: Some("Slack".into()),
            text: "Dana: hey can you send me the final launch deck before friday so we can review it together with the board next week".into(),
        }];
        let json = r#"{"summary":"Cofounder at Acme.","claims":[
            {"text":"Said: hey can you send me the final launch deck before friday so we can review it together with the board next week","basis":"on_screen","weight":"core","event_ids":[4]},
            {"text":"On 1970-01-01, Dana asked you for the launch deck before Friday","basis":"on_screen","weight":"core","event_ids":[4]}]}"#;
        let draft = parse_draft(&t, json).unwrap();
        let texts: Vec<&str> = draft.page.claims.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, ["On 1970-01-01, Dana asked you for the launch deck before Friday"]);
        // A related page is linked as listed; an unknown bracketed name is plain text.
        let json = r#"{"summary":"Cofounder at Acme.","claims":[
            {"text":"Leads [[acme launch]] with [[Someone Else]]","basis":"on_screen","weight":"core","event_ids":[4]}]}"#;
        let draft = parse_draft(&t, json).unwrap();
        assert_eq!(draft.page.claims[0].text, "Leads [[Acme Launch]] with Someone Else");
        assert!(render(&draft.page).starts_with("Cofounder at Acme.\n- Leads Acme Launch with Someone Else ("), "the description is plain text");
    }

    #[test]
    fn a_failed_rewrite_keeps_the_page_and_backs_off_after_three() {
        let conn = crate::open_in_memory().unwrap();
        let dana = person(&conn, "Dana");
        conn.execute("UPDATE people SET relationship_summary='Old words about Dana here' WHERE id=?1", [dana]).unwrap();
        let mut t = task(&[]);
        t.id = dana;
        for _ in 0..3 {
            assert!(!apply_page(&conn, &t, "{}", 100 * H).unwrap());
        }
        assert_eq!(get(&conn, dana, "page_failures"), 3);
        assert_eq!(get(&conn, dana, "page_blocked_until"), 101 * H);
        let kept: String = conn.query_row("SELECT relationship_summary FROM people WHERE id=?1", [dana], |r| r.get(0)).unwrap();
        assert_eq!(kept, "Old words about Dana here");
    }

    fn get_text(conn: &Connection, id: i64, col: &str) -> String {
        conn.query_row(&format!("SELECT {col} FROM people WHERE id=?1"), [id], |r| r.get(0)).unwrap()
    }

    fn listed(conn: &Connection) -> Vec<String> {
        crate::state::list_people(conn).unwrap().into_iter().map(|p| p.display_name).collect()
    }

    #[test]
    fn singletons_are_hidden_then_pruned_unless_kept() {
        let mut conn = crate::open_in_memory().unwrap();
        let once = person(&conn, "Once");
        let twice = person(&conn, "Twice");
        let owed = person(&conn, "Owed");
        let imported = {
            conn.execute("INSERT INTO people(display_name,confidence,base_confidence,created_at,updated_at,last_evidence_at) VALUES('Imported',0.8,0.8,1,1,1)", []).unwrap();
            conn.last_insert_rowid()
        };
        for (id, ts) in [(once, H), (twice, H), (twice, 2 * H), (owed, H), (imported, H)] {
            link(&conn, id, event(&conn, ts, "x"), ts);
        }
        let e = event(&conn, H, "promise");
        conn.execute("INSERT INTO commitments(direction,counterparty_id,description,status,confidence,created_at,updated_at,last_evidence_at) VALUES('mine',?1,'send it','open',0.85,1,1,1)", [owed]).unwrap();
        let c = conn.last_insert_rowid();
        conn.execute("INSERT INTO state_provenance(state_table,state_id,event_id,weight) VALUES('commitments',?1,?2,1.0)", params![c, e]).unwrap();
        let mut names = listed(&conn);
        names.sort();
        assert_eq!(names, ["Imported", "Twice"], "the review's singletons are hidden; other rows stay");
        assert!(crate::state::get_person(&conn, once).unwrap().is_some(), "a read by id still finds it");
        // Listed rows read as facts everywhere; the World model still shows hidden ones.
        conn.execute("UPDATE people SET confidence=0.6", []).unwrap();
        assert!(crate::state::list_people(&conn).unwrap().iter().all(|p| p.confidence >= LISTED_CONFIDENCE));
        assert_eq!(crate::state::get_person(&conn, once).unwrap().unwrap().confidence, 0.6);
        let model = crate::state_edit::export(&conn).unwrap();
        assert!(model.people.iter().any(|p| p.title == "Once"), "the correction surface lists hidden rows");
        let listed: Vec<String> = crate::state::list_people(&conn).unwrap().into_iter().map(|p| p.display_name).collect();
        assert!(model.people.iter().filter(|p| listed.contains(&p.title)).all(|p| p.confidence >= LISTED_CONFIDENCE));
        // A one-off with a written page, and a twice-seen row hidden only for its class, stay.
        let paged = person(&conn, "Paged");
        link(&conn, paged, event(&conn, H, "x"), H);
        conn.execute("UPDATE people SET page='{}' WHERE id=?1", [paged]).unwrap();
        let thing = person(&conn, "Thing");
        link(&conn, thing, event(&conn, H, "x"), H);
        link(&conn, thing, event(&conn, 2 * H, "x"), 2 * H);
        conn.execute("UPDATE people SET person_class='not_a_person' WHERE id=?1", [thing]).unwrap();
        assert_eq!(PRUNE_AFTER_MS, 60 * 24 * H, "one-offs are kept 60 days");
        assert_eq!(prune(&mut conn, H + PRUNE_AFTER_MS - 1).unwrap(), 0, "not yet stale");
        assert_eq!(prune(&mut conn, 3 * H + PRUNE_AFTER_MS).unwrap(), 1);
        let left: i64 = conn.query_row("SELECT count(*) FROM people", [], |r| r.get(0)).unwrap();
        assert_eq!(left, 5, "only the stale one-off with no page nobody owes anything to is removed");
        assert!(crate::state::get_person(&conn, owed).unwrap().is_some(), "a commitment keeps its counterparty");
    }

    #[test]
    fn engagement_orders_the_list_and_public_figures_come_last() {
        let conn = crate::open_in_memory().unwrap();
        let star = person(&conn, "Star");
        let busy = person(&conn, "Busy");
        let quiet = person(&conn, "Quiet");
        let recent = person(&conn, "Recent");
        for id in [star, busy, quiet, recent] {
            link(&conn, id, event(&conn, H, "a"), H);
            link(&conn, id, event(&conn, 2 * H, "b"), 2 * H);
        }
        // Busy has more mentions, but Recent has more of the user's own recent dealings.
        link(&conn, busy, event(&conn, 3 * H, "c"), 3 * H);
        let claim = |basis, at| Claim { text: "t".into(), basis, weight: Weight::Core, event_ids: vec![1], at };
        let day = 24 * H;
        let set = |id: i64, claims: Vec<Claim>| {
            conn.execute("UPDATE people SET engagement=?2 WHERE id=?1", params![id, engagement(&claims)]).unwrap();
        };
        set(recent, vec![claim(Basis::UserAction, ENGAGEMENT_EPOCH_MS + 90 * day)]);
        set(quiet, vec![claim(Basis::UserSaid, ENGAGEMENT_EPOCH_MS), claim(Basis::UserSaid, ENGAGEMENT_EPOCH_MS)]);
        set(busy, vec![claim(Basis::OnScreen, ENGAGEMENT_EPOCH_MS + 100 * day)]);
        set(star, vec![claim(Basis::UserAction, ENGAGEMENT_EPOCH_MS + 120 * day)]);
        conn.execute("UPDATE people SET person_class='public_figure' WHERE id=?1", [star]).unwrap();
        // One recent dealing outweighs two from three months earlier; seen-only claims count nothing.
        assert_eq!(listed(&conn), ["Recent", "Quiet", "Busy", "Star"]);
    }

    #[test]
    fn project_class_hides_or_ranks_last_and_the_owner_is_never_listed() {
        let conn = crate::open_in_memory().unwrap();
        for (name, class, engagement) in [
            ("Media", Some("media_or_public"), 9.0),
            ("Label", Some("not_a_project"), 9.0),
            ("Fix Visual Recall feature and UI", Some("user_project"), 9.0),
            ("Improve automatic Scribe form filling, memory coverage and latency", Some("user_project"), 9.0),
            ("Fixture Lab", Some("user_project"), 2.0),
            ("Work", None, 1.0),
        ] {
            conn.execute(
                "INSERT INTO projects(name,status,extraction_key,confidence,base_confidence,created_at,updated_at,last_evidence_at,mention_count,engagement,project_class)
                 VALUES(?1,'active','project:'||?1,0.85,0.85,1,1,1,2,?2,?3)",
                params![name, engagement, class],
            )
            .unwrap();
        }
        let projects: Vec<String> = conn
            .prepare(&format!(
                "SELECT a.name FROM projects a WHERE {} ORDER BY {}",
                visible_sql(StateTable::Projects, "a"),
                rank_sql(StateTable::Projects, "a")
            ))
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(projects, ["Fixture Lab", "Work", "Media"], "not-a-project and task-shaped names hidden; media last");
        // A later hour's `user_project` does not undo the page's `media_or_public`.
        let media: i64 = conn.query_row("SELECT id FROM projects WHERE name='Media'", [], |r| r.get(0)).unwrap();
        note_mention(&conn, StateTable::Projects, media, 3 * H, Some("user_project")).unwrap();
        let class: String = conn.query_row("SELECT project_class FROM projects WHERE id=?1", [media], |r| r.get(0)).unwrap();
        assert_eq!(class, "media_or_public");
        // A review's later `contact` never overrides the owner's own row, and the user pinning it
        // does not list it either.
        let me = person(&conn, "Me Myself");
        conn.execute("UPDATE people SET person_class='self', user_pinned=1 WHERE id=?1", [me]).unwrap();
        link(&conn, me, event(&conn, H, "a"), H);
        link(&conn, me, event(&conn, 2 * H, "b"), 2 * H);
        assert_eq!(get_text(&conn, me, "person_class"), "self");
        assert!(!is_listed(&conn, StateTable::People, me).unwrap());
        assert!(dirty_pages(&conn, 0, 10).unwrap().is_empty(), "no page is written for the owner");
    }

    #[test]
    fn old_light_user_project_pages_are_requeued_once() {
        let conn = crate::open_in_memory().unwrap();
        let old = r#"{"summary":"s","claims":[],"seen":1}"#;
        let new = format!(r#"{{"summary":"s","claims":[],"seen":1,"v":{PAGE_VERSION}}}"#);
        for (name, class, mentions, page, pinned) in [
            ("dream", "user_project", 2, old.to_string(), 0),
            ("Busy Work", "user_project", 9, old.to_string(), 0),
            ("Pinned", "user_project", 2, old.to_string(), 1),
            ("Film", "media_or_public", 2, old.to_string(), 0),
            ("Fresh", "user_project", 2, new.clone(), 0),
        ] {
            conn.execute(
                "INSERT INTO projects(name,status,extraction_key,confidence,base_confidence,created_at,updated_at,last_evidence_at,mention_count,project_class,page,user_pinned)
                 VALUES(?1,'active','project:'||?1,0.7,0.7,1,1,1,?2,?3,?4,?5)",
                params![name, mentions, class, page, pinned],
            )
            .unwrap();
        }
        assert_eq!(requeue_stale_user_projects(&conn).unwrap(), 1, "only the old, light, untouched user_project page");
        let dirty: String = conn.query_row("SELECT name FROM projects WHERE page_dirty = 1", [], |r| r.get(0)).unwrap();
        assert_eq!(dirty, "dream");
        // Still waiting: not queued twice. Rewritten under the current instruction: never again.
        assert_eq!(requeue_stale_user_projects(&conn).unwrap(), 0);
        conn.execute("UPDATE projects SET page_dirty = 0, page = ?1 WHERE name = 'dream'", [&new]).unwrap();
        assert_eq!(requeue_stale_user_projects(&conn).unwrap(), 0);
        let page: Page = serde_json::from_str(old).unwrap();
        assert_eq!(page.v, 0, "a page from before versions reads as version 0");
    }

    #[test]
    fn rewrite_input_reads_only_what_the_batch_lane_may_see() {
        let conn = crate::open_in_memory().unwrap();
        let dana = person(&conn, "Dana");
        let screen = event(&conn, H, &format!("{} Dana asked for the deck", "menu ".repeat(400)));
        link(&conn, dana, screen, H);
        let meeting = crate::event_log::insert(&conn, &crate::event_log::NewEvent {
            ts: 2 * H, source: "meeting", kind: "transcript", app_bundle_id: None, window_title: None,
            content: "Dana: the budget is secret", content_hash: "m1", dwell_ms: 0, display_id: None, window_bounds: None,
        }).unwrap();
        link(&conn, dana, meeting, 2 * H);
        conn.execute("UPDATE state_provenance SET quote='Dana asked for the deck' WHERE event_id=?1", [screen]).unwrap();
        let task = page_task(&conn, StateTable::People, dana).unwrap().unwrap();
        assert_eq!(task.excerpts.len(), 1, "meeting text never leaves the device");
        assert!(task.excerpts[0].text.contains("Dana asked for the deck"), "the excerpt is cut around the quote");
        assert!(!task.citable.contains_key(&meeting));
        assert!(!task.data().contains("budget"));
        page_skipped(&conn, &task).unwrap();
        assert_eq!(get(&conn, dana, "page_dirty"), 0, "nothing sendable: clean, no failure counted");
        assert_eq!(get(&conn, dana, "page_failures"), 0);
    }

    #[test]
    fn work_items_are_not_projects() {
        for name in ["#1124", "PR #1100", "pr 1068", "1066", "feat(mcp): get_events", "fix: thing", "feat/hourly-review-sync", "relay.rs", "extract.rs"] {
            assert!(looks_like_artifact(name), "{name}");
        }
        for name in ["ShogunAI", "Goldfish", "Torutesu/ShogunAI-", "Fix chat message thread storage", "Road to Wembley", "9 to 3", "Node.js app"] {
            assert!(!looks_like_artifact(name), "{name}");
        }
    }
}
