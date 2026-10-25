//! Audit export with a hash chain (ShogunAgent E26): what an enterprise hands an auditor to prove
//! the audit log it exported is complete and unaltered.
//!
//! `GET /v1/audit/export?since_ms=&until_ms=&format=jsonl|csv` (the operator's admin credential
//! only, never an agent tool) and `shogun audit export` stream the principal's audit records in
//! append order (by row id), each carrying:
//!
//! - `seq`: 1, 2, 3, … within this export;
//! - `prev_hash`: the previous record's `hash` (64 zeros for the first, [`GENESIS`]);
//! - `hash`: lowercase hex SHA-256 of the record's canonical form ([`canonical`]: the fields in a
//!   fixed order, `prev_hash` included, `hash` excluded), so changing, dropping, reordering or
//!   inserting any record breaks every hash after it.
//!
//! The export ends with a trailer naming the record count and the head hash (the last record's
//! `hash`). A truncated stream has no trailer and fails verification. The chain is deterministic:
//! the same log and window export byte-identical records, so a head hash kept out of band (in a
//! ticket, a WORM bucket) proves a later export of the same window is the same log.
//! `shogun audit verify <file>` ([`verify`]) recomputes everything offline, for either format.
//!
//! Records are labels only (who, which route, tier, outcome, time, request id; see
//! `shogun_memory::audit`): an export carries no body, no argument value, no row text.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use shogun_memory::audit::AuditEntry;

/// `prev_hash` of an export's first record.
pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";
/// The CSV header row.
pub const CSV_HEADER: &str = "seq,id,ts,actor,principal,route,level,outcome,request_id,prev_hash,hash";
/// The CSV trailer row's first cell.
const CSV_TRAILER: &str = "#trailer";
/// Records one page reads from the store.
pub const PAGE: usize = 500;

/// The two export formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Jsonl,
    Csv,
}

impl Format {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "jsonl" => Some(Self::Jsonl),
            "csv" => Some(Self::Csv),
            _ => None,
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Self::Jsonl => "application/x-ndjson",
            Self::Csv => "text/csv; charset=utf-8",
        }
    }
}

/// One exported record's fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainRecord {
    pub seq: u64,
    pub id: i64,
    pub ts: i64,
    pub actor: String,
    pub principal: Option<String>,
    pub route: String,
    pub level: String,
    pub outcome: String,
    pub request_id: String,
    pub prev_hash: String,
}

fn quoted(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into())
}

/// The canonical form a record's hash is taken over: a JSON object with the fields in this fixed
/// order and no whitespace. Independent of the export format.
pub fn canonical(r: &ChainRecord) -> String {
    format!(
        "{{\"seq\":{},\"id\":{},\"ts\":{},\"actor\":{},\"principal\":{},\"route\":{},\"level\":{},\"outcome\":{},\"request_id\":{},\"prev_hash\":{}}}",
        r.seq,
        r.id,
        r.ts,
        quoted(&r.actor),
        r.principal.as_deref().map_or_else(|| "null".to_string(), quoted),
        quoted(&r.route),
        quoted(&r.level),
        quoted(&r.outcome),
        quoted(&r.request_id),
        quoted(&r.prev_hash),
    )
}

