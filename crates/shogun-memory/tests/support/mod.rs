//! Fixtures shared by more than one integration-test binary. `tests/support/` is a module
//! directory, not a test target: cargo compiles only the top-level `tests/*.rs` files as binaries.
//! Each binary uses only part of this module, hence the `dead_code` allowance.
#![allow(dead_code)]

use shogun_memory::event_log::{insert, NewEvent};
use shogun_memory::meeting_index::index_session;
use shogun_memory::meeting_recaps;
use shogun_memory::session::{close, open, NewSession};
use shogun_memory::transcript_segments::{append, NewSegment, Speaker};

/// Commit `imports` × `per_import` imported claims (#391) straight into the V52 tables, every one
/// Medium (0.6), `active`, and matching "routine evidence" on both terms — the worst case for the
/// imported lane, which scans with `instr` and has no index to narrow the pass. Returns the
/// `memory_imports.id` of each import, oldest first.
pub fn insert_matching_imported_claims(
    conn: &mut rusqlite::Connection,
    imports: usize,
    per_import: usize,
) -> Vec<i64> {
    let tx = conn.transaction().unwrap();
    let mut ids = Vec::with_capacity(imports);
    for _ in 0..imports {
        tx.execute(
            "INSERT INTO memory_imports (origin, provenance, format, via, package_bytes,
                 package_xxh64, item_count, created_at)
             VALUES ('computer_history', 'imported:computer_history', 'shogun-memory-import/v1',
                     'ui', 1, 'x', ?1, 1)",
            [per_import as i64],
        )
        .unwrap();
        let import_id = tx.last_insert_rowid();
        ids.push(import_id);
        for item in 0..per_import {
            tx.execute(
                "INSERT INTO memory_import_items (import_id, package_item, section, name, summary,
                     status, confidence, requested_confidence, match_key, created_at)
                 VALUES (?1, ?2, 'goal', ?3, 'Routine evidence review for the quarterly plan',
                         'active', 0.6, 'high', ?3, 1)",
                rusqlite::params![
                    import_id,
                    format!("goals/{item}"),
                    format!("Imported routine goal {import_id}-{item}")
                ],
            )
            .unwrap();
        }
    }
    tx.commit().unwrap();
    ids
}

fn event(conn: &rusqlite::Connection, ts: i64, text: &str, hash: &str) -> i64 {
    insert(
        conn,
        &NewEvent {
            ts,
            source: "capture",
            kind: "text",
            app_bundle_id: None,
            window_title: Some("Evidence"),
            content: text,
            content_hash: hash,
            dwell_ms: 0,
            display_id: None,
            window_bounds: None,
        },
    )
    .unwrap()
}

fn weak_meeting(conn: &mut rusqlite::Connection, ordinal: usize, text: &str) -> i64 {
    let started_at = 1_000 + ordinal as i64 * 10;
    let title = format!("Routine sync {ordinal}");
    let session_id = open(
        conn,
        &NewSession {
            kind: "meeting",
            started_at,
            title: Some(&title),
            app_bundle_id: None,
            calendar_occurrence_id: None,
            confidence: 0.9,
            provenance: "{}",
        },
    )
    .unwrap();
    append(
        conn,
        &NewSegment {
            session_id,
            ts: started_at + 1,
            speaker: Speaker::Other,
            text,
            confidence: 0.9,
        },
        started_at + 1,
    )
    .unwrap();
    close(conn, session_id, started_at + 2).unwrap();
    index_session(conn, session_id).unwrap();
    meeting_recaps::save(
        conn,
        session_id,
        text,
        "[]",
        "[]",
        None,
        "test-model",
        started_at + 3,
    )
    .unwrap();
    session_id
}

/// One meeting whose recap and transcript match "routine evidence", with `event_count` captured
/// events in its session that match too: every one is persisted evidence the federated search
/// validates before it may call the meeting answer-eligible. 100 × 30 is the worst case the
/// local-search SLO gate uses (`limit` 100 candidates, each carrying 30 evidence rows).
pub fn evidence_heavy_meeting(
    conn: &mut rusqlite::Connection,
    ordinal: usize,
    event_count: usize,
) -> i64 {
    let session_id = weak_meeting(conn, ordinal, "Routine evidence review");
    for event_ordinal in 0..event_count {
        let event_id = event(
            conn,
            10_000 + (ordinal * event_count + event_ordinal) as i64,
            "Routine evidence review",
            &format!("evidence-heavy-{ordinal}-{event_ordinal}"),
        );
        conn.execute(
            "UPDATE event_log SET session_id = ?1 WHERE id = ?2",
            rusqlite::params![session_id, event_id],
        )
        .unwrap();
    }
    session_id
}

