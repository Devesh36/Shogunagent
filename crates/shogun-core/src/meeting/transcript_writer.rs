//! Ordered, bounded background persistence for live transcript lines (issue #917).
//!
//! The live caption path must not wait on the database: a busy shared connection would hold the
//! final caption, its translation and the audio lane's result behind one synchronous
//! insert. Lines are handed to one worker thread per audio lane and written in the order accepted.
//!
//! What this does **not** promise:
//! - **Crash safety.** Queued lines live in RAM until written. A hard kill loses what the queue
//!   held at that instant (bounded by the limits below); a normal stop drains it.
//! - **Unbounded absorption.** When the queue is full, `push` waits for the worker
//!   instead of dropping a line. Saturation slows captions down; it never
//!   loses transcript text. Each wait is counted.
//!
//! Content-free by construction: the worker only calls the caller's write closure and counts
//! outcomes and timings. It never logs, formats or inspects the lines.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::metrics::Histogram;

/// A database write that fails is retried this many times with these pauses (a locked or busy
/// SQLite usually clears within a few hundred ms) before the line is counted as failed.
const RETRY_BACKOFF: [Duration; 2] = [Duration::from_millis(50), Duration::from_millis(250)];

/// After this many lines in a row failed even with retries, the database is treated as down and
/// later lines get a single attempt until one succeeds: a database that stays down must not turn
/// a Stop-time drain into hundreds of lines × the pauses above. One isolated failure does not
/// cost the next line its retries (issue #1005).
const FAST_FAIL_AFTER: u32 = 3;

/// Content-free outcome of one writer's lifetime.
#[derive(Debug, Clone)]
pub struct WriterStats {
    pub written: u64,
    /// Lines that still failed after the retries. They are not in the saved transcript.
    pub failed: u64,
    /// Pushes that had to wait because the queue was at its entry or byte bound.
    pub saturated: u64,
    /// Accepted → write started.
    pub queue_wait: Histogram,
    /// One write, including its retries.
    pub write: Histogram,
}

struct Queue<T> {
    items: VecDeque<(T, usize, Instant)>,
    bytes: usize,
    closed: bool,
    failed: u64,
    failed_items: Vec<T>,
    /// The most recent line failed; cleared by the next successful write.
    failing: bool,
    saturated: u64,
}

struct Shared<T> {
    queue: Mutex<Queue<T>>,
    changed: Condvar,
}

impl<T> Shared<T> {
    // A poisoned lock still holds a consistent queue (no code here panics mid-update).
    fn lock(&self) -> MutexGuard<'_, Queue<T>> {
        self.queue.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// One ordered writer. Dropping it closes the queue and waits for every accepted line.
pub struct TranscriptWriter<T: Send + 'static> {
    shared: Arc<Shared<T>>,
    max_items: usize,
    max_bytes: usize,
    join: Option<JoinHandle<WriterStats>>,
}

