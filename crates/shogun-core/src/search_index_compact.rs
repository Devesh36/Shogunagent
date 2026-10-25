//! "Compact search index": the user-triggered, resumable, bounded FTS5 compaction job (#714 step
//! 3, `docs/fts-storage-714.md` §7 Step B). Feature `db`.
//!
//! What it does: for each trigram index (`event_fts`, then `search_documents_fts`, then
//! `knowledge_entities_fts`) it merges every segment into one, in small steps
//! ([`shogun_memory::fts_compact`]). That drops the stale postings re-indexed rows left behind and
//! rewrites leaves written before V57 at `pgsz` 3960, so they stop spilling into SQLCipher overflow
//! pages. Search results are unchanged: the merge rewrites the same postings.
//!
//! How it stays out of the way:
//! - **Bounded steps.** Each step is one `'merge'` command in its own transaction, writing at most
//!   [`Options::max_pages`] (64) leaf pages; #714 measured ≤ 238 ms and ≤ 16 MB of WAL per 64-page
//!   step on a 747 MB index. The step size halves when a step runs past ~250 ms and grows back
//!   below ~50 ms, so a slower Mac holds the connection for less, not longer.
//! - **Paced.** After each step the job rests at least [`Options::rest_factor`] times as long as
//!   the step took (and at least [`Options::min_rest`]), so it keeps the shared connection at most
//!   ~20% of the time: capture and search wait for at most one step, and CPU use while it runs
//!   stays around a fifth of one core. It never runs on its own: no schedule, no idle trigger, no
//!   resume at launch. When it is not running, the app's only cost is one `stat` every two seconds.
//! - **Stoppable and resumable.** Stop takes effect between steps. Nothing is stored about
//!   progress: FTS5 keeps a merge in progress in its structure record, and the next run continues
//!   it (a positive `'merge'`), so a stop, a quit or a crash loses nothing.
//! - **Disk-checked.** It refuses to start, and stops between steps, when the database's volume has
//!   less than [`required_free_bytes`] free: max(1 GiB, 10% of the database file).
//!
//! Where the freed space goes: merged-away pages move to SQLite's free list. On a database with
//! `auto_vacuum = INCREMENTAL` (every database created since that pragma landed) the job then hands
//! them back to the filesystem in bounded `incremental_vacuum` slices. A legacy `auto_vacuum = NONE`
//! database keeps them in the file for future growth; shrinking the file itself needs a `VACUUM`
//! (about the database's size in free disk, one long transaction), which this job never runs — that
//! is a separate decision. The status says which case applies.
//!
//! Every face shares this module: the desktop owns the job ([`Driver`]); the Memory API faces
//! request a start or stop through [`shogun_mcp::search_index_api`] and read the same
//! [`status_json`]. Every number reported is a count, a size, a duration or a time.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use shogun_mcp::search_index_api::{self, Action, SearchIndexPaths, RUNNING_STALE_MS};
use shogun_memory::fts_compact::{DbPages, Move, TableState, FTS_TABLES};

use crate::daemon::Db;

/// Free disk the job needs whatever the database's size: every step's WAL (≤ 16 MB measured) is
/// checkpointed back into the file at the next commit, and a merge trims the leaves it consumed as
/// it goes, so the file did not grow at all during the #714 loop. The floor covers the case where
/// the WAL cannot be reset for a while (another process holding a read snapshot: ~64 steps' worth
/// of WAL), pages the free list cannot absorb yet, and the user's own work on a nearly full disk.
pub const MIN_FREE_BYTES: u64 = 1 << 30;
/// On a large database the floor scales with it: 10% of the file.
pub const MIN_FREE_DB_PERCENT: u64 = 10;
/// Consecutive failed DB calls (e.g. another process holding the write lock past the busy
/// timeout) before the job gives up with `failed`. A new start resumes where it stopped.
const MAX_CONSECUTIVE_FAILURES: u32 = 5;
/// How often the desktop's driver looks for a start / stop request when idle.
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// How often a running job republishes its status and looks for a stop from another face.
const PUBLISH_INTERVAL: Duration = Duration::from_secs(1);
/// Sleep granularity while resting, so a stop is noticed quickly.
const REST_SLICE: Duration = Duration::from_millis(50);

