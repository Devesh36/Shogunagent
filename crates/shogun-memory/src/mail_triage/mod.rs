//! Mail triage state (issue #319, `docs/specs/issue-319-mail-triage-spec.md`): the per-message
//! metadata, the per-thread estimate, the label registry, user corrections and the sender rules
//! learned from them, the Batch refinement ledger, the incremental sync cursor, and the user's
//! own mailbox identities.
//!
//! Layering: [`local`] is the pure Stage L classifier; this module is the DB effect layer the
//! daemon calls. Nothing here touches the network (the Batch lane lives in shogun-core behind
//! its `Classifier` seam, invariant 5). The estimate is a **state row** (provenance +
//! confidence, FR-ST-02/20): `latest_event_id` is its provenance and `confidence` gates how it
//! is shown.

pub mod local;

use rusqlite::{params, Connection, OptionalExtension};

use crate::state::{NewOpenLoop, OpenLoopKind, Provenance};
use crate::MemoryError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Bucket {
    Important,
    Other,
}

impl Bucket {
    pub fn as_str(self) -> &'static str {
        match self {
            Bucket::Important => "important",
            Bucket::Other => "other",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "important" => Some(Bucket::Important),
            "other" => Some(Bucket::Other),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Attention {
    NeedsReply,
    WaitingOnThem,
    Fyi,
    None,
}

impl Attention {
    pub fn as_str(self) -> &'static str {
        match self {
            Attention::NeedsReply => "needs_reply",
            Attention::WaitingOnThem => "waiting_on_them",
            Attention::Fyi => "fyi",
            Attention::None => "none",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "needs_reply" => Some(Attention::NeedsReply),
            "waiting_on_them" => Some(Attention::WaitingOnThem),
            "fyi" => Some(Attention::Fyi),
            "none" => Some(Attention::None),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Local,
    Batch,
    User,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Local => "local",
            Stage::Batch => "batch",
            Stage::User => "user",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "local" => Some(Stage::Local),
            "batch" => Some(Stage::Batch),
            "user" => Some(Stage::User),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BucketLock {
    KnownPerson,
    SameDomain,
    MyReply,
    Commitment,
    User,
}

impl BucketLock {
    pub fn as_str(self) -> &'static str {
        match self {
            BucketLock::KnownPerson => "known_person",
            BucketLock::SameDomain => "same_domain",
            BucketLock::MyReply => "my_reply",
            BucketLock::Commitment => "commitment",
            BucketLock::User => "user",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "known_person" => Some(BucketLock::KnownPerson),
            "same_domain" => Some(BucketLock::SameDomain),
            "my_reply" => Some(BucketLock::MyReply),
            "commitment" => Some(BucketLock::Commitment),
            "user" => Some(BucketLock::User),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveStatus {
    None,
    /// An explicit archive request from an API face, awaiting the desktop write-back tick.
    Pending,
    /// An explicit "put it back in the inbox" request, awaiting the same tick.
    UnarchivePending,
    Done,
    Failed,
    Undone,
}

impl ArchiveStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ArchiveStatus::None => "none",
            ArchiveStatus::Pending => "pending",
            ArchiveStatus::UnarchivePending => "unarchive_pending",
            ArchiveStatus::Done => "done",
            ArchiveStatus::Failed => "failed",
            ArchiveStatus::Undone => "undone",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(ArchiveStatus::None),
            "pending" => Some(ArchiveStatus::Pending),
            "unarchive_pending" => Some(ArchiveStatus::UnarchivePending),
            "done" => Some(ArchiveStatus::Done),
            "failed" => Some(ArchiveStatus::Failed),
            "undone" => Some(ArchiveStatus::Undone),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LabelKind {
    Builtin,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SenderRuleMatch {
    Addr,
    Domain,
}

impl SenderRuleMatch {
    pub fn as_str(self) -> &'static str {
        match self {
            SenderRuleMatch::Addr => "addr",
            SenderRuleMatch::Domain => "domain",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SenderRuleSource {
    User,
    FeedbackX2,
}

impl SenderRuleSource {
    pub fn as_str(self) -> &'static str {
        match self {
            SenderRuleSource::User => "user",
            SenderRuleSource::FeedbackX2 => "feedback_x2",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackOrigin {
    Ui,
    Api,
}

impl FeedbackOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            FeedbackOrigin::Ui => "ui",
            FeedbackOrigin::Api => "api",
        }
    }
}

/// One Gmail message's metadata (never the body — that is the event log's).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MailMessage {
    pub message_id: String,
    pub thread_key: String,
    pub event_id: i64,
    pub from_addr: String,
    pub from_name: Option<String>,
    pub from_domain: String,
    pub to_addrs: Vec<String>,
    pub cc_addrs: Vec<String>,
    pub is_from_me: bool,
    pub in_reply_to: Option<String>,
    pub gmail_label_ids: Vec<String>,
    pub has_list_unsubscribe: bool,
    pub has_ics: bool,
    pub internal_date_ms: i64,
}

/// A label registry row.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LabelRow {
    pub key: String,
    pub name: String,
    pub kind: LabelKind,
    pub definition: Option<String>,
    pub examples: Vec<String>,
    pub default_bucket: Bucket,
    pub auto_archive: bool,
    pub sync_to_gmail: bool,
    pub gmail_label_id: Option<String>,
    pub enabled: bool,
    pub sort_order: i64,
}

/// Maximum custom labels (FR-TG-06).
pub const MAX_CUSTOM_LABELS: usize = 20;
/// Prefix every custom label key carries.
pub const CUSTOM_LABEL_PREFIX: &str = "custom:";

/// A sender rule.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SenderRule {
    pub id: i64,
    pub match_kind: SenderRuleMatch,
    pub match_value: String,
    pub label_key: String,
    pub bucket: Option<Bucket>,
    pub source: SenderRuleSource,
}

/// The classifier's verdict (Stage L or Stage B), before it is written.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Triage {
    pub label_key: String,
    pub bucket: Bucket,
    pub attention: Attention,
    pub confidence: f64,
    pub reasons: Vec<String>,
    pub bucket_lock: Option<BucketLock>,
}

/// A stored triage row joined with the thread's display facts.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TriageRow {
    pub thread_key: String,
    /// The Gmail thread id (the part after `gmail:`), for "Open in Gmail".
    pub thread_id: String,
    pub latest_event_id: i64,
    pub label_key: String,
    pub bucket: Bucket,
    pub attention: Attention,
    pub confidence: f64,
    pub stage: Stage,
    pub reasons: Vec<String>,
    pub bucket_lock: Option<BucketLock>,
    pub archive_status: ArchiveStatus,
    pub archived_at_ms: Option<i64>,
    pub handled_at_ms: Option<i64>,
    pub open_loop_id: Option<i64>,
    pub synced_label_key: Option<String>,
    pub classified_at_ms: i64,
    pub updated_at_ms: i64,
    // Display facts (from mail_messages / threads):
    pub subject: String,
    pub from_addr: String,
    pub from_name: Option<String>,
    pub last_message_ms: i64,
    pub message_count: i64,
    /// A short excerpt (≤ 160 chars) of the latest message — the list row's second line.
    pub snippet: String,
}

/// List filter (FR-TG-30 `mail.triage.list`).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TriageFilter {
    pub bucket: Option<Bucket>,
    pub attention: Option<Attention>,
    pub label_key: Option<String>,
    pub since_ms: Option<i64>,
    pub include_handled: bool,
    pub include_archived: bool,
    pub limit: usize,
}

/// Incremental sync cursor.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SyncCursor {
    pub watermark_ms: i64,
    pub page_token: Option<String>,
}

/// Counts for the Morning Brief / Evening Wrap line (FR-TG-29) and the Notch counter.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TriageCounts {
    pub triaged: i64,
    pub other: i64,
    pub auto_archived: i64,
    pub needs_reply_open: i64,
    /// `(label_key, count)` of threads that landed in Other, descending.
    pub other_by_label: Vec<(String, i64)>,
}

/// A user correction row.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FeedbackRow {
    pub id: i64,
    pub ts_ms: i64,
    pub thread_key: String,
    pub from_label_key: String,
    pub to_label_key: String,
    pub from_bucket: Bucket,
    pub to_bucket: Bucket,
    pub origin: FeedbackOrigin,
    pub feedback_event_id: Option<i64>,
    pub distilled: bool,
}

fn json_vec(s: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(s).unwrap_or_default()
}

fn vec_json(v: &[String]) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "[]".to_string())
}

/// The domain half of an address, lowercase (empty when there is no `@`).
pub fn domain_of(addr: &str) -> String {
    addr.trim().rsplit_once('@').map(|(_, d)| d.trim().to_ascii_lowercase()).unwrap_or_default()
}

/// The Gmail thread id behind a `gmail:<id>` thread key (the key itself when unprefixed).
pub fn thread_id_of(thread_key: &str) -> &str {
    thread_key.strip_prefix("gmail:").unwrap_or(thread_key)
}

/// Upsert message metadata rows. A re-seen message keeps its first-seen `event_id` unless the
/// newer snapshot has a higher id (the latest snapshot is the provenance). Messages Gmail marks
/// `SENT` register the sender as one of the user's own identities.
pub fn upsert_messages(conn: &Connection, msgs: &[MailMessage], now: i64) -> Result<usize, rusqlite::Error> {
    let mut n = 0;
    for m in msgs {
        conn.execute(
            "INSERT INTO mail_messages
               (message_id, thread_key, event_id, from_addr, from_name, from_domain, to_addrs,
                cc_addrs, is_from_me, in_reply_to, gmail_label_ids, has_list_unsubscribe,
                has_ics, internal_date_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
             ON CONFLICT(message_id) DO UPDATE SET
               event_id = max(event_id, excluded.event_id),
               gmail_label_ids = excluded.gmail_label_ids,
               is_from_me = excluded.is_from_me",
            params![
                m.message_id,
                m.thread_key,
                m.event_id,
                m.from_addr.to_ascii_lowercase(),
                m.from_name,
                m.from_domain.to_ascii_lowercase(),
                vec_json(&m.to_addrs),
                vec_json(&m.cc_addrs),
                i64::from(m.is_from_me),
                m.in_reply_to,
                vec_json(&m.gmail_label_ids),
                i64::from(m.has_list_unsubscribe),
                i64::from(m.has_ics),
                m.internal_date_ms,
            ],
        )?;
        n += 1;
        if m.is_from_me && m.from_addr.contains('@') {
            conn.execute(
                "INSERT INTO mail_identities (addr, domain, seen_at_ms) VALUES (?1, ?2, ?3)
                 ON CONFLICT(addr) DO UPDATE SET seen_at_ms = excluded.seen_at_ms",
                params![m.from_addr.to_ascii_lowercase(), domain_of(&m.from_addr), now],
            )?;
        }
    }
    Ok(n)
}

/// The user's own addresses (lowercase).
pub fn identities(conn: &Connection) -> Result<Vec<String>, rusqlite::Error> {
    let mut stmt = conn.prepare("SELECT addr FROM mail_identities ORDER BY seen_at_ms DESC")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    rows.collect()
}

/// The user's own domains (distinct, lowercase).
pub fn identity_domains(conn: &Connection) -> Result<Vec<String>, rusqlite::Error> {
    let mut stmt = conn.prepare("SELECT DISTINCT domain FROM mail_identities")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    rows.collect()
}

/// Explicitly register an identity (e.g. the connected account's address from onboarding).
pub fn add_identity(conn: &Connection, addr: &str, now: i64) -> Result<(), rusqlite::Error> {
    if !addr.contains('@') {
        return Ok(());
    }
    conn.execute(
        "INSERT INTO mail_identities (addr, domain, seen_at_ms) VALUES (?1, ?2, ?3)
         ON CONFLICT(addr) DO UPDATE SET seen_at_ms = excluded.seen_at_ms",
        params![addr.trim().to_ascii_lowercase(), domain_of(addr), now],
    )?;
    Ok(())
}

/// Messages of a thread, oldest first.
pub fn messages_for_thread(conn: &Connection, thread_key: &str) -> Result<Vec<MailMessage>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT message_id, thread_key, event_id, from_addr, from_name, from_domain, to_addrs, cc_addrs,
                is_from_me, in_reply_to, gmail_label_ids, has_list_unsubscribe, has_ics, internal_date_ms
         FROM mail_messages WHERE thread_key = ?1 ORDER BY internal_date_ms ASC, message_id ASC",
    )?;
    let rows = stmt.query_map(params![thread_key], |r| {
        Ok(MailMessage {
            message_id: r.get(0)?,
            thread_key: r.get(1)?,
            event_id: r.get(2)?,
            from_addr: r.get(3)?,
            from_name: r.get(4)?,
            from_domain: r.get(5)?,
            to_addrs: json_vec(&r.get::<_, String>(6)?),
            cc_addrs: json_vec(&r.get::<_, String>(7)?),
            is_from_me: r.get::<_, i64>(8)? != 0,
            in_reply_to: r.get(9)?,
            gmail_label_ids: json_vec(&r.get::<_, String>(10)?),
            has_list_unsubscribe: r.get::<_, i64>(11)? != 0,
            has_ics: r.get::<_, i64>(12)? != 0,
            internal_date_ms: r.get(13)?,
        })
    })?;
    rows.collect()
}

// signals (the loaders for `local::classify`)

/// Build the thread signals for the classifier from stored messages. `body_excerpt` is passed in
/// by the caller (it has the latest snapshot's text in hand); `None` when the thread has no
/// stored messages.
pub fn thread_signals(
    conn: &Connection,
    thread_key: &str,
    subject: &str,
    body_excerpt: &str,
    now: i64,
) -> Result<Option<local::ThreadSignals>, rusqlite::Error> {
    let msgs = messages_for_thread(conn, thread_key)?;
    let Some(last) = msgs.last() else {
        return Ok(None);
    };
    let mine = identities(conn)?;
    let is_me = |a: &String| mine.iter().any(|m| m.eq_ignore_ascii_case(a));
    let to_me = last.to_addrs.iter().any(is_me);
    let cc_me = last.cc_addrs.iter().any(is_me);
    let excerpt: String = body_excerpt.chars().take(2_000).collect();
    Ok(Some(local::ThreadSignals {
        from_addr: last.from_addr.clone(),
        from_name: last.from_name.clone(),
        from_domain: last.from_domain.clone(),
        to_me: to_me || (!cc_me && mine.is_empty()),
        cc_only: cc_me && !to_me,
        latest_from_me: last.is_from_me,
        my_reply_in_thread: msgs.iter().any(|m| m.is_from_me),
        message_count: msgs.len(),
        subject: subject.to_string(),
        body_excerpt: excerpt,
        gmail_label_ids: last.gmail_label_ids.clone(),
        has_list_unsubscribe: last.has_list_unsubscribe,
        has_ics: msgs.iter().any(|m| m.has_ics),
        age_ms: (now - last.internal_date_ms).max(0),
    }))
}