impl<T: Send + 'static> TranscriptWriter<T> {
    /// Start the worker. `write` returns `false` on failure and is retried per [`RETRY_BACKOFF`].
    /// A single item larger than `max_bytes` is still accepted once the queue is empty.
    pub fn spawn(
        max_items: usize,
        max_bytes: usize,
        mut write: impl FnMut(&T) -> bool + Send + 'static,
    ) -> std::io::Result<Self> {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue {
                items: VecDeque::new(),
                bytes: 0,
                closed: false,
                failed: 0,
                failed_items: Vec::new(),
                failing: false,
                saturated: 0,
            }),
            changed: Condvar::new(),
        });
        let worker = Arc::clone(&shared);
        let join = std::thread::Builder::new()
            .name("transcript-writer".into())
            .spawn(move || {
                let mut written = 0;
                let mut failed_in_a_row = 0_u32;
                let mut queue_wait = Histogram::latency_ms();
                let mut write_ms = Histogram::latency_ms();
                loop {
                    let (item, accepted) = {
                        let mut q = worker.lock();
                        while q.items.is_empty() && !q.closed {
                            q = worker.changed.wait(q).unwrap_or_else(|p| p.into_inner());
                        }
                        let Some((item, bytes, accepted)) = q.items.pop_front() else {
                            break;
                        };
                        q.bytes = q.bytes.saturating_sub(bytes);
                        (item, accepted)
                    };
                    worker.changed.notify_all();
                    let started = Instant::now();
                    queue_wait.record(started.duration_since(accepted).as_secs_f64() * 1000.0);
                    let mut ok = write(&item);
                    let retries = if failed_in_a_row >= FAST_FAIL_AFTER {
                        0
                    } else {
                        RETRY_BACKOFF.len()
                    };
                    for pause in RETRY_BACKOFF.into_iter().take(retries) {
                        if ok {
                            break;
                        }
                        std::thread::sleep(pause);
                        ok = write(&item);
                    }
                    write_ms.record(started.elapsed().as_secs_f64() * 1000.0);
                    if ok {
                        failed_in_a_row = 0;
                        written += 1;
                        worker.lock().failing = false;
                    } else {
                        failed_in_a_row = failed_in_a_row.saturating_add(1);
                        let mut q = worker.lock();
                        q.failed += 1;
                        q.failed_items.push(item);
                        q.failing = true;
                    }
                }
                let q = worker.lock();
                WriterStats {
                    written,
                    failed: q.failed,
                    saturated: q.saturated,
                    queue_wait,
                    write: write_ms,
                }
            })?;
        Ok(Self { shared, max_items: max_items.max(1), max_bytes, join: Some(join) })
    }

    /// Accept one line in order. Waits only while the queue is at a bound; never drops.
    pub fn push(&self, item: T, bytes: usize) {
        let mut q = self.shared.lock();
        let full = |q: &Queue<T>| {
            !q.items.is_empty()
                && (q.items.len() >= self.max_items || q.bytes.saturating_add(bytes) > self.max_bytes)
        };
        if full(&q) {
            q.saturated += 1;
            while full(&q) && !q.closed {
                q = self.shared.changed.wait(q).unwrap_or_else(|p| p.into_inner());
            }
        }
        q.bytes = q.bytes.saturating_add(bytes);
        q.items.push_back((item, bytes, Instant::now()));
        drop(q);
        self.shared.changed.notify_all();
    }

    /// Lines that failed after retries so far — read by the live status so a failing save is
    /// visible during the meeting, not only afterwards.
    pub fn failed(&self) -> u64 {
        self.shared.lock().failed
    }

    /// Whether saving is failing right now: the latest line failed after its retries. Clears as
    /// soon as a later line is written, so the live status recovers with the database; lines
    /// already lost stay counted in [`Self::failed`] and in the final stats.
    pub fn failing(&self) -> bool {
        self.shared.lock().failing
    }

    /// Close, write everything accepted, and return the outcome.
    pub fn finish(mut self) -> Option<WriterStats> {
        self.close_and_join()
    }

    /// Drain while returning unsaved text to the owner instead of discarding it.
    pub fn finish_retaining(mut self) -> (Option<WriterStats>, Vec<T>) {
        let stats = self.close_and_join();
        let failed = std::mem::take(&mut self.shared.lock().failed_items);
        (stats, failed)
    }

    fn close_and_join(&mut self) -> Option<WriterStats> {
        self.shared.lock().closed = true;
        self.shared.changed.notify_all();
        self.join.take().and_then(|join| join.join().ok())
    }
}

