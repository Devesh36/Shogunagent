//! The user corrects the world model (issue #290): edit a state row, delete one, and
//! export the whole model in a portable, human-readable form.
//!
//! **Edit = pin.** An edited row is the user's own statement, so it is stored at confidence 1.0 and
//! flagged `user_pinned` (V49). Every automatic writer skips pinned rows — decay and corroboration
//! ([`crate::recompute`]), faded-row pruning, extraction promotion ([`crate::extract`]), and the
//! mail-triage refresh. New evidence may still *link* to a pinned row (a provenance row, which is
//! the log, not the belief), and a disagreeing new commitment still raises a `state_disputes` flag
//! — the conflict is shown to the user as a proposed change, never applied over their edit.
//!
//! **Delete = gone, and stays gone.** The state row, its provenance links, and any dispute naming
//! it are removed; the events themselves stay (they are the log). Because the Dream Cycle
//! re-classifies a window of past events, the same belief would otherwise come straight back from
//! the same old events on the next pass. A `state_suppressions` row (a digest of the key the
//! extractor derives — never the text) stops that for evidence at or before the deletion; the same
//! thing said again *later* is new evidence and is extracted as usual.
//!
//! Known gap (documented, not handled here): a person created from an email/handle identity
//! (`identity::observe`) has no extraction key, so a deleted one reappears when new mail from the
//! same address arrives — that is new evidence, not the old events resurfacing. Splitting a wrong
//! entity merge (FR-ST-10) lives in [`crate::state_split`].

use std::collections::HashMap;

use rusqlite::{params, Connection, OptionalExtension, ToSql};
use serde::Serialize;

use crate::state::StateTable;

/// Longest name / title a user edit may store.
pub const MAX_NAME_CHARS: usize = 512;
/// Longest description / summary a user edit may store.
pub const MAX_TEXT_CHARS: usize = 4000;

/// Why an edit or delete was refused. Messages carry no user text (they reach logs and API
/// responses) and start with `invalid ` so the REST face maps them to 400.
#[derive(Debug)]
pub enum EditError {
    Invalid(&'static str),
    Db(rusqlite::Error),
}

impl From<rusqlite::Error> for EditError {
    fn from(e: rusqlite::Error) -> Self {
        EditError::Db(e)
    }
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditError::Invalid(why) => f.write_str(why),
            EditError::Db(_) => f.write_str("state edit failed"),
        }
    }
}

/// The user-facing fields a correction may change. `None` = leave as is. For the nullable columns
/// the inner `Option` distinguishes "clear it" (`Some(None)`) from "leave it" (`None`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StatePatch {
    /// `people.display_name` / `projects.name`.
    pub name: Option<String>,
    /// `people.relationship_summary` / `projects.summary`.
    pub summary: Option<Option<String>>,
    /// `commitments.description` / `open_loops.description`.
    pub description: Option<String>,
    /// `projects.status` / `commitments.status` / `open_loops.status`, validated per table.
    pub status: Option<String>,
    /// `commitments.due_at` (Unix ms).
    pub due_at: Option<Option<i64>>,
    /// `people.aliases` / `emails` / `handles` / `interaction_channel` (people only). A list
    /// replaces the stored list; an empty list clears it.
    pub aliases: Option<Vec<String>>,
    pub emails: Option<Vec<String>>,
    pub handles: Option<Vec<String>>,
    pub channel: Option<Option<String>>,
    /// `commitments.deferred_until` / `open_loops.deferred_until` (Unix ms): snooze. `Some(None)`
    /// clears it.
    pub deferred_until: Option<Option<i64>>,
}

impl StatePatch {
    fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.summary.is_none()
            && self.description.is_none()
            && self.status.is_none()
            && self.due_at.is_none()
            && self.aliases.is_none()
            && self.emails.is_none()
            && self.handles.is_none()
            && self.channel.is_none()
            && self.deferred_until.is_none()
    }
}

/// A parsed `state.update` / `state.delete` request body: `{"table": "...", "id": N, ...fields}`.
/// One parser for every face (MCP, REST, CLI, desktop) so they cannot drift.
pub fn parse_request(body: &str) -> Result<(StateTable, i64, StatePatch), EditError> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| EditError::Invalid("invalid state request"))?;
    let obj = value.as_object().ok_or(EditError::Invalid("invalid state request"))?;
    let table = obj
        .get("table")
        .and_then(|v| v.as_str())
        .and_then(StateTable::parse)
        .ok_or(EditError::Invalid("invalid table"))?;
    let id = obj
        .get("id")
        .and_then(|v| v.as_i64())
        .filter(|id| *id > 0)
        .ok_or(EditError::Invalid("invalid id"))?;
    let text = |key: &str| -> Result<Option<String>, EditError> {
        match obj.get(key) {
            None => Ok(None),
            Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(EditError::Invalid("invalid field type")),
        }
    };
    let mut patch = StatePatch {
        name: text("name")?,
        description: text("description")?,
        status: text("status")?,
        ..StatePatch::default()
    };
    patch.summary = match obj.get("summary") {
        None => None,
        Some(serde_json::Value::Null) => Some(None),
        Some(serde_json::Value::String(s)) => Some(Some(s.clone())),
        Some(_) => return Err(EditError::Invalid("invalid field type")),
    };
    patch.due_at = match obj.get("due_at") {
        None => None,
        Some(serde_json::Value::Null) => Some(None),
        Some(v) => Some(Some(v.as_i64().ok_or(EditError::Invalid("invalid field type"))?)),
    };
    let list = |key: &str| -> Result<Option<Vec<String>>, EditError> {
        match obj.get(key) {
            None => Ok(None),
            Some(serde_json::Value::Null) => Ok(Some(Vec::new())),
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .map(|v| v.as_str().map(str::to_string).ok_or(EditError::Invalid("invalid field type")))
                .collect::<Result<_, _>>()
                .map(Some),
            Some(_) => Err(EditError::Invalid("invalid field type")),
        }
    };
    patch.aliases = list("aliases")?;
    patch.emails = list("emails")?;
    patch.handles = list("handles")?;
    patch.channel = match obj.get("channel") {
        None => None,
        Some(serde_json::Value::Null) => Some(None),
        Some(serde_json::Value::String(s)) => Some(Some(s.clone())),
        Some(_) => return Err(EditError::Invalid("invalid field type")),
    };
    patch.deferred_until = match obj.get("deferred_until") {
        None => None,
        Some(serde_json::Value::Null) => Some(None),
        Some(v) => Some(Some(v.as_i64().ok_or(EditError::Invalid("invalid field type"))?)),
    };
    for key in obj.keys() {
        if !matches!(
            key.as_str(),
            "table" | "id" | "name" | "summary" | "description" | "status" | "due_at" | "deferred_until"
                | "aliases" | "emails" | "handles" | "channel"
        ) {
            return Err(EditError::Invalid("invalid field"));
        }
    }
    Ok((table, id, patch))
}

/// Digest of the identity an extractor would re-derive for a belief. Never the text itself.
pub fn suppression_digest(table: StateTable, key: &str) -> String {
    crate::event_log::content_hash(&format!("{}\u{1f}{}", table.as_str(), key.trim()))
}

/// Record that evidence at or before `now` must not re-create this belief.
fn suppress(
    conn: &Connection,
    table: StateTable,
    key: &str,
    reason: &str,
    now: i64,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO state_suppressions (state_table, match_digest, reason, suppressed_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (state_table, match_digest) DO UPDATE
            SET reason = excluded.reason,
                suppressed_at = MAX(suppressed_at, excluded.suppressed_at)",
        params![table.as_str(), suppression_digest(table, key), reason, now],
    )?;
    Ok(())
}

