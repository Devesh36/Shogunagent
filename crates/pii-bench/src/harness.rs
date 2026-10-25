//! Running a case through the real write path, and checking where it landed.
//!
//! Deliberately uses `event_log::insert` rather than calling `redact()` directly. Calling the
//! redactor would re-test what 39 unit tests already cover; the open question is whether the
//! redactor is **reached** from every column, and only the real insert path can answer that.
//!
//! Each case is written twice — once through `content`, once through `window_title` — and the row
//! is then read back and the FTS index queried. Both destinations are checked because the FTS
//! trigger copies the two columns independently, so a masked body says nothing about the title
//! beside it.

use rusqlite::Connection;
use serde::Serialize;

use crate::corpus::Case;

/// The column a payload is written through.
///
/// Not a detail — it is the independent variable. PII-001 exists precisely because `content` is
/// sanitized and `window_title` is not, and no test that fixed the column could have found it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Boundary {
    Content,
    WindowTitle,
    /// The third-party handoff: `shogun_redact::redact_egress`, the function every prompt
    /// builder in `shogun-core` runs stored or captured text through before it leaves — Batch
    /// consolidation / brief / mail triage, chat and draft context, tool results, transcripts
    /// (the call sites are listed on that function; `shogun-core` has a per-lane test for each).
    ///
    /// Not a database column at all, which is the point. The DB path and the egress path have
    /// different jobs; scoring only the DB path would make every national-ID and card result a
    /// statement about storage, not about what leaves the machine.
    Egress,
}

impl Boundary {
    pub fn as_str(self) -> &'static str {
        match self {
            Boundary::Content => "content",
            Boundary::WindowTitle => "window_title",
            Boundary::Egress => "egress",
        }
    }

    /// True for boundaries that leave the device.
    pub fn is_egress(self) -> bool {
        matches!(self, Boundary::Egress)
    }

    pub const ALL: [Boundary; 3] = [Boundary::Content, Boundary::WindowTitle, Boundary::Egress];
}

/// What happened to one case at one boundary.
#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    pub case_id: &'static str,
    pub category: crate::corpus::Category,
    pub expectation: crate::corpus::Expectation,
    pub boundary: Boundary,
    /// The needle was still in the stored row.
    pub survived_in_row: bool,
    /// The needle was still findable through the FTS index.
    ///
    /// Tracked separately because a derived index can leak what the base table masked — and
    /// because FTS is what the product searches, so a hit here is user-visible.
    pub survived_in_fts: bool,
    /// The row changed at all — proof the gate ran, even when it did not remove the needle.
    pub row_was_modified: bool,
}

impl Outcome {
    /// Survived anywhere it should not have.
    pub fn leaked(&self) -> bool {
        self.survived_in_row || self.survived_in_fts
    }
}

/// Write one case through one boundary and observe where the needle ends up.
fn probe(conn: &Connection, case: &Case, boundary: Boundary, ts: i64) -> rusqlite::Result<Outcome> {
    if boundary.is_egress() {
        return Ok(probe_egress(case));
    }
    let filler = "routine standup note about the release schedule";
    let (content, title) = match boundary {
        Boundary::Content => (case.payload, filler),
        // Unreachable: handled above. Matching it explicitly keeps this exhaustive, so adding a
        // fourth boundary is a compile error rather than a silently mis-probed case.
        Boundary::Egress => (case.payload, filler),
        Boundary::WindowTitle => (filler, case.payload),
    };

    let hash = shogun_memory::event_log::content_hash(content);
    let id = shogun_memory::event_log::insert(
        conn,
        &shogun_memory::event_log::NewEvent {
            ts,
            source: "capture",
            kind: "text",
            app_bundle_id: Some("com.apple.Safari"),
            window_title: Some(title),
            content,
            content_hash: &hash,
            dwell_ms: 0,
            display_id: None,
            window_bounds: None,
        },
    )?;

    let (stored_content, stored_title): (String, Option<String>) = conn.query_row(
        "SELECT content, window_title FROM event_log WHERE id = ?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;

    let stored = match boundary {
        Boundary::Content | Boundary::Egress => stored_content.clone(),
        Boundary::WindowTitle => stored_title.clone().unwrap_or_default(),
    };
    let survived_in_row = stored.contains(case.needle);
    let row_was_modified = stored != case.payload;

    // Query FTS directly for the needle. The trigram tokenizer makes a raw substring a usable
    // MATCH term; quoting keeps operator characters (`-`, `/`, `=`) from being parsed as syntax.
    let escaped = case.needle.replace('"', "");
    let survived_in_fts: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM event_fts WHERE event_fts MATCH ?1 AND rowid = ?2)",
            rusqlite::params![format!("\"{escaped}\""), id],
            |r| r.get(0),
        )
        .unwrap_or(false);

    Ok(Outcome {
        case_id: case.id,
        category: case.category,
        expectation: crate::corpus::expectation_at(case, false),
        boundary,
        survived_in_row,
        survived_in_fts,
        row_was_modified,
    })
}

/// Push one case through the egress redactor and observe what comes out.
///
/// No database is involved: this boundary is a pure function over text, and routing it through
/// `event_log` would measure the DB gate a second time instead of the egress gate once.
fn probe_egress(case: &Case) -> Outcome {
    let masked = shogun_redact::redact_egress(case.payload);
    Outcome {
        case_id: case.id,
        category: case.category,
        // The expectation is boundary-dependent: a card must survive the DB write and must not
        // survive this.
        expectation: crate::corpus::expectation_at(case, true),
        boundary: Boundary::Egress,
        survived_in_row: masked.contains(case.needle),
        // There is no index on this path. Reported as `false` rather than `None` so the field
        // keeps one meaning across boundaries; `leaked()` is then simply "survived".
        survived_in_fts: false,
        row_was_modified: masked.as_ref() != case.payload,
    }
}

/// Run every case through every boundary against a fresh in-memory database.
///
/// In-memory is correct here: this measures *what survives*, which is a property of the code path,
/// not of the filesystem. Latency is measured separately, on disk, where fsync is real.
pub fn run_all() -> rusqlite::Result<Vec<Outcome>> {
    let conn = shogun_memory::open_in_memory()
        .map_err(|e| rusqlite::Error::InvalidPath(std::path::PathBuf::from(e.to_string())))?;
    let mut out = Vec::with_capacity(crate::corpus::CASES.len() * Boundary::ALL.len());
    let mut ts = 1_000i64;
    for case in crate::corpus::CASES {
        for boundary in Boundary::ALL {
            ts += 60_000;
            out.push(probe(&conn, case, boundary, ts)?);
        }
    }
    Ok(out)
}