/// The free space the job requires for a database file of `db_file_bytes`.
pub fn required_free_bytes(db_file_bytes: u64) -> u64 {
    MIN_FREE_BYTES.max(db_file_bytes.saturating_mul(MIN_FREE_DB_PERCENT) / 100)
}

/// Free space on the database's volume.
pub trait FreeSpace: Send + Sync {
    /// Bytes available to this user, or `None` if the volume cannot be asked (the job then does
    /// not start: an unknown margin is not a safe one).
    fn available_bytes(&self) -> Option<u64>;
}

/// [`FreeSpace`] for the volume holding `path` (`statvfs`).
pub struct VolumeFreeSpace {
    path: PathBuf,
}

impl VolumeFreeSpace {
    /// The volume of `db_path`'s directory (the file itself may not exist yet in tests).
    pub fn for_db(db_path: impl AsRef<Path>) -> Self {
        let path = db_path.as_ref();
        let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        Self { path: dir.to_path_buf() }
    }
}

impl FreeSpace for VolumeFreeSpace {
    #[cfg(unix)]
    fn available_bytes(&self) -> Option<u64> {
        use std::os::unix::ffi::OsStrExt;
        let c_path = std::ffi::CString::new(self.path.as_os_str().as_bytes()).ok()?;
        // SAFETY: `statvfs` only writes into the zeroed struct we own, and `c_path` is a valid
        // NUL-terminated string for the duration of the call.
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
            return None;
        }
        #[allow(clippy::useless_conversion)] // the field widths differ between macOS and Linux
        let (blocks, size) = (u64::from(stat.f_bavail), u64::from(stat.f_frsize));
        Some(blocks.saturating_mul(size))
    }

    #[cfg(not(unix))]
    fn available_bytes(&self) -> Option<u64> {
        None
    }
}

/// Pacing and bounds. [`Options::default`] is the product's.
#[derive(Debug, Clone)]
pub struct Options {
    /// Leaf pages per step, at most (the #714-measured 64).
    pub max_pages: u32,
    /// Leaf pages per step, at least, however slow steps get.
    pub min_pages: u32,
    /// The step duration the size adapts toward.
    pub target_step: Duration,
    /// Rest this many times the last step's duration after it.
    pub rest_factor: u32,
    /// Rest at least this long after a step.
    pub min_rest: Duration,
    /// Free pages handed back per `incremental_vacuum` slice (8 MiB at 4 KiB pages).
    pub release_pages: i64,
    /// Hard cap on steps in one run (a safety net; a 3.5 GB index needs ~10k).
    pub max_steps: u64,
    /// Sleep between steps. `false` only in tests.
    pub pace: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            max_pages: 64,
            min_pages: 8,
            target_step: Duration::from_millis(150),
            rest_factor: 4,
            min_rest: Duration::from_millis(100),
            release_pages: 2_048,
            max_steps: 200_000,
            pace: true,
        }
    }
}

/// Where the job stands. Stable wire names (snake_case) shared by every face.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Never run on this Mac.
    Idle,
    /// A start is waiting for the app (it takes it within a couple of seconds while open).
    Queued,
    Running,
    /// Every table reached one segment (or nothing FTS5 can merge), and free pages were handed back
    /// where the database allows it.
    Completed,
    /// The index was already compact: no step ran.
    NothingToDo,
    /// Stopped by the user between steps. A new start resumes.
    Stopped,
    /// The app quit or crashed while the job ran. A new start resumes.
    Interrupted,
    /// Free disk fell below the floor mid-run; stopped between steps. A new start resumes.
    LowDisk,
    /// Not started: free disk below the floor.
    RefusedLowDisk,
    /// Not started: the volume's free space could not be read.
    RefusedDiskUnknown,
    /// The database kept failing (e.g. busy). A new start resumes.
    Failed,
    /// The run hit its step cap (a safety net that should never trigger). A new start resumes.
    StepLimit,
}