/// Whether the user deleted (or rewrote) this belief after the evidence at `evidence_ts` was
/// captured — i.e. re-extracting it from that evidence would undo the user's correction.
pub fn is_suppressed(
    conn: &Connection,
    table: StateTable,
    key: &str,
    evidence_ts: i64,
) -> Result<bool, rusqlite::Error> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM state_suppressions
              WHERE state_table = ?1 AND match_digest = ?2 AND suppressed_at >= ?3",
            params![table.as_str(), suppression_digest(table, key), evidence_ts],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// The `ts` of an event, for the suppression check. `None` if the event is gone.
pub fn event_ts(conn: &Connection, event_id: i64) -> Result<Option<i64>, rusqlite::Error> {
    conn.query_row("SELECT ts FROM event_log WHERE id = ?1", [event_id], |r| r.get(0))
        .optional()
}

pub(crate) fn clean_text(raw: &str, max: usize, empty_ok: bool) -> Result<Option<String>, EditError> {
    let body = crate::sanitize::persist_body(raw.trim());
    let text = body.text.trim();
    if text.is_empty() {
        return if empty_ok { Ok(None) } else { Err(EditError::Invalid("invalid empty text")) };
    }
    if text.chars().count() > max {
        return Err(EditError::Invalid("invalid text too long"));
    }
    Ok(Some(text.to_string()))
}

fn valid_status(table: StateTable, status: &str) -> bool {
    match table {
        StateTable::People => false,
        StateTable::Projects => matches!(status, "active" | "waiting" | "paused" | "done"),
        StateTable::Commitments => matches!(status, "open" | "done" | "overdue" | "cancelled"),
        StateTable::OpenLoops => matches!(status, "open" | "closed"),
    }
}

pub(crate) fn source_ref(table: StateTable, id: i64) -> crate::search_documents::SourceRef {
    use crate::search_documents::SourceRef;
    match table {
        StateTable::People => SourceRef::Person(id),
        StateTable::Projects => SourceRef::Project(id),
        StateTable::Commitments => SourceRef::Commitment(id),
        StateTable::OpenLoops => SourceRef::OpenLoop(id),
    }
}

/// Check the stored target namespace, not the caller's destination namespace. Callers hold the
/// mutation savepoint so the authorization and write observe the same row and grant state.
fn may_write_state(
    conn: &Connection,
    table: StateTable,
    id: i64,
    ctx: &crate::actor::WriteContext,
) -> Result<bool, EditError> {
    let namespace: Option<String> = conn
        .query_row(&format!("SELECT namespace FROM {} WHERE id = ?1", table.as_str()), [id], |r| r.get(0))
        .optional()?;
    let Some(namespace) = namespace else {
        return Ok(false);
    };
    crate::acl::may_write(conn, &ctx.actor, &namespace)
        .map_err(|_| EditError::Db(crate::event_log::scratch_denied()))
}

/// Apply a user correction and pin the row. `Ok(false)` when absent or not writable. The patch is
/// validated against the table first — a field the table does not have is refused, not ignored.
pub fn update_state(
    conn: &mut Connection,
    table: StateTable,
    id: i64,
    patch: &StatePatch,
    now: i64,
) -> Result<bool, EditError> {
    update_state_as(conn, table, id, patch, now, &crate::actor::current_write())
}

/// [`update_state`] as `ctx` (E2-T4): the row's `actor_last` records the editor. [`update_state`]
/// itself writes as the call's current context ([`crate::actor::current_write`]).
pub fn update_state_as(
    conn: &mut Connection,
    table: StateTable,
    id: i64,
    patch: &StatePatch,
    now: i64,
    ctx: &crate::actor::WriteContext,
) -> Result<bool, EditError> {
    if patch.is_empty() {
        return Err(EditError::Invalid("invalid empty edit"));
    }
    let names = matches!(table, StateTable::People | StateTable::Projects);
    if (patch.name.is_some() || patch.summary.is_some()) && !names {
        return Err(EditError::Invalid("invalid field for table"));
    }
    if patch.description.is_some() && names {
        return Err(EditError::Invalid("invalid field for table"));
    }
    if patch.due_at.is_some() && table != StateTable::Commitments {
        return Err(EditError::Invalid("invalid field for table"));
    }
    if (patch.aliases.is_some() || patch.emails.is_some() || patch.handles.is_some() || patch.channel.is_some())
        && table != StateTable::People
    {
        return Err(EditError::Invalid("invalid field for table"));
    }
    if patch.deferred_until.is_some()
        && !matches!(table, StateTable::Commitments | StateTable::OpenLoops)
    {
        return Err(EditError::Invalid("invalid field for table"));
    }
    if let Some(status) = &patch.status {
        if !valid_status(table, status) {
            return Err(EditError::Invalid("invalid status"));
        }
    }

    // Column names below come from this function's own literals, never from the caller.
    let mut sets: Vec<&'static str> = Vec::new();
    let mut values: Vec<Box<dyn ToSql>> = Vec::new();
    if let Some(name) = &patch.name {
        let name = clean_text(name, MAX_NAME_CHARS, false)?;
        sets.push(if table == StateTable::People { "display_name" } else { "name" });
        values.push(Box::new(name));
    }
    if let Some(summary) = &patch.summary {
        let summary = match summary {
            Some(s) => clean_text(s, MAX_TEXT_CHARS, true)?,
            None => None,
        };
        sets.push(if table == StateTable::People { "relationship_summary" } else { "summary" });
        values.push(Box::new(summary));
    }
    for (column, list) in [("aliases", &patch.aliases), ("emails", &patch.emails), ("handles", &patch.handles)] {
        if let Some(items) = list {
            let mut clean: Vec<String> = Vec::new();
            for item in items {
                if let Some(v) = clean_text(item, 200, true)? {
                    if !clean.iter().any(|c| c.eq_ignore_ascii_case(&v)) {
                        clean.push(v);
                    }
                }
            }
            if clean.len() > 20 {
                return Err(EditError::Invalid("invalid too many entries"));
            }
            sets.push(column);
            values.push(Box::new((!clean.is_empty()).then(|| serde_json::json!(clean).to_string())));
        }
    }
    if let Some(channel) = &patch.channel {
        let channel = match channel {
            Some(c) => clean_text(c, 40, true)?,
            None => None,
        };
        sets.push("interaction_channel");
        values.push(Box::new(channel));
    }
    if patch.name.is_some() || patch.summary.is_some() || patch.description.is_some() {
        sets.push("wording_edited");
        values.push(Box::new(true));
    }
    let mut new_description = None;
    if let Some(description) = &patch.description {
        let description = clean_text(description, MAX_TEXT_CHARS, false)?;
        new_description.clone_from(&description);
        sets.push("description");
        values.push(Box::new(description));
    }
    if let Some(status) = &patch.status {
        sets.push("status");
        values.push(Box::new(status.clone()));
    }
    if let Some(due_at) = patch.due_at {
        sets.push("due_at");
        values.push(Box::new(due_at));
    }
    if let Some(deferred_until) = patch.deferred_until {
        sets.push("deferred_until");
        values.push(Box::new(deferred_until));
    }

    let tx = conn.savepoint()?;
    if !may_write_state(&tx, table, id, ctx)? {
        return Ok(false);
    }
    let old_description: Option<String> = match table {
        StateTable::Commitments | StateTable::OpenLoops => tx
            .query_row(
                &format!("SELECT description FROM {} WHERE id = ?1", table.as_str()),
                [id],
                |r| r.get(0),
            )
            .optional()?,
        StateTable::People | StateTable::Projects => tx
            .query_row(&format!("SELECT '' FROM {} WHERE id = ?1", table.as_str()), [id], |r| {
                r.get(0)
            })
            .optional()?,
    };
    let Some(old_description) = old_description else {
        return Ok(false);
    };

    let mut sql = format!("UPDATE {} SET ", table.as_str());
    for (i, column) in sets.iter().enumerate() {
        sql.push_str(&format!("{column} = ?{}, ", i + 1));
    }
    let n = sets.len();
    sql.push_str(&format!(
        "confidence = 1.0, base_confidence = 1.0, user_pinned = 1, user_edited_at = ?{a},
         updated_at = ?{a}, actor_last = ?{c} WHERE id = ?{b}",
        a = n + 1,
        b = n + 2,
        c = n + 3
    ));
    values.push(Box::new(now));
    values.push(Box::new(id));
    values.push(Box::new(ctx.actor_sql().to_string()));
    let refs: Vec<&dyn ToSql> = values.iter().map(|v| v.as_ref()).collect();
    tx.execute(&sql, refs.as_slice())?;

    // The old wording must not come back as a twin from the events that produced it.
    if new_description.as_deref().is_some_and(|d| d != old_description) {
        suppress(&tx, table, &old_description, "edited", now)?;
    }
    tx.commit()?;
    let _ = crate::search_documents::project_source_after_write(conn, &source_ref(table, id), now);
    Ok(true)
}

