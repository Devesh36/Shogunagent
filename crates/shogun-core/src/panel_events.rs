//! Coalesced "the panel's state changed" notifications (issue #357).
//!
//! Producers call [`notify`] with the [`PanelPart`] they just changed, so the notch panel does
//! not re-read six IPC commands on a timer; a single worker coalesces bursts and hands the host
//! shell one batch of part names to push to the webview, which re-reads only those parts. The webview keeps a slow poll as a backstop for writers in
//! *other* processes (CLI / MCP / REST daemon writing app-data files), which cannot reach this
//! process's sink.
//!
//! Cost model (idle-CPU SLO): until a host installs a sink, [`notify`] is one atomic load and a
//! return — the CLI, the API daemon and every unit test pay nothing. With a sink installed, the
//! worker thread blocks on a condvar while nothing is pending, so an idle app spends no cycles
//! here. Each part is *throttled*, not debounced: the first change schedules one flush after the
//! part's delay and later changes inside that window ride along, so a continuous stream (capture
//! extraction) yields at most one event per delay window instead of never settling.
//!
//! Payloads are part names only — never row content (logs/telemetry rule).

use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

/// One independently re-readable piece of the panel's state. The wire name ([`Self::as_str`])
/// is what the webview receives; keep the two in sync with `App.tsx`'s `PanelPart`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PanelPart {
    /// `shogun_status`: key presence / rejection, due counts.
    Status,
    /// `shogun_state`: the commitments / open loops the panel lists.
    State,
    /// `entitlement_status`: the plan in force (billing, trial stamp).
    Entitlement,
    /// `onboarding_state`.
    Onboarding,
    /// `list_approvals`: the pending L3 queue.
    Approvals,
    /// `get_notch_status_visible`.
    NotchStatus,
    /// `summary_state`: daily summary settings / seen dates.
    Summary,
    /// Local dictionary auto-add settings or terms changed.
    DictionaryAutoAdd,
}

impl PanelPart {
    /// Every part, in flush order.
    pub const ALL: [PanelPart; 8] = [
        PanelPart::Status,
        PanelPart::State,
        PanelPart::Entitlement,
        PanelPart::Onboarding,
        PanelPart::Approvals,
        PanelPart::NotchStatus,
        PanelPart::Summary,
        PanelPart::DictionaryAutoAdd,
    ];

    /// The wire name the webview matches on.
    pub fn as_str(self) -> &'static str {
        match self {
            PanelPart::Status => "status",
            PanelPart::State => "state",
            PanelPart::Entitlement => "entitlement",
            PanelPart::Onboarding => "onboarding",
            PanelPart::Approvals => "approvals",
            PanelPart::NotchStatus => "notch_status",
            PanelPart::Summary => "summary",
            PanelPart::DictionaryAutoAdd => "dictionary_autoadd",
        }
    }

    fn index(self) -> usize {
        match self {
            PanelPart::Status => 0,
            PanelPart::State => 1,
            PanelPart::Entitlement => 2,
            PanelPart::Onboarding => 3,
            PanelPart::Approvals => 4,
            PanelPart::NotchStatus => 5,
            PanelPart::Summary => 6,
            PanelPart::DictionaryAutoAdd => 7,
        }
    }

    /// How long a change to this part waits for companions before it is pushed. User-driven
    /// parts (a click, a settings toggle) are short so the panel feels immediate; the state
    /// tables are written in bursts by capture extraction and maintenance, so they are
    /// throttled harder — two seconds late is invisible for a list, a re-read per capture is not.
    pub fn delay(self) -> Duration {
        match self {
            PanelPart::State => Duration::from_millis(2_000),
            PanelPart::Status => Duration::from_millis(250),
            PanelPart::Entitlement
            | PanelPart::Onboarding
            | PanelPart::Approvals
            | PanelPart::NotchStatus
            | PanelPart::Summary
            | PanelPart::DictionaryAutoAdd => Duration::from_millis(100),
        }
    }
}

/// The pure scheduling core: which parts are pending and when each is due. No clock, no thread.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Pending {
    due: [Option<Instant>; PanelPart::ALL.len()],
}

impl Pending {
    /// Record a change to `part` at `now`, due after `delay`. Returns `true` when this scheduled
    /// a new flush; `false` when the part was already pending (the change rides along — the
    /// existing due time is kept so a steady stream cannot postpone the flush forever).
    pub fn mark(&mut self, part: PanelPart, now: Instant, delay: Duration) -> bool {
        let slot = &mut self.due[part.index()];
        if slot.is_some() {
            return false;
        }
        *slot = Some(now + delay);
        true
    }