impl JobState {
    /// Whether a run is going on in this state.
    pub fn is_active(self) -> bool {
        matches!(self, JobState::Running | JobState::Queued)
    }
}

/// Which part of the run is going on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Merging index segments.
    Merging,
    /// Handing free pages back to the filesystem (`auto_vacuum = INCREMENTAL` only).
    Releasing,
}

/// A byte view of [`DbPages`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bytes {
    pub file_bytes: i64,
    pub used_bytes: i64,
    pub free_bytes: i64,
}

impl From<DbPages> for Bytes {
    fn from(p: DbPages) -> Self {
        Self { file_bytes: p.file_bytes(), used_bytes: p.used_bytes(), free_bytes: p.free_bytes() }
    }
}

/// The job's status: what [`run`] reports after each step and what the desktop publishes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobStatus {
    pub state: JobState,
    pub phase: Option<Phase>,
    /// The index being merged (one of the three table names).
    pub table: Option<String>,
    pub steps: u64,
    pub pages_per_step: u32,
    pub last_step_ms: u64,
    pub max_step_ms: u64,
    /// Steps left, estimated from the structure records; `None` when unknown.
    pub estimated_remaining_steps: Option<u64>,
    /// Free pages handed back to the filesystem this run.
    pub released_pages: i64,
    pub started_at_ms: Option<u64>,
    pub updated_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    /// The database file when the run started.
    pub before: Option<Bytes>,
    /// The database file now (while running) or when the run ended.
    pub after: Option<Bytes>,
}

impl JobStatus {
    fn idle() -> Self {
        Self {
            state: JobState::Idle,
            phase: None,
            table: None,
            steps: 0,
            pages_per_step: 0,
            last_step_ms: 0,
            max_step_ms: 0,
            estimated_remaining_steps: None,
            released_pages: 0,
            started_at_ms: None,
            updated_at_ms: 0,
            finished_at_ms: None,
            before: None,
            after: None,
        }
    }
}

fn now_ms(db: &Db) -> u64 {
    u64::try_from(db.now_ms()).unwrap_or(0)
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Whether the volume has room: `Ok` or the refusal state.
fn disk_ok(free: &dyn FreeSpace, db_file_bytes: i64, refused: bool) -> Result<(), JobState> {
    let required = required_free_bytes(u64::try_from(db_file_bytes).unwrap_or(0));
    match free.available_bytes() {
        None if refused => Err(JobState::RefusedDiskUnknown),
        None => Err(JobState::LowDisk),
        Some(available) if available < required => {
            Err(if refused { JobState::RefusedLowDisk } else { JobState::LowDisk })
        }
        Some(_) => Ok(()),
    }
}

/// Sleep after a step (pacing), in slices so a stop is noticed quickly.
fn rest(step: Duration, options: &Options, cancel: &AtomicBool) {
    if !options.pace {
        return;
    }
    let total = options.min_rest.max(step.saturating_mul(options.rest_factor));
    let deadline = Instant::now() + total;
    while !cancel.load(Ordering::Relaxed) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        std::thread::sleep(left.min(REST_SLICE));
    }
}

/// Steps a table still needs at `pages` per step (`None` if its structure could not be read).
fn steps_for(state: &TableState, pages: u32) -> Option<u64> {
    state.structure_known.then(|| state.pages_to_write.div_ceil(u64::from(pages.max(1))))
}