/// A parsed `state.create` body: `{"table": "commitments"|"open_loops", "description": "...",
/// "due_at"?: ms (commitments only), "person_id"?: N}`. One parser for every face.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateRequest {
    pub table: StateTable,
    pub description: String,
    pub due_at: Option<i64>,
    pub person_id: Option<i64>,
}

pub fn parse_create_request(body: &str) -> Result<CreateRequest, EditError> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| EditError::Invalid("invalid state request"))?;
    let obj = value.as_object().ok_or(EditError::Invalid("invalid state request"))?;
    for key in obj.keys() {
        if !matches!(key.as_str(), "table" | "description" | "due_at" | "person_id") {
            return Err(EditError::Invalid("invalid field"));
        }
    }
    let table = obj
        .get("table")
        .and_then(|v| v.as_str())
        .and_then(StateTable::parse)
        .filter(|t| matches!(t, StateTable::Commitments | StateTable::OpenLoops))
        .ok_or(EditError::Invalid("invalid table"))?;
    let description = obj
        .get("description")
        .and_then(|v| v.as_str())
        .ok_or(EditError::Invalid("invalid description"))?
        .to_string();
    let optional = |key: &str| -> Result<Option<i64>, EditError> {
        match obj.get(key) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(v) => v.as_i64().map(Some).ok_or(EditError::Invalid("invalid field type")),
        }
    };
    let due_at = optional("due_at")?;
    if due_at.is_some() && table != StateTable::Commitments {
        return Err(EditError::Invalid("invalid field for table"));
    }
    let person_id = optional("person_id")?;
    if person_id.is_some_and(|id| id <= 0) {
        return Err(EditError::Invalid("invalid id"));
    }
    Ok(CreateRequest { table, description, due_at, person_id })
}

/// Add a commitment / open loop the user wrote themselves. It is the user's own statement: pinned
/// at confidence 1.0 with `user_edited_at` set and no event provenance (a pinned row needs none;
/// [`crate::state::supported_provenance`]). Returns the new id; `Ok(None)` when `person_id`
/// names no readable person. A scratch destination requires the writer's own namespace or an
/// explicit write grant; a read grant is sufficient to reference an existing person.
pub fn create_state(
    conn: &mut Connection,
    req: &CreateRequest,
    now: i64,
) -> Result<Option<i64>, EditError> {
    create_state_as(conn, req, now, &crate::actor::current_write())
}

/// [`create_state`] as `ctx` (E2-T4): the new row records `ctx.actor` and lands in
/// `ctx.namespace` (V93).
pub fn create_state_as(
    conn: &mut Connection,
    req: &CreateRequest,
    now: i64,
    ctx: &crate::actor::WriteContext,
) -> Result<Option<i64>, EditError> {
    let description = clean_text(&req.description, MAX_TEXT_CHARS, false)?
        .ok_or(EditError::Invalid("invalid empty text"))?;
    let tx = conn.savepoint()?;
    if !crate::acl::may_write(&tx, &ctx.actor, &ctx.namespace_sql())
        .map_err(|_| EditError::Db(crate::event_log::scratch_denied()))?
    {
        return Err(EditError::Invalid("invalid namespace access"));
    }
    if let Some(person) = req.person_id {
        let namespace: Option<String> =
            tx.query_row("SELECT namespace FROM people WHERE id = ?1", [person], |r| r.get(0)).optional()?;
        let Some(namespace) = namespace else {
            return Ok(None);
        };
        if !crate::acl::may_read(&tx, &ctx.actor, &namespace)
            .map_err(|_| EditError::Db(crate::event_log::scratch_denied()))?
        {
            return Ok(None);
        }
    }
    match req.table {
        StateTable::Commitments => tx.execute(
            "INSERT INTO commitments
               (direction, counterparty_id, description, due_at, status, confidence, base_confidence,
                created_at, updated_at, last_evidence_at, user_pinned, user_edited_at, wording_edited,
                actor_last, namespace)
             VALUES ('mine', ?1, ?2, ?3, 'open', 1.0, 1.0, ?4, ?4, ?4, 1, ?4, 1, ?5, ?6)",
            params![req.person_id, description, req.due_at, now, ctx.actor_sql(), ctx.namespace_sql()],
        )?,
        _ => tx.execute(
            "INSERT INTO open_loops
               (kind, description, counterparty_id, opened_at, staleness_days, status, confidence,
                base_confidence, created_at, updated_at, last_evidence_at, user_pinned,
                user_edited_at, wording_edited, actor_last, namespace)
             VALUES ('other', ?1, ?2, ?3, 0, 'open', 1.0, 1.0, ?3, ?3, ?3, 1, ?3, 1, ?4, ?5)",
            params![description, req.person_id, now, ctx.actor_sql(), ctx.namespace_sql()],
        )?,
    };
    let id = tx.last_insert_rowid();
    tx.commit()?;
    let _ = crate::search_documents::project_source_after_write(conn, &source_ref(req.table, id), now);
    Ok(Some(id))
}

/// Delete a state row outright. Its provenance links and any contradiction flag naming it go with
/// it; the events stay. References from other state rows are cleared first (the schema has no
/// ON DELETE clause). `Ok(false)` when the row is absent or not writable by the caller.
pub fn delete_state(
    conn: &mut Connection,
    table: StateTable,
    id: i64,
    now: i64,
) -> Result<bool, EditError> {
    delete_state_as(conn, table, id, now, &crate::actor::current_write())
}