/// The record's hash: lowercase hex SHA-256 of [`canonical`].
pub fn record_hash(r: &ChainRecord) -> String {
    Sha256::digest(canonical(r).as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn csv_cell(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

/// Writes one export, record by record, carrying the chain.
#[derive(Debug, Clone)]
pub struct ChainWriter {
    format: Format,
    seq: u64,
    head: String,
    since_ms: Option<i64>,
    until_ms: Option<i64>,
}

impl ChainWriter {
    pub fn new(format: Format, since_ms: Option<i64>, until_ms: Option<i64>) -> Self {
        Self { format, seq: 0, head: GENESIS.to_string(), since_ms, until_ms }
    }

    /// What comes before the first record (the CSV header; nothing for JSON Lines).
    pub fn header(&self) -> String {
        match self.format {
            Format::Jsonl => String::new(),
            Format::Csv => format!("{CSV_HEADER}\n"),
        }
    }

    /// One record, chained to the previous one, with its line ending.
    pub fn line(&mut self, entry: &AuditEntry) -> String {
        self.seq += 1;
        let record = ChainRecord {
            seq: self.seq,
            id: entry.id,
            ts: entry.record.ts,
            actor: entry.record.actor.clone(),
            principal: entry.record.principal.clone(),
            route: entry.record.route.clone(),
            level: entry.record.level.clone(),
            outcome: entry.record.outcome.clone(),
            request_id: entry.record.request_id.clone(),
            prev_hash: self.head.clone(),
        };
        let hash = record_hash(&record);
        let line = match self.format {
            Format::Jsonl => {
                let canonical = canonical(&record);
                // The canonical object with `hash` appended, still one line.
                format!("{},\"hash\":\"{hash}\"}}\n", &canonical[..canonical.len() - 1])
            }
            Format::Csv => format!(
                "{},{},{},{},{},{},{},{},{},{},{}\n",
                record.seq,
                record.id,
                record.ts,
                csv_cell(&record.actor),
                csv_cell(record.principal.as_deref().unwrap_or("")),
                csv_cell(&record.route),
                csv_cell(&record.level),
                csv_cell(&record.outcome),
                csv_cell(&record.request_id),
                record.prev_hash,
                hash,
            ),
        };
        self.head = hash;
        line
    }

    /// The trailer: the record count and the head hash. Its absence marks a truncated export.
    pub fn trailer(&self) -> String {
        match self.format {
            Format::Jsonl => format!(
                "{}\n",
                json!({
                    "trailer": true,
                    "records": self.seq,
                    "head_hash": self.head,
                    "since_ms": self.since_ms,
                    "until_ms": self.until_ms,
                    "chain": "sha256-v1",
                })
            ),
            Format::Csv => format!("{CSV_TRAILER},records={},head_hash={},chain=sha256-v1\n", self.seq, self.head),
        }
    }
}

/// What [`verify`] found in a well-formed, complete export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    pub format: Format,
    pub records: u64,
    pub head_hash: String,
    /// Store row ids missing between consecutive exported records (a time window or retention can
    /// leave them; listed so an auditor can ask why). Informational, not a failure.
    pub id_gaps: u64,
}

/// Why an export failed verification. Names the first record that broke (by `seq`), never content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    /// A line could not be read as a record of this format.
    Malformed { line: usize },
    /// `seq` did not follow the previous one.
    Sequence { seq: u64 },
    /// `prev_hash` is not the previous record's hash.
    BrokenLink { seq: u64 },
    /// The record's hash does not match its content.
    BadHash { seq: u64 },
    /// No trailer: the export was cut short.
    MissingTrailer,
    /// The trailer's count or head hash disagrees with the records.
    TrailerMismatch,
    /// Something follows the trailer.
    AfterTrailer,
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed { line } => write!(f, "line {line} is not an audit export record"),
            Self::Sequence { seq } => write!(f, "record {seq} is out of sequence (a record was removed or inserted)"),
            Self::BrokenLink { seq } => write!(f, "record {seq}'s prev_hash does not match the record before it"),
            Self::BadHash { seq } => write!(f, "record {seq}'s hash does not match its content (it was altered)"),
            Self::MissingTrailer => write!(f, "the export has no trailer: it was truncated"),
            Self::TrailerMismatch => write!(f, "the trailer's record count or head hash does not match the records"),
            Self::AfterTrailer => write!(f, "data follows the trailer"),
        }
    }
}

/// Split one CSV row (quoted cells, `""` escapes).
fn csv_cells(line: &str) -> Option<Vec<String>> {
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut chars = line.chars().peekable();
    let mut quoted = false;
    let mut was_quoted = false;
    while let Some(c) = chars.next() {
        match (quoted, c) {
            (true, '"') if chars.peek() == Some(&'"') => {
                chars.next();
                cell.push('"');
            }
            (true, '"') => quoted = false,
            (true, c) => cell.push(c),
            (false, '"') if cell.is_empty() && !was_quoted => {
                quoted = true;
                was_quoted = true;
            }
            (false, ',') => {
                cells.push(std::mem::take(&mut cell));
                was_quoted = false;
            }
            (false, c) => cell.push(c),
        }
    }
    if quoted {
        return None;
    }
    cells.push(cell);
    Some(cells)
}

fn parse_jsonl(value: &Value) -> Option<(ChainRecord, String)> {
    let text = |key: &str| value.get(key)?.as_str().map(str::to_string);
    let record = ChainRecord {
        seq: value.get("seq")?.as_u64()?,
        id: value.get("id")?.as_i64()?,
        ts: value.get("ts")?.as_i64()?,
        actor: text("actor")?,
        principal: match value.get("principal")? {
            Value::Null => None,
            Value::String(p) => Some(p.clone()),
            _ => return None,
        },
        route: text("route")?,
        level: text("level")?,
        outcome: text("outcome")?,
        request_id: text("request_id")?,
        prev_hash: text("prev_hash")?,
    };
    Some((record, text("hash")?))
}

fn parse_csv(cells: &[String]) -> Option<(ChainRecord, String)> {
    if cells.len() != 11 {
        return None;
    }
    let record = ChainRecord {
        seq: cells[0].parse().ok()?,
        id: cells[1].parse().ok()?,
        ts: cells[2].parse().ok()?,
        actor: cells[3].clone(),
        principal: Some(cells[4].clone()).filter(|p| !p.is_empty()),
        route: cells[5].clone(),
        level: cells[6].clone(),
        outcome: cells[7].clone(),
        request_id: cells[8].clone(),
        prev_hash: cells[9].clone(),
    };
    Some((record, cells[10].clone()))
}