impl<T: Send + 'static> Drop for TranscriptWriter<T> {
    fn drop(&mut self) {
        let _ = self.close_and_join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// A blocked write must not hold the caller while the queue has room, and every accepted
    /// line is written once, in order, after the block clears.
    #[test]
    fn blocked_write_does_not_hold_pushes_within_capacity_and_keeps_order() -> TestResult {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let written = Arc::new(Mutex::new(Vec::new()));
        let (g, w) = (Arc::clone(&gate), Arc::clone(&written));
        let writer = TranscriptWriter::spawn(64, 1 << 20, move |n: &u32| {
            let (open, cv) = &*g;
            let mut open = open.lock().unwrap_or_else(|p| p.into_inner());
            while !*open {
                open = cv.wait(open).unwrap_or_else(|p| p.into_inner());
            }
            w.lock().unwrap_or_else(|p| p.into_inner()).push(*n);
            true
        })?;
        let started = Instant::now();
        for n in 0..32 {
            writer.push(n, 10);
        }
        assert!(started.elapsed() < Duration::from_millis(200), "push waited on a blocked write");
        {
            let (open, cv) = &*gate;
            *open.lock().unwrap_or_else(|p| p.into_inner()) = true;
            cv.notify_all();
        }
        let stats = writer.finish().ok_or("worker panicked")?;
        assert_eq!(stats.written, 32);
        assert_eq!(stats.failed, 0);
        assert_eq!(stats.saturated, 0);
        assert_eq!(*written.lock().unwrap_or_else(|p| p.into_inner()), (0..32).collect::<Vec<_>>());
        Ok(())
    }

    #[test]
    fn saturation_waits_and_counts_instead_of_dropping() -> TestResult {
        let release = Arc::new(AtomicBool::new(false));
        let count = Arc::new(AtomicUsize::new(0));
        let (r, c) = (Arc::clone(&release), Arc::clone(&count));
        let writer = TranscriptWriter::spawn(2, 1 << 20, move |_: &u8| {
            while !r.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            c.fetch_add(1, Ordering::Relaxed);
            true
        })?;
        let releaser = {
            let release = Arc::clone(&release);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                release.store(true, Ordering::Release);
            })
        };
        for _ in 0..6 {
            writer.push(0, 1);
        }
        let _ = releaser.join();
        let stats = writer.finish().ok_or("worker panicked")?;
        assert_eq!(count.load(Ordering::Relaxed), 6);
        assert_eq!(stats.written, 6);
        assert!(stats.saturated >= 1);
        Ok(())
    }

    #[test]
    fn byte_bound_waits_but_admits_an_oversized_line_alone() -> TestResult {
        let release = Arc::new(AtomicBool::new(false));
        let r = Arc::clone(&release);
        let writer = TranscriptWriter::spawn(100, 10, move |_: &()| {
            while !r.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            true
        })?;
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            release.store(true, Ordering::Release);
        });
        // Whatever the worker has popped, at most one oversized line may sit in the queue.
        for _ in 0..3 {
            writer.push((), 50);
        }
        let _ = releaser.join();
        let stats = writer.finish().ok_or("worker panicked")?;
        assert_eq!(stats.written, 3);
        assert!(stats.saturated >= 1);
        Ok(())
    }

    #[test]
    fn failures_retry_then_count_visibly() -> TestResult {
        let attempts = Arc::new(AtomicUsize::new(0));
        let a = Arc::clone(&attempts);
        let writer = TranscriptWriter::spawn(8, 1 << 20, move |fail_always: &bool| {
            let n = a.fetch_add(1, Ordering::Relaxed);
            !*fail_always && n > 0
        })?;
        writer.push(false, 1); // fails once, then succeeds on retry
        writer.push(true, 1); // never succeeds
        writer.push(true, 1); // follows one failure: still retried in full
        let deadline = Instant::now() + Duration::from_secs(5);
        while writer.failed() == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(writer.failed() >= 1);
        let stats = writer.finish().ok_or("worker panicked")?;
        assert_eq!(stats.written, 1);
        assert_eq!(stats.failed, 2);
        assert_eq!(
            attempts.load(Ordering::Relaxed),
            2 + 2 * (1 + RETRY_BACKOFF.len())
        );
        Ok(())
    }

    /// Issue #1005: one failed line must not cost the next line its retries, and the live
    /// "failing" state clears once the database writes again.
    #[test]
    fn a_line_after_a_failure_is_still_retried_and_recovery_clears_failing() -> TestResult {
        // Each item is how many attempts fail before one succeeds (u32::MAX: never).
        let left = Arc::new(Mutex::new(std::collections::HashMap::<usize, u32>::new()));
        let l = Arc::clone(&left);
        let writer = TranscriptWriter::spawn(8, 1 << 20, move |&(id, fails): &(usize, u32)| {
            let mut map = l.lock().unwrap_or_else(|p| p.into_inner());
            let seen = map.entry(id).or_insert(0);
            *seen += 1;
            *seen > fails
        })?;
        writer.push((0, u32::MAX), 1); // the database is down for this line
        let deadline = Instant::now() + Duration::from_secs(5);
        while !writer.failing() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(writer.failing(), "a failed line is visible while it is the latest");
        writer.push((1, 1), 1); // one transient BUSY, then fine: the retry saves it
        let deadline = Instant::now() + Duration::from_secs(5);
        while writer.failing() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!writer.failing(), "a successful write clears the live failure");
        let stats = writer.finish().ok_or("worker panicked")?;
        assert_eq!((stats.written, stats.failed), (1, 1));
        Ok(())
    }

    /// A database that stays down still drains quickly: after a run of failed lines the rest get
    /// one attempt each, until a write succeeds again.
    #[test]
    fn a_database_that_stays_down_stops_paying_retry_pauses() -> TestResult {
        let attempts = Arc::new(AtomicUsize::new(0));
        let a = Arc::clone(&attempts);
        let writer = TranscriptWriter::spawn(64, 1 << 20, move |ok: &bool| {
            a.fetch_add(1, Ordering::Relaxed);
            *ok
        })?;
        let lines = FAST_FAIL_AFTER as usize + 5;
        for _ in 0..lines {
            writer.push(false, 1);
        }
        writer.push(true, 1);
        let stats = writer.finish().ok_or("worker panicked")?;
        assert_eq!(stats.failed, lines as u64);
        assert_eq!(stats.written, 1);
        let full = FAST_FAIL_AFTER as usize * (1 + RETRY_BACKOFF.len());
        assert_eq!(attempts.load(Ordering::Relaxed), full + (lines - FAST_FAIL_AFTER as usize) + 1);
        Ok(())
    }

    /// Drop drains: the audio lane's join (and so close/recap) waits for accepted lines.
    #[test]
    fn drop_drains_accepted_lines() -> TestResult {
        let count = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&count);
        let writer = TranscriptWriter::spawn(8, 1 << 20, move |_: &u8| {
            std::thread::sleep(Duration::from_millis(2));
            c.fetch_add(1, Ordering::Relaxed);
            true
        })?;
        for _ in 0..5 {
            writer.push(0, 1);
        }
        drop(writer);
        assert_eq!(count.load(Ordering::Relaxed), 5);
        Ok(())
    }
}

#[cfg(test)]
mod retention_tests {
    use super::*;

    #[test]
    fn failed_lines_survive_the_final_drain() {
        let writer = TranscriptWriter::spawn(10, 100, |_: &u32| false).unwrap();
        writer.push(17, 4);
        let (stats, failed) = writer.finish_retaining();
        assert_eq!(stats.unwrap().failed, 1);
        assert_eq!(failed, vec![17]);
    }
}