/// Run the job to its end (completed, stopped, refused, …) on the calling thread. `on_progress`
/// sees the status after each step; the returned status is the final one.
pub fn run(
    db: &Db,
    free: &dyn FreeSpace,
    cancel: &AtomicBool,
    options: &Options,
    on_progress: &mut dyn FnMut(&JobStatus),
) -> JobStatus {
    let started = now_ms(db);
    let mut status = JobStatus {
        state: JobState::Running,
        started_at_ms: Some(started),
        updated_at_ms: started,
        pages_per_step: options.max_pages,
        ..JobStatus::idle()
    };
    let finish = |mut status: JobStatus, state: JobState| {
        status.state = state;
        status.phase = None;
        status.table = None;
        // A finished job has nothing left; any other end keeps the estimate of what a resume
        // still has to do.
        if matches!(state, JobState::Completed | JobState::NothingToDo) {
            status.estimated_remaining_steps = Some(0);
        }
        if let Ok(pages) = db.db_pages() {
            status.after = Some(pages.into());
        }
        let now = now_ms(db);
        status.updated_at_ms = now;
        status.finished_at_ms = Some(now);
        status
    };

    let Ok(before) = db.db_pages() else {
        return finish(status, JobState::Failed);
    };
    status.before = Some(before.into());
    status.after = status.before;
    if let Err(refusal) = disk_ok(free, before.file_bytes(), true) {
        return finish(status, refusal);
    }
    let mut initial = Vec::with_capacity(FTS_TABLES.len());
    for table in FTS_TABLES {
        match db.fts_compact_plan(table) {
            Ok(state) => initial.push(state),
            Err(_) => return finish(status, JobState::Failed),
        }
    }
    if initial.iter().all(|s| s.next == Move::Done) {
        return finish(status, JobState::NothingToDo);
    }
    status.phase = Some(Phase::Merging);
    status.estimated_remaining_steps =
        initial.iter().map(|s| steps_for(s, options.max_pages)).sum::<Option<u64>>();
    on_progress(&status);

    let mut pages = options.max_pages.max(options.min_pages);
    let mut failures = 0u32;
    for (index, table) in FTS_TABLES.iter().enumerate() {
        // Each run begins at most one full merge per table: once the merge it began is done, what
        // captures added meanwhile is left to FTS5's own automerge (rewriting the whole index
        // again for a few new segments would be write amplification for nothing).
        let mut begun = false;
        let later: Option<u64> =
            initial.iter().skip(index + 1).map(|s| steps_for(s, options.max_pages)).sum();
        loop {
            if cancel.load(Ordering::Relaxed) {
                return finish(status, JobState::Stopped);
            }
            if status.steps >= options.max_steps {
                return finish(status, JobState::StepLimit);
            }
            if let Err(state) = disk_ok(free, before.file_bytes(), false) {
                return finish(status, state);
            }
            let state = match db.fts_compact_plan(table) {
                Ok(state) => state,
                Err(_) => {
                    failures += 1;
                    if failures >= MAX_CONSECUTIVE_FAILURES {
                        return finish(status, JobState::Failed);
                    }
                    rest(options.min_rest, options, cancel);
                    continue;
                }
            };
            match state.next {
                Move::Done => break,
                Move::Begin if begun && state.structure_known => break,
                Move::Begin => begun = true,
                Move::Continue => {}
            }
            let step_started = Instant::now();
            let outcome = match db.fts_compact_step(table, state.next, pages) {
                Ok(outcome) => outcome,
                Err(_) => {
                    failures += 1;
                    if failures >= MAX_CONSECUTIVE_FAILURES {
                        return finish(status, JobState::Failed);
                    }
                    rest(options.min_rest, options, cancel);
                    continue;
                }
            };
            failures = 0;
            let took = step_started.elapsed();
            let took_ms = millis(took);
            status.steps += 1;
            status.last_step_ms = took_ms;
            status.max_step_ms = status.max_step_ms.max(took_ms);
            status.table = Some((*table).to_string());
            // Adapt toward the target: halve above ~5/3 of it, double below ~1/3 of it.
            let target = millis(options.target_step);
            if took_ms > target * 5 / 3 {
                pages = (pages / 2).max(options.min_pages);
            } else if took_ms < target / 3 {
                pages = pages.saturating_mul(2).min(options.max_pages);
            }
            status.pages_per_step = pages;
            let this_table = state.structure_known.then(|| {
                state.pages_to_write.saturating_sub(u64::from(pages)).div_ceil(u64::from(pages.max(1)))
            });
            status.estimated_remaining_steps = this_table.zip(later).map(|(a, b)| a + b);
            if let Ok(now) = db.db_pages() {
                status.after = Some(now.into());
            }
            status.updated_at_ms = now_ms(db);
            on_progress(&status);
            if !outcome.progressed {
                // FTS5 had nothing to merge for this command: this table is as compact as it gets.
                break;
            }
            rest(took, options, cancel);
        }
    }

    if before.returns_free_pages() {
        status.phase = Some(Phase::Releasing);
        status.table = None;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return finish(status, JobState::Stopped);
            }
            if let Err(state) = disk_ok(free, before.file_bytes(), false) {
                return finish(status, state);
            }
            let step_started = Instant::now();
            let freed = match db.release_free_pages(options.release_pages) {
                Ok(freed) => freed,
                Err(_) => {
                    failures += 1;
                    if failures >= MAX_CONSECUTIVE_FAILURES {
                        return finish(status, JobState::Failed);
                    }
                    rest(options.min_rest, options, cancel);
                    continue;
                }
            };
            failures = 0;
            let took = step_started.elapsed();
            status.steps += 1;
            status.last_step_ms = millis(took);
            status.max_step_ms = status.max_step_ms.max(status.last_step_ms);
            status.released_pages += freed;
            if let Ok(now) = db.db_pages() {
                status.after = Some(now.into());
            }
            status.updated_at_ms = now_ms(db);
            on_progress(&status);
            if freed == 0 {
                break;
            }
            rest(took, options, cancel);
        }
    }
    finish(status, JobState::Completed)
}