/// Load what the world model knows about a sender / subject (FR-TG-12 `WorldSignals`).
pub fn world_signals(
    conn: &Connection,
    from_addr: &str,
    subject: &str,
) -> Result<local::WorldSignals, rusqlite::Error> {
    let my_domains = identity_domains(conn)?;
    let addr = from_addr.trim().to_ascii_lowercase();
    // people.emails is a JSON array; json_each is available in the bundled SQLite.
    let known_person = conn
        .query_row(
            "SELECT p.id, p.last_interaction_at IS NOT NULL
             FROM people p, json_each(COALESCE(p.emails, '[]')) e
             WHERE lower(e.value) = ?1
             ORDER BY p.confidence DESC LIMIT 1",
            params![addr],
            |r| Ok(local::KnownPerson { person_id: r.get(0)?, has_interaction: r.get::<_, i64>(1)? != 0 }),
        )
        .optional()?;
    // Also treat a person the user has actually mailed as known even if the people row lacks
    // `last_interaction_at` (the mailbox is the interaction).
    let known_person = match known_person {
        Some(mut p) => {
            if !p.has_interaction {
                let mailed: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM mail_messages m, json_each(m.to_addrs) t
                     WHERE m.is_from_me = 1 AND lower(t.value) = ?1",
                    params![addr],
                    |r| r.get(0),
                )?;
                p.has_interaction = mailed > 0;
            }
            Some(p)
        }
        None => {
            let mailed: i64 = conn.query_row(
                "SELECT COUNT(*) FROM mail_messages m, json_each(m.to_addrs) t
                 WHERE m.is_from_me = 1 AND lower(t.value) = ?1",
                params![addr],
                |r| r.get(0),
            )?;
            (mailed > 0).then_some(local::KnownPerson { person_id: 0, has_interaction: true })
        }
    };
    let subj = subject.trim().to_lowercase();
    let subj_core = normalise_subject(&subj);
    let commitment_linked = if let Some(p) = known_person.as_ref().filter(|p| p.person_id > 0) {
        conn.query_row(
            "SELECT COUNT(*) FROM commitments WHERE status IN ('open','overdue') AND counterparty_id = ?1",
            params![p.person_id],
            |r| r.get::<_, i64>(0),
        )? > 0
    } else {
        false
    } || (subj_core.len() >= 6
        && conn.query_row(
            "SELECT COUNT(*) FROM commitments WHERE status IN ('open','overdue') AND instr(lower(description), ?1) > 0",
            params![subj_core],
            |r| r.get::<_, i64>(0),
        )? > 0);
    let open_loop_linked = if let Some(p) = known_person.as_ref().filter(|p| p.person_id > 0) {
        conn.query_row(
            "SELECT COUNT(*) FROM open_loops WHERE status = 'open' AND counterparty_id = ?1",
            params![p.person_id],
            |r| r.get::<_, i64>(0),
        )? > 0
    } else {
        false
    };
    Ok(local::WorldSignals { my_domains, known_person, commitment_linked, open_loop_linked })
}

/// Strip reply/forward prefixes from a subject for matching.
fn normalise_subject(s: &str) -> String {
    let mut t = s.trim();
    loop {
        let next = t
            .strip_prefix("re:")
            .or_else(|| t.strip_prefix("fwd:"))
            .or_else(|| t.strip_prefix("fw:"))
            .map(str::trim);
        match next {
            Some(n) if n != t => t = n,
            _ => break,
        }
    }
    t.to_string()
}

fn row_to_label(r: &rusqlite::Row<'_>) -> rusqlite::Result<LabelRow> {
    Ok(LabelRow {
        key: r.get(0)?,
        name: r.get(1)?,
        kind: if r.get::<_, String>(2)? == "custom" { LabelKind::Custom } else { LabelKind::Builtin },
        definition: r.get(3)?,
        examples: json_vec(&r.get::<_, String>(4)?),
        default_bucket: Bucket::parse(&r.get::<_, String>(5)?).unwrap_or(Bucket::Important),
        auto_archive: r.get::<_, i64>(6)? != 0,
        sync_to_gmail: r.get::<_, i64>(7)? != 0,
        gmail_label_id: r.get(8)?,
        enabled: r.get::<_, i64>(9)? != 0,
        sort_order: r.get(10)?,
    })
}

const LABEL_COLS: &str = "key, name, kind, definition, examples, default_bucket, auto_archive, sync_to_gmail, gmail_label_id, enabled, sort_order";

/// All labels in display order.
pub fn list_labels(conn: &Connection) -> Result<Vec<LabelRow>, rusqlite::Error> {
    let mut stmt = conn.prepare(&format!("SELECT {LABEL_COLS} FROM mail_labels ORDER BY sort_order, key"))?;
    let rows = stmt.query_map([], row_to_label)?;
    rows.collect()
}

/// One label.
pub fn get_label(conn: &Connection, key: &str) -> Result<Option<LabelRow>, rusqlite::Error> {
    conn.query_row(&format!("SELECT {LABEL_COLS} FROM mail_labels WHERE key = ?1"), params![key], row_to_label)
        .optional()
}

/// Turn a display name into a `custom:<slug>` key.
pub fn custom_key_for(name: &str) -> String {
    let slug: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    format!("{CUSTOM_LABEL_PREFIX}{}", if slug.is_empty() { "label".to_string() } else { slug })
}

/// Errors from label management.
#[derive(Debug, PartialEq, Eq)]
pub enum LabelError {
    TooMany,
    NotCustom,
    Unknown,
    InvalidKey,
}

/// Create or update a custom label (FR-TG-06). Returns the key. The standing-approval flags are
/// **not** set here — see [`set_label_flags`], which the API face guards with `confirm_standing`.
pub fn upsert_custom_label(
    conn: &Connection,
    key: Option<&str>,
    name: &str,
    definition: &str,
    default_bucket: Bucket,
    now: i64,
) -> Result<Result<String, LabelError>, rusqlite::Error> {
    let key = match key {
        Some(k) => k.to_string(),
        None => custom_key_for(name),
    };
    if !key.starts_with(CUSTOM_LABEL_PREFIX) {
        return Ok(Err(LabelError::InvalidKey));
    }
    let exists = get_label(conn, &key)?.is_some();
    if !exists {
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM mail_labels WHERE kind = 'custom'", [], |r| r.get(0))?;
        if n as usize >= MAX_CUSTOM_LABELS {
            return Ok(Err(LabelError::TooMany));
        }
    }
    let name = name.trim();
    let definition = definition.trim();
    conn.execute(
        "INSERT INTO mail_labels (key, name, kind, definition, default_bucket, sort_order, created_at_ms, updated_at_ms)
         VALUES (?1, ?2, 'custom', ?3, ?4, 1000, ?5, ?5)
         ON CONFLICT(key) DO UPDATE SET name = excluded.name, definition = excluded.definition,
           default_bucket = excluded.default_bucket, updated_at_ms = excluded.updated_at_ms",
        params![key, name, definition, default_bucket.as_str(), now],
    )?;
    Ok(Ok(key))
}

/// Flags a label carries. `None` leaves a flag untouched.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LabelFlags {
    pub enabled: Option<bool>,
    pub auto_archive: Option<bool>,
    pub sync_to_gmail: Option<bool>,
    pub default_bucket: Option<Bucket>,
}

/// Set a label's flags (enabled / standing approvals / default bucket).
pub fn set_label_flags(conn: &Connection, key: &str, flags: LabelFlags, now: i64) -> Result<bool, rusqlite::Error> {
    let Some(cur) = get_label(conn, key)? else {
        return Ok(false);
    };
    conn.execute(
        "UPDATE mail_labels SET enabled = ?2, auto_archive = ?3, sync_to_gmail = ?4, default_bucket = ?5, updated_at_ms = ?6
         WHERE key = ?1",
        params![
            key,
            i64::from(flags.enabled.unwrap_or(cur.enabled)),
            i64::from(flags.auto_archive.unwrap_or(cur.auto_archive)),
            i64::from(flags.sync_to_gmail.unwrap_or(cur.sync_to_gmail)),
            flags.default_bucket.unwrap_or(cur.default_bucket).as_str(),
            now,
        ],
    )?;
    Ok(true)
}

/// Remember the Gmail label id created for a synced label.
pub fn set_gmail_label_id(conn: &Connection, key: &str, gmail_label_id: &str, now: i64) -> Result<(), rusqlite::Error> {
    conn.execute(
        "UPDATE mail_labels SET gmail_label_id = ?2, updated_at_ms = ?3 WHERE key = ?1",
        params![key, gmail_label_id, now],
    )?;
    Ok(())
}

/// Delete a custom label. Threads carrying it fall back to `people` at low confidence so Stage B
/// re-evaluates them; sender rules pointing at it are removed.
pub fn delete_custom_label(conn: &mut Connection, key: &str, now: i64) -> Result<Result<(), LabelError>, rusqlite::Error> {
    let Some(label) = get_label(conn, key)? else {
        return Ok(Err(LabelError::Unknown));
    };
    if label.kind != LabelKind::Custom {
        return Ok(Err(LabelError::NotCustom));
    }
    let tx = conn.savepoint()?;
    tx.execute("DELETE FROM mail_sender_rules WHERE label_key = ?1", params![key])?;
    tx.execute(
        "UPDATE mail_triage SET label_key = 'people', confidence = 0.4, stage = 'local', updated_at_ms = ?2
         WHERE label_key = ?1",
        params![key, now],
    )?;
    tx.execute("DELETE FROM mail_labels WHERE key = ?1", params![key])?;
    tx.commit()?;
    Ok(Ok(()))
}

/// Whether any custom label is enabled (FR-TG-03: then every new thread goes to Stage B).
pub fn any_custom_label_enabled(conn: &Connection) -> Result<bool, rusqlite::Error> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM mail_labels WHERE kind = 'custom' AND enabled = 1",
        [],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

fn row_to_rule(r: &rusqlite::Row<'_>) -> rusqlite::Result<SenderRule> {
    Ok(SenderRule {
        id: r.get(0)?,
        match_kind: if r.get::<_, String>(1)? == "addr" { SenderRuleMatch::Addr } else { SenderRuleMatch::Domain },
        match_value: r.get(2)?,
        label_key: r.get(3)?,
        bucket: r.get::<_, Option<String>>(4)?.as_deref().and_then(Bucket::parse),
        source: if r.get::<_, String>(5)? == "user" { SenderRuleSource::User } else { SenderRuleSource::FeedbackX2 },
    })
}

/// All sender rules.
pub fn list_sender_rules(conn: &Connection) -> Result<Vec<SenderRule>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, match_kind, match_value, label_key, bucket, source FROM mail_sender_rules ORDER BY id",
    )?;
    let rows = stmt.query_map([], row_to_rule)?;
    rows.collect()
}

/// Insert or replace a sender rule (the unique key is `(match_kind, match_value)`).
pub fn upsert_sender_rule(
    conn: &Connection,
    match_kind: SenderRuleMatch,
    match_value: &str,
    label_key: &str,
    bucket: Option<Bucket>,
    source: SenderRuleSource,
    now: i64,
) -> Result<i64, rusqlite::Error> {
    conn.execute(
        "INSERT INTO mail_sender_rules (match_kind, match_value, label_key, bucket, source, created_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(match_kind, match_value) DO UPDATE SET label_key = excluded.label_key,
           bucket = excluded.bucket, source = excluded.source, created_at_ms = excluded.created_at_ms",
        params![
            match_kind.as_str(),
            match_value.trim().to_ascii_lowercase(),
            label_key,
            bucket.map(Bucket::as_str),
            source.as_str(),
            now
        ],
    )?;
    conn.query_row(
        "SELECT id FROM mail_sender_rules WHERE match_kind = ?1 AND match_value = ?2",
        params![match_kind.as_str(), match_value.trim().to_ascii_lowercase()],
        |r| r.get(0),
    )
}

/// Delete a sender rule. Returns whether a row was removed.
pub fn delete_sender_rule(conn: &Connection, id: i64) -> Result<bool, rusqlite::Error> {
    Ok(conn.execute("DELETE FROM mail_sender_rules WHERE id = ?1", params![id])? > 0)
}

/// A stored row's mutable verdict columns, as read before a Stage L write:
/// `(stage, latest_event_id, label_key, bucket, attention, confidence, reasons, bucket_lock)`.
type StoredVerdict = (String, i64, String, String, String, f64, String, Option<String>);
/// `(custom_id, thread_key, submitted_at_ms)` of one ledger row.
pub type BatchLedgerItem = (String, String, i64);
/// One provider batch and its ledger rows.
pub type BatchLedger = (String, Vec<BatchLedgerItem>);

const TRIAGE_SELECT: &str = "SELECT t.thread_key, t.latest_event_id, t.label_key, t.bucket, t.attention, t.confidence, t.stage,
        t.reasons, t.bucket_lock, t.archive_status, t.archived_at_ms, t.handled_at_ms, t.open_loop_id,
        t.synced_label_key, t.classified_at_ms, t.updated_at_ms,
        COALESCE((SELECT window_title FROM event_log WHERE id = t.latest_event_id), ''),
        COALESCE((SELECT from_addr FROM mail_messages WHERE thread_key = t.thread_key ORDER BY internal_date_ms DESC LIMIT 1), ''),
        (SELECT from_name FROM mail_messages WHERE thread_key = t.thread_key ORDER BY internal_date_ms DESC LIMIT 1),
        COALESCE((SELECT MAX(internal_date_ms) FROM mail_messages WHERE thread_key = t.thread_key), 0),
        (SELECT COUNT(*) FROM mail_messages WHERE thread_key = t.thread_key),
        COALESCE((SELECT substr(content, 1, 400) FROM event_log WHERE id = t.latest_event_id), '')
 FROM mail_triage t";

fn row_to_triage(r: &rusqlite::Row<'_>) -> rusqlite::Result<TriageRow> {
    let thread_key: String = r.get(0)?;
    let raw_snippet: String = r.get(21)?;
    let snippet: String = raw_snippet.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(160).collect();
    Ok(TriageRow {
        thread_id: thread_id_of(&thread_key).to_string(),
        thread_key,
        latest_event_id: r.get(1)?,
        label_key: r.get(2)?,
        bucket: Bucket::parse(&r.get::<_, String>(3)?).unwrap_or(Bucket::Important),
        attention: Attention::parse(&r.get::<_, String>(4)?).unwrap_or(Attention::None),
        confidence: r.get(5)?,
        stage: Stage::parse(&r.get::<_, String>(6)?).unwrap_or(Stage::Local),
        reasons: json_vec(&r.get::<_, String>(7)?),
        bucket_lock: r.get::<_, Option<String>>(8)?.as_deref().and_then(BucketLock::parse),
        archive_status: ArchiveStatus::parse(&r.get::<_, String>(9)?).unwrap_or(ArchiveStatus::None),
        archived_at_ms: r.get(10)?,
        handled_at_ms: r.get(11)?,
        open_loop_id: r.get(12)?,
        synced_label_key: r.get(13)?,
        classified_at_ms: r.get(14)?,
        updated_at_ms: r.get(15)?,
        subject: r.get(16)?,
        from_addr: r.get(17)?,
        from_name: r.get(18)?,
        last_message_ms: r.get(19)?,
        message_count: r.get(20)?,
        snippet,
    })
}