    /// The earliest pending due time, if anything is pending.
    pub fn next_due(&self) -> Option<Instant> {
        self.due.iter().flatten().min().copied()
    }

    /// Remove and return every part due at or before `now`, in [`PanelPart::ALL`] order.
    pub fn take_due(&mut self, now: Instant) -> Vec<PanelPart> {
        let mut out = Vec::new();
        for part in PanelPart::ALL {
            let slot = &mut self.due[part.index()];
            if slot.is_some_and(|t| t <= now) {
                *slot = None;
                out.push(part);
            }
        }
        out
    }

    /// Whether nothing is pending.
    pub fn is_empty(&self) -> bool {
        self.due.iter().all(Option::is_none)
    }
}

/// Receives each coalesced batch (never empty). Called on the coalescer's worker thread, outside
/// its lock, so a slow sink delays later batches but never blocks a producer.
pub type Sink = Arc<dyn Fn(&[PanelPart]) + Send + Sync>;

struct Inner {
    pending: Pending,
    shutdown: bool,
}

struct Shared {
    inner: Mutex<Inner>,
    wake: Condvar,
    delay: fn(PanelPart) -> Duration,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A coalescer with its own worker thread. The process-wide instance behind [`install`] /
/// [`notify`] is the one the app uses; constructing one directly is for tests and embedding.
pub struct Coalescer {
    shared: Arc<Shared>,
}

impl Coalescer {
    /// A coalescer using each part's production [`PanelPart::delay`].
    pub fn new(sink: Sink) -> std::io::Result<Self> {
        Self::with_delays(sink, PanelPart::delay)
    }

    /// A coalescer with custom per-part delays. Fails only if the worker thread cannot spawn.
    pub fn with_delays(sink: Sink, delay: fn(PanelPart) -> Duration) -> std::io::Result<Self> {
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                pending: Pending::default(),
                shutdown: false,
            }),
            wake: Condvar::new(),
            delay,
        });
        let worker = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("panel-events".into())
            .spawn(move || run(&worker, &sink))?;
        Ok(Self { shared })
    }

    /// Record that `part` changed. Never blocks beyond a short uncontended lock.
    pub fn notify(&self, part: PanelPart) {
        let now = Instant::now();
        let mut inner = self.shared.lock();
        let before = inner.pending.next_due();
        if inner.pending.mark(part, now, (self.shared.delay)(part)) {
            // Only an earlier deadline needs the worker re-armed; a later one is picked up when
            // the current wait ends.
            let rearm = match (before, inner.pending.next_due()) {
                (None, _) => true,
                (Some(b), Some(n)) => n < b,
                (Some(_), None) => false,
            };
            if rearm {
                self.shared.wake.notify_one();
            }
        }
    }
}

impl Drop for Coalescer {
    fn drop(&mut self) {
        self.shared.lock().shutdown = true;
        self.shared.wake.notify_one();
    }
}

fn run(shared: &Shared, sink: &Sink) {
    let mut inner = shared.lock();
    loop {
        if inner.shutdown {
            return;
        }
        let Some(due) = inner.pending.next_due() else {
            // Nothing pending: sleep until a producer marks something. Zero CPU while idle.
            inner = shared
                .wake
                .wait(inner)
                .unwrap_or_else(PoisonError::into_inner);
            continue;
        };
        let now = Instant::now();
        if due > now {
            inner = shared
                .wake
                .wait_timeout(inner, due - now)
                .map(|(guard, _)| guard)
                .unwrap_or_else(|poisoned| poisoned.into_inner().0);
            continue;
        }
        let batch = inner.pending.take_due(now);
        if batch.is_empty() {
            continue;
        }
        drop(inner);
        sink(&batch);
        inner = shared.lock();
    }
}

static GLOBAL: OnceLock<Coalescer> = OnceLock::new();

/// Install the process-wide sink (the desktop shell's `app.emit`). First call wins; later calls
/// are ignored and return `false`, as does a failure to spawn the worker (the webview's backstop
/// poll still converges in that case).
pub fn install(sink: Sink) -> bool {
    if GLOBAL.get().is_some() {
        return false;
    }
    match Coalescer::new(sink) {
        Ok(c) => GLOBAL.set(c).is_ok(),
        Err(_) => false,
    }
}