/// Why a start was refused before anything was queued. The wire text starts with the code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartRefusal {
    /// Free disk below [`required_free_bytes`].
    InsufficientDisk { available: u64, required: u64 },
    /// The volume's free space could not be read.
    DiskUnknown,
    /// The database could not be read.
    Unavailable,
}

impl std::fmt::Display for StartRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartRefusal::InsufficientDisk { available, required } => write!(
                f,
                "insufficient_disk: compacting the search index needs at least {required} bytes free on the disk that holds ShogunAI's memory; {available} are free"
            ),
            StartRefusal::DiskUnknown => {
                f.write_str("disk_unknown: the free space on the disk that holds ShogunAI's memory could not be read")
            }
            StartRefusal::Unavailable => f.write_str("unavailable: the memory database could not be read"),
        }
    }
}

/// Check the disk now, so a caller hears about a refusal at once rather than from a later status.
pub fn preflight(db: &Db, free: &dyn FreeSpace) -> Result<(), StartRefusal> {
    let pages = db.db_pages().map_err(|_| StartRefusal::Unavailable)?;
    let required = required_free_bytes(u64::try_from(pages.file_bytes()).unwrap_or(0));
    match free.available_bytes() {
        None => Err(StartRefusal::DiskUnknown),
        Some(available) if available < required => {
            Err(StartRefusal::InsufficientDisk { available, required })
        }
        Some(_) => Ok(()),
    }
}

/// Ask for a start from any face: preflight, then record the request for the desktop.
pub fn request_start(
    db: &Db,
    paths: &SearchIndexPaths,
    free: &dyn FreeSpace,
) -> Result<(), String> {
    preflight(db, free).map_err(|refusal| refusal.to_string())?;
    search_index_api::request(&paths.requests, Action::Start, now_ms(db))
}

/// Ask for a stop from any face. A start still waiting for the app is cancelled with it.
pub fn request_stop(db: &Db, paths: &SearchIndexPaths) -> Result<(), String> {
    search_index_api::request(&paths.requests, Action::Stop, now_ms(db))
}