/// Add `count` more evidence events to the session of meeting `ordinal` (made by
/// [`evidence_heavy_meeting`]), numbered from `from`. Lets a test grow the evidence per candidate
/// on one database instead of rebuilding it. Timestamps stay before the search's `now` (20,000).
///
/// Neither their text nor their title contains the query words, so growing them grows only the evidence each
/// meeting carries, not the raw-event lexical lane: that lane reads a capped number of postings
/// per term (`lexical.rs`), so its work plateaus by design and would blur a linearity measurement
/// of evidence validation.
pub fn add_session_evidence(
    conn: &mut rusqlite::Connection,
    session_id: i64,
    ordinal: usize,
    from: usize,
    count: usize,
) {
    let tx = conn.transaction().unwrap();
    for index in from..from + count {
        let event_id = insert(
            &tx,
            &NewEvent {
                ts: 10_000 + (ordinal * 64 + index) as i64,
                source: "capture",
                kind: "text",
                app_bundle_id: None,
                window_title: Some("Minutes"),
                content: "Attached minutes page",
                content_hash: &format!("evidence-extra-{ordinal}-{index}"),
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap();
        tx.execute(
            "UPDATE event_log SET session_id = ?1 WHERE id = ?2",
            rusqlite::params![session_id, event_id],
        )
        .unwrap();
    }
    tx.commit().unwrap();
}

/// Deterministic generator for the Visual Recall fixture (a fixed seed, so every run builds the
/// same database).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// #727 fixture: `events` Accessibility-style events (~600 chars of Zipf-skewed synthetic words,
/// some Japanese, a few planted phrases) in one transaction, and a Visual Recall frame on every
/// `events / frames`-th one, the way the capture path links them: the frame's OCR event, the
/// Accessibility event from the same poll (absent for a quarter of frames), and a BLOB
/// (8–24 KB, smaller than a real frame to keep the fixture small) so every frame row carries
/// overflow pages like a real one.
pub fn visual_recall_corpus(conn: &rusqlite::Connection, events: usize, frames: usize) {
    const SYLLABLES: &[&str] = &[
        "ka", "ri", "to", "men", "dor", "vel", "quin", "sta", "ber", "lo", "nex", "tra", "mor",
        "pli", "gan", "ser", "vo", "lum", "tek", "rash", "ob", "und", "fel", "cor", "ish", "ent",
    ];
    const COMMON: &[&str] = &[
        "status", "update", "budget", "review", "sprint", "ticket", "vendor", "customer",
        "invoice", "schedule", "latency", "dashboard", "release", "meeting", "deploy", "owner",
    ];
    const JAPANESE: &[&str] = &["確認します", "ミーティング", "議事録", "スケジュール", "予定"];
    const TITLES: &[&str] = &["Inbox", "Standup", "Design review", "Terminal", "Budget.xlsx"];
    let mut rng = Lcg(727);
    let vocabulary: Vec<String> = (0..20_000)
        .map(|_| (0..2 + rng.below(3)).map(|_| SYLLABLES[rng.below(SYLLABLES.len())]).collect())
        .collect();
    let every = (events / frames).max(1);
    conn.execute_batch("BEGIN").unwrap();
    {
        let mut event = conn
            .prepare(
                "INSERT INTO event_log (ts, source, kind, window_title, content, content_hash,
                     last_seen_at)
                 VALUES (?1, ?2, 'text', ?3, ?4, ?5, ?1)",
            )
            .unwrap();
        let mut frame = conn
            .prepare(
                "INSERT INTO screen_frames (created_at_ms, event_id, accessibility_event_id,
                     app_bundle_id, window_title, display_id, width, height, mime, bytes)
                 VALUES (?1, ?2, ?3, 'com.apple.Safari', 'w', 1, 1512, 982, 'image/jpeg', ?4)",
            )
            .unwrap();
        let mut previous = None;
        for n in 0..events {
            let ts = 1_770_940_800_000 + n as i64 * 60_000;
            let mut words = Vec::with_capacity(140);
            let mut chars = 0;
            while chars < 600 {
                let u = (rng.next() % 1_000_000) as f64 / 1_000_000.0;
                let word = match rng.below(20) {
                    0..=4 => COMMON[rng.below(COMMON.len())],
                    5 => JAPANESE[rng.below(JAPANESE.len())],
                    // Cubed uniform: a few words very common, a long tail of rare ones.
                    _ => vocabulary[((u * u * u) * vocabulary.len() as f64) as usize].as_str(),
                };
                chars += word.len() + 1;
                words.push(word);
            }
            match rng.below(1_000) {
                0 => words.push("zephyrquota"),
                1..=20 => words.push("renewal pricing"),
                21..=30 => words.push("請求書の再発行"),
                _ => {}
            }
            let with_frame = n % every == every - 1 && n / every < frames;
            let source = if with_frame { "screen_ocr" } else { "capture" };
            event
                .execute(rusqlite::params![
                    ts,
                    source,
                    TITLES[rng.below(TITLES.len())],
                    words.join(" "),
                    format!("vr727-{n}")
                ])
                .unwrap();
            let id = conn.last_insert_rowid();
            if with_frame {
                let accessibility = if rng.below(4) == 0 { None } else { previous };
                let mut jpeg = vec![0_u8; 8_000 + rng.below(16_000)];
                for byte in jpeg.iter_mut().step_by(61) {
                    *byte = rng.next() as u8;
                }
                frame.execute(rusqlite::params![ts, id, accessibility, jpeg]).unwrap();
            }
            previous = Some(id);
        }
    }
    conn.execute_batch("COMMIT").unwrap();
}