/// Record that `part` changed in this process. A no-op until [`install`] has run, so library
/// code (Db writes, the approval store) can call it unconditionally.
pub fn notify(part: PanelPart) {
    if let Some(c) = GLOBAL.get() {
        c.notify(part);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn fast(part: PanelPart) -> Duration {
        match part {
            PanelPart::State => Duration::from_millis(120),
            _ => Duration::from_millis(30),
        }
    }

    fn channel_sink() -> (Sink, mpsc::Receiver<Vec<PanelPart>>) {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let sink: Sink = Arc::new(move |parts: &[PanelPart]| {
            let _ = tx.lock().unwrap().send(parts.to_vec());
        });
        (sink, rx)
    }

    #[test]
    fn wire_names_are_unique_and_indices_dense() {
        let mut names: Vec<_> = PanelPart::ALL.iter().map(|p| p.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), PanelPart::ALL.len());
        for (i, p) in PanelPart::ALL.iter().enumerate() {
            assert_eq!(p.index(), i);
        }
    }

    #[test]
    fn state_is_throttled_harder_than_user_driven_parts() {
        assert!(PanelPart::State.delay() > PanelPart::Approvals.delay());
        assert!(PanelPart::State.delay() > PanelPart::Status.delay());
    }

    #[test]
    fn mark_keeps_the_first_deadline() {
        let t0 = Instant::now();
        let mut p = Pending::default();
        assert!(p.mark(PanelPart::State, t0, Duration::from_millis(100)));
        assert!(!p.mark(
            PanelPart::State,
            t0 + Duration::from_millis(90),
            Duration::from_millis(100)
        ));
        assert_eq!(p.next_due(), Some(t0 + Duration::from_millis(100)));
    }

    #[test]
    fn take_due_returns_only_expired_parts_in_order() {
        let t0 = Instant::now();
        let mut p = Pending::default();
        p.mark(PanelPart::Summary, t0, Duration::from_millis(10));
        p.mark(PanelPart::Approvals, t0, Duration::from_millis(10));
        p.mark(PanelPart::State, t0, Duration::from_millis(500));
        assert!(p.take_due(t0).is_empty());
        assert_eq!(
            p.take_due(t0 + Duration::from_millis(10)),
            vec![PanelPart::Approvals, PanelPart::Summary]
        );
        assert_eq!(p.next_due(), Some(t0 + Duration::from_millis(500)));
        assert_eq!(
            p.take_due(t0 + Duration::from_secs(1)),
            vec![PanelPart::State]
        );
        assert!(p.is_empty());
        assert_eq!(p.next_due(), None);
    }

    #[test]
    fn a_burst_on_one_part_is_one_batch() {
        let (sink, rx) = channel_sink();
        let c = Coalescer::with_delays(sink, fast).unwrap();
        for _ in 0..200 {
            c.notify(PanelPart::State);
        }
        let batch = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(batch, vec![PanelPart::State]);
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
    }

    #[test]
    fn nearby_changes_to_different_parts_share_a_batch() {
        let (sink, rx) = channel_sink();
        let c = Coalescer::with_delays(sink, fast).unwrap();
        c.notify(PanelPart::Approvals);
        c.notify(PanelPart::Entitlement);
        c.notify(PanelPart::Approvals);
        let batch = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(batch, vec![PanelPart::Entitlement, PanelPart::Approvals]);
    }

    #[test]
    fn an_earlier_deadline_is_not_held_behind_a_later_one() {
        let (sink, rx) = channel_sink();
        let c = Coalescer::with_delays(sink, fast).unwrap();
        let t0 = Instant::now();
        c.notify(PanelPart::State); // due at +120ms
        c.notify(PanelPart::Approvals); // due at +30ms — must re-arm the worker
        let first = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(first, vec![PanelPart::Approvals]);
        assert!(t0.elapsed() < Duration::from_millis(110));
        let second = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(second, vec![PanelPart::State]);
    }

    #[test]
    fn a_change_after_a_flush_schedules_a_new_batch() {
        let (sink, rx) = channel_sink();
        let c = Coalescer::with_delays(sink, fast).unwrap();
        c.notify(PanelPart::Summary);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            vec![PanelPart::Summary]
        );
        c.notify(PanelPart::Summary);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            vec![PanelPart::Summary]
        );
    }

    #[test]
    fn dropping_the_coalescer_stops_the_worker() {
        let (sink, rx) = channel_sink();
        let c = Coalescer::with_delays(sink, fast).unwrap();
        drop(c);
        // The worker exits and drops the sink (and with it the sender).
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(2)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn notify_without_an_installed_sink_is_a_no_op() {
        // The global is never installed in unit tests; this must not panic or spawn anything.
        notify(PanelPart::State);
    }
}