/// The last published status, with a `running` left by an app that is gone read as `interrupted`.
fn stored_status(paths: &SearchIndexPaths, now: u64) -> JobStatus {
    let Some(mut status) = search_index_api::read_status(&paths.status)
        .and_then(|value| serde_json::from_value::<JobStatus>(value).ok())
    else {
        return JobStatus::idle();
    };
    if status.state == JobState::Running && now.saturating_sub(status.updated_at_ms) > RUNNING_STALE_MS {
        status.state = JobState::Interrupted;
        status.phase = None;
    }
    status
}

fn auto_vacuum_name(mode: i64) -> &'static str {
    match mode {
        1 => "full",
        2 => "incremental",
        _ => "none",
    }
}

/// The one status every face returns (`search_index.status`, Settings). `live` is the desktop's
/// in-process status while its job runs; the API faces pass `None` and read the published one.
pub fn status_json(
    db: &Db,
    paths: &SearchIndexPaths,
    free: &dyn FreeSpace,
    live: Option<&JobStatus>,
) -> Value {
    let now = now_ms(db);
    let mut job = live.cloned().unwrap_or_else(|| stored_status(paths, now));
    let pending = search_index_api::pending(&paths.requests, now);
    if pending == Some(Action::Start) && job.state != JobState::Running {
        job.state = JobState::Queued;
    }
    let tables: Vec<TableState> =
        FTS_TABLES.iter().filter_map(|table| db.fts_compact_plan(table).ok()).collect();
    let estimated_steps: Option<u64> = if tables.len() == FTS_TABLES.len() {
        tables.iter().map(|s| steps_for(s, Options::default().max_pages)).sum()
    } else {
        None
    };
    let needs_compaction = tables.iter().any(|s| s.next != Move::Done);
    let pages = db.db_pages().ok();
    let available = free.available_bytes();
    let required = pages.map(|p| required_free_bytes(u64::try_from(p.file_bytes()).unwrap_or(0)));
    json!({
        "job": job,
        "pending_request": pending.map(Action::as_str),
        "index": {
            "needs_compaction": needs_compaction,
            "estimated_steps": estimated_steps,
            "tables": tables,
        },
        "database": pages.map(|p| json!({
            "page_size": p.page_size,
            "file_bytes": p.file_bytes(),
            "used_bytes": p.used_bytes(),
            "free_bytes": p.free_bytes(),
            "auto_vacuum": auto_vacuum_name(p.auto_vacuum),
            // `false` (auto_vacuum none): pages the job frees stay in the file for later writes;
            // only a VACUUM, which this job never runs, would shrink the file.
            "free_pages_return_to_disk": p.returns_free_pages(),
        })),
        "disk": {
            "available_bytes": available,
            "required_bytes": required,
            "enough": matches!((available, required), (Some(a), Some(r)) if a >= r),
        },
    })
}

struct Shared {
    db: Db,
    paths: SearchIndexPaths,
    free: Arc<dyn FreeSpace>,
    options: Options,
    cancel: AtomicBool,
    /// The running job's latest status; `None` when no job runs in this process.
    live: Mutex<Option<JobStatus>>,
    /// Set by an in-process start / stop so the driver wakes at once instead of at its next poll.
    wake: Mutex<bool>,
    signal: Condvar,
}

/// The desktop's compaction driver: one thread that takes start / stop requests (from Settings in
/// process, from the API faces through [`search_index_api::REQUESTS_FILE`]) and runs the job.
#[derive(Clone)]
pub struct Driver {
    shared: Arc<Shared>,
}

impl Driver {
    /// Start the driver thread. It runs nothing until someone asks. A `running` status left by a
    /// previous launch is republished as `interrupted`: the job never resumes on its own.
    pub fn spawn(db: Db, paths: SearchIndexPaths, free: Arc<dyn FreeSpace>, options: Options) -> Self {
        let driver = Self {
            shared: Arc::new(Shared {
                db,
                paths,
                free,
                options,
                cancel: AtomicBool::new(false),
                live: Mutex::new(None),
                wake: Mutex::new(false),
                signal: Condvar::new(),
            }),
        };
        let mut left_over = stored_status(&driver.shared.paths, 0);
        if left_over.state == JobState::Running {
            left_over.state = JobState::Interrupted;
            left_over.phase = None;
            publish(&driver.shared.paths, &left_over);
        }
        let shared = Arc::clone(&driver.shared);
        let spawned = std::thread::Builder::new()
            .name("search-index-compact".into())
            .spawn(move || driver_loop(&shared));
        if spawned.is_err() {
            eprintln!("[search_index] could not start the compaction driver");
        }
        driver
    }