/// Recompute an export's chain (either format, detected from its first line): every `seq`, every
/// link, every hash, and the trailer.
pub fn verify(text: &str) -> Result<VerifyReport, VerifyError> {
    let mut lines = text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()).peekable();
    let format = match lines.peek() {
        Some((_, first)) if first.trim_end() == CSV_HEADER => {
            lines.next();
            Format::Csv
        }
        _ => Format::Jsonl,
    };
    let (mut seq, mut head, mut last_id, mut id_gaps) = (0u64, GENESIS.to_string(), None::<i64>, 0u64);
    let mut trailer: Option<(u64, String)> = None;
    for (index, line) in lines {
        let number = index + 1;
        if trailer.is_some() {
            return Err(VerifyError::AfterTrailer);
        }
        // The trailer.
        match format {
            Format::Jsonl => {
                let value: Value = serde_json::from_str(line).map_err(|_| VerifyError::Malformed { line: number })?;
                if value.get("trailer") == Some(&Value::Bool(true)) {
                    let records = value.get("records").and_then(Value::as_u64).ok_or(VerifyError::Malformed { line: number })?;
                    let head_hash = value.get("head_hash").and_then(Value::as_str).ok_or(VerifyError::Malformed { line: number })?;
                    trailer = Some((records, head_hash.to_string()));
                    continue;
                }
                let (record, hash) = parse_jsonl(&value).ok_or(VerifyError::Malformed { line: number })?;
                check(&record, &hash, &mut seq, &mut head)?;
                note_gap(record.id, &mut last_id, &mut id_gaps);
            }
            Format::Csv => {
                if let Some(rest) = line.strip_prefix(CSV_TRAILER) {
                    let field = |key: &str| {
                        rest.split(',').find_map(|kv| kv.strip_prefix(&format!("{key}="))).map(str::to_string)
                    };
                    let records = field("records").and_then(|r| r.parse().ok()).ok_or(VerifyError::Malformed { line: number })?;
                    let head_hash = field("head_hash").ok_or(VerifyError::Malformed { line: number })?;
                    trailer = Some((records, head_hash));
                    continue;
                }
                let cells = csv_cells(line).ok_or(VerifyError::Malformed { line: number })?;
                let (record, hash) = parse_csv(&cells).ok_or(VerifyError::Malformed { line: number })?;
                check(&record, &hash, &mut seq, &mut head)?;
                note_gap(record.id, &mut last_id, &mut id_gaps);
            }
        }
    }
    match trailer {
        None => Err(VerifyError::MissingTrailer),
        Some((records, head_hash)) if records != seq || head_hash != head => Err(VerifyError::TrailerMismatch),
        Some(_) => Ok(VerifyReport { format, records: seq, head_hash: head, id_gaps }),
    }
}

fn check(record: &ChainRecord, hash: &str, seq: &mut u64, head: &mut String) -> Result<(), VerifyError> {
    if record.seq != *seq + 1 {
        return Err(VerifyError::Sequence { seq: record.seq });
    }
    if record.prev_hash != *head {
        return Err(VerifyError::BrokenLink { seq: record.seq });
    }
    if record_hash(record) != hash {
        return Err(VerifyError::BadHash { seq: record.seq });
    }
    *seq = record.seq;
    *head = hash.to_string();
    Ok(())
}

fn note_gap(id: i64, last: &mut Option<i64>, gaps: &mut u64) {
    if let Some(previous) = *last {
        *gaps += u64::try_from(id.saturating_sub(previous).saturating_sub(1)).unwrap_or(0);
    }
    *last = Some(id);
}

/// Parse `since_ms` / `until_ms` / `format` / `principal` from an export query string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportQuery {
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub format: Format,
    pub principal: Option<String>,
}