/// One triage row.
pub fn get_triage(conn: &Connection, thread_key: &str) -> Result<Option<TriageRow>, rusqlite::Error> {
    conn.query_row(&format!("{TRIAGE_SELECT} WHERE t.thread_key = ?1"), params![thread_key], row_to_triage)
        .optional()
}

/// List triage rows by filter, newest activity first.
pub fn list_triage(conn: &Connection, f: &TriageFilter) -> Result<Vec<TriageRow>, rusqlite::Error> {
    let mut clauses: Vec<String> = Vec::new();
    let mut args: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(b) = f.bucket {
        args.push(b.as_str().to_string().into());
        clauses.push(format!("t.bucket = ?{}", args.len()));
    }
    if let Some(a) = f.attention {
        args.push(a.as_str().to_string().into());
        clauses.push(format!("t.attention = ?{}", args.len()));
    }
    if let Some(l) = &f.label_key {
        args.push(l.clone().into());
        clauses.push(format!("t.label_key = ?{}", args.len()));
    }
    if let Some(s) = f.since_ms {
        args.push(s.into());
        clauses.push(format!("t.updated_at_ms >= ?{}", args.len()));
    }
    if !f.include_handled {
        clauses.push("t.handled_at_ms IS NULL".to_string());
    }
    if !f.include_archived {
        clauses.push("t.archive_status <> 'done'".to_string());
    }
    let where_sql = if clauses.is_empty() { String::new() } else { format!(" WHERE {}", clauses.join(" AND ")) };
    let limit = if f.limit == 0 { 200 } else { f.limit.min(1_000) };
    let sql = format!("{TRIAGE_SELECT}{where_sql} ORDER BY t.updated_at_ms DESC, t.thread_key LIMIT {limit}");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), row_to_triage)?;
    rows.collect()
}

/// Stage L write (FR-TG-02): insert, or update a non-user row. A `stage='user'` row keeps its
/// label / bucket / attention and only advances `latest_event_id`. A `stage='batch'` row is
/// re-evaluated only when the thread gained a message (a new snapshot event). Returns the stored
/// verdict (which may differ from `t` when the row was user-locked).
pub fn upsert_local(
    conn: &Connection,
    thread_key: &str,
    latest_event_id: i64,
    t: &Triage,
    now: i64,
) -> Result<Triage, rusqlite::Error> {
    let existing: Option<StoredVerdict> = conn
        .query_row(
            "SELECT stage, latest_event_id, label_key, bucket, attention, confidence, reasons, bucket_lock
             FROM mail_triage WHERE thread_key = ?1",
            params![thread_key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)),
        )
        .optional()?;
    match existing {
        Some((stage, _, label, bucket, attention, confidence, reasons, lock)) if stage == "user" => {
            conn.execute(
                "UPDATE mail_triage SET latest_event_id = ?2, updated_at_ms = ?3 WHERE thread_key = ?1",
                params![thread_key, latest_event_id, now],
            )?;
            Ok(Triage {
                label_key: label,
                bucket: Bucket::parse(&bucket).unwrap_or(Bucket::Important),
                attention: Attention::parse(&attention).unwrap_or(Attention::None),
                confidence,
                reasons: json_vec(&reasons),
                bucket_lock: lock.as_deref().and_then(BucketLock::parse),
            })
        }
        Some((stage, prev_event, label, bucket, attention, confidence, reasons, lock))
            if stage == "batch" && prev_event == latest_event_id =>
        {
            // Nothing new on the thread: keep the refined verdict, attention included. Only a
            // thread the review left at `none` follows the local rules (waiting_on_them ages in
            // with the clock). A newer message falls through and resets to the local verdict.
            let attention = if attention == "none" { t.attention.as_str().to_string() } else { attention };
            let t = Triage { attention: Attention::parse(&attention).unwrap_or(t.attention), ..t.clone() };
            conn.execute(
                "UPDATE mail_triage SET attention = ?2, updated_at_ms = ?3 WHERE thread_key = ?1",
                params![thread_key, t.attention.as_str(), now],
            )?;
            Ok(Triage {
                label_key: label,
                bucket: Bucket::parse(&bucket).unwrap_or(Bucket::Important),
                attention: t.attention,
                confidence,
                reasons: json_vec(&reasons),
                bucket_lock: lock.as_deref().and_then(BucketLock::parse),
            })
        }
        _ => {
            conn.execute(
                "INSERT INTO mail_triage
                   (thread_key, latest_event_id, label_key, bucket, attention, confidence, stage, reasons,
                    bucket_lock, classified_at_ms, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'local', ?7, ?8, ?9, ?9)
                 ON CONFLICT(thread_key) DO UPDATE SET
                   latest_event_id = excluded.latest_event_id, label_key = excluded.label_key,
                   bucket = excluded.bucket, attention = excluded.attention, confidence = excluded.confidence,
                   stage = 'local', reasons = excluded.reasons, bucket_lock = excluded.bucket_lock,
                   classified_at_ms = excluded.classified_at_ms, updated_at_ms = excluded.updated_at_ms",
                params![
                    thread_key,
                    latest_event_id,
                    t.label_key,
                    t.bucket.as_str(),
                    t.attention.as_str(),
                    t.confidence.clamp(0.0, 1.0),
                    vec_json(&t.reasons),
                    t.bucket_lock.map(BucketLock::as_str),
                    now,
                ],
            )?;
            Ok(t.clone())
        }
    }
}

/// A Stage B result for one thread.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BatchVerdict {
    pub thread_key: String,
    pub label_key: String,
    pub attention_hint: Option<Attention>,
    pub confidence: f64,
    pub reason_codes: Vec<String>,
}

/// Apply a Stage B verdict (FR-TG-18): never over a `user` row; never move a locked bucket; take
/// the attention hint only at confidence ≥ 0.7; unknown labels and reason codes are dropped.
/// Returns whether the row changed.
///
/// A verdict for a thread that gained a message after submission is stale and dropped below (the
/// row keeps its Stage L verdict and `stage = 'local'`, so [`pending_for_batch`] sends the new
/// content once the ledger row is marked applied). That resend is also what carries
/// the new message's commitments and open loops to Stage B — the consolidation lane does not
/// read Gmail — so a stale verdict must never set `stage = 'batch'`.
pub fn apply_batch(conn: &mut Connection, v: &BatchVerdict, now: i64) -> Result<bool, MemoryError> {
    let Some(label) = get_label(conn, &v.label_key)? else {
        return Ok(false);
    };
    if !label.enabled {
        return Ok(false);
    }
    let existing: Option<(String, String, Option<String>, String)> = conn
        .query_row(
            "SELECT stage, attention, bucket_lock, reasons FROM mail_triage WHERE thread_key = ?1",
            params![v.thread_key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((stage, attention, lock, reasons)) = existing else {
        return Ok(false);
    };
    if stage == "user" {
        return Ok(false);
    }
    // A verdict describes the thread as it was submitted. If a message arrived after that (the
    // user replied, say), it is stale: applying it would reopen a reply loop for an answered
    // thread. Drop it; once the ledger row is marked applied, the thread is submitted again.
    let submitted: Option<i64> = conn.query_row(
        "SELECT MAX(submitted_at_ms) FROM mail_triage_batch_items WHERE thread_key = ?1 AND applied_at_ms IS NULL",
        params![v.thread_key],
        |r| r.get(0),
    )?;
    let latest_ts: Option<i64> = conn
        .query_row(
            "SELECT e.ts FROM event_log e JOIN mail_triage t ON e.id = t.latest_event_id WHERE t.thread_key = ?1",
            params![v.thread_key],
            |r| r.get(0),
        )
        .optional()?;
    if let (Some(submitted), Some(latest_ts)) = (submitted, latest_ts) {
        if latest_ts > submitted {
            return Ok(false);
        }
    }
    let bucket = if lock.is_some() { Bucket::Important } else { label.default_bucket };
    let attention = match v.attention_hint {
        Some(a) if v.confidence >= 0.7 => a.as_str().to_string(),
        _ => attention,
    };
    let mut reasons = json_vec(&reasons);
    for code in &v.reason_codes {
        if local::ALL_REASON_CODES.contains(&code.as_str()) && !reasons.iter().any(|r| r == code) {
            reasons.push(code.clone());
        }
    }
    if !reasons.iter().any(|r| r == local::R_BATCH_REFINED) {
        reasons.push(local::R_BATCH_REFINED.to_string());
    }
    if label.kind == LabelKind::Custom && !reasons.iter().any(|r| r == local::R_CUSTOM_LABEL_MATCH) {
        reasons.push(local::R_CUSTOM_LABEL_MATCH.to_string());
    }
    conn.execute(
        "UPDATE mail_triage SET label_key = ?2, bucket = ?3, attention = ?4, confidence = ?5, stage = 'batch',
           reasons = ?6, updated_at_ms = ?7
         WHERE thread_key = ?1",
        params![
            v.thread_key,
            v.label_key,
            bucket.as_str(),
            attention,
            v.confidence.clamp(0.0, 1.0),
            vec_json(&reasons),
            now
        ],
    )?;
    // The reviewed verdict drives the thread's open loop too (same pin / user-close rules as the
    // local pass), and `stage = 'batch'` is what clears the loop's inferred mark.
    if let Some(row) = get_triage(conn, &v.thread_key)? {
        let evidence_ts: i64 = conn
            .query_row("SELECT ts FROM event_log WHERE id = ?1", [row.latest_event_id], |r| r.get(0))
            .optional()?
            .unwrap_or(now);
        let triage = Triage {
            label_key: row.label_key,
            bucket: row.bucket,
            attention: row.attention,
            confidence: row.confidence,
            reasons: row.reasons,
            bucket_lock: row.bucket_lock,
        };
        // ponytail: no counterparty here; the local pass that first opened the loop set it.
        sync_open_loop(conn, &v.thread_key, row.latest_event_id, &row.subject, &triage, None, evidence_ts, now)?;
    }
    Ok(true)
}

/// A Stage U change request. `None` fields are untouched.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UserChange {
    pub label_key: Option<String>,
    pub bucket: Option<Bucket>,
    pub handled: Option<bool>,
}

/// Errors from a user change.
#[derive(Debug, PartialEq, Eq)]
pub enum UserChangeError {
    UnknownThread,
    UnknownLabel,
}

/// Apply a user correction (FR-TG-19): the row becomes `stage='user'`, a feedback row is written
/// when the label or bucket changed, and the second same-direction correction for a sender
/// creates a `feedback_x2` sender rule. Returns the feedback row id when one was written.
pub fn apply_user_change(
    conn: &mut Connection,
    thread_key: &str,
    change: &UserChange,
    origin: FeedbackOrigin,
    feedback_event_id: Option<i64>,
    now: i64,
) -> Result<Result<Option<i64>, UserChangeError>, rusqlite::Error> {
    let Some(cur) = get_triage(conn, thread_key)? else {
        return Ok(Err(UserChangeError::UnknownThread));
    };
    if let Some(l) = &change.label_key {
        if get_label(conn, l)?.is_none() {
            return Ok(Err(UserChangeError::UnknownLabel));
        }
    }
    let new_label = change.label_key.clone().unwrap_or_else(|| cur.label_key.clone());
    let new_bucket = change.bucket.unwrap_or(cur.bucket);
    let relabelled = new_label != cur.label_key || new_bucket != cur.bucket;
    let tx = conn.savepoint()?;
    if relabelled {
        let mut reasons = cur.reasons.clone();
        if !reasons.iter().any(|r| r == local::R_USER_OVERRIDE) {
            reasons.push(local::R_USER_OVERRIDE.to_string());
        }
        let lock = if new_bucket == Bucket::Important { Some("user") } else { None };
        tx.execute(
            "UPDATE mail_triage SET label_key = ?2, bucket = ?3, stage = 'user', confidence = 1.0,
               reasons = ?4, bucket_lock = ?5, updated_at_ms = ?6
             WHERE thread_key = ?1",
            params![thread_key, new_label, new_bucket.as_str(), vec_json(&reasons), lock, now],
        )?;
    }
    if let Some(h) = change.handled {
        tx.execute(
            "UPDATE mail_triage SET handled_at_ms = ?2, updated_at_ms = ?3 WHERE thread_key = ?1",
            params![thread_key, if h { Some(now) } else { None }, now],
        )?;
    }
    let mut feedback_id = None;
    if relabelled {
        tx.execute(
            "INSERT INTO mail_triage_feedback
               (ts_ms, thread_key, from_label_key, to_label_key, from_bucket, to_bucket, origin, feedback_event_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                now,
                thread_key,
                cur.label_key,
                new_label,
                cur.bucket.as_str(),
                new_bucket.as_str(),
                origin.as_str(),
                feedback_event_id
            ],
        )?;
        feedback_id = Some(tx.last_insert_rowid());
        // x2 rule: two same-direction corrections for the same **counterpart** address. Keyed on
        // the latest message the user did not send — on a thread they answered last the newest
        // sender is the user, and a rule matching your own address relabels your whole mailbox.
        let counterpart = counterpart_addr(&tx, thread_key)?.unwrap_or_default();
        if !counterpart.is_empty() {
            let same_addr: i64 = tx.query_row(
                "SELECT COUNT(DISTINCT f.thread_key) FROM mail_triage_feedback f
                 WHERE f.to_label_key = ?1 AND f.to_bucket = ?2 AND f.thread_key IN (
                   SELECT DISTINCT thread_key FROM mail_messages WHERE lower(from_addr) = ?3 AND is_from_me = 0)",
                params![new_label, new_bucket.as_str(), counterpart.to_ascii_lowercase()],
                |r| r.get(0),
            )?;
            if same_addr >= 2 {
                tx.execute(
                    "INSERT INTO mail_sender_rules (match_kind, match_value, label_key, bucket, source, created_at_ms)
                     VALUES ('addr', ?1, ?2, ?3, 'feedback_x2', ?4)
                     ON CONFLICT(match_kind, match_value) DO UPDATE SET label_key = excluded.label_key,
                       bucket = excluded.bucket, source = excluded.source, created_at_ms = excluded.created_at_ms",
                    params![counterpart.to_ascii_lowercase(), new_label, new_bucket.as_str(), now],
                )?;
            }
        }
    }
    tx.commit()?;
    Ok(Ok(feedback_id))
}

/// Who made the thread's latest relabel, if anyone did: `Some(FeedbackOrigin::Api)` when an
/// MCP / REST / CLI `mail.triage.set` put it under its current label (issue #779). A label's
/// standing Auto Archive approval covers threads SHOGUN or the user filed there, not threads an
/// API caller moved there to skip the per-thread confirm.
pub fn last_relabel_origin(conn: &Connection, thread_key: &str) -> Result<Option<FeedbackOrigin>, rusqlite::Error> {
    let origin: Option<String> = conn
        .query_row(
            "SELECT origin FROM mail_triage_feedback WHERE thread_key = ?1 ORDER BY ts_ms DESC, rowid DESC LIMIT 1",
            params![thread_key],
            |r| r.get(0),
        )
        .optional()?;
    Ok(origin.map(|o| if o == FeedbackOrigin::Ui.as_str() { FeedbackOrigin::Ui } else { FeedbackOrigin::Api }))
}

/// Undistilled feedback rows, oldest first (for Dream Cycle lesson distillation).
pub fn undistilled_feedback(conn: &Connection, limit: usize) -> Result<Vec<FeedbackRow>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, ts_ms, thread_key, from_label_key, to_label_key, from_bucket, to_bucket, origin, feedback_event_id, distilled
         FROM mail_triage_feedback WHERE distilled = 0 ORDER BY ts_ms ASC LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit as i64], |r| {
        Ok(FeedbackRow {
            id: r.get(0)?,
            ts_ms: r.get(1)?,
            thread_key: r.get(2)?,
            from_label_key: r.get(3)?,
            to_label_key: r.get(4)?,
            from_bucket: Bucket::parse(&r.get::<_, String>(5)?).unwrap_or(Bucket::Important),
            to_bucket: Bucket::parse(&r.get::<_, String>(6)?).unwrap_or(Bucket::Important),
            origin: if r.get::<_, String>(7)? == "api" { FeedbackOrigin::Api } else { FeedbackOrigin::Ui },
            feedback_event_id: r.get(8)?,
            distilled: r.get::<_, i64>(9)? != 0,
        })
    })?;
    rows.collect()
}