    /// Settings' "Compact search index": preflight, then run (or resume).
    pub fn start(&self) -> Result<(), String> {
        request_start(&self.shared.db, &self.shared.paths, self.shared.free.as_ref())?;
        self.notify();
        Ok(())
    }

    /// Settings' "Stop": takes effect before the next step.
    pub fn stop(&self) -> Result<(), String> {
        self.shared.cancel.store(true, Ordering::Relaxed);
        let result = request_stop(&self.shared.db, &self.shared.paths);
        self.notify();
        result
    }

    /// The same JSON `search_index.status` returns.
    pub fn status_json(&self) -> Value {
        let live = self.shared.live.lock().ok().and_then(|guard| guard.clone());
        status_json(&self.shared.db, &self.shared.paths, self.shared.free.as_ref(), live.as_ref())
    }

    fn notify(&self) {
        if let Ok(mut woken) = self.shared.wake.lock() {
            *woken = true;
            self.shared.signal.notify_all();
        }
    }
}

fn publish(paths: &SearchIndexPaths, status: &JobStatus) {
    let value = match serde_json::to_value(status) {
        Ok(value) => value,
        Err(_) => return,
    };
    if search_index_api::write_status(&paths.status, &value).is_err() {
        eprintln!("[search_index] could not publish the compaction status");
    }
}

fn driver_loop(shared: &Shared) {
    loop {
        // Sleep until woken in process or the poll interval passes (a face's request file).
        if let Ok(guard) = shared.wake.lock() {
            let (mut guard, _) = shared
                .signal
                .wait_timeout_while(guard, POLL_INTERVAL, |woken| !*woken)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *guard = false;
        } else {
            std::thread::sleep(POLL_INTERVAL);
        }
        let now = now_ms(&shared.db);
        if search_index_api::take(&shared.paths.requests, now) == Some(Action::Start) {
            run_once(shared);
        }
    }
}

fn run_once(shared: &Shared) {
    shared.cancel.store(false, Ordering::Relaxed);
    let mut last_publish: Option<Instant> = None;
    let mut last_check = Instant::now();
    let mut on_progress = |status: &JobStatus| {
        if let Ok(mut live) = shared.live.lock() {
            *live = Some(status.clone());
        }
        let due = match last_publish {
            None => true,
            Some(at) => at.elapsed() >= PUBLISH_INTERVAL,
        };
        if due {
            publish(&shared.paths, status);
            last_publish = Some(Instant::now());
        }
        // A stop from another face arrives through the request file.
        if last_check.elapsed() >= PUBLISH_INTERVAL {
            last_check = Instant::now();
            if search_index_api::take(&shared.paths.requests, status.updated_at_ms) == Some(Action::Stop) {
                shared.cancel.store(true, Ordering::Relaxed);
            }
        }
    };
    let final_status = run(
        &shared.db,
        shared.free.as_ref(),
        &shared.cancel,
        &shared.options,
        &mut on_progress,
    );
    publish(&shared.paths, &final_status);
    if let Ok(mut live) = shared.live.lock() {
        *live = None;
    }
    // Counts only (log hygiene): the outcome, steps and bytes, never a table's content.
    let state = serde_json::to_value(final_status.state).unwrap_or(Value::Null);
    eprintln!(
        "[search_index] compaction {state}: {} steps, max step {} ms",
        final_status.steps, final_status.max_step_ms
    );
}

#[cfg(test)]
mod tests;