/// Read the query string of `GET /v1/audit/export`. `Err` is the refusal message.
pub fn export_query(raw_query: &str) -> Result<ExportQuery, String> {
    let mut query = ExportQuery { since_ms: None, until_ms: None, format: Format::Jsonl, principal: None };
    for pair in raw_query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        match key {
            "since_ms" => query.since_ms = Some(value.parse().map_err(|_| "invalid_since_ms: since_ms takes unix milliseconds.".to_string())?),
            "until_ms" => query.until_ms = Some(value.parse().map_err(|_| "invalid_until_ms: until_ms takes unix milliseconds.".to_string())?),
            "format" => query.format = Format::parse(value).ok_or_else(|| "invalid_format: format is jsonl or csv.".to_string())?,
            "principal" if !value.is_empty() => query.principal = Some(value.to_string()),
            _ => {}
        }
    }
    if let (Some(since), Some(until)) = (query.since_ms, query.until_ms) {
        if until < since {
            return Err("invalid_window: until_ms is before since_ms.".into());
        }
    }
    Ok(query)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shogun_memory::audit::{AuditEntry, AuditRecord};

    fn entries() -> Vec<AuditEntry> {
        [
            (1, 10, "agent:codex", Some("alice"), "memory.ingest", "L1", "ok"),
            (2, 20, "web:approval-page", Some("alice"), "approvals.decide", "L3", "approved"),
            (4, 30, "human:admin", None, "POST /v1/acl/grant", "admin", "ok"),
        ]
        .into_iter()
        .map(|(id, ts, actor, principal, route, level, outcome)| AuditEntry {
            id,
            record: AuditRecord::new(ts, actor, principal, route, level, outcome, &format!("req-{id}")).expect("record"),
        })
        .collect()
    }

    fn export(format: Format) -> String {
        let mut writer = ChainWriter::new(format, Some(0), None);
        let mut out = writer.header();
        for entry in entries() {
            out.push_str(&writer.line(&entry));
        }
        out.push_str(&writer.trailer());
        out
    }

    #[test]
    fn both_formats_chain_the_same_records_and_verify() {
        let jsonl = export(Format::Jsonl);
        let csv = export(Format::Csv);
        let a = verify(&jsonl).expect("jsonl verifies");
        let b = verify(&csv).expect("csv verifies");
        assert_eq!((a.records, a.format, b.format), (3, Format::Jsonl, Format::Csv));
        assert_eq!(a.head_hash, b.head_hash, "the hash is over the record, not the format");
        assert_eq!(a.id_gaps, 1, "row 3 is not in the export");
        // The first record links to genesis and every line carries its prev_hash.
        let first: Value = serde_json::from_str(jsonl.lines().next().expect("line")).expect("json");
        assert_eq!(first["prev_hash"], GENESIS);
        assert_eq!(first["seq"], 1);
        // Deterministic: the same records export byte-identically.
        assert_eq!(jsonl, export(Format::Jsonl));
    }

    #[test]
    fn any_change_drop_reorder_or_truncation_fails() {
        for format in [Format::Jsonl, Format::Csv] {
            let good = export(format);
            let lines: Vec<&str> = good.lines().collect();
            let body = if format == Format::Csv { 1 } else { 0 };
            // Altered outcome.
            let altered = good.replacen("approved", "rejected", 1);
            assert!(matches!(verify(&altered), Err(VerifyError::BadHash { seq: 2 })), "{format:?}");
            // A record dropped from the middle.
            let mut dropped = lines.clone();
            dropped.remove(body + 1);
            assert!(matches!(verify(&dropped.join("\n")), Err(VerifyError::Sequence { .. })), "{format:?}");
            // Two records swapped.
            let mut swapped = lines.clone();
            swapped.swap(body, body + 1);
            assert!(verify(&swapped.join("\n")).is_err(), "{format:?}");
            // Truncated: no trailer.
            let truncated = lines[..lines.len() - 1].join("\n");
            assert_eq!(verify(&truncated), Err(VerifyError::MissingTrailer), "{format:?}");
            // The last record dropped and the trailer kept: the head does not match.
            let mut tail = lines.clone();
            tail.remove(lines.len() - 2);
            assert!(verify(&tail.join("\n")).is_err(), "{format:?}");
        }
    }

    #[test]
    fn an_empty_window_still_has_a_trailer() {
        let writer = ChainWriter::new(Format::Jsonl, Some(5), Some(6));
        let report = verify(&writer.trailer()).expect("verifies");
        assert_eq!((report.records, report.head_hash.as_str()), (0, GENESIS));
        assert_eq!(verify(""), Err(VerifyError::MissingTrailer));
    }

    #[test]
    fn csv_cells_survive_quotes_and_commas() {
        let cells = csv_cells(r#"1,2,"a,b","say ""hi""","""#).expect("cells");
        assert_eq!(cells, vec!["1", "2", "a,b", "say \"hi\"", ""]);
        assert!(csv_cells(r#""open"#).is_none());
    }

    #[test]
    fn the_query_is_validated() {
        let q = export_query("since_ms=5&until_ms=9&format=csv&principal=alice").expect("query");
        assert_eq!(q, ExportQuery { since_ms: Some(5), until_ms: Some(9), format: Format::Csv, principal: Some("alice".into()) });
        assert_eq!(export_query("").expect("default").format, Format::Jsonl);
        assert!(export_query("format=xml").is_err());
        assert!(export_query("since_ms=x").is_err());
        assert!(export_query("since_ms=9&until_ms=5").is_err());
    }
}