/// Mark feedback rows distilled.
pub fn mark_feedback_distilled(conn: &Connection, ids: &[i64]) -> Result<usize, rusqlite::Error> {
    let mut n = 0;
    for id in ids {
        n += conn.execute("UPDATE mail_triage_feedback SET distilled = 1 WHERE id = ?1", params![id])?;
    }
    Ok(n)
}

/// The sender domain of a thread's latest message **from the counterpart** (for distillation),
/// lowercase. Messages the user sent are skipped: on a thread the user answered last, the latest
/// message's domain is the user's own, and a lesson keyed on it would describe the user.
pub fn thread_sender_domain(conn: &Connection, thread_key: &str) -> Result<Option<String>, rusqlite::Error> {
    conn.query_row(
        "SELECT from_domain FROM mail_messages WHERE thread_key = ?1 AND is_from_me = 0
         ORDER BY internal_date_ms DESC LIMIT 1",
        params![thread_key],
        |r| r.get(0),
    )
    .optional()
}

/// The address of the latest message on a thread that the user did **not** send — the party a
/// learned sender rule is about. `None` when every stored message is the user's own.
pub fn counterpart_addr(conn: &Connection, thread_key: &str) -> Result<Option<String>, rusqlite::Error> {
    conn.query_row(
        "SELECT from_addr FROM mail_messages WHERE thread_key = ?1 AND is_from_me = 0
         ORDER BY internal_date_ms DESC LIMIT 1",
        params![thread_key],
        |r| r.get(0),
    )
    .optional()
}

// open loops (FR-TG-15)

/// Keep the `open_loops` row for a thread in step with its attention: `needs_reply` /
/// `waiting_on_them` at confidence ≥ 0.5 upserts an open loop (provenance = the latest event);
/// anything else closes the loop the thread owns. Returns the open loop id when one is open.
#[allow(clippy::too_many_arguments)]
pub fn sync_open_loop(
    conn: &mut Connection,
    thread_key: &str,
    latest_event_id: i64,
    subject: &str,
    triage: &Triage,
    counterparty_id: Option<i64>,
    evidence_ts: i64,
    now: i64,
) -> Result<Option<i64>, MemoryError> {
    let existing: Option<i64> = conn
        .query_row("SELECT open_loop_id FROM mail_triage WHERE thread_key = ?1", params![thread_key], |r| r.get(0))
        .optional()?
        .flatten();
    let wanted = match triage.attention {
        Attention::NeedsReply if triage.confidence >= 0.5 => Some(OpenLoopKind::ReplyNeeded),
        Attention::WaitingOnThem if triage.confidence >= 0.5 => Some(OpenLoopKind::WaitingOnThem),
        _ => None,
    };
    match (existing, wanted) {
        (Some(id), Some(kind)) => {
            // Still open: refresh kind / evidence, keep the row. A loop the user closed stays
            // closed until the thread has evidence newer than the one they closed over.
            conn.execute(
                // A loop the user edited is pinned (issue #290): the thread's new evidence does
                // not rewrite its kind, status or confidence.
                "UPDATE open_loops SET kind = ?2, status = 'open', last_evidence_at = ?3, updated_at = ?4,
                   confidence = max(confidence, ?5)
                 WHERE id = ?1 AND user_pinned = 0
                   AND (status = 'open' OR ?3 > COALESCE(last_evidence_at, 0))",
                params![id, kind.as_str(), evidence_ts, now, triage.confidence.min(0.9)],
            )?;
            Ok(Some(id))
        }
        (Some(id), None) => {
            // Closing is automatic here, so a loop the user edited (issue #290) is left exactly as
            // they set it — and stays this thread's loop, so a later reply-needed does not open
            // a twin beside it.
            let pinned = conn
                .query_row("SELECT user_pinned FROM open_loops WHERE id = ?1", [id], |r| {
                    r.get::<_, i64>(0)
                })
                .optional()?
                .is_some_and(|flag| flag != 0);
            if pinned {
                return Ok(Some(id));
            }
            crate::state::close_open_loop(conn, id, now)?;
            conn.execute(
                "UPDATE mail_triage SET open_loop_id = NULL WHERE thread_key = ?1",
                params![thread_key],
            )?;
            Ok(None)
        }
        (None, Some(kind)) => {
            let desc = match kind {
                OpenLoopKind::ReplyNeeded => format!("Reply: {}", subject.trim()),
                _ => format!("Waiting on reply: {}", subject.trim()),
            };
            let desc: String = desc.chars().take(200).collect();
            let id = crate::state::insert_open_loop(
                conn,
                &NewOpenLoop {
                    kind,
                    description: &desc,
                    counterparty_id: counterparty_id.filter(|id| *id > 0),
                    project_id: None,
                    opened_at: evidence_ts,
                    confidence: triage.confidence.min(0.9),
                    now,
                },
                &[Provenance::new(latest_event_id)],
            )?;
            conn.execute(
                "UPDATE mail_triage SET open_loop_id = ?2 WHERE thread_key = ?1",
                params![thread_key, id],
            )?;
            Ok(Some(id))
        }
        (None, None) => Ok(None),
    }
}

// Stage B inputs (FR-TG-17)

/// The prefix every triage-derived lesson instruction carries, so the Batch prompt can pick
/// them out of `lessons` without a new column.
pub const LESSON_PREFIX: &str = "Mail from ";

/// The per-thread prompt input for Stage B: the processing chunk (AR-12). Sender **display name
/// and domain only** (no local part), subject, a bounded body excerpt, categories, the Stage L
/// verdict.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BatchThreadInput {
    pub thread_key: String,
    /// The `event_log` row the excerpt was cut from (`mail_triage.latest_event_id`, the thread's
    /// latest snapshot). A commitment or open loop Stage B anchors on a line of the excerpt is
    /// attributed to this event (zero-base plan L0(b)): the quote is a substring of its content.
    #[serde(default)]
    pub event_id: i64,
    pub subject: String,
    pub body_excerpt: String,
    pub sender_name: Option<String>,
    pub sender_domain: String,
    pub to_me: bool,
    pub gmail_categories: Vec<String>,
    pub local_label_key: String,
    pub local_reasons: Vec<String>,
}

/// Body characters Stage B receives (FR-TG-17).
pub const BATCH_BODY_CHARS: usize = 1_200;

/// What a thread's event row joins its messages with, oldest first (`gmail_shape::thread_body`).
pub const THREAD_MESSAGE_SEPARATOR: &str = "\n\n---\n\n";

/// The ≤ `max_chars` of a thread body Stage B reads, **latest message first**. The event row for
/// a thread is every message joined oldest-first, and a new message is what makes the thread
/// pending again — so an excerpt cut from the head of the body would show the model the oldest
/// message every time and never the one that triggered the send (for any thread longer than the
/// budget, the promise in the reply is never seen). This takes the latest message whole (its head
/// if it alone exceeds the budget), then the heads of the earlier messages newest-first, whole
/// lines as far as they fit, and joins the kept parts in thread order with the same separator.
/// Every line of the result is a substring of `body`, so an anchor resolved against it is still
/// verified against the event's stored content.
pub fn stage_b_excerpt(body: &str, max_chars: usize) -> String {
    let mut messages: Vec<&str> = body.split(THREAD_MESSAGE_SEPARATOR).collect();
    let Some(latest) = messages.pop() else {
        return String::new();
    };
    let latest_chars = latest.chars().count();
    if latest_chars >= max_chars {
        return latest.chars().take(max_chars).collect();
    }
    let separator_chars = THREAD_MESSAGE_SEPARATOR.chars().count();
    let mut budget = max_chars - latest_chars;
    // Heads of the earlier messages, newest first.
    let mut kept: Vec<&str> = Vec::new();
    for earlier in messages.iter().rev() {
        if budget <= separator_chars {
            break;
        }
        let head = head_within(earlier, budget - separator_chars);
        if head.is_empty() {
            break;
        }
        budget -= separator_chars + head.chars().count();
        kept.push(head);
    }
    kept.reverse();
    kept.push(latest);
    kept.join(THREAD_MESSAGE_SEPARATOR)
}

/// The longest run of whole lines at the start of `text` within `max_chars`; when not even the
/// first line fits, its first `max_chars` characters (a partial line is still a substring of the
/// event, which is all a quote has to be). Trailing whitespace is dropped.
fn head_within(text: &str, max_chars: usize) -> &str {
    let text = text.trim_end();
    let mut end = 0usize;
    let mut chars = 0usize;
    for line in text.split_inclusive('\n') {
        let n = line.chars().count();
        if chars + n > max_chars {
            break;
        }
        chars += n;
        end += line.len();
    }
    if end == 0 {
        end = text.char_indices().nth(max_chars).map_or(text.len(), |(i, _)| i);
    }
    text[..end].trim_end()
}

/// Assemble the Stage B input for a thread from stored rows. `None` when the thread has no
/// triage row or no messages.
pub fn batch_thread_input(conn: &Connection, thread_key: &str) -> Result<Option<BatchThreadInput>, rusqlite::Error> {
    let Some(row) = get_triage(conn, thread_key)? else {
        return Ok(None);
    };
    let msgs = messages_for_thread(conn, thread_key)?;
    let Some(last) = msgs.last() else {
        return Ok(None);
    };
    let mine = identities(conn)?;
    let to_me = last.to_addrs.iter().any(|a| mine.iter().any(|m| m.eq_ignore_ascii_case(a)));
    let body: String = conn
        .query_row("SELECT content FROM event_log WHERE id = ?1", params![row.latest_event_id], |r| r.get::<_, String>(0))
        .optional()?
        .unwrap_or_default();
    let excerpt = stage_b_excerpt(&body, BATCH_BODY_CHARS);
    Ok(Some(BatchThreadInput {
        thread_key: thread_key.to_string(),
        event_id: row.latest_event_id,
        subject: row.subject,
        body_excerpt: excerpt,
        sender_name: last.from_name.clone(),
        sender_domain: last.from_domain.clone(),
        to_me,
        gmail_categories: last
            .gmail_label_ids
            .iter()
            .filter(|l| l.starts_with("CATEGORY_"))
            .cloned()
            .collect(),
        local_label_key: row.label_key,
        local_reasons: row.reasons,
    }))
}

/// Reset recent non-user rows to Stage L below the Batch bar (the "Re-run" action). Rows in an
/// unapplied batch are left alone (they are already in flight).
pub fn reset_for_rerun(conn: &Connection, since_ms: i64, now: i64) -> Result<usize, rusqlite::Error> {
    conn.execute(
        "UPDATE mail_triage SET stage = 'local', confidence = min(confidence, 0.5), updated_at_ms = ?2
         WHERE stage <> 'user' AND classified_at_ms >= ?1
           AND NOT EXISTS (SELECT 1 FROM mail_triage_batch_items b WHERE b.thread_key = mail_triage.thread_key AND b.applied_at_ms IS NULL)",
        params![since_ms, now],
    )
}

/// Active triage lessons (instructions starting with [`LESSON_PREFIX`]), strongest first.
pub fn triage_lessons(conn: &Connection, top_k: usize) -> Result<Vec<String>, rusqlite::Error> {
    use crate::lessons::{active_lessons, LessonScope, ScopeFilter};
    let filters = [
        ScopeFilter { scope: LessonScope::Person, scope_ref: None },
        ScopeFilter { scope: LessonScope::Global, scope_ref: None },
    ];
    let all = active_lessons(conn, &filters, 200)?;
    Ok(all
        .into_iter()
        .filter(|l| l.instruction.starts_with(LESSON_PREFIX))
        .map(|l| l.instruction)
        .take(top_k)
        .collect())
}

// Batch ledger (FR-TG-03/16)

/// The built-in labels that mean a human wrote to the user: where commitments and open loops
/// live. Stage B reads every thread Stage L files under one of these, however sure Stage L was
/// (FR-TG-03 as amended by the zero-base plan L0(b)), because Stage B is the only model pass a
/// Gmail thread gets — the consolidation lane does not read `gmail` rows while triage is on. The
/// automated categories (`notification`, `marketing`, `news`, `social`, `finance`, `calendar`,
/// `pitch`) are sent only when Stage L is unsure.
pub const HUMAN_CORRESPONDENCE_LABELS: &[&str] = &["people", "team"];

/// Threads awaiting Stage B (FR-TG-03): `stage='local'` rows that are **either** below the
/// confidence bar (every local row when a custom label is enabled) **or** filed as human
/// correspondence ([`HUMAN_CORRESPONDENCE_LABELS`]) at any confidence; not already in an
/// unapplied batch. Ordered so that, when the daily cap binds, threads addressed to the user
/// (one of `mail_identities` among the recipients) go first, and **oldest pending first** within
/// each group (`classified_at_ms`, the time of the Stage L verdict the row carries). Newest-first
/// starved a backlog for good: on an inbox whose eligible inflow exceeds the cap, yesterday's
/// leftovers sorted behind every thread of today and never went. First-in-first-out clears them
/// at the cap's pace; the inbox itself is never empty meanwhile because every thread already
/// carries its Stage L verdict.
pub fn pending_for_batch(conn: &Connection, limit: usize) -> Result<Vec<(String, i64)>, rusqlite::Error> {
    let custom = any_custom_label_enabled(conn)?;
    let bar = if custom { 1.01 } else { 0.9 };
    let human = HUMAN_CORRESPONDENCE_LABELS
        .iter()
        .map(|k| format!("'{}'", k.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(",");
    let mut stmt = conn.prepare(&format!(
        "SELECT t.thread_key, t.latest_event_id FROM mail_triage t
         WHERE t.stage = 'local' AND (t.confidence < ?1 OR t.label_key IN ({human}))
           AND NOT EXISTS (SELECT 1 FROM mail_triage_batch_items b WHERE b.thread_key = t.thread_key AND b.applied_at_ms IS NULL)
         ORDER BY EXISTS (SELECT 1 FROM mail_messages m JOIN mail_identities i
                          WHERE m.thread_key = t.thread_key AND instr(lower(m.to_addrs), i.addr) > 0) DESC,
                  t.classified_at_ms ASC
         LIMIT ?2"
    ))?;
    let rows = stmt.query_map(params![bar, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
    rows.collect()
}

/// How many threads were submitted since `since_ms` (the daily cap, FR-TG-03).
pub fn submitted_since(conn: &Connection, since_ms: i64) -> Result<i64, rusqlite::Error> {
    conn.query_row(
        "SELECT COUNT(*) FROM mail_triage_batch_items WHERE submitted_at_ms >= ?1",
        params![since_ms],
        |r| r.get(0),
    )
}

/// Record a submitted batch.
pub fn record_batch_items(
    conn: &Connection,
    batch_id: &str,
    items: &[(String, String)],
    now: i64,
) -> Result<(), rusqlite::Error> {
    for (custom_id, thread_key) in items {
        conn.execute(
            "INSERT OR REPLACE INTO mail_triage_batch_items (batch_id, custom_id, thread_key, submitted_at_ms)
             VALUES (?1, ?2, ?3, ?4)",
            params![batch_id, custom_id, thread_key, now],
        )?;
    }
    Ok(())
}

/// Batches with results not yet applied, with their `(custom_id, thread_key, submitted_at_ms)`.
pub fn unapplied_batches(conn: &Connection) -> Result<Vec<BatchLedger>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT batch_id, custom_id, thread_key, submitted_at_ms FROM mail_triage_batch_items
         WHERE applied_at_ms IS NULL ORDER BY submitted_at_ms ASC, batch_id, custom_id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?))
    })?;
    let mut out: Vec<BatchLedger> = Vec::new();
    for row in rows {
        let (b, c, t, s) = row?;
        match out.iter_mut().find(|(id, _)| *id == b) {
            Some((_, items)) => items.push((c, t, s)),
            None => out.push((b, vec![(c, t, s)])),
        }
    }
    Ok(out)
}