/// [`delete_state`] as `ctx` (E2-T4): the deletion leaves an `export_tombstones` marker (V95)
/// naming the table, the id and the actor, so an incremental export or sync peer retires its
/// copy; the deleted content itself is never kept.
pub fn delete_state_as(
    conn: &mut Connection,
    table: StateTable,
    id: i64,
    now: i64,
    ctx: &crate::actor::WriteContext,
) -> Result<bool, EditError> {
    let tx = conn.savepoint()?;
    if !may_write_state(&tx, table, id, ctx)? {
        return Ok(false);
    }
    let key: Option<Option<String>> = match table {
        StateTable::Commitments | StateTable::OpenLoops => tx
            .query_row(
                &format!("SELECT description FROM {} WHERE id = ?1", table.as_str()),
                [id],
                |r| r.get::<_, String>(0).map(Some),
            )
            .optional()?,
        StateTable::People | StateTable::Projects => tx
            .query_row(
                &format!("SELECT extraction_key FROM {} WHERE id = ?1", table.as_str()),
                [id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?,
    };
    let Some(key) = key else {
        return Ok(false);
    };
    remove_state_row(&tx, table, id)?;
    if let Some(key) = key {
        suppress(&tx, table, &key, "deleted", now)?;
    }
    tombstone(&tx, table.as_str(), id, now, ctx)?;
    tx.commit()?;
    let _ = crate::search_documents::project_source_after_write(conn, &source_ref(table, id), now);
    Ok(true)
}

/// Record a deletion for the export format and sync (V95): `(kind, id)` with the time and the
/// actor as the reason. One marker per row; a repeat refreshes the time. Never the content.
pub(crate) fn tombstone(
    conn: &Connection,
    kind: &str,
    id: i64,
    now: i64,
    ctx: &crate::actor::WriteContext,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO export_tombstones (kind, id, deleted_at, reason) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (kind, id) DO UPDATE SET deleted_at = excluded.deleted_at, reason = excluded.reason",
        params![kind, id, now, format!("deleted_by:{}", ctx.actor_sql())],
    )?;
    Ok(())
}

/// Caller owns the transaction and the user-intent policy; maintenance must not create tombstones.
pub(crate) fn remove_state_row(conn: &Connection, table: StateTable, id: i64) -> Result<usize, rusqlite::Error> {
    match table {
        StateTable::People => {
            for t in ["commitments", "open_loops"] {
                conn.execute(
                    &format!("UPDATE {t} SET counterparty_id = NULL WHERE counterparty_id = ?1"),
                    [id],
                )?;
            }
        }
        StateTable::Projects => {
            for t in ["commitments", "open_loops", "threads"] {
                conn.execute(&format!("UPDATE {t} SET project_id = NULL WHERE project_id = ?1"), [id])?;
            }
        }
        StateTable::Commitments => {
            conn.execute(
                "DELETE FROM state_disputes
                  WHERE state_table = 'commitments' AND (earlier_id = ?1 OR later_id = ?1)",
                [id],
            )?;
        }
        StateTable::OpenLoops => {}
    }
    conn.execute(
        "DELETE FROM state_provenance WHERE state_table = ?1 AND state_id = ?2",
        params![table.as_str(), id],
    )?;
    conn.execute(&format!("DELETE FROM {} WHERE id = ?1", table.as_str()), [id])
}

/// One state row in the export.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ExportRow {
    pub id: i64,
    /// Name (people / projects) or description (commitments / open loops).
    pub title: String,
    /// Relationship summary / project summary, when there is one.
    pub detail: Option<String>,
    pub status: Option<String>,
    pub due_at: Option<i64>,
    pub confidence: f64,
    /// Below the fact band: shown as "possibly". Set by the caller that applies the band rule.
    pub possibly: bool,
    /// The user edited this row.
    pub pinned: bool,
    /// People / projects: `detail` is a written page or the user's own wording. Otherwise the
    /// stored text is the review's raw words, which the app does not show as a description.
    pub described: bool,
    /// People / projects: newest evidence timestamp (Unix ms) and provenance-link count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_evidence_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence_count: Option<i64>,
    /// Every event id behind the row, oldest id first.
    pub provenance: Vec<i64>,
    /// People only: the editable identity fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub person: Option<PersonFields>,
}

/// The people-only fields the editor shows and `state.update` accepts.
#[derive(Debug, Clone, Serialize, PartialEq, Default)]
pub struct PersonFields {
    pub aliases: Vec<String>,
    pub emails: Vec<String>,
    pub handles: Vec<String>,
    pub channel: Option<String>,
}

fn json_list(raw: Option<String>) -> Vec<String> {
    raw.and_then(|r| serde_json::from_str::<Vec<String>>(&r).ok()).unwrap_or_default()
}

/// The world model, one list per state table.
#[derive(Debug, Clone, Serialize, PartialEq, Default)]
pub struct WorldModel {
    pub people: Vec<ExportRow>,
    pub projects: Vec<ExportRow>,
    pub commitments: Vec<ExportRow>,
    pub open_loops: Vec<ExportRow>,
}

impl WorldModel {
    /// Keep only rows `keep` accepts, letting it set the `possibly` flag.
    pub fn filter(&mut self, mut keep: impl FnMut(&mut ExportRow) -> bool) {
        for rows in [&mut self.people, &mut self.projects, &mut self.commitments, &mut self.open_loops] {
            rows.retain_mut(&mut keep);
        }
    }

    pub fn tables(&self) -> [(StateTable, &Vec<ExportRow>); 4] {
        [
            (StateTable::People, &self.people),
            (StateTable::Projects, &self.projects),
            (StateTable::Commitments, &self.commitments),
            (StateTable::OpenLoops, &self.open_loops),
        ]
    }
}

