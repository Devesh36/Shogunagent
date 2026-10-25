//! The job around the #714 primitives: refusal, stop and resume, disk running low mid-run, the
//! shared status, and the desktop driver taking a face's request.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::{params, Connection};
use shogun_mcp::search_index_api::{self, Action, SearchIndexPaths, REQUEST_TTL_MS};
use shogun_memory::fts_compact::{self, Move, FTS_TABLES};
use shogun_memory::DbKey;

use super::*;
use crate::daemon::{Clock, Db};

const GIB: u64 = 1 << 30;

fn key() -> DbKey {
    DbKey::new([0x3c; 32])
}

fn clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    })
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("shogun_compact_{}_{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Free space that is whatever the test says, and counts how often it was asked.
struct FakeFree {
    bytes: Option<u64>,
    /// After this many calls, report `low` instead.
    low_after: Option<(u32, u64)>,
    calls: AtomicU32,
}

impl FakeFree {
    fn plenty() -> Self {
        Self { bytes: Some(500 * GIB), low_after: None, calls: AtomicU32::new(0) }
    }
    fn fixed(bytes: Option<u64>) -> Self {
        Self { bytes, low_after: None, calls: AtomicU32::new(0) }
    }
}

impl FreeSpace for FakeFree {
    fn available_bytes(&self) -> Option<u64> {
        let n = self.calls.fetch_add(1, Ordering::Relaxed);
        match self.low_after {
            Some((after, low)) if n >= after => Some(low),
            _ => self.bytes,
        }
    }
}

fn fast() -> Options {
    Options { pace: false, max_pages: 16, min_pages: 8, ..Options::default() }
}

/// Deterministic synthetic capture text (words, hex ids, a CJK run): no real user data.
fn text(i: usize) -> String {
    const WORDS: &[&str] = &[
        "invoice", "deploy", "roadmap", "latency", "renewal", "forecast", "kubernetes", "review",
        "customer", "contract", "migration", "schedule",
    ];
    let mut state = (i as u64 + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let mut next = move |n: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % n
    };
    let mut out = Vec::with_capacity(160);
    for _ in 0..150 {
        match next(10) {
            0..=5 => out.push(WORDS[next(WORDS.len() as u64) as usize].to_string()),
            6..=8 => out.push(format!("{:08x}", next(u64::from(u32::MAX)))),
            _ => out.push("請求書の確認".to_string()),
        }
    }
    out.join(" ")
}

/// An encrypted database with a fragmented `event_fts`: capture-sized batches through the
/// `event_log` triggers, then older rows re-indexed (unchanged text, fresh postings).
fn seeded(dir: &Path, rows: usize) -> PathBuf {
    let path = dir.join("memory.db");
    let conn = shogun_memory::open_encrypted(&path, &key()).unwrap();
    for batch in (0..rows).collect::<Vec<_>>().chunks(12) {
        let tx = conn.unchecked_transaction().unwrap();
        for &i in batch {
            tx.execute(
                "INSERT INTO event_log (ts, source, kind, window_title, content, content_hash, last_seen_at)
                 VALUES (?1, 'capture', 'text', ?2, ?3, ?4, ?1)",
                params![1_000 + i as i64, format!("window {}", i % 9), text(i), format!("h{i}")],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }
    for round in 0..10 {
        conn.execute("UPDATE event_log SET content = content WHERE id % ?1 = 0", [round + 4]).unwrap();
    }
    drop(conn);
    path
}

fn open(path: &Path) -> Db {
    Db::open_encrypted(path, &key(), clock()).unwrap()
}

fn raw(path: &Path) -> Connection {
    shogun_memory::open_encrypted(path, &key()).unwrap()
}

fn ranked(conn: &Connection, query: &str) -> Vec<(i64, f64)> {
    let mut stmt = conn
        .prepare("SELECT rowid, bm25(event_fts) FROM event_fts WHERE event_fts MATCH ?1 ORDER BY rank, rowid")
        .unwrap();
    let rows = stmt.query_map([query], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    rows.map(|r| r.unwrap()).collect()
}

const QUERIES: &[&str] = &["\"kubernetes\"", "\"invoice\" OR \"renewal\"", "\"請求書\"", "\"oic\""];

fn results(path: &Path) -> Vec<Vec<(i64, f64)>> {
    let conn = raw(path);
    QUERIES.iter().map(|q| ranked(&conn, q)).collect()
}

fn structure(path: &Path) -> Option<Vec<u8>> {
    fts_compact::structure_record(&raw(path), "event_fts").unwrap()
}

#[test]
fn required_free_space_is_a_gib_or_a_tenth_of_the_database() {
    assert_eq!(required_free_bytes(0), GIB);
    assert_eq!(required_free_bytes(4 * GIB), GIB, "3.5 GB-class databases: the floor");
    assert_eq!(required_free_bytes(20 * GIB), 2 * GIB, "20 GB: 10%");
}

#[test]
fn a_run_stops_between_steps_and_the_next_one_finishes_with_identical_results() {
    let dir = temp_dir("resume");
    let path = seeded(&dir, 700);
    let before_results = results(&path);
    assert!(before_results.iter().all(|r| !r.is_empty()), "every query must match");
    let db = open(&path);
    assert_ne!(db.fts_compact_plan("event_fts").unwrap().next, Move::Done, "fixture is fragmented");

    // Stop after three steps, as Settings' Stop would.
    let cancel = AtomicBool::new(false);
    let mut seen = Vec::new();
    let first = run(&db, &FakeFree::plenty(), &cancel, &fast(), &mut |status| {
        seen.push(status.clone());
        if status.steps >= 3 {
            cancel.store(true, Ordering::Relaxed);
        }
    });
    assert_eq!(first.state, JobState::Stopped);
    assert_eq!(first.steps, 3);
    assert!(first.estimated_remaining_steps.is_some_and(|n| n > 0), "{first:?}");
    assert!(first.finished_at_ms.is_some() && first.before.is_some() && first.after.is_some());
    assert_eq!(seen.first().map(|s| s.steps), Some(0), "an initial status before the first step");
    assert!(seen.iter().all(|s| s.state == JobState::Running));
    assert_eq!(results(&path), before_results, "a stopped job leaves an index that answers the same");

    // Resume: nothing was stored but the index itself.
    let resume = AtomicBool::new(false);
    let second = run(&db, &FakeFree::plenty(), &resume, &fast(), &mut |_| {});
    assert_eq!(second.state, JobState::Completed, "{second:?}");
    assert_eq!(second.estimated_remaining_steps, Some(0));
    for table in FTS_TABLES {
        assert_eq!(db.fts_compact_plan(table).unwrap().next, Move::Done, "{table}");
    }
    let (start, end) = (first.before.unwrap(), second.after.unwrap());
    assert!(end.used_bytes < start.used_bytes, "used bytes shrink: {start:?} -> {end:?}");
    // A new database is auto_vacuum = INCREMENTAL, so the freed pages went back to the disk.
    assert!(second.released_pages > 0, "{second:?}");
    assert!(end.file_bytes < start.file_bytes, "the file shrinks: {start:?} -> {end:?}");
    assert_eq!(results(&path), before_results, "match sets and bm25 order unchanged");

    // Nothing left: the next run does no step at all.
    let third = run(&db, &FakeFree::plenty(), &AtomicBool::new(false), &fast(), &mut |_| {});
    assert_eq!(third.state, JobState::NothingToDo);
    assert_eq!(third.steps, 0);

    // The status is counts, sizes and names: no indexed text.
    let json = serde_json::to_string(&second).unwrap();
    for word in ["kubernetes", "invoice", "請求書", "window"] {
        assert!(!json.contains(word), "{word} leaked into the status");
    }
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn low_disk_refuses_to_start_and_touches_nothing() {
    let dir = temp_dir("refuse");
    let path = seeded(&dir, 200);
    let paths = SearchIndexPaths::for_db(&path);
    let untouched = structure(&path);
    let db = open(&path);

    let low = FakeFree::fixed(Some(GIB - 1));
    let status = run(&db, &low, &AtomicBool::new(false), &fast(), &mut |_| panic!("no step may run"));
    assert_eq!(status.state, JobState::RefusedLowDisk);
    assert_eq!(status.steps, 0);
    let unknown = FakeFree::fixed(None);
    let status = run(&db, &unknown, &AtomicBool::new(false), &fast(), &mut |_| panic!("no step"));
    assert_eq!(status.state, JobState::RefusedDiskUnknown);
    assert_eq!(structure(&path), untouched, "a refused start merges nothing");

    // A face hears the refusal at once, and nothing is queued for the app.
    let error = request_start(&db, &paths, &low).unwrap_err();
    assert!(error.starts_with("insufficient_disk:"), "{error}");
    assert!(error.contains(&GIB.to_string()), "the required size is stated: {error}");
    assert!(request_start(&db, &paths, &unknown).unwrap_err().starts_with("disk_unknown:"));
    assert!(!paths.requests.exists());
    request_start(&db, &paths, &FakeFree::plenty()).unwrap();
    assert_eq!(search_index_api::pending(&paths.requests, now_ms(&db)), Some(Action::Start));
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn disk_running_low_mid_run_stops_between_steps() {
    let dir = temp_dir("low_mid_run");
    let path = seeded(&dir, 700);
    let before_results = results(&path);
    let db = open(&path);
    // Plenty for the preflight, the initial check and four steps; then below the floor.
    let shrinking = FakeFree { bytes: Some(500 * GIB), low_after: Some((5, 10 * 1024 * 1024)), calls: AtomicU32::new(0) };
    let status = run(&db, &shrinking, &AtomicBool::new(false), &fast(), &mut |_| {});
    assert_eq!(status.state, JobState::LowDisk, "{status:?}");
    assert!(status.steps >= 1 && status.steps <= 4, "{status:?}");
    assert_eq!(results(&path), before_results);
    let resumed = run(&db, &FakeFree::plenty(), &AtomicBool::new(false), &fast(), &mut |_| {});
    assert_eq!(resumed.state, JobState::Completed);
    assert_eq!(results(&path), before_results);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_step_is_short_and_paced_steps_rest_between() {
    let dir = temp_dir("paced");
    let path = seeded(&dir, 400);
    let db = open(&path);
    let options = Options { min_rest: Duration::from_millis(20), rest_factor: 4, ..Options::default() };
    let mut busy = Duration::ZERO;
    let mut last = Instant::now();
    let mut gaps = Vec::new();
    let started = Instant::now();
    let status = run(&db, &FakeFree::plenty(), &AtomicBool::new(false), &options, &mut |s| {
        if s.steps > 0 {
            busy += Duration::from_millis(s.last_step_ms);
            gaps.push(last.elapsed());
        }
        last = Instant::now();
    });
    let wall = started.elapsed();
    assert_eq!(status.state, JobState::Completed, "{status:?}");
    assert!(status.steps >= 3, "{status:?}");
    // Generous for an unoptimised build; the product bound is the #714 238 ms per 64-page step.
    assert!(status.max_step_ms < 2_000, "longest step {} ms", status.max_step_ms);
    assert!(status.pages_per_step >= 8 && status.pages_per_step <= 64);
    // Pacing: the job held the connection for at most about a third of the wall time.
    assert!(busy * 3 <= wall + Duration::from_millis(50), "busy {busy:?} of {wall:?}");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_shared_status_reads_queued_interrupted_and_the_database() {
    let dir = temp_dir("status");
    let path = seeded(&dir, 120);
    let paths = SearchIndexPaths::for_db(&path);
    let db = open(&path);
    let free = FakeFree::plenty();

    let idle = status_json(&db, &paths, &free, None);
    assert_eq!(idle["job"]["state"], "idle");
    assert_eq!(idle["pending_request"], serde_json::Value::Null);
    assert_eq!(idle["index"]["tables"].as_array().unwrap().len(), 3);
    assert_eq!(idle["index"]["needs_compaction"], true);
    assert!(idle["index"]["estimated_steps"].as_u64().unwrap() > 0);
    assert_eq!(idle["database"]["auto_vacuum"], "incremental");
    assert_eq!(idle["database"]["free_pages_return_to_disk"], true);
    assert_eq!(idle["disk"]["required_bytes"], GIB);
    assert_eq!(idle["disk"]["enough"], true);

    // A start the app has not taken yet reads as queued; a stop cancels it.
    request_start(&db, &paths, &free).unwrap();
    assert_eq!(status_json(&db, &paths, &free, None)["job"]["state"], "queued");
    request_stop(&db, &paths).unwrap();
    let stopped = status_json(&db, &paths, &free, None);
    assert_eq!(stopped["job"]["state"], "idle");
    assert_eq!(stopped["pending_request"], "stop");

    // A "running" left behind by an app that quit mid-run reads as interrupted.
    let now = now_ms(&db);
    let mut left = JobStatus { state: JobState::Running, updated_at_ms: now - 60_000, steps: 7, ..JobStatus::idle() };
    search_index_api::write_status(&paths.status, &serde_json::to_value(&left).unwrap()).unwrap();
    assert_eq!(status_json(&db, &paths, &free, None)["job"]["state"], "interrupted");
    left.updated_at_ms = now;
    search_index_api::write_status(&paths.status, &serde_json::to_value(&left).unwrap()).unwrap();
    assert_eq!(status_json(&db, &paths, &free, None)["job"]["state"], "running", "a live heartbeat");

    // A start nobody took within ten minutes is not reported as queued.
    let _ = std::fs::remove_file(&paths.status);
    search_index_api::request(&paths.requests, Action::Start, now - REQUEST_TTL_MS - 1).unwrap();
    assert_eq!(status_json(&db, &paths, &free, None)["job"]["state"], "idle");

    let low = status_json(&db, &paths, &FakeFree::fixed(Some(1)), None);
    assert_eq!(low["disk"]["enough"], false);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

fn wait_for(driver: &Driver, state: &str) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let status = driver.status_json();
        if status["job"]["state"] == state {
            return status;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {state}: {status}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn the_driver_runs_a_request_from_another_face_and_never_resumes_on_its_own() {
    let dir = temp_dir("driver");
    let path = seeded(&dir, 300);
    let paths = SearchIndexPaths::for_db(&path);

    // A previous launch died mid-run: the driver republishes it as interrupted, and does not run.
    let stale = JobStatus { state: JobState::Running, updated_at_ms: 1, steps: 2, ..JobStatus::idle() };
    search_index_api::write_status(&paths.status, &serde_json::to_value(&stale).unwrap()).unwrap();
    let db = open(&path);
    let untouched = structure(&path);
    let driver = Driver::spawn(db.clone(), paths.clone(), Arc::new(FakeFree::plenty()), fast());
    let published = search_index_api::read_status(&paths.status).unwrap();
    assert_eq!(published["state"], "interrupted");
    std::thread::sleep(Duration::from_millis(2_300));
    assert_eq!(structure(&path), untouched, "nothing runs without a request");

    // An API face (another process) only writes the request file; the driver takes it.
    request_start(&db, &paths, &FakeFree::plenty()).unwrap();
    let done = wait_for(&driver, "completed");
    assert!(done["job"]["steps"].as_u64().unwrap() > 0);
    assert_eq!(done["index"]["needs_compaction"], false);
    assert!(!paths.requests.exists(), "the request was taken");
    let published = search_index_api::read_status(&paths.status).unwrap();
    assert_eq!(published["state"], "completed", "every face reads the final status");

    // In process (Settings): start on a compact index is a no-op run.
    driver.start().unwrap();
    wait_for(&driver, "nothing_to_do");
    driver.stop().unwrap();
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