/// Mark a whole batch applied (or abandoned — either way it stops blocking resubmission).
pub fn mark_batch_applied(conn: &Connection, batch_id: &str, now: i64) -> Result<usize, rusqlite::Error> {
    conn.execute(
        "UPDATE mail_triage_batch_items SET applied_at_ms = ?2 WHERE batch_id = ?1 AND applied_at_ms IS NULL",
        params![batch_id, now],
    )
}

// archive (FR-TG-22)

/// Threads eligible for Auto Archive right now: label has `auto_archive`, bucket Other, no lock,
/// confidence ≥ 0.8, classified ≥ `grace_ms` ago, not yet archived (a `failed` first attempt
/// counts as eligible — one retry, FR-TG-22), latest message still carries INBOX. Ordered
/// oldest-classified first, capped. Explicit per-thread requests are [`explicit_requests`] —
/// they are the user's own one-tap, so none of the automatic safety conditions apply to them.
pub fn archive_candidates(conn: &Connection, now: i64, grace_ms: i64, limit: usize) -> Result<Vec<TriageRow>, rusqlite::Error> {
    let sql = format!(
        "{TRIAGE_SELECT}
         JOIN mail_labels l ON l.key = t.label_key
         WHERE l.auto_archive = 1 AND l.enabled = 1 AND t.bucket = 'other' AND t.bucket_lock IS NULL
           AND t.confidence >= 0.8 AND t.classified_at_ms <= ?1
           AND t.archive_status IN ('none', 'failed') AND t.handled_at_ms IS NULL
           AND EXISTS (SELECT 1 FROM mail_messages m WHERE m.thread_key = t.thread_key
                       AND m.internal_date_ms = (SELECT MAX(internal_date_ms) FROM mail_messages WHERE thread_key = t.thread_key)
                       AND instr(m.gmail_label_ids, '\"INBOX\"') > 0)
         ORDER BY t.classified_at_ms ASC LIMIT ?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![now - grace_ms, limit as i64], row_to_triage)?;
    rows.collect()
}

/// Threads whose label has `sync_to_gmail` and whose Gmail label is not yet in step.
pub fn label_sync_candidates(conn: &Connection, limit: usize) -> Result<Vec<TriageRow>, rusqlite::Error> {
    let sql = format!(
        "{TRIAGE_SELECT}
         JOIN mail_labels l ON l.key = t.label_key
         WHERE l.sync_to_gmail = 1 AND l.enabled = 1 AND (t.synced_label_key IS NULL OR t.synced_label_key <> t.label_key)
           AND COALESCE(t.synced_label_key, '') <> ('undone:' || t.label_key)
         ORDER BY t.updated_at_ms DESC LIMIT ?1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![limit as i64], row_to_triage)?;
    rows.collect()
}

/// Explicit per-thread archive / unarchive requests awaiting execution (FR-TG-30): an API
/// caller's request, recorded once the user confirmed it in the app or under the label's
/// standing approval (issue #779), and the desktop tick performs it. Returns `(row, unarchive)`
/// oldest first. The automatic safety conditions (bucket, lock, confidence, grace period)
/// deliberately do **not** apply: the request names this one thread and carries the user's
/// one-tap L2 grant for it (or the standing approval for its label).
pub fn explicit_requests(conn: &Connection, limit: usize) -> Result<Vec<(TriageRow, bool)>, rusqlite::Error> {
    let sql = format!(
        "{TRIAGE_SELECT} WHERE t.archive_status IN ('pending', 'unarchive_pending')
         ORDER BY t.updated_at_ms ASC LIMIT ?1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![limit as i64], row_to_triage)?;
    rows.map(|r| r.map(|row| {
        let unarchive = row.archive_status == ArchiveStatus::UnarchivePending;
        (row, unarchive)
    }))
    .collect()
}

/// Marker stored in `synced_label_key` when the user undoes a label sync: the label was removed
/// from Gmail on purpose, so the next tick must not put it straight back (an undo the job
/// immediately reverses is not an undo). A later relabel changes the suffix and re-qualifies the
/// thread naturally.
pub const LABEL_SYNC_UNDONE_PREFIX: &str = "undone:";

/// The `synced_label_key` value that opts a thread out of re-syncing `label_key`.
pub fn label_sync_undone_marker(label_key: &str) -> String {
    format!("{LABEL_SYNC_UNDONE_PREFIX}{label_key}")
}

/// Record the label that is now applied on Gmail for a thread.
pub fn set_synced_label(conn: &Connection, thread_key: &str, label_key: Option<&str>, now: i64) -> Result<(), rusqlite::Error> {
    conn.execute(
        "UPDATE mail_triage SET synced_label_key = ?2, updated_at_ms = ?3 WHERE thread_key = ?1",
        params![thread_key, label_key, now],
    )?;
    Ok(())
}

/// Set a thread's archive status. `Done` stamps `archived_at_ms`; `Undone` and `None` clear it.
pub fn set_archive_status(conn: &Connection, thread_key: &str, status: ArchiveStatus, now: i64) -> Result<bool, rusqlite::Error> {
    // `Done` stamps the archive time; `Undone` / `None` clear it; `Pending` / `Failed` leave the
    // previous stamp alone (a retry after a failed undo must not lose when it was archived).
    let sql = match status {
        // A pending request (either direction) and a failed attempt leave the previous stamp
        // alone: the thread's Gmail state has not changed yet.
        ArchiveStatus::Pending | ArchiveStatus::UnarchivePending | ArchiveStatus::Failed => {
            "UPDATE mail_triage SET archive_status = ?2, updated_at_ms = ?3 WHERE thread_key = ?1"
        }
        ArchiveStatus::Done => {
            "UPDATE mail_triage SET archive_status = ?2, archived_at_ms = ?3, updated_at_ms = ?3 WHERE thread_key = ?1"
        }
        ArchiveStatus::None | ArchiveStatus::Undone => {
            "UPDATE mail_triage SET archive_status = ?2, archived_at_ms = NULL, updated_at_ms = ?3 WHERE thread_key = ?1"
        }
    };
    Ok(conn.execute(sql, params![thread_key, status.as_str(), now])? > 0)
}

/// Reflect a Gmail-side INBOX state on the latest message (after an archive / unarchive write
/// succeeds, so the candidate query stops re-selecting the thread before the next sync).
pub fn set_inbox_label_locally(conn: &Connection, thread_key: &str, in_inbox: bool) -> Result<(), rusqlite::Error> {
    let latest: Option<(String, String)> = conn
        .query_row(
            "SELECT message_id, gmail_label_ids FROM mail_messages WHERE thread_key = ?1 ORDER BY internal_date_ms DESC LIMIT 1",
            params![thread_key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((message_id, labels)) = latest else {
        return Ok(());
    };
    let mut labels = json_vec(&labels);
    labels.retain(|l| l != "INBOX");
    if in_inbox {
        labels.push("INBOX".to_string());
    }
    conn.execute(
        "UPDATE mail_messages SET gmail_label_ids = ?2 WHERE message_id = ?1",
        params![message_id, vec_json(&labels)],
    )?;
    Ok(())
}

// write log (FR-AG-18 / FR-TG-21)

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteApproval {
    /// A one-tap L2 confirmation for this very action.
    L2,
    /// The per-label standing approval (FR-TG-21).
    L2Standing,
}

impl WriteApproval {
    pub fn as_str(self) -> &'static str {
        match self {
            WriteApproval::L2 => "l2",
            WriteApproval::L2Standing => "l2_standing",
        }
    }
}

/// One execution-history row.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WriteLogRow {
    pub id: i64,
    pub ts_ms: i64,
    pub thread_key: String,
    pub action_kind: String,
    pub add_label_ids: Vec<String>,
    pub remove_label_ids: Vec<String>,
    pub approval: WriteApproval,
    pub status: String,
    pub error_class: Option<String>,
    pub undone_at_ms: Option<i64>,
    /// Display: the thread's subject (from the triage row), for the history list.
    pub subject: String,
}

/// Record a write-back attempt. Returns the row id.
#[allow(clippy::too_many_arguments)]
pub fn insert_write_log(
    conn: &Connection,
    thread_key: &str,
    action_kind: &str,
    add: &[String],
    remove: &[String],
    approval: WriteApproval,
    ok: bool,
    error_class: Option<&str>,
    now: i64,
) -> Result<i64, rusqlite::Error> {
    conn.execute(
        "INSERT INTO mail_write_log (ts_ms, thread_key, action_kind, add_label_ids, remove_label_ids, approval, status, error_class)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            now,
            thread_key,
            action_kind,
            vec_json(add),
            vec_json(remove),
            approval.as_str(),
            if ok { "ok" } else { "failed" },
            error_class
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

const WRITE_LOG_SELECT: &str =
    "SELECT w.id, w.ts_ms, w.thread_key, w.action_kind, w.add_label_ids, w.remove_label_ids, w.approval, w.status,
            w.error_class, w.undone_at_ms,
            COALESCE((SELECT window_title FROM event_log e JOIN mail_triage t ON e.id = t.latest_event_id WHERE t.thread_key = w.thread_key), '')
     FROM mail_write_log w";

fn row_to_write_log(r: &rusqlite::Row<'_>) -> rusqlite::Result<WriteLogRow> {
    Ok(WriteLogRow {
        id: r.get(0)?,
        ts_ms: r.get(1)?,
        thread_key: r.get(2)?,
        action_kind: r.get(3)?,
        add_label_ids: json_vec(&r.get::<_, String>(4)?),
        remove_label_ids: json_vec(&r.get::<_, String>(5)?),
        approval: if r.get::<_, String>(6)? == "l2_standing" { WriteApproval::L2Standing } else { WriteApproval::L2 },
        status: r.get(7)?,
        error_class: r.get(8)?,
        undone_at_ms: r.get(9)?,
        subject: r.get(10)?,
    })
}

/// Recent write-back history, newest first.
pub fn list_write_log(conn: &Connection, limit: usize) -> Result<Vec<WriteLogRow>, rusqlite::Error> {
    let mut stmt = conn.prepare(&format!("{WRITE_LOG_SELECT} ORDER BY w.ts_ms DESC, w.id DESC LIMIT ?1"))?;
    let rows = stmt.query_map(params![limit as i64], row_to_write_log)?;
    rows.collect()
}

/// One write-log row.
pub fn get_write_log(conn: &Connection, id: i64) -> Result<Option<WriteLogRow>, rusqlite::Error> {
    // Addressed by id, never by scanning a recent window: the undo window is seven days, and at
    // auto-archive volumes a still-undoable row falls out of any fixed row count long before that.
    conn.query_row(&format!("{WRITE_LOG_SELECT} WHERE w.id = ?1"), params![id], row_to_write_log)
        .optional()
}

/// Mark a write undone.
pub fn mark_write_undone(conn: &Connection, id: i64, now: i64) -> Result<bool, rusqlite::Error> {
    Ok(conn.execute(
        "UPDATE mail_write_log SET status = 'undone', undone_at_ms = ?2 WHERE id = ?1 AND status = 'ok'",
        params![id, now],
    )? > 0)
}

/// The sync cursor for a service.
pub fn get_cursor(conn: &Connection, service: &str) -> Result<Option<SyncCursor>, rusqlite::Error> {
    conn.query_row(
        "SELECT watermark_ms, page_token FROM connector_sync_cursors WHERE service = ?1",
        params![service],
        |r| Ok(SyncCursor { watermark_ms: r.get(0)?, page_token: r.get(1)? }),
    )
    .optional()
}

/// Store the sync cursor for a service.
pub fn set_cursor(conn: &Connection, service: &str, cursor: &SyncCursor, now: i64) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO connector_sync_cursors (service, watermark_ms, page_token, updated_at_ms) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(service) DO UPDATE SET watermark_ms = excluded.watermark_ms, page_token = excluded.page_token,
           updated_at_ms = excluded.updated_at_ms",
        params![service, cursor.watermark_ms, cursor.page_token, now],
    )?;
    Ok(())
}

/// Is triage running at all (issue #319, the V34 master switch)?
///
/// Reads ON when the row is missing rather than OFF. The row is seeded by the migration, so a
/// missing one means something went wrong with the settings table — and failing that read closed
/// would silently stop classifying mail for a user who never asked for it, which is the worse of
/// the two wrong answers. Per-label `enabled` still applies underneath.
pub fn triage_enabled(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT enabled FROM mail_triage_settings WHERE id = 1",
        [],
        |r| r.get::<_, i64>(0),
    )
    .map(|v| v != 0)
    .unwrap_or(true)
}

/// Turn triage on or off. Idempotent; upserts the single row so a lost seed self-heals.
pub fn set_triage_enabled(conn: &Connection, enabled: bool) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO mail_triage_settings (id, enabled) VALUES (1, ?1)
         ON CONFLICT(id) DO UPDATE SET enabled = excluded.enabled",
        params![i64::from(enabled)],
    )?;
    Ok(())
}

/// How many corrections the user made since `since_ms` (issue #319 FR-TG-24's analytics figure).
///
/// A count of rows, never their content: which label a thread moved between is the classifier's
/// business and the user's, not a metric's. Separate from [`counts`] because it answers a
/// different question — that one describes the mail, this one describes how often the classifier
/// was wrong enough to fix.
pub fn feedback_count_since(conn: &Connection, since_ms: i64) -> Result<i64, rusqlite::Error> {
    conn.query_row(
        "SELECT COUNT(*) FROM mail_triage_feedback WHERE ts_ms >= ?1",
        params![since_ms],
        |r| r.get(0),
    )
}