fn provenance_map(conn: &Connection, table: StateTable) -> Result<HashMap<i64, Vec<i64>>, rusqlite::Error> {
    let mut stmt = conn.prepare(&format!(
        "SELECT p.state_id, p.event_id FROM state_provenance p JOIN event_log e ON e.id=p.event_id
          WHERE p.state_table = ?1 AND {} ORDER BY p.state_id, p.event_id",
        crate::actor::current_read().predicate("e."),
    ))?;
    let mut map: HashMap<i64, Vec<i64>> = HashMap::new();
    let rows = stmt.query_map([table.as_str()], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
    for row in rows {
        let (state_id, event_id) = row?;
        let ids = map.entry(state_id).or_default();
        if ids.last() != Some(&event_id) {
            ids.push(event_id);
        }
    }
    Ok(map)
}

/// Read one editable entity page within the caller's read scope. Hidden ids read as absent.
pub fn export_entity(conn: &Connection, table: StateTable, id: i64) -> Result<Option<ExportRow>, rusqlite::Error> {
    let scope = crate::actor::current_read();
    let (title, detail, status, fields) = match table {
        StateTable::People => ("display_name", "relationship_summary", "NULL", "aliases, emails, handles, interaction_channel"),
        StateTable::Projects => ("name", "summary", "status", "NULL, NULL, NULL, NULL"),
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    let name = table.as_str();
    let filter = if table == StateTable::People { " AND COALESCE(person_class, '') <> 'self'" } else { "" };
    let sql = format!(
        "SELECT id, {title}, {detail}, {status}, {}, user_pinned, page IS NOT NULL OR wording_edited = 1, {fields},
                last_evidence_at,
                (SELECT COUNT(*) FROM state_provenance p JOIN event_log e ON e.id=p.event_id
                 WHERE p.state_table='{name}' AND p.state_id={name}.id AND {})
           FROM {name} WHERE id = ?1{filter} AND {}",
        crate::entity_pages::listed_confidence_sql(table, name),
        scope.predicate("e."),
        scope.predicate(&format!("{name}.")),
    );
    let mut row = conn.query_row(&sql, [id], |r| {
        let confidence: f64 = r.get(4)?;
        Ok(ExportRow {
            id: r.get(0)?, title: r.get(1)?, detail: r.get(2)?, status: r.get(3)?,
            due_at: None, confidence, possibly: confidence < 0.8,
            pinned: r.get::<_, i64>(5)? != 0, described: r.get::<_, i64>(6)? != 0,
            last_evidence_at: r.get(11)?, evidence_count: r.get(12)?,
            provenance: Vec::new(),
            person: if table == StateTable::People { Some(PersonFields {
                aliases: json_list(r.get(7)?), emails: json_list(r.get(8)?),
                handles: json_list(r.get(9)?), channel: r.get(10)?,
            }) } else { None },
        })
    }).optional()?;
    if let Some(row) = &mut row {
        let mut stmt = conn.prepare(&format!(
            "SELECT DISTINCT p.event_id FROM state_provenance p JOIN event_log e ON e.id=p.event_id
             WHERE p.state_table = ?1 AND p.state_id = ?2 AND {} ORDER BY p.event_id",
            scope.predicate("e."),
        ))?;
        row.provenance = stmt.query_map(params![name, id], |r| r.get(0))?.collect::<Result<_, _>>()?;
    }
    Ok(row)
}

/// Read state and provenance within the caller's read scope (world-only by default).
/// Commitment confidence is read through the dispute cap, exactly as Fusion and the list tools see it.
pub fn export(conn: &Connection) -> Result<WorldModel, rusqlite::Error> {
    let scope = crate::actor::current_read();
    let read = |table: StateTable, sql: &str| -> Result<Vec<ExportRow>, rusqlite::Error> {
        let mut provenance = provenance_map(conn, table)?;
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([], |r| {
            Ok(ExportRow {
                id: r.get(0)?,
                title: r.get(1)?,
                detail: r.get(2)?,
                status: r.get(3)?,
                due_at: r.get(4)?,
                confidence: r.get(5)?,
                possibly: false,
                pinned: r.get::<_, i64>(6)? != 0,
                described: r.get::<_, i64>(7)? != 0,
                last_evidence_at: r.get(8)?,
                evidence_count: r.get(9)?,
                provenance: Vec::new(),
                person: None,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            let mut row = row?;
            row.provenance = provenance.remove(&row.id).unwrap_or_default();
            out.push(row);
        }
        Ok(out)
    };
    // The World model is the correction surface: it lists every person and project (a hidden
    // singleton too, so a wrong hide can be fixed); a listed row reads as a fact (`entity_pages`).
    use crate::entity_pages::listed_confidence_sql;
    let mut people = read(
        StateTable::People,
        &format!(
            "SELECT id, display_name, relationship_summary, NULL, NULL, {}, user_pinned, page IS NOT NULL OR wording_edited = 1,
                    last_evidence_at,
                    (SELECT COUNT(*) FROM state_provenance p JOIN event_log e ON e.id=p.event_id
                     WHERE p.state_table='people' AND p.state_id=people.id AND {})
               FROM people WHERE COALESCE(person_class, '') <> 'self' AND {} ORDER BY display_name COLLATE NOCASE, id",
            listed_confidence_sql(StateTable::People, "people"),
            scope.predicate("e."),
            scope.predicate("people."),
        ),
    )?;
    let mut fields: HashMap<i64, PersonFields> = HashMap::new();
    {
        let mut stmt = conn.prepare(&format!(
            "SELECT id, aliases, emails, handles, interaction_channel FROM people WHERE {}",
            scope.predicate("people."),
        ))?;
        let it = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, PersonFields {
                aliases: json_list(r.get(1)?),
                emails: json_list(r.get(2)?),
                handles: json_list(r.get(3)?),
                channel: r.get(4)?,
            }))
        })?;
        for f in it {
            let (id, f) = f?;
            fields.insert(id, f);
        }
    }
    for row in &mut people {
        row.person = Some(fields.remove(&row.id).unwrap_or_default());
    }
    Ok(WorldModel {
        people,
        projects: read(
            StateTable::Projects,
            &format!(
                "SELECT id, name, summary, status, NULL, {}, user_pinned, page IS NOT NULL OR wording_edited = 1,
                        last_evidence_at,
                        (SELECT COUNT(*) FROM state_provenance p JOIN event_log e ON e.id=p.event_id
                         WHERE p.state_table='projects' AND p.state_id=projects.id AND {})
                   FROM projects WHERE {} ORDER BY name COLLATE NOCASE, id",
                listed_confidence_sql(StateTable::Projects, "projects"),
                scope.predicate("e."),
                scope.predicate("projects."),
            ),
        )?,
        commitments: read(
            StateTable::Commitments,
            &format!(
                "SELECT id, description, NULL, status, due_at, {}, user_pinned, 0, NULL, NULL
                   FROM commitments WHERE {} ORDER BY (due_at IS NULL), due_at, id",
                crate::disputes::COMMITMENT_CONFIDENCE_SQL,
                scope.predicate("commitments."),
            ),
        )?,
        open_loops: read(
            StateTable::OpenLoops,
            &format!("SELECT id, description, kind, status, NULL, confidence, user_pinned, 0, NULL, NULL
               FROM open_loops WHERE {} ORDER BY opened_at, id", scope.predicate("open_loops.")),
        )?,
    })
}

/// A row's text on one Markdown line: newlines folded, table/list syntax escaped.
fn md_inline(s: &str) -> String {
    let folded: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::with_capacity(folded.len());
    for c in folded.chars() {
        if matches!(c, '\\' | '*' | '_' | '`' | '[' | ']' | '<' | '>' | '#' | '|') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// UTC calendar date for a Unix-ms instant (civil-from-days; no time-zone database needed).
fn utc_date(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Render the export as Markdown: one section per table, each row with its confidence, whether
/// the user edited it, and the event ids behind it.
pub fn to_markdown(model: &WorldModel, generated_at_ms: i64) -> String {
    let mut out = String::new();
    out.push_str("# World model export\n\n");
    out.push_str(&format!(
        "Generated {} (UTC). Confidence runs 0.00–1.00; \"possibly\" marks a row below the fact \
         band; \"edited\" marks a row you corrected. Sources are event ids in your memory.\n",
        utc_date(generated_at_ms)
    ));
    for (table, rows) in model.tables() {
        let heading = match table {
            StateTable::People => "People",
            StateTable::Projects => "Projects",
            StateTable::Commitments => "Commitments",
            StateTable::OpenLoops => "Open loops",
        };
        out.push_str(&format!("\n## {heading} ({})\n\n", rows.len()));
        if rows.is_empty() {
            out.push_str("_None._\n");
            continue;
        }
        for row in rows {
            let mut line = format!("- **{}**", md_inline(&row.title));
            let mut facts = vec![format!("confidence {:.2}", row.confidence)];
            if row.possibly {
                facts.push("possibly".into());
            }
            if row.pinned {
                facts.push("edited".into());
            }
            if let Some(status) = &row.status {
                facts.push(format!("status {}", md_inline(status)));
            }
            if let Some(due) = row.due_at {
                facts.push(format!("due {}", utc_date(due)));
            }
            line.push_str(&format!(" — {}", facts.join(", ")));
            out.push_str(&line);
            out.push('\n');
            // A person or project carries a description only once it is a written page or the
            // user's own words; before that the stored text is a raw review scrap.
            let described = row.described || !matches!(table, StateTable::People | StateTable::Projects);
            if let Some(detail) = row.detail.as_deref().filter(|d| described && !d.trim().is_empty()) {
                let label = if table == StateTable::OpenLoops { "Kind" } else { "Summary" };
                out.push_str(&format!("  - {label}: {}\n", md_inline(detail)));
            }
            let sources = if row.provenance.is_empty() {
                "none".to_string()
            } else {
                row.provenance.iter().map(|id| format!("#{id}")).collect::<Vec<_>>().join(", ")
            };
            out.push_str(&format!("  - Sources: {sources}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_log::{insert as insert_event, NewEvent};
    use crate::extract::{persist_candidates, persist_candidates_deduped, Candidate};
    use crate::state::{
        insert_commitment, insert_person, insert_project, CommitmentDirection, CommitmentStatus,
        NewCommitment, NewPerson, NewProject, ProjectStatus, Provenance,
    };

    fn event(conn: &Connection, hash: &str, ts: i64) -> i64 {
        insert_event(
            conn,
            &NewEvent {
                ts,
                source: "capture",
                kind: "text",
                app_bundle_id: None,
                window_title: None,
                content: "evidence",
                content_hash: hash,
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap()
    }

    fn commitment(conn: &mut Connection, desc: &str, e: i64, confidence: f64) -> i64 {
        insert_commitment(
            conn,
            &NewCommitment {
                direction: CommitmentDirection::Mine,
                counterparty_id: None,
                description: desc,
                due_at: None,
                status: CommitmentStatus::Open,
                project_id: None,
                confidence,
                now: 10,
            },
            &[Provenance::new(e)],
        )
        .unwrap()
    }

    fn row(conn: &Connection, id: i64) -> (String, f64, f64, i64, Option<i64>) {
        conn.query_row(
            "SELECT description, confidence, base_confidence, user_pinned, due_at
               FROM commitments WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap()
    }

    #[test]
    fn snooze_sets_clears_and_hides_the_row_until_due() {
        let mut conn = crate::open_in_memory().unwrap();
        let e = event(&conn, "snooze", 1);
        let id = commitment(&mut conn, "Send the deck", e, 0.9);
        let (t, i, patch) = parse_request(&format!(
            r#"{{"table":"commitments","id":{id},"deferred_until":5000}}"#
        ))
        .unwrap();
        assert!(update_state(&mut conn, t, i, &patch, 100).unwrap());
        let rows = crate::state::list_commitments(&conn).unwrap();
        assert_eq!(rows[0].deferred_until, Some(5000));
        assert!(rows[0].is_deferred(4999) && !rows[0].is_deferred(5000));
        let (t, i, patch) = parse_request(&format!(
            r#"{{"table":"commitments","id":{id},"deferred_until":null}}"#
        ))
        .unwrap();
        assert!(update_state(&mut conn, t, i, &patch, 200).unwrap());
        assert_eq!(crate::state::list_commitments(&conn).unwrap()[0].deferred_until, None);
        // People have no snooze.
        let (t, i, patch) = parse_request(r#"{"table":"people","id":1,"deferred_until":5}"#).unwrap();
        assert!(matches!(update_state(&mut conn, t, i, &patch, 1), Err(EditError::Invalid("invalid field for table"))));
    }

    #[test]
    fn snooze_applies_to_open_loops_too() {
        let mut conn = crate::open_in_memory().unwrap();
        conn.execute(
            "INSERT INTO open_loops (kind, description, opened_at, staleness_days, status, confidence,
               base_confidence, created_at, updated_at, last_evidence_at, user_pinned)
             VALUES ('other', 'Wait for Bob', 1, 0, 'open', 1.0, 1.0, 1, 1, 1, 1)",
            [],
        )
        .unwrap();
        let (t, i, patch) = parse_request(r#"{"table":"open_loops","id":1,"deferred_until":900}"#).unwrap();
        assert!(update_state(&mut conn, t, i, &patch, 5).unwrap());
        let loops = crate::state::list_open_loops(&conn).unwrap();
        assert!(loops[0].is_deferred(10));
    }

    #[test]
    fn create_adds_a_pinned_user_row_that_lists_without_provenance() {
        let mut conn = crate::open_in_memory().unwrap();
        let req = parse_create_request(
            r#"{"table":"commitments","description":"  Book flights ","due_at":9000}"#,
        )
        .unwrap();
        let id = create_state(&mut conn, &req, 50).unwrap().unwrap();
        let rows = crate::state::list_commitments(&conn).unwrap();
        assert_eq!((rows[0].id, rows[0].description.as_str(), rows[0].due_at), (id, "Book flights", Some(9000)));
        assert_eq!(rows[0].confidence, 1.0);
        let pinned: i64 = conn
            .query_row("SELECT user_pinned FROM commitments WHERE id = ?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(pinned, 1);
        let loop_req = parse_create_request(r#"{"table":"open_loops","description":"Chase invoice"}"#).unwrap();
        create_state(&mut conn, &loop_req, 60).unwrap().unwrap();
        assert_eq!(crate::state::list_open_loops(&conn).unwrap().len(), 1);
    }

    #[test]
    fn create_refuses_bad_requests_and_unknown_people() {
        let mut conn = crate::open_in_memory().unwrap();
        for bad in [
            r#"{"table":"people","description":"x"}"#,
            r#"{"table":"commitments"}"#,
            r#"{"table":"open_loops","description":"x","due_at":5}"#,
            r#"{"table":"commitments","description":"x","bogus":1}"#,
            r#"{"table":"commitments","description":"x","person_id":0}"#,
        ] {
            assert!(parse_create_request(bad).is_err(), "{bad}");
        }
        let blank = parse_create_request(r#"{"table":"commitments","description":"   "}"#).unwrap();
        assert!(create_state(&mut conn, &blank, 1).is_err());
        let ghost = parse_create_request(r#"{"table":"commitments","description":"x","person_id":99}"#).unwrap();
        assert_eq!(create_state(&mut conn, &ghost, 1).unwrap(), None);
        assert!(crate::state::list_commitments(&conn).unwrap().is_empty());
    }

    #[test]
    fn edit_pins_the_row_at_full_confidence() {
        let mut conn = crate::open_in_memory().unwrap();
        let e = event(&conn, "a", 1);
        let id = commitment(&mut conn, "send the deck", e, 0.3);
        let patch = StatePatch {
            description: Some("send the final deck".into()),
            due_at: Some(Some(5_000)),
            ..StatePatch::default()
        };
        assert!(update_state(&mut conn, StateTable::Commitments, id, &patch, 100).unwrap());
        let (desc, conf, base, pinned, due) = row(&conn, id);
        assert_eq!(desc, "send the final deck");
        assert_eq!((conf, base, pinned, due), (1.0, 1.0, 1, Some(5_000)));
        // provenance survives an edit — the evidence is still the evidence
        assert_eq!(crate::state::provenance_count(&conn, StateTable::Commitments, id).unwrap(), 1);
        // unknown id → false, not an error
        assert!(!update_state(&mut conn, StateTable::Commitments, 999, &patch, 100).unwrap());
    }

    #[test]
    fn person_fields_round_trip_through_the_shared_request() {
        let mut conn = crate::open_in_memory().unwrap();
        conn.execute(
            "INSERT INTO people(display_name,confidence,base_confidence,created_at,updated_at,last_evidence_at) VALUES('Dana',0.6,0.6,1,1,1)",
            [],
        )
        .unwrap();
        let body = r#"{"table":"people","id":1,"aliases":["Dani","dani"],"emails":["d@acme.test"],"handles":[],"channel":"Discord","summary":"Cofounder at Acme"}"#;
        let (table, id, patch) = parse_request(body).unwrap();
        assert!(update_state(&mut conn, table, id, &patch, 50).unwrap());
        let wm = export(&conn).unwrap();
        assert_eq!(export_entity(&conn, StateTable::People, 1).unwrap().as_ref(), wm.people.first());
        assert_eq!(export_entity(&conn, StateTable::People, 999).unwrap(), None);
        let p = wm.people[0].person.clone().unwrap();
        assert_eq!((p.aliases, p.emails, p.handles, p.channel), (vec!["Dani".into()], vec!["d@acme.test".into()], vec![], Some("Discord".into())));
        let edited: (i64, i64) = conn.query_row("SELECT wording_edited, user_pinned FROM people WHERE id=1", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(edited, (1, 1));
        assert!(parse_request(r#"{"table":"projects","id":1,"aliases":["x"]}"#).is_ok());
        let (t, i, bad) = parse_request(r#"{"table":"projects","id":1,"aliases":["x"]}"#).unwrap();
        assert!(update_state(&mut conn, t, i, &bad, 60).is_err(), "people-only fields refused elsewhere");
    }

    #[test]
    fn export_entity_keeps_list_freshness_and_counts_provenance_links() {
        let conn = crate::open_in_memory().expect("open fixture");
        let event_id = event(&conn, "entity-stats", 1);
        conn.execute_batch(
            "INSERT INTO people(display_name,relationship_summary,confidence,created_at,updated_at,last_evidence_at,user_pinned)
             VALUES('Dana','Raw person summary',1,1,1,60000,1);
             INSERT INTO projects(name,status,summary,confidence,created_at,updated_at,last_evidence_at,user_pinned)
             VALUES('Atlas','waiting','Raw project summary',1,1,1,120000,1);"
        ).expect("insert entities");
        for table in [StateTable::People, StateTable::Projects] {
            // The list counts links, including repeated links to the same event.
            for _ in 0..2 {
                conn.execute(
                    "INSERT INTO state_provenance(state_table,state_id,event_id) VALUES(?1,1,?2)",
                    params![table.as_str(), event_id],
                ).expect("insert provenance");
            }
        }
        let person = export_entity(&conn, StateTable::People, 1).expect("read person").expect("person exists");
        let project = export_entity(&conn, StateTable::Projects, 1).expect("read project").expect("project exists");
        let people = crate::state::list_people(&conn).expect("list people");
        let projects = crate::state::list_projects(&conn).expect("list projects");
        assert_eq!((person.last_evidence_at, person.evidence_count), (Some(people[0].last_evidence_at), Some(people[0].evidence_count)));
        assert_eq!((project.last_evidence_at, project.evidence_count), (Some(projects[0].last_evidence_at), Some(projects[0].evidence_count)));
        assert_eq!((person.last_evidence_at, person.evidence_count), (Some(60_000), Some(2)));
        assert_eq!((project.last_evidence_at, project.evidence_count), (Some(120_000), Some(2)));
        assert_eq!(person.provenance, vec![event_id]);
        assert_eq!(project.status.as_deref(), Some("waiting"));
        let model = export(&conn).expect("export model");
        assert_eq!(Some(&person), model.people.first());
        assert_eq!(Some(&project), model.projects.first());
        assert_eq!(export_entity(&conn, StateTable::Projects, 999).expect("missing project"), None);
    }

    #[test]
    fn export_entity_excludes_self() {
        let conn = crate::open_in_memory().unwrap();
        conn.execute(
            "INSERT INTO people(display_name,person_class,confidence,base_confidence,created_at,updated_at,last_evidence_at) VALUES('Me','self',1,1,1,1,1)",
            [],
        ).unwrap();
        assert_eq!(export_entity(&conn, StateTable::People, 1).unwrap(), None);
        assert!(export(&conn).unwrap().people.is_empty());
    }

    #[test]
    fn date_edits_preserve_sources_but_wording_edits_do_not_claim_old_proof() {
        let mut conn = crate::open_in_memory().unwrap();
        let e = event(&conn, "date-edit", 1);
        let id = commitment(&mut conn, "send the deck", e, 0.6);
        crate::state::retain_quote(&conn, StateTable::Commitments, id, e, "evidence").unwrap();
        update_state(&mut conn, StateTable::Commitments, id, &StatePatch {
            due_at: Some(Some(5_000)), ..StatePatch::default()
        }, 100).unwrap();
        let row = crate::state::get_commitment(&conn, id).unwrap().unwrap();
        assert_eq!(row.first_event_id, Some(e));
        assert_eq!(crate::state::evidence(&conn, StateTable::Commitments, id).unwrap()[0].evidence_kind,
            Some(crate::state::EvidenceKind::SourceQuote));
        update_state(&mut conn, StateTable::Commitments, id, &StatePatch {
            description: Some("send a different deck".into()), ..StatePatch::default()
        }, 101).unwrap();
        assert_eq!(crate::state::get_commitment(&conn, id).unwrap().unwrap().first_event_id, None);
        assert_eq!(crate::state::evidence(&conn, StateTable::Commitments, id).unwrap()[0].evidence_kind,
            Some(crate::state::EvidenceKind::EditedContext));
    }

    #[test]
    fn edit_refuses_fields_the_table_does_not_have() {
        let mut conn = crate::open_in_memory().unwrap();
        let e = event(&conn, "a", 1);
        let id = commitment(&mut conn, "x", e, 0.3);
        let bad = [
            StatePatch { name: Some("n".into()), ..StatePatch::default() },
            StatePatch { status: Some("active".into()), ..StatePatch::default() },
            StatePatch { description: Some("   ".into()), ..StatePatch::default() },
            StatePatch::default(),
        ];
        for patch in bad {
            assert!(matches!(
                update_state(&mut conn, StateTable::Commitments, id, &patch, 1),
                Err(EditError::Invalid(_))
            ));
        }
        let person = insert_person(
            &mut conn,
            &NewPerson { display_name: "Ann", confidence: 0.4, now: 1, ..Default::default() },
            &[Provenance::new(e)],
        )
        .unwrap();
        let due = StatePatch { due_at: Some(None), ..StatePatch::default() };
        assert!(update_state(&mut conn, StateTable::People, person, &due, 1).is_err());
    }

    /// The acceptance property: no automatic pass overwrites a pinned row.
    #[test]
    fn automatic_passes_skip_pinned_rows() {
        let mut conn = crate::open_in_memory().unwrap();
        let e1 = event(&conn, "a", 1);
        let e2 = event(&conn, "b", 2);
        let e3 = event(&conn, "c", 3);
        let pinned = commitment(&mut conn, "ship it", e1, 0.3);
        let patch = StatePatch { status: Some("open".into()), ..StatePatch::default() };
        update_state(&mut conn, StateTable::Commitments, pinned, &patch, 10).unwrap();
        for e in [e2, e3] {
            conn.execute(
                "INSERT INTO state_provenance (state_table, state_id, event_id) VALUES ('commitments', ?1, ?2)",
                [pinned, e],
            )
            .unwrap();
        }
        // decay a long time later
        crate::recompute::decay_confidence(&mut conn, 10_000_000_000, 1_000).unwrap();
        // corroboration (it has three events now) would cap it at 0.75
        crate::recompute::corroborate_at(&mut conn, 20).unwrap();
        // pruning
        crate::recompute::prune_faded(&mut conn, 10_000_000_000, 2.0, 0).unwrap();
        // model promotion of the same candidate
        let cand = Candidate::Commitment {
            direction: CommitmentDirection::Mine,
            description: "ship it".into(),
            confidence: 0.9,
            evidence: None,
        };
        crate::extract::promote_candidate_for_event(&mut conn, e1, &cand, 30).unwrap();
        let (_, conf, base, flag, _) = row(&conn, pinned);
        assert_eq!((conf, base, flag), (1.0, 1.0, 1));
    }

    #[test]
    fn delete_removes_row_and_links_but_keeps_events() {
        let mut conn = crate::open_in_memory().unwrap();
        let e = event(&conn, "a", 1);
        let person = insert_person(
            &mut conn,
            &NewPerson { display_name: "Ann", confidence: 0.6, now: 1, ..Default::default() },
            &[Provenance::new(e)],
        )
        .unwrap();
        let c = insert_commitment(
            &mut conn,
            &NewCommitment {
                direction: CommitmentDirection::Theirs,
                counterparty_id: Some(person),
                description: "Ann sends notes",
                due_at: None,
                status: CommitmentStatus::Open,
                project_id: None,
                confidence: 0.6,
                now: 1,
            },
            &[Provenance::new(e)],
        )
        .unwrap();
        assert!(delete_state(&mut conn, StateTable::People, person, 5).unwrap());
        let people: i64 = conn.query_row("SELECT count(*) FROM people", [], |r| r.get(0)).unwrap();
        assert_eq!(people, 0);
        assert_eq!(crate::state::provenance_count(&conn, StateTable::People, person).unwrap(), 0);
        let cp: Option<i64> =
            conn.query_row("SELECT counterparty_id FROM commitments WHERE id = ?1", [c], |r| r.get(0)).unwrap();
        assert_eq!(cp, None, "the commitment survives, unlinked");
        let events: i64 = conn.query_row("SELECT count(*) FROM event_log", [], |r| r.get(0)).unwrap();
        assert_eq!(events, 1, "events are the log and stay");
        assert!(!delete_state(&mut conn, StateTable::People, person, 6).unwrap());
    }

    /// The Dream Cycle re-classifies past events; a deleted belief must not come back from them.
    #[test]
    fn deleted_belief_is_not_resurrected_from_the_same_old_events() {
        let mut conn = crate::open_in_memory().unwrap();
        let old = event(&conn, "old", 1_000);
        let cand = vec![Candidate::Commitment {
            direction: CommitmentDirection::Mine,
            description: "send the deck".into(),
            confidence: 0.3,
            evidence: None,
        }];
        let ids = persist_candidates(&mut conn, old, &cand, 1_000, 1_000).unwrap();
        assert_eq!(ids.len(), 1);
        assert!(delete_state(&mut conn, StateTable::Commitments, ids[0], 2_000).unwrap());

        // consolidation over the same window (evidence_ts is the cycle time there)
        assert!(persist_candidates(&mut conn, old, &cand, 3_000, 3_000).unwrap().is_empty());
        assert!(persist_candidates_deduped(&mut conn, old, &cand, 1_000, 3_000).unwrap().is_empty());
        let n: i64 = conn.query_row("SELECT count(*) FROM commitments", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);

        // said again later: new evidence, extracted as usual
        let later = event(&conn, "new", 5_000);
        assert_eq!(persist_candidates(&mut conn, later, &cand, 5_000, 5_000).unwrap().len(), 1);
    }

    #[test]
    fn edited_wording_does_not_come_back_as_a_twin() {
        let mut conn = crate::open_in_memory().unwrap();
        let old = event(&conn, "old", 1_000);
        let cand = vec![Candidate::Commitment {
            direction: CommitmentDirection::Mine,
            description: "send the deck".into(),
            confidence: 0.3,
            evidence: None,
        }];
        let id = persist_candidates(&mut conn, old, &cand, 1_000, 1_000).unwrap()[0];
        let patch = StatePatch { description: Some("send the Q3 deck".into()), ..StatePatch::default() };
        update_state(&mut conn, StateTable::Commitments, id, &patch, 2_000).unwrap();
        assert!(persist_candidates(&mut conn, old, &cand, 3_000, 3_000).unwrap().is_empty());
        let n: i64 = conn.query_row("SELECT count(*) FROM commitments", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn deleted_extracted_person_is_not_recreated_from_old_events() {
        let mut conn = crate::open_in_memory().unwrap();
        let content = "Met Dana Reyes from Acme about the launch";
        let e = insert_event(
            &conn,
            &NewEvent {
                ts: 1_000,
                source: "capture",
                kind: "text",
                app_bundle_id: None,
                window_title: None,
                content,
                content_hash: "k",
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap();
        let entity = crate::knowledge::Candidate {
            kind: crate::knowledge::Kind::Person,
            name: "Dana Reyes".into(),
            context: "Acme".into(),
            quote: content.into(), ..Default::default() };
        let id = crate::knowledge::persist(&mut conn, e, &entity, 1_000).unwrap().unwrap();
        assert!(delete_state(&mut conn, StateTable::People, id, 2_000).unwrap());
        assert_eq!(crate::knowledge::persist(&mut conn, e, &entity, 3_000).unwrap(), None);
        let n: i64 = conn.query_row("SELECT count(*) FROM people", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn a_pinned_commitment_is_not_capped_by_a_dispute_it_did_not_ask_for() {
        let mut conn = crate::open_in_memory().unwrap();
        let e1 = event(&conn, "a", 1);
        let e2 = event(&conn, "b", 2);
        let earlier = commitment(&mut conn, "send the report", e1, 0.9);
        let patch = StatePatch { due_at: Some(Some(86_400_000 * 10)), ..StatePatch::default() };
        update_state(&mut conn, StateTable::Commitments, earlier, &patch, 3).unwrap();
        let later = insert_commitment(
            &mut conn,
            &NewCommitment {
                direction: CommitmentDirection::Mine,
                counterparty_id: None,
                description: "send the report",
                due_at: Some(86_400_000 * 20),
                status: CommitmentStatus::Open,
                project_id: None,
                confidence: 0.9,
                now: 4,
            },
            &[Provenance::new(e2)],
        )
        .unwrap();
        // later evidence disagrees: it becomes a proposed change, the pinned row is untouched
        assert!(crate::disputes::check_new_commitment(&conn, later, 4).unwrap().is_some());
        let model = export(&conn).unwrap();
        let pinned = model.commitments.iter().find(|r| r.id == earlier).unwrap();
        let proposed = model.commitments.iter().find(|r| r.id == later).unwrap();
        assert_eq!(pinned.confidence, 1.0);
        assert!(proposed.confidence < 0.8);
        assert_eq!(pinned.due_at, Some(86_400_000 * 10));
    }

    #[test]
    fn export_lists_every_table_with_confidence_and_provenance() {
        let mut conn = crate::open_in_memory().unwrap();
        let e1 = event(&conn, "a", 1);
        let e2 = event(&conn, "b", 2);
        insert_person(
            &mut conn,
            &NewPerson { display_name: "Ann | Lee", confidence: 0.9, now: 1, ..Default::default() },
            &[Provenance::new(e2), Provenance::new(e1)],
        )
        .unwrap();
        insert_project(
            &mut conn,
            &NewProject {
                name: "Launch",
                status: ProjectStatus::Active,
                summary: Some("Q3\nlaunch"),
                participants_json: None,
                sources_json: None,
                confidence: 0.6,
                now: 1,
            },
            &[Provenance::new(e1)],
        )
        .unwrap();
        let c = commitment(&mut conn, "send the deck", e1, 0.3);
        update_state(
            &mut conn,
            StateTable::Commitments,
            c,
            &StatePatch { due_at: Some(Some(0)), ..StatePatch::default() },
            5,
        )
        .unwrap();
        let model = export(&conn).unwrap();
        assert_eq!(model.people[0].provenance, vec![e1, e2]);
        assert!(model.commitments[0].pinned);
        let md = to_markdown(&model, 0);
        assert!(md.starts_with("# World model export"));
        for heading in ["## People (1)", "## Projects (1)", "## Commitments (1)", "## Open loops (0)"] {
            assert!(md.contains(heading), "{heading}\n{md}");
        }
        assert!(md.contains("**Ann \\| Lee** — confidence 0.90"), "{md}");
        assert!(md.contains(&format!("Sources: #{e1}, #{e2}")), "{md}");
        assert!(!md.contains("Summary:"), "a review's raw words are not a description\n{md}");
        assert!(md.contains("edited, status open, due 1970-01-01"), "{md}");
        conn.execute("UPDATE projects SET page = '{}'", []).unwrap();
        let md = to_markdown(&export(&conn).unwrap(), 0);
        assert!(md.contains("Summary: Q3 launch"), "a written page is\n{md}");
        assert!(serde_json::to_string(&model).unwrap().contains("\"provenance\""));
    }

    #[test]
    fn request_parser_is_strict() {
        let (t, id, p) = parse_request(
            r#"{"table":"commitments","id":3,"description":"x","due_at":null}"#,
        )
        .unwrap();
        assert_eq!((t, id), (StateTable::Commitments, 3));
        assert_eq!(p.due_at, Some(None));
        assert_eq!(p.description.as_deref(), Some("x"));
        let (_, _, p) = parse_request(r#"{"table":"people","id":1,"summary":null}"#).unwrap();
        assert_eq!(p.summary, Some(None));
        for bad in [
            "",
            "[]",
            r#"{"table":"events","id":1}"#,
            r#"{"table":"people","id":0}"#,
            r#"{"table":"people","id":1,"confidence":1}"#,
            r#"{"table":"people","id":1,"name":3}"#,
        ] {
            assert!(parse_request(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn utc_dates_are_calendar_correct() {
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(utc_date(951_782_400_000), "2000-02-29");
        assert_eq!(utc_date(1_790_000_000_000), "2026-09-21");
    }
}