/// Counts over rows classified since `since_ms` plus the live needs-reply backlog.
pub fn counts(conn: &Connection, since_ms: i64) -> Result<TriageCounts, rusqlite::Error> {
    let triaged: i64 = conn.query_row(
        "SELECT COUNT(*) FROM mail_triage WHERE classified_at_ms >= ?1",
        params![since_ms],
        |r| r.get(0),
    )?;
    let other: i64 = conn.query_row(
        "SELECT COUNT(*) FROM mail_triage WHERE classified_at_ms >= ?1 AND bucket = 'other'",
        params![since_ms],
        |r| r.get(0),
    )?;
    let auto_archived: i64 = conn.query_row(
        "SELECT COUNT(*) FROM mail_triage WHERE archived_at_ms >= ?1 AND archive_status = 'done'",
        params![since_ms],
        |r| r.get(0),
    )?;
    let needs_reply_open: i64 = needs_reply_open_count(conn)?;
    let mut stmt = conn.prepare(
        "SELECT label_key, COUNT(*) FROM mail_triage WHERE classified_at_ms >= ?1 AND bucket = 'other'
         GROUP BY label_key ORDER BY COUNT(*) DESC, label_key",
    )?;
    let other_by_label = stmt
        .query_map(params![since_ms], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(TriageCounts { triaged, other, auto_archived, needs_reply_open, other_by_label })
}

/// Unhandled `needs_reply` threads (the Notch counter, FR-TG-28).
pub fn needs_reply_open_count(conn: &Connection) -> Result<i64, rusqlite::Error> {
    conn.query_row(
        "SELECT COUNT(*) FROM mail_triage WHERE attention = 'needs_reply' AND handled_at_ms IS NULL AND archive_status <> 'done'",
        [],
        |r| r.get(0),
    )
}

/// Find the triage row whose subject matches an on-screen title (the Fusion hook, FR-TG-28).
/// Archive is a one-tap mutation, so the target must be unambiguous: the title is the subject
/// (exact, after Re:/Fwd:), or it contains the subject (a browser title such as
/// "Subject - me@x - Gmail") with no other thread's subject as specific. Two threads with the same
/// winning subject, or a title that is only part of a subject, give no target.
pub fn find_by_subject(conn: &Connection, on_screen_title: &str) -> Result<Option<TriageRow>, rusqlite::Error> {
    let wanted = normalise_subject(&on_screen_title.trim().to_lowercase());
    if wanted.len() < 3 {
        return Ok(None);
    }
    // Only (thread_key, subject) is needed to choose; the full row — six correlated subqueries —
    // is fetched once, for the winner. This runs on the context-action path (NFR-SLO-02, 150ms).
    let mut stmt = conn.prepare(
        "SELECT t.thread_key, COALESCE((SELECT window_title FROM event_log WHERE id = t.latest_event_id), '')
         FROM mail_triage t WHERE t.archive_status <> 'done'
         ORDER BY t.updated_at_ms DESC LIMIT 300",
    )?;
    let candidates: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, normalise_subject(&r.get::<_, String>(1)?.to_lowercase()))))?
        .collect::<Result<_, _>>()?;
    let unique = |matches: Vec<&(String, String)>| match matches.as_slice() {
        [(k, _)] => Some(k.clone()),
        _ => None,
    };
    let exact: Vec<_> = candidates.iter().filter(|(_, s)| *s == wanted).collect();
    let key = if !exact.is_empty() {
        unique(exact)
    } else {
        // Containment only in this direction, and only for subjects specific enough to mean one
        // thread; the longest contained subject wins and must belong to one thread.
        let contained: Vec<_> =
            candidates.iter().filter(|(_, s)| s.chars().count() >= 8 && wanted.contains(s.as_str())).collect();
        let longest = contained.iter().map(|(_, s)| s.len()).max().unwrap_or(0);
        unique(contained.into_iter().filter(|(_, s)| s.len() == longest).collect())
    };
    match key {
        Some(k) => get_triage(conn, &k),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_log::{insert_with_thread, NewEvent};

    fn conn() -> Connection {
        crate::open_in_memory().unwrap()
    }

    /// The V34 seed. An install that upgrades into the switch must land in exactly the behaviour
    /// it already had — a migration that silently paused a working feature is a bug, not a new
    /// option.
    #[test]
    fn triage_starts_enabled_on_a_fresh_database() {
        assert!(triage_enabled(&conn()));
    }

    #[test]
    fn the_master_switch_round_trips_and_is_idempotent() {
        let c = conn();
        set_triage_enabled(&c, false).unwrap();
        assert!(!triage_enabled(&c));
        set_triage_enabled(&c, false).unwrap();
        assert!(!triage_enabled(&c));
        set_triage_enabled(&c, true).unwrap();
        assert!(triage_enabled(&c));
    }

    /// Fails OPEN. A settings row we cannot read is not a user saying "stop sorting my mail", and
    /// reading it closed would silently stop classifying for someone who never asked.
    #[test]
    fn a_missing_settings_row_reads_as_enabled() {
        let c = conn();
        set_triage_enabled(&c, false).unwrap();
        c.execute("DELETE FROM mail_triage_settings", []).unwrap();
        assert!(triage_enabled(&c));
        // …and the next write heals the row rather than failing on the missing one.
        set_triage_enabled(&c, false).unwrap();
        assert!(!triage_enabled(&c));
    }

    fn event(c: &Connection, thread_id: &str, subject: &str, body: &str, ts: i64) -> i64 {
        let ev = NewEvent {
            ts,
            source: "gmail",
            kind: "email",
            app_bundle_id: None,
            window_title: Some(subject),
            content: body,
            content_hash: &crate::event_log::content_hash(body),
            dwell_ms: 0,
            display_id: None,
            window_bounds: None,
        };
        insert_with_thread(c, &ev, Some(thread_id)).unwrap()
    }

    fn msg(thread_key: &str, event_id: i64, id: &str, from: &str, ts: i64) -> MailMessage {
        MailMessage {
            message_id: id.into(),
            thread_key: thread_key.into(),
            event_id,
            from_addr: from.into(),
            from_name: None,
            from_domain: domain_of(from),
            to_addrs: vec!["me@select.co.jp".into()],
            cc_addrs: vec![],
            is_from_me: false,
            in_reply_to: None,
            gmail_label_ids: vec!["INBOX".into()],
            has_list_unsubscribe: false,
            has_ics: false,
            internal_date_ms: ts,
        }
    }

    fn triage(label: &str, bucket: Bucket, attention: Attention, conf: f64) -> Triage {
        Triage { label_key: label.into(), bucket, attention, confidence: conf, reasons: vec![], bucket_lock: None }
    }

    #[test]
    fn builtin_labels_are_seeded() {
        let c = conn();
        let labels = list_labels(&c).unwrap();
        assert_eq!(labels.len(), 9);
        assert!(labels.iter().all(|l| l.kind == LabelKind::Builtin && !l.auto_archive && !l.sync_to_gmail));
        assert_eq!(labels[0].key, "people");
    }

    #[test]
    fn messages_and_identities_round_trip() {
        let c = conn();
        let e = event(&c, "t1", "Hi", "hello", 10);
        let mut mine = msg("gmail:t1", e, "m0", "me@select.co.jp", 5);
        mine.is_from_me = true;
        upsert_messages(&c, &[mine, msg("gmail:t1", e, "m1", "Alice@Client.Example", 10)], 100).unwrap();
        assert_eq!(identities(&c).unwrap(), vec!["me@select.co.jp".to_string()]);
        assert_eq!(identity_domains(&c).unwrap(), vec!["select.co.jp".to_string()]);
        let msgs = messages_for_thread(&c, "gmail:t1").unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].from_addr, "alice@client.example");
        let sig = thread_signals(&c, "gmail:t1", "Hi", "hello", 1_000).unwrap().unwrap();
        assert!(sig.my_reply_in_thread);
        assert!(!sig.latest_from_me);
        assert!(sig.to_me);
        assert_eq!(sig.message_count, 2);
    }

    #[test]
    fn upsert_local_respects_user_stage_and_batch_stage() {
        let mut c = conn();
        let e = event(&c, "t1", "Hi", "hello", 10);
        upsert_messages(&c, &[msg("gmail:t1", e, "m1", "a@b.example", 10)], 100).unwrap();
        let t = triage("marketing", Bucket::Other, Attention::None, 0.9);
        let stored = upsert_local(&c, "gmail:t1", e, &t, 100).unwrap();
        assert_eq!(stored, t);

        // User relabels → later local pass cannot move it.
        let fb = apply_user_change(
            &mut c,
            "gmail:t1",
            &UserChange { label_key: Some("people".into()), bucket: Some(Bucket::Important), handled: None },
            FeedbackOrigin::Ui,
            None,
            200,
        )
        .unwrap()
        .unwrap();
        assert!(fb.is_some());
        let again = upsert_local(&c, "gmail:t1", e, &t, 300).unwrap();
        assert_eq!(again.label_key, "people");
        assert_eq!(again.bucket, Bucket::Important);
        let row = get_triage(&c, "gmail:t1").unwrap().unwrap();
        assert_eq!(row.stage, Stage::User);
        assert_eq!(row.bucket_lock, Some(BucketLock::User));
        assert!(row.reasons.iter().any(|r| r == local::R_USER_OVERRIDE));

        // Batch cannot overwrite a user row either.
        let changed = apply_batch(
            &mut c,
            &BatchVerdict {
                thread_key: "gmail:t1".into(),
                label_key: "marketing".into(),
                attention_hint: None,
                confidence: 0.95,
                reason_codes: vec![],
            },
            400,
        )
        .unwrap();
        assert!(!changed);
    }

    #[test]
    fn batch_respects_lock_and_attention_threshold() {
        let mut c = conn();
        let e = event(&c, "t2", "Q", "could you?", 10);
        upsert_messages(&c, &[msg("gmail:t2", e, "m2", "alice@client.example", 10)], 100).unwrap();
        let mut t = triage("people", Bucket::Important, Attention::NeedsReply, 0.6);
        t.bucket_lock = Some(BucketLock::KnownPerson);
        upsert_local(&c, "gmail:t2", e, &t, 100).unwrap();
        // Batch says marketing at 0.95 with hint none → label moves, bucket stays Important
        // (locked), attention follows the hint (≥0.7).
        let changed = apply_batch(
            &mut c,
            &BatchVerdict {
                thread_key: "gmail:t2".into(),
                label_key: "marketing".into(),
                attention_hint: Some(Attention::Fyi),
                confidence: 0.95,
                reason_codes: vec!["bogus_code".into(), local::R_SALES_VOCAB.into()],
            },
            200,
        )
        .unwrap();
        assert!(changed);
        let row = get_triage(&c, "gmail:t2").unwrap().unwrap();
        assert_eq!(row.label_key, "marketing");
        assert_eq!(row.bucket, Bucket::Important);
        assert_eq!(row.attention, Attention::Fyi);
        assert_eq!(row.stage, Stage::Batch);
        assert!(!row.reasons.iter().any(|r| r == "bogus_code"));
        assert!(row.reasons.iter().any(|r| r == local::R_BATCH_REFINED));

        // Low-confidence hint is ignored.
        apply_batch(
            &mut c,
            &BatchVerdict {
                thread_key: "gmail:t2".into(),
                label_key: "people".into(),
                attention_hint: Some(Attention::NeedsReply),
                confidence: 0.6,
                reason_codes: vec![],
            },
            300,
        )
        .unwrap();
        let row = get_triage(&c, "gmail:t2").unwrap().unwrap();
        assert_eq!(row.attention, Attention::Fyi);

        // Same-event local re-run on a batch row keeps the refined label.
        let again = upsert_local(&c, "gmail:t2", e, &triage("pitch", Bucket::Other, Attention::None, 0.6), 400).unwrap();
        assert_eq!(again.label_key, "people");
        assert_eq!(get_triage(&c, "gmail:t2").unwrap().unwrap().stage, Stage::Batch);
    }

    #[test]
    fn open_loop_follows_attention() {
        let mut c = conn();
        let e = event(&c, "t3", "Contract", "could you sign?", 10);
        upsert_messages(&c, &[msg("gmail:t3", e, "m3", "alice@client.example", 10)], 100).unwrap();
        let t = triage("people", Bucket::Important, Attention::NeedsReply, 0.7);
        upsert_local(&c, "gmail:t3", e, &t, 100).unwrap();
        let id = sync_open_loop(&mut c, "gmail:t3", e, "Contract", &t, None, 10, 100).unwrap().unwrap();
        let loops = crate::state::list_open_loops(&c).unwrap();
        assert_eq!(loops.len(), 1);
        assert_eq!(loops[0].kind, "reply_needed");
        assert_eq!(loops[0].first_event_id, Some(e));
        assert_eq!(get_triage(&c, "gmail:t3").unwrap().unwrap().open_loop_id, Some(id));

        // The user replied → attention none → loop closes.
        let t2 = triage("people", Bucket::Important, Attention::None, 0.7);
        assert!(sync_open_loop(&mut c, "gmail:t3", e, "Contract", &t2, None, 20, 200).unwrap().is_none());
        let status: String = c.query_row("SELECT status FROM open_loops WHERE id = ?1", [id], |r| r.get(0)).unwrap();
        assert_eq!(status, "closed");

        // Below the confidence gate nothing is written to state.
        let t3 = triage("people", Bucket::Important, Attention::NeedsReply, 0.4);
        assert!(sync_open_loop(&mut c, "gmail:t3", e, "Contract", &t3, None, 30, 300).unwrap().is_none());
        assert_eq!(crate::state::list_open_loops(&c).unwrap().len(), 1);
    }

    #[test]
    fn a_batch_verdict_drives_the_loop_and_survives_a_local_resync() {
        let mut c = conn();
        let e = event(&c, "t10", "Contract", "fyi only", 10);
        upsert_messages(&c, &[msg("gmail:t10", e, "m10", "alice@client.example", 10)], 100).unwrap();
        let t = triage("people", Bucket::Important, Attention::Fyi, 0.8);
        upsert_local(&c, "gmail:t10", e, &t, 100).unwrap();
        assert!(sync_open_loop(&mut c, "gmail:t10", e, "Contract", &t, None, 10, 100).unwrap().is_none());
        let v = BatchVerdict {
            thread_key: "gmail:t10".into(),
            label_key: "people".into(),
            attention_hint: Some(Attention::NeedsReply),
            confidence: 0.9,
            reason_codes: vec![],
        };
        assert!(apply_batch(&mut c, &v, 200).unwrap());
        let loops = crate::state::list_open_loops(&c).unwrap();
        assert_eq!(loops.len(), 1);
        assert!(!loops[0].inferred, "reviewed, not a guess");
        // A local re-sync of the same message keeps the Batch attention.
        let again = upsert_local(&c, "gmail:t10", e, &t, 300).unwrap();
        assert_eq!(again.attention, Attention::NeedsReply);
        // A newer message resets to the local verdict.
        let e2 = event(&c, "t10b", "Contract", "thanks", 20);
        let again = upsert_local(&c, "gmail:t10", e2, &t, 400).unwrap();
        assert_eq!(again.attention, Attention::Fyi);
    }

    #[test]
    fn a_verdict_for_a_thread_with_a_newer_message_is_dropped() {
        let mut c = conn();
        let e = event(&c, "t11", "Contract", "could you sign?", 10);
        upsert_messages(&c, &[msg("gmail:t11", e, "m11", "alice@client.example", 10)], 100).unwrap();
        let t = triage("people", Bucket::Important, Attention::Fyi, 0.6);
        upsert_local(&c, "gmail:t11", e, &t, 100).unwrap();
        record_batch_items(&c, "b1", &[("c11".into(), "gmail:t11".into())], 50).unwrap();
        // The user answered after submission.
        let e2 = event(&c, "t11b", "Contract", "signed, attached", 60);
        upsert_local(&c, "gmail:t11", e2, &t, 120).unwrap();
        let v = BatchVerdict {
            thread_key: "gmail:t11".into(),
            label_key: "people".into(),
            attention_hint: Some(Attention::NeedsReply),
            confidence: 0.9,
            reason_codes: vec![],
        };
        assert!(!apply_batch(&mut c, &v, 200).unwrap());
        assert_eq!(get_triage(&c, "gmail:t11").unwrap().unwrap().attention, Attention::Fyi);
        assert!(crate::state::list_open_loops(&c).unwrap().is_empty());
    }

    #[test]
    fn a_user_closed_loop_stays_closed_until_newer_evidence() {
        let mut c = conn();
        let e = event(&c, "t9", "Contract", "could you sign?", 10);
        upsert_messages(&c, &[msg("gmail:t9", e, "m9", "alice@client.example", 10)], 100).unwrap();
        let t = triage("people", Bucket::Important, Attention::NeedsReply, 0.7);
        upsert_local(&c, "gmail:t9", e, &t, 100).unwrap();
        let id = sync_open_loop(&mut c, "gmail:t9", e, "Contract", &t, None, 10, 100).unwrap().unwrap();
        crate::state::close_open_loop(&c, id, 150).unwrap();
        let status = |c: &Connection| -> String { c.query_row("SELECT status FROM open_loops WHERE id = ?1", [id], |r| r.get(0)).unwrap() };
        sync_open_loop(&mut c, "gmail:t9", e, "Contract", &t, None, 10, 200).unwrap();
        assert_eq!(status(&c), "closed");
        sync_open_loop(&mut c, "gmail:t9", e, "Contract", &t, None, 300, 400).unwrap();
        assert_eq!(status(&c), "open");
    }

    /// Issue #926: a loop from the on-device first pass is marked inferred — visible and
    /// actionable, but not "reviewed" however high its score. The Batch review or a user edit
    /// clears the mark.
    #[test]
    fn a_local_mail_guess_is_marked_inferred_until_reviewed_or_edited() {
        let mut c = conn();
        let loop_for = |c: &mut Connection, key: &str, id: i64| {
            crate::state::list_open_loops(c).unwrap().into_iter().find(|l| l.id == id).unwrap_or_else(|| panic!("{key}"))
        };
        // Local verdict at 0.9: a strong guess.
        let e = event(&c, "t5", "Contract", "could you sign by Friday?", 10);
        upsert_messages(&c, &[msg("gmail:t5", e, "m5", "alice@client.example", 10)], 100).unwrap();
        let t = triage("people", Bucket::Important, Attention::NeedsReply, 0.9);
        upsert_local(&c, "gmail:t5", e, &t, 100).unwrap();
        let id = sync_open_loop(&mut c, "gmail:t5", e, "Contract", &t, None, 10, 100).unwrap().unwrap();
        let guess = loop_for(&mut c, "t5", id);
        assert!(guess.inferred, "a local guess is not reviewed");
        assert!(guess.is_actionable(), "and it still counts for action surfaces");
        assert!(guess.confidence >= 0.8, "its likelihood is kept as is");

        // The Batch review refines the verdict → no longer a bare guess.
        apply_batch(
            &mut c,
            &BatchVerdict {
                thread_key: "gmail:t5".into(),
                label_key: "people".into(),
                attention_hint: Some(Attention::NeedsReply),
                confidence: 0.8,
                reason_codes: vec![],
            },
            200,
        )
        .unwrap();
        assert!(!loop_for(&mut c, "t5", id).inferred);

        // A user edit pins the loop → theirs, never an inference.
        let e6 = event(&c, "t6", "Invoice", "can you confirm?", 20);
        upsert_messages(&c, &[msg("gmail:t6", e6, "m6", "bob@client.example", 20)], 100).unwrap();
        let t6 = triage("people", Bucket::Important, Attention::NeedsReply, 0.7);
        upsert_local(&c, "gmail:t6", e6, &t6, 100).unwrap();
        let id6 = sync_open_loop(&mut c, "gmail:t6", e6, "Invoice", &t6, None, 20, 100).unwrap().unwrap();
        assert!(loop_for(&mut c, "t6", id6).inferred);
        let patch = crate::state_edit::StatePatch { description: Some("Confirm the invoice".into()), ..Default::default() };
        crate::state_edit::update_state(&mut c, crate::state::StateTable::OpenLoops, id6, &patch, 150).unwrap();
        assert!(!loop_for(&mut c, "t6", id6).inferred);
    }

    /// Issue #290: a loop the user edited is theirs — the thread's later triage neither rewrites
    /// nor closes it.
    #[test]
    fn a_user_pinned_loop_is_left_as_the_user_set_it() {
        let mut c = conn();
        let e = event(&c, "t4", "Contract", "could you sign?", 10);
        upsert_messages(&c, &[msg("gmail:t4", e, "m4", "alice@client.example", 10)], 100).unwrap();
        let t = triage("people", Bucket::Important, Attention::NeedsReply, 0.7);
        upsert_local(&c, "gmail:t4", e, &t, 100).unwrap();
        let id = sync_open_loop(&mut c, "gmail:t4", e, "Contract", &t, None, 10, 100).unwrap().unwrap();
        let patch = crate::state_edit::StatePatch {
            description: Some("Sign and return the contract".into()),
            ..Default::default()
        };
        crate::state_edit::update_state(&mut c, crate::state::StateTable::OpenLoops, id, &patch, 150)
            .unwrap();

        let waiting = triage("people", Bucket::Important, Attention::WaitingOnThem, 0.9);
        assert_eq!(sync_open_loop(&mut c, "gmail:t4", e, "Contract", &waiting, None, 20, 200).unwrap(), Some(id));
        let none = triage("people", Bucket::Important, Attention::None, 0.7);
        assert_eq!(sync_open_loop(&mut c, "gmail:t4", e, "Contract", &none, None, 30, 300).unwrap(), Some(id));
        let (kind, status, conf): (String, String, f64) = c
            .query_row("SELECT kind, status, confidence FROM open_loops WHERE id = ?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!((kind.as_str(), status.as_str(), conf), ("reply_needed", "open", 1.0));
        assert_eq!(crate::state::list_open_loops(&c).unwrap().len(), 1, "no twin");
    }

    #[test]
    fn two_corrections_make_a_sender_rule() {
        let mut c = conn();
        for (i, tid) in ["a", "b"].iter().enumerate() {
            let e = event(&c, tid, &format!("Offer {i}"), &format!("body {i}"), 10 + i as i64);
            upsert_messages(&c, &[msg(&format!("gmail:{tid}"), e, &format!("m{i}"), "deals@shop.example", 10)], 100).unwrap();
            upsert_local(&c, &format!("gmail:{tid}"), e, &triage("marketing", Bucket::Other, Attention::None, 0.9), 100).unwrap();
        }
        assert!(list_sender_rules(&c).unwrap().is_empty());
        let change = UserChange { label_key: Some("finance".into()), bucket: Some(Bucket::Important), handled: None };
        apply_user_change(&mut c, "gmail:a", &change, FeedbackOrigin::Ui, None, 200).unwrap().unwrap();
        assert!(list_sender_rules(&c).unwrap().is_empty());
        apply_user_change(&mut c, "gmail:b", &change, FeedbackOrigin::Api, None, 300).unwrap().unwrap();
        let rules = list_sender_rules(&c).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].match_value, "deals@shop.example");
        assert_eq!(rules[0].label_key, "finance");
        assert_eq!(rules[0].source, SenderRuleSource::FeedbackX2);
        assert_eq!(undistilled_feedback(&c, 10).unwrap().len(), 2);
        assert!(delete_sender_rule(&c, rules[0].id).unwrap());
    }

    #[test]
    fn custom_labels_cap_and_delete() {
        let mut c = conn();
        for i in 0..MAX_CUSTOM_LABELS {
            let r = upsert_custom_label(&c, None, &format!("Label {i}"), "def", Bucket::Other, 1).unwrap();
            assert!(r.is_ok(), "{i}");
        }
        assert_eq!(
            upsert_custom_label(&c, None, "One more", "def", Bucket::Other, 1).unwrap(),
            Err(LabelError::TooMany)
        );
        // Editing an existing custom label does not count against the cap.
        assert!(upsert_custom_label(&c, Some("custom:label-0"), "Label 0", "new def", Bucket::Important, 2).unwrap().is_ok());
        assert_eq!(get_label(&c, "custom:label-0").unwrap().unwrap().definition.as_deref(), Some("new def"));
        assert!(any_custom_label_enabled(&c).unwrap());
        assert_eq!(delete_custom_label(&mut c, "people", 3).unwrap(), Err(LabelError::NotCustom));
        assert!(delete_custom_label(&mut c, "custom:label-0", 3).unwrap().is_ok());
        assert!(get_label(&c, "custom:label-0").unwrap().is_none());
        assert_eq!(custom_key_for("  Job Applications!! "), "custom:job-applications");
    }

    #[test]
    fn archive_candidates_apply_every_condition() {
        let c = conn();
        let e = event(&c, "t4", "50% off", "shop", 10);
        upsert_messages(&c, &[msg("gmail:t4", e, "m4", "deals@shop.example", 10)], 100).unwrap();
        upsert_local(&c, "gmail:t4", e, &triage("marketing", Bucket::Other, Attention::None, 0.9), 100).unwrap();
        // Standing approval OFF → nothing.
        assert!(archive_candidates(&c, 10_000_000, 600_000, 50).unwrap().is_empty());
        set_label_flags(&c, "marketing", LabelFlags { auto_archive: Some(true), ..Default::default() }, 100).unwrap();
        // Inside the grace period → nothing.
        assert!(archive_candidates(&c, 100 + 1_000, 600_000, 50).unwrap().is_empty());
        let cands = archive_candidates(&c, 100 + 600_001, 600_000, 50).unwrap();
        assert_eq!(cands.len(), 1);
        // A locked row is never a candidate.
        let e2 = event(&c, "t5", "Team digest", "shop", 11);
        upsert_messages(&c, &[msg("gmail:t5", e2, "m5", "all@select.co.jp", 11)], 100).unwrap();
        let mut locked = triage("marketing", Bucket::Other, Attention::None, 0.95);
        locked.bucket_lock = Some(BucketLock::SameDomain);
        locked.bucket = Bucket::Important;
        upsert_local(&c, "gmail:t5", e2, &locked, 100).unwrap();
        assert_eq!(archive_candidates(&c, 100 + 600_001, 600_000, 50).unwrap().len(), 1);
        // Done → not a candidate; INBOX removed locally too.
        set_archive_status(&c, "gmail:t4", ArchiveStatus::Done, 800_000).unwrap();
        set_inbox_label_locally(&c, "gmail:t4", false).unwrap();
        assert!(archive_candidates(&c, 900_000, 600_000, 50).unwrap().is_empty());
        let counts = counts(&c, 0).unwrap();
        assert_eq!(counts.auto_archived, 1);
        assert_eq!(counts.other, 1);
        assert_eq!(counts.other_by_label, vec![("marketing".to_string(), 1)]);
        // Undo → back in inbox & a candidate again after the grace period.
        set_archive_status(&c, "gmail:t4", ArchiveStatus::Undone, 900_000).unwrap();
        set_inbox_label_locally(&c, "gmail:t4", true).unwrap();
        let row = get_triage(&c, "gmail:t4").unwrap().unwrap();
        assert_eq!(row.archive_status, ArchiveStatus::Undone);
        assert!(row.archived_at_ms.is_none());
    }

    #[test]
    fn explicit_requests_ignore_the_automatic_safety_conditions() {
        let c = conn();
        let e = event(&c, "t9", "50% off", "shop", 10);
        upsert_messages(&c, &[msg("gmail:t9", e, "m9", "deals@shop.example", 10)], 100).unwrap();
        // A low-confidence, important, locked row: everything the automatic sweep refuses.
        let mut t = triage("people", Bucket::Important, Attention::Fyi, 0.4);
        t.bucket_lock = Some(BucketLock::KnownPerson);
        upsert_local(&c, "gmail:t9", e, &t, 100).unwrap();
        assert!(archive_candidates(&c, 10_000_000, 0, 50).unwrap().is_empty());

        // The caller named this thread, which is their own one-tap: it runs.
        set_archive_status(&c, "gmail:t9", ArchiveStatus::Pending, 200).unwrap();
        let reqs = explicit_requests(&c, 50).unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!((reqs[0].0.thread_id.as_str(), reqs[0].1), ("t9", false));

        // The opposite direction is a distinct request, not a local flag flip.
        set_archive_status(&c, "gmail:t9", ArchiveStatus::UnarchivePending, 300).unwrap();
        let reqs = explicit_requests(&c, 50).unwrap();
        assert_eq!((reqs[0].0.thread_id.as_str(), reqs[0].1), ("t9", true));
        // A pending request leaves the archive stamp alone (Gmail has not changed yet).
        set_archive_status(&c, "gmail:t9", ArchiveStatus::Done, 400).unwrap();
        set_archive_status(&c, "gmail:t9", ArchiveStatus::UnarchivePending, 500).unwrap();
        assert_eq!(get_triage(&c, "gmail:t9").unwrap().unwrap().archived_at_ms, Some(400));
    }

    #[test]
    fn an_undone_label_sync_is_not_re_applied_until_the_label_changes() {
        let c = conn();
        let e = event(&c, "ta", "Offer", "shop", 10);
        upsert_messages(&c, &[msg("gmail:ta", e, "ma", "deals@shop.example", 10)], 100).unwrap();
        upsert_local(&c, "gmail:ta", e, &triage("marketing", Bucket::Other, Attention::None, 0.9), 100).unwrap();
        set_label_flags(&c, "marketing", LabelFlags { sync_to_gmail: Some(true), ..Default::default() }, 100).unwrap();
        assert_eq!(label_sync_candidates(&c, 10).unwrap().len(), 1);

        // Applied, then undone by the user: the marker keeps the next tick from putting it back.
        set_synced_label(&c, "gmail:ta", Some("marketing"), 200).unwrap();
        assert!(label_sync_candidates(&c, 10).unwrap().is_empty());
        set_synced_label(&c, "gmail:ta", Some(&label_sync_undone_marker("marketing")), 300).unwrap();
        assert!(label_sync_candidates(&c, 10).unwrap().is_empty(), "an undo the job reverses is not an undo");

        // A later relabel is a new decision, so syncing resumes.
        set_label_flags(&c, "news", LabelFlags { sync_to_gmail: Some(true), ..Default::default() }, 400).unwrap();
        let mut c2 = c;
        apply_user_change(
            &mut c2,
            "gmail:ta",
            &UserChange { label_key: Some("news".into()), bucket: Some(Bucket::Other), handled: None },
            FeedbackOrigin::Ui,
            None,
            500,
        )
        .unwrap()
        .unwrap();
        assert_eq!(label_sync_candidates(&c2, 10).unwrap().len(), 1);
    }

    #[test]
    fn a_learned_sender_rule_keys_on_the_counterpart_never_on_the_user() {
        let mut c = conn();
        for (i, tid) in ["p", "q"].iter().enumerate() {
            let e = event(&c, tid, &format!("Thread {i}"), &format!("body {i}"), 10 + i as i64);
            let key = format!("gmail:{tid}");
            let mut theirs = msg(&key, e, &format!("in{i}"), "alice@client.example", 10);
            theirs.to_addrs = vec!["me@select.co.jp".into()];
            // The user answered last, so the newest message on the thread is their own.
            let mut mine = msg(&key, e, &format!("out{i}"), "me@select.co.jp", 20);
            mine.is_from_me = true;
            mine.to_addrs = vec!["alice@client.example".into()];
            upsert_messages(&c, &[theirs, mine], 100).unwrap();
            upsert_local(&c, &key, e, &triage("people", Bucket::Important, Attention::None, 0.85), 100).unwrap();
        }
        assert_eq!(counterpart_addr(&c, "gmail:p").unwrap().as_deref(), Some("alice@client.example"));
        assert_eq!(thread_sender_domain(&c, "gmail:p").unwrap().as_deref(), Some("client.example"));

        let change = UserChange { label_key: Some("finance".into()), bucket: Some(Bucket::Important), handled: None };
        for tid in ["p", "q"] {
            apply_user_change(&mut c, &format!("gmail:{tid}"), &change, FeedbackOrigin::Ui, None, 200).unwrap().unwrap();
        }
        let rules = list_sender_rules(&c).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].match_value, "alice@client.example", "a rule matching your own address relabels your mailbox");
    }

    #[test]
    fn the_write_log_is_addressed_by_id_not_by_a_recent_window() {
        let c = conn();
        let first = insert_write_log(&c, "gmail:t1", "archive_thread", &[], &["INBOX".into()], WriteApproval::L2Standing, true, None, 1_000).unwrap();
        for i in 0..50 {
            insert_write_log(&c, "gmail:other", "archive_thread", &[], &[], WriteApproval::L2Standing, true, None, 2_000 + i).unwrap();
        }
        let row = get_write_log(&c, first).unwrap().expect("still addressable after newer rows");
        assert_eq!((row.id, row.ts_ms, row.status.as_str()), (first, 1_000, "ok"));
        assert!(mark_write_undone(&c, first, 3_000).unwrap());
        assert_eq!(get_write_log(&c, first).unwrap().unwrap().status, "undone");
        assert!(get_write_log(&c, 99_999).unwrap().is_none());
    }

    #[test]
    fn find_by_subject_needs_a_unique_exact_match() {
        let c = conn();
        for (t, subj, ts) in [("a", "Quarterly budget review", 10), ("b", "Quarterly budget review", 11), ("c", "Invoice 4411 attached", 12)] {
            let e = event(&c, t, subj, "x", ts);
            upsert_messages(&c, &[msg(&format!("gmail:{t}"), e, &format!("m{t}"), "x@y.example", ts)], 100).unwrap();
            upsert_local(&c, &format!("gmail:{t}"), e, &triage("people", Bucket::Important, Attention::NeedsReply, 0.7), 100).unwrap();
        }
        assert!(find_by_subject(&c, "Quarterly budget review").unwrap().is_none(), "two same-subject threads");
        assert!(find_by_subject(&c, "Quarterly budget review - me@x.example - Gmail").unwrap().is_none(), "two same-subject threads in a browser title");
        assert!(find_by_subject(&c, "Invoice 4411").unwrap().is_none(), "title is only part of a subject");
        assert!(find_by_subject(&c, "Re: Invoice 4411 attached").unwrap().is_some(), "unique exact");
        assert!(find_by_subject(&c, "Invoice 4411 attached - me@x.example - Gmail").unwrap().is_some(), "browser title");
    }

    #[test]
    fn list_filters_and_cursor() {
        let c = conn();
        let e = event(&c, "t6", "Hello", "hi there", 10);
        upsert_messages(&c, &[msg("gmail:t6", e, "m6", "x@y.example", 10)], 100).unwrap();
        upsert_local(&c, "gmail:t6", e, &triage("people", Bucket::Important, Attention::NeedsReply, 0.7), 100).unwrap();
        let rows = list_triage(&c, &TriageFilter { attention: Some(Attention::NeedsReply), ..Default::default() }).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].thread_id, "t6");
        assert_eq!(rows[0].subject, "Hello");
        assert_eq!(rows[0].snippet, "hi there");
        assert!(list_triage(&c, &TriageFilter { bucket: Some(Bucket::Other), ..Default::default() }).unwrap().is_empty());
        assert_eq!(needs_reply_open_count(&c).unwrap(), 1);
        assert!(find_by_subject(&c, "Re: Hello").unwrap().is_some());
        assert!(find_by_subject(&c, "Something else entirely").unwrap().is_none());

        assert!(get_cursor(&c, "gmail").unwrap().is_none());
        set_cursor(&c, "gmail", &SyncCursor { watermark_ms: 42, page_token: Some("p".into()) }, 1).unwrap();
        assert_eq!(get_cursor(&c, "gmail").unwrap().unwrap().watermark_ms, 42);
    }

    #[test]
    fn batch_ledger_and_daily_cap() {
        let c = conn();
        let e = event(&c, "t7", "Hmm", "unsure", 10);
        upsert_messages(&c, &[msg("gmail:t7", e, "m7", "x@y.example", 10)], 100).unwrap();
        upsert_local(&c, "gmail:t7", e, &triage("people", Bucket::Important, Attention::Fyi, 0.4), 100).unwrap();
        let pending = pending_for_batch(&c, 10).unwrap();
        assert_eq!(pending, vec![("gmail:t7".to_string(), e)]);
        record_batch_items(&c, "b1", &[("c1".into(), "gmail:t7".into())], 200).unwrap();
        assert!(pending_for_batch(&c, 10).unwrap().is_empty(), "in flight → not pending");
        assert_eq!(submitted_since(&c, 150).unwrap(), 1);
        assert_eq!(unapplied_batches(&c).unwrap().len(), 1);
        mark_batch_applied(&c, "b1", 300).unwrap();
        assert!(unapplied_batches(&c).unwrap().is_empty());
        // Still local & low → pending again (a retry is allowed once applied/abandoned).
        assert_eq!(pending_for_batch(&c, 10).unwrap().len(), 1);
    }

    /// FR-TG-03 as amended by the zero-base plan L0(b): human correspondence rides Stage B at any
    /// confidence (it is the only model pass a Gmail thread gets while triage is on), the
    /// automated categories only when Stage L was unsure, and when the cap binds the threads
    /// addressed to the user go first.
    #[test]
    fn stage_b_selection_covers_human_correspondence_and_prefers_addressed_threads() {
        let c = conn();
        add_identity(&c, "me@select.co.jp", 1).unwrap();
        // A sure `team` thread, cc-only (the user is not among the recipients).
        let team = event(&c, "team", "Standup", "notes", 10);
        let mut cc = msg("gmail:team", team, "m-team", "lead@select.co.jp", 10);
        cc.to_addrs = vec!["other@select.co.jp".into()];
        cc.cc_addrs = vec!["me@select.co.jp".into()];
        upsert_messages(&c, &[cc], 100).unwrap();
        upsert_local(&c, "gmail:team", team, &triage("team", Bucket::Important, Attention::Fyi, 0.9), 100).unwrap();
        // A sure `people` thread addressed to the user, older than the team thread.
        let people = event(&c, "people", "Re: contract", "I'll send it Friday", 5);
        upsert_messages(&c, &[msg("gmail:people", people, "m-people", "jane@client.example", 5)], 90).unwrap();
        upsert_local(&c, "gmail:people", people, &triage("people", Bucket::Important, Attention::NeedsReply, 0.85), 90).unwrap();
        // A sure automated thread: never sent.
        let news = event(&c, "news", "Weekly digest", "read", 20);
        upsert_messages(&c, &[msg("gmail:news", news, "m-news", "digest@news.example", 20)], 110).unwrap();
        upsert_local(&c, "gmail:news", news, &triage("news", Bucket::Other, Attention::None, 0.9), 110).unwrap();
        // An unsure automated thread: sent.
        let unsure = event(&c, "unsure", "Hmm", "?", 30);
        upsert_messages(&c, &[msg("gmail:unsure", unsure, "m-unsure", "x@y.example", 30)], 120).unwrap();
        upsert_local(&c, "gmail:unsure", unsure, &triage("notification", Bucket::Other, Attention::None, 0.6), 120).unwrap();

        let pending: Vec<String> = pending_for_batch(&c, 10).unwrap().into_iter().map(|(k, _)| k).collect();
        // Addressed threads first (oldest pending first among them), then the rest.
        assert_eq!(pending, vec!["gmail:people", "gmail:unsure", "gmail:team"]);
        // The cap binds: the addressed threads are the ones that go, the longest-waiting first.
        let capped: Vec<String> = pending_for_batch(&c, 2).unwrap().into_iter().map(|(k, _)| k).collect();
        assert_eq!(capped, vec!["gmail:people", "gmail:unsure"]);
        // The input carries the event the excerpt came from.
        let input = batch_thread_input(&c, "gmail:people").unwrap().unwrap();
        assert_eq!(input.event_id, people);
        assert!(input.to_me);
    }

    /// The excerpt is cut from the latest message, not the head of the thread: a promise in the
    /// third message of a thread whose first message is longer than the whole budget is in it,
    /// and what the budget leaves is filled with the heads of the earlier messages, newest first,
    /// in thread order — every line still a substring of the body.
    #[test]
    fn stage_b_excerpt_starts_from_the_latest_message_and_back_fills_earlier_heads() {
        let first = format!("Hi team,\n{}\nRegards, Ann", "Background paragraph. ".repeat(100));
        let second = "Thanks Ann.\nTwo quick questions below.\nCould you share the budget?\n-- Bob";
        let third = "Sure.\nI will send the signed contract tomorrow.\nBest, Ann";
        let body = [first.as_str(), second, third].join(THREAD_MESSAGE_SEPARATOR);
        assert!(first.chars().count() > BATCH_BODY_CHARS, "the first message alone exceeds the budget");

        let excerpt = stage_b_excerpt(&body, BATCH_BODY_CHARS);
        assert!(excerpt.chars().count() <= BATCH_BODY_CHARS, "{}", excerpt.chars().count());
        assert!(excerpt.ends_with(third), "the latest message is whole and last: {excerpt}");
        assert!(excerpt.contains(second), "the previous message fits whole: {excerpt}");
        assert!(excerpt.starts_with("Hi team,\n"), "the first message's head back-fills the rest: {excerpt}");
        assert!(!excerpt.contains("Regards, Ann"), "its tail does not fit: {excerpt}");
        for line in excerpt.lines().filter(|l| !l.trim().is_empty() && *l != "---") {
            assert!(body.contains(line), "every line is a substring of the body: {line:?}");
        }
        // Whole lines only when back-filling: the cut falls on a line boundary of the first message.
        let head = excerpt.split(THREAD_MESSAGE_SEPARATOR).next().unwrap();
        assert!(first.starts_with(head) && (head.len() == first.len() || first[head.len()..].starts_with('\n')), "{head:?}");

        // A thread that fits is sent as it is stored.
        let short = ["Hello", "World"].join(THREAD_MESSAGE_SEPARATOR);
        assert_eq!(stage_b_excerpt(&short, BATCH_BODY_CHARS), short);
        // A latest message longer than the budget: its head, nothing earlier.
        let long_last = format!("Old\n{}{}", THREAD_MESSAGE_SEPARATOR, "x".repeat(2 * BATCH_BODY_CHARS));
        let e = stage_b_excerpt(&long_last, BATCH_BODY_CHARS);
        assert_eq!(e.chars().count(), BATCH_BODY_CHARS);
        assert!(!e.contains("Old"));
        // A single-paragraph earlier message that does not fit whole is cut rather than dropped.
        let paragraph = format!("{}{}Reply", "y".repeat(2 * BATCH_BODY_CHARS), THREAD_MESSAGE_SEPARATOR);
        let e = stage_b_excerpt(&paragraph, BATCH_BODY_CHARS);
        assert!(e.ends_with("Reply") && e.starts_with("yyyy"), "{e}");
        assert_eq!(e.chars().count(), BATCH_BODY_CHARS);
        assert_eq!(stage_b_excerpt("", BATCH_BODY_CHARS), "");
        // The stored input uses it.
        let c = conn();
        let ev = event(&c, "t-long", "Re: contract", &body, 10);
        upsert_messages(&c, &[msg("gmail:t-long", ev, "m-long", "ann@client.example", 10)], 100).unwrap();
        upsert_local(&c, "gmail:t-long", ev, &triage("people", Bucket::Important, Attention::NeedsReply, 0.85), 100).unwrap();
        let input = batch_thread_input(&c, "gmail:t-long").unwrap().unwrap();
        assert_eq!(input.body_excerpt, excerpt);
    }

    #[test]
    fn world_signals_find_people_by_email_and_mailed_senders() {
        let mut c = conn();
        let e = event(&c, "t8", "x", "y", 10);
        let pid = crate::state::insert_person(
            &mut c,
            &crate::state::NewPerson {
                display_name: "Alice",
                emails_json: Some(r#"["Alice@Client.Example"]"#),
                confidence: 0.9,
                now: 1,
                ..Default::default()
            },
            &[Provenance::new(e)],
        )
        .unwrap();
        let w = world_signals(&c, "alice@client.example", "x").unwrap();
        assert_eq!(w.known_person.as_ref().map(|p| p.person_id), Some(pid));
        assert!(!w.known_person.as_ref().unwrap().has_interaction, "no interaction yet");
        // A sent message to them counts as interaction.
        let mut sent = msg("gmail:t8", e, "m8", "me@select.co.jp", 5);
        sent.is_from_me = true;
        sent.to_addrs = vec!["alice@client.example".into()];
        upsert_messages(&c, &[sent], 100).unwrap();
        let w = world_signals(&c, "alice@client.example", "x").unwrap();
        assert!(w.known_person.unwrap().has_interaction);
        assert_eq!(w.my_domains, vec!["select.co.jp".to_string()]);
        // Unknown-to-people but mailed by me → known with no person id.
        let mut sent2 = msg("gmail:t8", e, "m9", "me@select.co.jp", 6);
        sent2.is_from_me = true;
        sent2.to_addrs = vec!["bob@other.example".into()];
        upsert_messages(&c, &[sent2], 100).unwrap();
        let w = world_signals(&c, "bob@other.example", "x").unwrap();
        assert_eq!(w.known_person, Some(local::KnownPerson { person_id: 0, has_interaction: true }));
    }
}
