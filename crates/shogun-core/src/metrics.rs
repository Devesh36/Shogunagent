//! Always-on SLO metrics (NFR-SLO-00): a lightweight fixed-bucket histogram plus a registry
//! that tracks each of the six SLOs (NFR-SLO-01..06) against its p95 budget.
//!
//! Design constraints:
//! - **O(1) record, bounded memory.** Fixed bucket bounds, no per-sample storage — this runs
//!   on hot paths (every expand, every cache update) and in a resident process for days.
//! - **Pure and Linux-testable.** The macOS adapter wraps a registry in a lock and feeds it
//!   observed durations; the percentile / pass-fail logic lives here, unit-tested off-device.
//! - **Honest under overflow.** A percentile that lands above the largest bound is reported as
//!   that bound (a floor) and flagged, never silently interpolated to a fabricated value — the
//!   budgets are chosen to be one of the bounds, so an over-budget p95 still reads as FAIL.

/// A fixed-bucket histogram over `f64` samples (milliseconds, or percent for CPU).
///
/// `bounds` are ascending *inclusive upper* edges; there is one extra overflow bucket for
/// samples greater than the last bound. `record` is O(bounds.len()) (small, ≤~12).
#[derive(Clone, Debug)]
pub struct Histogram {
    /// Ascending upper edges. `counts[i]` holds samples with `value <= bounds[i]` and
    /// `value > bounds[i-1]`; `counts[bounds.len()]` is the overflow bucket.
    bounds: Vec<f64>,
    counts: Vec<u64>,
    total: u64,
    sum: f64,
    max: f64,
}

impl Histogram {
    /// Create a histogram with the given ascending upper-edge bounds. Non-ascending or empty
    /// bounds are rejected (returns `None`) — a misconfigured histogram must not silently
    /// mis-bucket.
    pub fn new(bounds: Vec<f64>) -> Option<Self> {
        if bounds.is_empty() || bounds.windows(2).any(|w| w[1] <= w[0]) {
            return None;
        }
        Some(Self::with_valid_bounds(bounds))
    }

    fn with_valid_bounds(bounds: Vec<f64>) -> Self {
        let n = bounds.len() + 1;
        Self { bounds, counts: vec![0; n], total: 0, sum: 0.0, max: f64::MIN }
    }

    /// Record one sample. NaN is ignored (a bad clock read must not corrupt the tally).
    pub fn record(&mut self, value: f64) {
        if value.is_nan() {
            return;
        }
        let idx = match self.bounds.iter().position(|b| value <= *b) {
            Some(i) => i,
            None => self.bounds.len(), // overflow
        };
        self.counts[idx] += 1;
        self.total += 1;
        self.sum += value;
        if value > self.max {
            self.max = value;
        }
    }

    pub fn count(&self) -> u64 {
        self.total
    }

    pub fn max(&self) -> Option<f64> {
        (self.total > 0).then_some(self.max)
    }

    pub fn mean(&self) -> Option<f64> {
        (self.total > 0).then(|| self.sum / self.total as f64)
    }

    /// Fraction of samples `<= threshold` (e.g. the "95% of samples within 5%" CPU rule).
    /// Counts only whole buckets at or below `threshold`; a bucket straddling the threshold is
    /// excluded, so this is a conservative (lower-bound) fraction. `None` if no samples.
    pub fn fraction_within(&self, threshold: f64) -> Option<f64> {
        if self.total == 0 {
            return None;
        }
        let mut within = 0u64;
        for (i, c) in self.counts.iter().enumerate() {
            let upper = self.bounds.get(i).copied().unwrap_or(f64::INFINITY);
            if upper <= threshold {
                within += c;
            }
        }
        Some(within as f64 / self.total as f64)
    }

    /// Estimate the `q`-quantile (0.0..=1.0) by linear interpolation within the crossing
    /// bucket. Returns `(estimate, overflowed)`: when the quantile lands in the overflow
    /// bucket (above the largest bound), `estimate` is the largest bound (a floor) and
    /// `overflowed` is true. `None` if there are no samples.
    pub fn quantile(&self, q: f64) -> Option<(f64, bool)> {
        if self.total == 0 {
            return None;
        }
        let q = q.clamp(0.0, 1.0);
        // Fractional rank in [0, total].
        let target = q * self.total as f64;
        let mut cum = 0u64;
        for (i, c) in self.counts.iter().enumerate() {
            let next = cum + c;
            if (next as f64) >= target && *c > 0 {
                // Overflow bucket: no finite upper edge — floor at the last bound.
                let Some(&upper) = self.bounds.get(i) else {
                    return Some((*self.bounds.last().unwrap_or(&0.0), true));
                };
                let lower = if i == 0 { 0.0 } else { self.bounds[i - 1] };
                // How far into this bucket's count the target falls.
                let into = (target - cum as f64) / *c as f64;
                return Some((lower + (upper - lower) * into.clamp(0.0, 1.0), false));
            }
            cum = next;
        }
        // All mass at/below target — return the top finite bound.
        Some((*self.bounds.last().unwrap_or(&0.0), false))
    }

    /// Convenience: p50 / p95 / p99 (each with its overflow flag).
    pub fn p50(&self) -> Option<(f64, bool)> {
        self.quantile(0.50)
    }
    pub fn p95(&self) -> Option<(f64, bool)> {
        self.quantile(0.95)
    }
    pub fn p99(&self) -> Option<(f64, bool)> {
        self.quantile(0.99)
    }

    /// A millisecond histogram for content-free pipeline timing logs (issue #917). Bounds are
    /// fixed and ascending, so this cannot fail the way [`Self::new`] can.
    pub fn latency_ms() -> Self {
        const BOUNDS: [f64; 13] =
            [1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10000.0];
        Self::with_valid_bounds(BOUNDS.to_vec())
    }

    /// `n=… p50=…ms p95=…ms max=…ms` — numbers only, safe for logs. An overflowed quantile is
    /// shown as a floor (`>=10000ms`), never as an invented value.
    pub fn summary(&self) -> String {
        let q = |v: Option<(f64, bool)>| match v {
            Some((ms, true)) => format!(">={ms:.0}ms"),
            Some((ms, false)) => format!("{ms:.0}ms"),
            None => "-".into(),
        };
        format!(
            "n={} p50={} p95={} max={}",
            self.total,
            q(self.p50()),
            q(self.p95()),
            self.max().map_or("-".into(), |ms| format!("{ms:.0}ms"))
        )
    }
}

/// The six SLOs (NFR-SLO-01..06). `budget_p95` is the acceptance ceiling; `unit` documents
/// the sample unit (ms for latencies, percent for idle CPU).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Slo {
    /// Notch expand: frontend open handler → first painted open frame (≤150ms).
    Expand,
    /// Context actions presented: Expanded draw → 4 buttons drawn (≤150ms).
    ActionsPresented,
    /// Action execute → first streamed token (≤1000ms). NFR-SLO-03's primary metric: the first
    /// *visible* character, including preamble a tool turn later discards (2026-09-25 definition).
    FirstToken,
    /// NFR-SLO-03's secondary metric (issue #750): action execute → first character of the
    /// *final* answer — the text of the model turn that is committed, never a tool turn's
    /// discarded preamble. For a chat turn without tools it equals [`Slo::FirstToken`]. Reported
    /// as an improvement guide against the same 1s line; it is not an acceptance criterion
    /// ([`Slo::is_acceptance`]). Codex delegation records neither metric.
    FirstAnswerToken,
    /// Local search: query committed → results drawn (≤500ms).
    Search,
    /// Context cache update: focus.changed → cache swapped (≤300ms).
    CacheUpdate,
    /// Idle CPU 1-min average, percent (≤5%).
    IdleCpu,
    /// Mail triage Stage L per thread, ms (≤5ms; issue #319 §6).
    MailTriageLocal,
    /// Mail triage Stage B apply (results → rows), ms (≤500ms; the wall-clock batch latency is a
    /// separate, minutes-scale number that is recorded as ledger timestamps, not here).
    MailTriageBatchApply,
    /// One Gmail label / archive write-back, ms (≤5s; issue #319 §6).
    MailArchiveWrite,
    /// Clarify question card presented (issue #394): question known to the webview (push or
    /// expand read) → card drawn, ms (≤150ms — the same presentation budget as NFR-SLO-02).
    ClarifyPresented,
}

impl Slo {
    pub const ALL: [Slo; 11] = [
        Slo::Expand,
        Slo::ActionsPresented,
        Slo::FirstToken,
        Slo::FirstAnswerToken,
        Slo::Search,
        Slo::CacheUpdate,
        Slo::IdleCpu,
        Slo::MailTriageLocal,
        Slo::MailTriageBatchApply,
        Slo::MailArchiveWrite,
        Slo::ClarifyPresented,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Slo::Expand => "NFR-SLO-01",
            Slo::ActionsPresented => "NFR-SLO-02",
            Slo::FirstToken => "NFR-SLO-03",
            Slo::FirstAnswerToken => "NFR-SLO-03-final",
            Slo::Search => "NFR-SLO-04",
            Slo::CacheUpdate => "NFR-SLO-05",
            Slo::IdleCpu => "NFR-SLO-06",
            Slo::MailTriageLocal => "TG-SLO-01",
            Slo::MailTriageBatchApply => "TG-SLO-02",
            Slo::MailArchiveWrite => "TG-SLO-03",
            Slo::ClarifyPresented => "CL-SLO-01",
        }
    }

    /// Map a UI-reported metric name to its SLO. This is the contract for the desktop shell's
    /// `record_ui_slo(name, ms)` command: the webview reports the durations only
    /// it can observe (buttons painted, results drawn) under these fixed names, and the mapping
    /// lives here so the shell shim stays a dumb pipe and the naming is Linux-tested. `IdleCpu`
    /// is deliberately unmappable — it is sampled out-of-process, never a UI duration.
    pub fn from_ui_name(name: &str) -> Option<Slo> {
        match name {
            "expand" => Some(Slo::Expand),
            "actions_present" => Some(Slo::ActionsPresented),
            "first_token" => Some(Slo::FirstToken),
            "local_search" => Some(Slo::Search),
            "cache_update" => Some(Slo::CacheUpdate),
            "clarify_present" => Some(Slo::ClarifyPresented),
            _ => None,
        }
    }

    /// Whether this row is an acceptance criterion (its `pass` gates a release). Every SLO is,
    /// except NFR-SLO-03's secondary metric: §7.1 records it as an improvement guide only, so a
    /// reader of `pass:false` on that row must not take it for a missed SLO.
    pub fn is_acceptance(self) -> bool {
        !matches!(self, Slo::FirstAnswerToken)
    }

    /// The p95 acceptance ceiling (ms, or percent for `IdleCpu`).
    pub fn budget_p95(self) -> f64 {
        match self {
            Slo::Expand => 150.0,
            Slo::ActionsPresented => 150.0,
            Slo::FirstToken => 1000.0,
            // The primary's line, as a guide: the secondary is judged by nobody (is_acceptance).
            Slo::FirstAnswerToken => 1000.0,
            Slo::Search => 500.0,
            Slo::CacheUpdate => 300.0,
            Slo::IdleCpu => 5.0,
            Slo::MailTriageLocal => 5.0,
            Slo::MailTriageBatchApply => 500.0,
            Slo::MailArchiveWrite => 5000.0,
            Slo::ClarifyPresented => 150.0,
        }
    }

    /// Default bucket bounds — chosen so the budget is one of the edges (an over-budget p95
    /// therefore reads as strictly greater than the budget bound, never rounded under it).
    fn default_bounds(self) -> Vec<f64> {
        match self {
            Slo::Expand => vec![5.0, 10.0, 20.0, 30.0, 50.0, 75.0, 100.0, 150.0, 250.0, 500.0, 1000.0],
            Slo::ActionsPresented => vec![20.0, 40.0, 60.0, 80.0, 100.0, 150.0, 250.0, 500.0, 1000.0],
            Slo::FirstToken => vec![100.0, 250.0, 500.0, 750.0, 1000.0, 1500.0, 3000.0, 5000.0],
            // A tool turn's final answer starts after one or more tool reads, so the tail is
            // seconds long; wider top buckets keep its p95 a value rather than an overflow floor.
            Slo::FirstAnswerToken => {
                vec![100.0, 250.0, 500.0, 750.0, 1000.0, 1500.0, 3000.0, 5000.0, 10000.0, 20000.0, 30000.0]
            }
            Slo::Search => vec![50.0, 100.0, 200.0, 300.0, 500.0, 750.0, 1000.0, 2000.0],
            Slo::CacheUpdate => vec![30.0, 60.0, 100.0, 150.0, 300.0, 500.0, 1000.0],
            Slo::IdleCpu => vec![1.0, 2.0, 3.0, 4.0, 5.0, 8.0, 15.0, 30.0, 60.0, 100.0],
            Slo::MailTriageLocal => vec![0.5, 1.0, 2.0, 3.0, 5.0, 10.0, 20.0, 50.0, 100.0],
            Slo::MailTriageBatchApply => vec![50.0, 100.0, 250.0, 500.0, 1000.0, 2000.0, 5000.0],
            Slo::MailArchiveWrite => vec![250.0, 500.0, 1000.0, 2000.0, 5000.0, 10000.0, 30000.0],
            Slo::ClarifyPresented => vec![20.0, 40.0, 60.0, 80.0, 100.0, 150.0, 250.0, 500.0, 1000.0],
        }
    }
}

/// The process-wide registry the daemon's own hot paths record into (mail triage, and any other
/// core-side measurement that has no UI shim). The desktop shell merges this into the snapshot
/// it renders under Advanced / `shogun metrics` (NFR-SLO-00).
pub fn global_registry() -> &'static std::sync::Mutex<SloRegistry> {
    static REGISTRY: std::sync::OnceLock<std::sync::Mutex<SloRegistry>> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| std::sync::Mutex::new(SloRegistry::new()))
}

/// Record one sample into the process-wide registry. Never blocks on a poisoned lock.
pub fn record_global(slo: Slo, value: f64) {
    let mut guard = match global_registry().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    guard.record(slo, value);
}

/// NFR-SLO-03's two clocks for one chat turn (issue #750): when the first *visible* character
/// arrived (primary, [`Slo::FirstToken`]) and when the first character of the *final* answer did
/// (secondary, [`Slo::FirstAnswerToken`]).
///
/// The shells feed it the stream as it arrives, stamping each piece with the milliseconds since
/// the user's action; it keeps timestamps only, never the text. On the tool lane a model turn's
/// text is provisional until the provider says whether it was the answer (commit) or a preamble
/// before a tool call (discard), and the discard arrives *after* that turn's deltas — so the
/// first-character time of each open model turn is held until its verdict.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FirstTokenClock {
    first_visible_ms: Option<f64>,
    first_answer_ms: Option<f64>,
    /// (model turn, first visible character of that turn) for turns still awaiting a verdict.
    pending: Vec<(u32, f64)>,
}

impl FirstTokenClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// A delta on the tool-free stream: every character there is answer text, so the final
    /// answer starts with the first visible character. Empty text paints nothing and is ignored.
    pub fn plain_delta(&mut self, text: &str, at_ms: f64) {
        if text.is_empty() || !at_ms.is_finite() {
            return;
        }
        self.first_visible_ms.get_or_insert(at_ms);
        self.first_answer_ms.get_or_insert(at_ms);
    }

    /// A delta of tool-lane `model_turn`. It is visible now (primary), and becomes answer text
    /// only if that turn is later committed.
    pub fn tool_delta(&mut self, model_turn: u32, text: &str, at_ms: f64) {
        if text.is_empty() || !at_ms.is_finite() {
            return;
        }
        self.first_visible_ms.get_or_insert(at_ms);
        if !self.pending.iter().any(|(turn, _)| *turn == model_turn) {
            self.pending.push((model_turn, at_ms));
        }
    }

    /// `model_turn` was the final answer: its first character is the secondary metric. A later
    /// commit never moves an earlier one (the answer already started on screen).
    pub fn tool_commit(&mut self, model_turn: u32) {
        if let Some(i) = self.pending.iter().position(|(turn, _)| *turn == model_turn) {
            let (_, at) = self.pending.swap_remove(i);
            self.first_answer_ms.get_or_insert(at);
        }
    }

    /// `model_turn` ended in a tool call: what it painted was preamble, not the answer.
    pub fn tool_discard(&mut self, model_turn: u32) {
        self.pending.retain(|(turn, _)| *turn != model_turn);
    }

    /// Primary metric, ms since the action. `None` when nothing visible streamed.
    pub fn first_visible_ms(&self) -> Option<f64> {
        self.first_visible_ms
    }

    /// Secondary metric, ms since the action. `None` when no committed text streamed (a turn
    /// cancelled mid-answer, a body that arrived whole) — silence is not a zero.
    pub fn first_answer_ms(&self) -> Option<f64> {
        self.first_answer_ms
    }

    /// Each metric from `self` when it has one, else from `other` — how a shell merges the
    /// tool-lane clock with the tool-free stream it falls back to.
    pub fn or(self, other: Self) -> Self {
        Self {
            first_visible_ms: self.first_visible_ms.or(other.first_visible_ms),
            first_answer_ms: self.first_answer_ms.or(other.first_answer_ms),
            pending: Vec::new(),
        }
    }
}

/// One SLO's current standing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SloSnapshot {
    pub slo: Slo,
    pub count: u64,
    pub p50: f64,
    pub p95: f64,
    pub budget_p95: f64,
    /// True once at least one sample exists AND p95 ≤ budget. `false` while unmeasured — an
    /// SLO with no samples is UNMEASURED, never a pass (spec §4.5: silence ≠ success).
    pub pass: bool,
    pub measured: bool,
    /// True if p95 fell in the overflow bucket (reported value is a floor).
    pub p95_overflowed: bool,
}

/// The always-on registry: one histogram per SLO. The macOS adapter records into it on the
/// hot paths and reads snapshots for `shogun metrics` / the Advanced UI (NFR-SLO-00).
#[derive(Clone, Debug)]
pub struct SloRegistry {
    hists: Vec<(Slo, Histogram)>,
    /// Whether snapshots overlay the process-wide core-side registry (mail triage SLOs) when this
    /// registry has no samples of its own. On for the shells that render metrics; off (default)
    /// for the global registry itself and for tests.
    core_overlay: bool,
}

impl Default for SloRegistry {
    /// The shells build their registry through `Default` (the desktop `SloRegister` derives it):
    /// that registry renders metrics, so it carries the core-side overlay. The process-wide
    /// registry and tests use [`SloRegistry::new`], which stays isolated.
    fn default() -> Self {
        Self::with_core_overlay()
    }
}

impl SloRegistry {
    pub fn new() -> Self {
        let hists = Slo::ALL
            .iter()
            .map(|&s| {
                // default_bounds is always valid (ascending, non-empty); fall back to a
                // trivial single-bound histogram rather than unwrap, to honour the no-panic rule.
                let h = Histogram::new(s.default_bounds()).unwrap_or_else(|| {
                    Histogram::new(vec![s.budget_p95()]).unwrap_or_else(|| Histogram {
                        bounds: vec![1.0],
                        counts: vec![0, 0],
                        total: 0,
                        sum: 0.0,
                        max: f64::MIN,
                    })
                });
                (s, h)
            })
            .collect();
        Self { hists, core_overlay: false }
    }

    /// A registry whose snapshots also surface the core-side (process-wide) measurements — what
    /// the desktop shell and the `shogun-api` daemon render (NFR-SLO-00).
    pub fn with_core_overlay() -> Self {
        Self { core_overlay: true, ..Self::new() }
    }

    pub fn record(&mut self, slo: Slo, value: f64) {
        if let Some((_, h)) = self.hists.iter_mut().find(|(s, _)| *s == slo) {
            h.record(value);
        }
    }

    pub fn snapshot(&self, slo: Slo) -> SloSnapshot {
        let budget = slo.budget_p95();
        let Some((_, h)) = self.hists.iter().find(|(s, _)| *s == slo) else {
            return SloSnapshot { slo, count: 0, p50: 0.0, p95: 0.0, budget_p95: budget, pass: false, measured: false, p95_overflowed: false };
        };
        // The core-side measurements (mail triage, issue #319) land in the process-wide registry,
        // not in the shell's own. A shell registry with no samples of its own for one of those
        // SLOs reports the global histogram instead, so `shogun metrics` / Advanced show them
        // without every caller threading a second registry through. `try_lock` keeps the global
        // registry's own snapshot from re-entering itself.
        let core_side = matches!(slo, Slo::MailTriageLocal | Slo::MailTriageBatchApply | Slo::MailArchiveWrite);
        if self.core_overlay && core_side && h.count() == 0 {
            if let Ok(global) = global_registry().try_lock() {
                if let Some((_, gh)) = global.hists.iter().find(|(s, _)| *s == slo) {
                    if gh.count() > 0 {
                        return Self::snapshot_of(slo, gh, budget);
                    }
                }
            }
        }
        Self::snapshot_of(slo, h, budget)
    }

    fn snapshot_of(slo: Slo, h: &Histogram, budget: f64) -> SloSnapshot {
        match (h.p50(), h.p95()) {
            (Some((p50, _)), Some((p95, over))) => SloSnapshot {
                slo,
                count: h.count(),
                p50,
                p95,
                budget_p95: budget,
                pass: p95 <= budget,
                measured: true,
                p95_overflowed: over,
            },
            _ => SloSnapshot { slo, count: 0, p50: 0.0, p95: 0.0, budget_p95: budget, pass: false, measured: false, p95_overflowed: false },
        }
    }

    pub fn snapshot_all(&self) -> Vec<SloSnapshot> {
        Slo::ALL.iter().map(|&s| self.snapshot(s)).collect()
    }
}

/// NFR-SLO-06 sampling window (issue #355): turns a stream of short CPU% readings into one
/// "idle minute" average, and only when every reading in that minute was taken while the notch was
/// idle. A minute that included an expand, a chat answer or any other panel activity is not idle
/// CPU and is discarded rather than averaged in — the SLO is about what SHOGUN costs while the
/// user is not using it.
#[derive(Clone, Debug)]
pub struct IdleCpuWindow {
    per_window: usize,
    sum: f64,
    count: usize,
}

impl IdleCpuWindow {
    /// `per_window` readings make one sample (12 × 5s = the SLO's 1-minute average).
    pub fn new(per_window: usize) -> Self {
        Self {
            per_window: per_window.max(1),
            sum: 0.0,
            count: 0,
        }
    }

    /// Feed one reading. Returns the window's mean once `per_window` consecutive idle readings
    /// have accumulated; a non-idle or non-finite reading restarts the window.
    pub fn push(&mut self, cpu_pct: f64, idle: bool) -> Option<f64> {
        if !idle || !cpu_pct.is_finite() || cpu_pct < 0.0 {
            self.sum = 0.0;
            self.count = 0;
            return None;
        }
        self.sum += cpu_pct;
        self.count += 1;
        if self.count < self.per_window {
            return None;
        }
        let mean = self.sum / self.count as f64;
        self.sum = 0.0;
        self.count = 0;
        Some(mean)
    }
}

/// L5 lesson counters for the `shogun metrics` surface: how many lessons are active
/// and how much feedback the last week recorded. Counts only — no instruction text, and never
/// any `feedback_events` content (CLAUDE.md: capture/user text stays out of metrics surfaces).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LessonCounters {
    pub active_lessons: i64,
    pub feedback_events_last_7d: i64,
    /// Approval-time edits in the window — the distill input (designs §5.5).
    pub edit_before_approve_last_7d: i64,
    /// Unchanged approvals in the window — the "took the draft as-is" rate numerator.
    pub approve_unchanged_last_7d: i64,
}

/// Work Patterns counters (FR-PAT-07): how much was offered vs adopted in the last week, and how
/// many profile rows are live. Counts only — never pattern keys, never captured text; and per
/// FR-PAT-07 these render on the LOCAL metrics face only (no telemetry path exists for them).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PatternCounters {
    /// Offered candidates in the last 7 days (`offer_impressions`) — the CTR denominator.
    pub offers_shown_7d: i64,
    /// Decisions (adopt or reject) on offered kinds in the window.
    pub offers_decided_7d: i64,
    /// Adoptions in the window — the CTR numerator.
    pub offers_adopted_7d: i64,
    /// Live `work_patterns` rows (active = 1).
    pub active_patterns: i64,
}

/// Last-assemble compression snapshot for `shogun metrics` / `GET /v1/metrics`. Counts only —
/// never query text, never prompt body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompressionHarness {
    pub enabled: bool,
    pub pre_tokens: u64,
    pub post_tokens: u64,
    pub dropped: u64,
}

/// Last tool-loop snapshot. `uses` is executed reads; refusals never spend the tool budget.
///
/// The latency pair is per-tool-call round-trip time in milliseconds, measured by
/// `shogun_mcp::tool_loop::TimedEvents` over the same event stream the UI microcopy rides on.
///
/// A median and a maximum rather than a p50/p95 pair, because this snapshot covers **one turn**,
/// which executes at most `MAX_TOOL_USES` reads: a nearest-rank 95th percentile over that few is
/// the maximum for every possible input, so publishing it as a percentile would put a number in
/// the SLO face that means something other than what it says. `slowest_ms` is the honest name for
/// it, and a usefully conservative reading against the per-tool budget. Cross-turn p50/p95 needs a
/// window outliving one turn and belongs to the wiring layer.
///
/// Both are `None` when the turn called no tools: a measurement of nothing is not zero, and a
/// fabricated 0ms in the SLO face is worse than an honest "unmeasured".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolLoopHarness {
    pub uses: u32,
    pub refusals: u32,
    pub proposes: u32,
    pub timeouts: u32,
    /// Median round-trip of this turn's reads.
    pub median_ms: Option<u32>,
    /// Slowest read of this turn.
    pub slowest_ms: Option<u32>,
}

/// Harness counters riding next to `lessons` on the SLO JSON face. Either sub-block may be
/// unknown independently; a process that has never assembled or looped reports the whole
/// object as `measured:false` rather than a fabricated zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HarnessCounters {
    pub compression: Option<CompressionHarness>,
    pub tool_loop: Option<ToolLoopHarness>,
}

impl HarnessCounters {
    /// `None` when nothing has been observed — the renderer flags that as unmeasured.
    pub fn measured(self) -> Option<Self> {
        if self.compression.is_none() && self.tool_loop.is_none() {
            None
        } else {
            Some(self)
        }
    }
}

/// One route's shared-service round trips (issue #29 §4-6). Deliberately **not** an [`Slo`]: the
/// NFR-SLO rows are local latencies SHOGUN owns, while these depend on the network and on the
/// shared service, so they are reported in their own block with no pass/fail budget. Timings and
/// counts only — no URL, no id, no content.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SharedServiceLatency {
    /// The traceability route vocabulary (`batch_relay`, `billing`, …).
    pub route: &'static str,
    pub count: u64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    /// Round trips the service answered 429 / 503.
    pub throttled: u64,
}

/// The `shared_service` block: `{"window_days":7,"routes":[…],"measured":true}`, or
/// `{"measured":false}` when it could not be read. A readable table with no rows is measured and
/// empty — nothing crossed to a shared service this week.
fn shared_service_json(shared: Option<&[SharedServiceLatency]>) -> String {
    match shared {
        None => r#"{"measured":false}"#.to_string(),
        Some(routes) => {
            let items: Vec<String> = routes
                .iter()
                .map(|r| {
                    format!(
                        r#"{{"route":"{}","count":{},"p50_ms":{},"p95_ms":{},"throttled":{}}}"#,
                        r.route,
                        r.count,
                        num(r.p50_ms),
                        num(r.p95_ms),
                        r.throttled
                    )
                })
                .collect();
            format!(r#"{{"window_days":7,"routes":[{}],"measured":true}}"#, items.join(","))
        }
    }
}

/// [`render_snapshots_json_full`] plus the `shared_service` block (issue #29 §4-6): the p95 of
/// each shared-service route's round trips, apart from the NFR-SLO rows. This is what the
/// `shogun-api` process serves at `GET /v1/metrics`, so REST, MCP and `shogun metrics` all read
/// the same block.
pub fn render_snapshots_json_with_shared_service(
    snapshots: &[SloSnapshot],
    lessons: Option<LessonCounters>,
    harness: Option<HarnessCounters>,
    sanitizer: SanitizerCounters,
    patterns: Option<PatternCounters>,
    shared_service: Option<&[SharedServiceLatency]>,
) -> String {
    let base = render_snapshots_json_full(snapshots, lessons, harness, sanitizer, patterns);
    // `base` always ends with the closing brace of the top-level object.
    let head = base.strip_suffix('}').unwrap_or(&base);
    format!(r#"{head},"shared_service":{}}}"#, shared_service_json(shared_service))
}

/// Render SLO snapshots as the JSON both `shogun metrics` and the Advanced UI read (NFR-SLO-00).
/// Hand-rolled (no serde dep in this ungated module). An unmeasured SLO reports `measured:false`
/// and `pass:false` — silence is never success (spec §4.5).
pub fn render_snapshots_json(snapshots: &[SloSnapshot]) -> String {
    format!(r#"{{"metrics":[{}]}}"#, slo_items(snapshots).join(","))
}

/// Hidden-format sanitizer counts (memory-poisoning P2) plus persist-gate drops (P4).
/// Zeros are a real measurement — every persist goes through the strip. Never the captured text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct SanitizerCounters {
    pub events_stripped: u64,
    pub chars_removed: u64,
    pub instruction_shaped_dropped: u64,
}

/// [`render_snapshots_json`] plus the `lessons` block. `None` (counters not computable —
/// no DB behind this process, or a read failure) renders `"lessons":{"measured":false}` in the
/// crate's convention: an unmeasured value is flagged, never fabricated as zero.
///
/// Always includes a `harness` object: unmeasured until compression or a tool loop has run.
/// Always includes a `sanitizer` object (counts since process start; zeros are a real measurement).
pub fn render_snapshots_json_with_lessons(
    snapshots: &[SloSnapshot],
    lessons: Option<LessonCounters>,
) -> String {
    render_snapshots_json_with_lessons_harness_and_sanitizer(
        snapshots,
        lessons,
        None,
        SanitizerCounters::default(),
    )
}

/// [`render_snapshots_json_with_lessons`] plus the H1 `harness` block (compression + tool loop).
pub fn render_snapshots_json_with_lessons_and_harness(
    snapshots: &[SloSnapshot],
    lessons: Option<LessonCounters>,
    harness: Option<HarnessCounters>,
) -> String {
    render_snapshots_json_with_lessons_harness_and_sanitizer(
        snapshots,
        lessons,
        harness,
        SanitizerCounters::default(),
    )
}

/// [`render_snapshots_json_with_lessons`] plus live sanitizer counters from the persist path.
pub fn render_snapshots_json_with_lessons_and_sanitizer(
    snapshots: &[SloSnapshot],
    lessons: Option<LessonCounters>,
    sanitizer: SanitizerCounters,
) -> String {
    render_snapshots_json_with_lessons_harness_and_sanitizer(snapshots, lessons, None, sanitizer)
}

/// Full metrics face: SLOs + lessons + harness + sanitizer. Never captured text.
pub fn render_snapshots_json_with_lessons_harness_and_sanitizer(
    snapshots: &[SloSnapshot],
    lessons: Option<LessonCounters>,
    harness: Option<HarnessCounters>,
    sanitizer: SanitizerCounters,
) -> String {
    render_snapshots_json_full(snapshots, lessons, harness, sanitizer, None)
}

/// The complete metrics face: SLOs + lessons + harness + sanitizer + Work Patterns (FR-PAT-07).
/// `patterns: None` renders `{"measured":false}` — unmeasured is flagged, never a fabricated zero.
pub fn render_snapshots_json_full(
    snapshots: &[SloSnapshot],
    lessons: Option<LessonCounters>,
    harness: Option<HarnessCounters>,
    sanitizer: SanitizerCounters,
    patterns: Option<PatternCounters>,
) -> String {
    format!(
        r#"{{"metrics":[{}],"lessons":{},"harness":{},"sanitizer":{},"patterns":{}}}"#,
        slo_items(snapshots).join(","),
        lessons_json(lessons),
        harness_json(harness),
        sanitizer_json(sanitizer),
        patterns_json(patterns),
    )
}

fn patterns_json(patterns: Option<PatternCounters>) -> String {
    match patterns {
        Some(c) => format!(
            r#"{{"offers_shown_7d":{},"offers_decided_7d":{},"offers_adopted_7d":{},"active_patterns":{},"measured":true}}"#,
            c.offers_shown_7d, c.offers_decided_7d, c.offers_adopted_7d, c.active_patterns
        ),
        None => r#"{"measured":false}"#.to_string(),
    }
}

fn lessons_json(lessons: Option<LessonCounters>) -> String {
    match lessons {
        Some(c) => format!(
            r#"{{"active_lessons":{},"feedback_events_last_7d":{},"edit_before_approve_last_7d":{},"approve_unchanged_last_7d":{},"lesson_hit_rate":{{"measured":false}},"measured":true}}"#,
            c.active_lessons,
            c.feedback_events_last_7d,
            c.edit_before_approve_last_7d,
            c.approve_unchanged_last_7d
        ),
        None => r#"{"measured":false}"#.to_string(),
    }
}

fn harness_json(harness: Option<HarnessCounters>) -> String {
    match harness.and_then(HarnessCounters::measured) {
        None => r#"{"measured":false}"#.to_string(),
        Some(h) => format!(
            r#"{{"measured":true,"compression":{},"tool_loop":{}}}"#,
            compression_json(h.compression),
            tool_loop_json(h.tool_loop),
        ),
    }
}

fn compression_json(snap: Option<CompressionHarness>) -> String {
    match snap {
        Some(c) => format!(
            r#"{{"pre_tokens":{},"post_tokens":{},"dropped":{},"enabled":{},"measured":true}}"#,
            c.pre_tokens, c.post_tokens, c.dropped, c.enabled
        ),
        None => r#"{"measured":false}"#.to_string(),
    }
}

fn tool_loop_json(snap: Option<ToolLoopHarness>) -> String {
    match snap {
        Some(t) => format!(
            r#"{{"uses":{},"refusals":{},"proposes":{},"timeouts":{},"median_ms":{},"slowest_ms":{},"measured":true}}"#,
            t.uses,
            t.refusals,
            t.proposes,
            t.timeouts,
            // `null`, not 0 — a turn with no tool calls measured no latency.
            optional_ms_json(t.median_ms),
            optional_ms_json(t.slowest_ms)
        ),
        None => r#"{"measured":false}"#.to_string(),
    }
}

fn optional_ms_json(ms: Option<u32>) -> String {
    ms.map_or_else(|| "null".to_string(), |v| v.to_string())
}

fn sanitizer_json(s: SanitizerCounters) -> String {
    format!(
        r#"{{"events_stripped":{},"chars_removed":{},"instruction_shaped_dropped":{}}}"#,
        s.events_stripped, s.chars_removed, s.instruction_shaped_dropped
    )
}

fn slo_items(snapshots: &[SloSnapshot]) -> Vec<String> {
    snapshots
        .iter()
        .map(|s| {
            format!(
                r#"{{"slo":"{}","count":{},"p50":{},"p95":{},"budget_p95":{},"pass":{},"measured":{},"p95_overflowed":{},"acceptance":{}}}"#,
                s.slo.id(),
                s.count,
                num(s.p50),
                num(s.p95),
                num(s.budget_p95),
                s.pass,
                s.measured,
                s.p95_overflowed,
                s.slo.is_acceptance(),
            )
        })
        .collect()
}

/// Format an f64 as a finite JSON number (non-finite → 0 so the payload is always valid JSON).
fn num(v: f64) -> String {
    if v.is_finite() {
        format!("{v}")
    } else {
        "0".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_service_block_is_separate_from_the_slo_rows() {
        let reg = SloRegistry::new();
        let routes = [SharedServiceLatency {
            route: "batch_relay",
            count: 12,
            p50_ms: 180.0,
            p95_ms: 950.0,
            throttled: 1,
        }];
        let json = render_snapshots_json_with_shared_service(
            &reg.snapshot_all(),
            None,
            None,
            SanitizerCounters::default(),
            None,
            Some(&routes),
        );
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        let block = &v["shared_service"];
        assert_eq!(block["measured"], true);
        assert_eq!(block["routes"][0]["route"], "batch_relay");
        assert_eq!(block["routes"][0]["p95_ms"], 950.0);
        // Not an SLO row: no NFR id, no budget, no pass/fail.
        let slo_ids: Vec<&str> = v["metrics"]
            .as_array()
            .expect("metrics array")
            .iter()
            .filter_map(|m| m["slo"].as_str())
            .collect();
        assert!(slo_ids.iter().all(|id| !id.contains("SVC")));
        assert!(block["routes"][0].get("budget_p95").is_none());

        let unreadable = render_snapshots_json_with_shared_service(
            &[],
            None,
            None,
            SanitizerCounters::default(),
            None,
            None,
        );
        let v: serde_json::Value = serde_json::from_str(&unreadable).expect("valid JSON");
        assert_eq!(v["shared_service"]["measured"], false);
    }

    #[test]
    fn idle_cpu_window_averages_a_full_idle_minute() {
        let mut w = IdleCpuWindow::new(3);
        assert_eq!(w.push(1.0, true), None);
        assert_eq!(w.push(2.0, true), None);
        assert_eq!(w.push(3.0, true), Some(2.0));
        // The next window starts empty.
        assert_eq!(w.push(9.0, true), None);
    }

    #[test]
    fn idle_cpu_window_discards_a_minute_with_activity() {
        let mut w = IdleCpuWindow::new(3);
        w.push(1.0, true);
        w.push(1.0, true);
        // The panel expanded: this minute is not idle CPU.
        assert_eq!(w.push(40.0, false), None);
        assert_eq!(w.push(1.0, true), None);
        assert_eq!(w.push(1.0, true), None);
        assert_eq!(w.push(4.0, true), Some(2.0));
        // Garbage readings restart rather than poison the mean.
        assert_eq!(w.push(f64::NAN, true), None);
    }

    #[test]
    fn rejects_bad_bounds() {
        assert!(Histogram::new(vec![]).is_none());
        assert!(Histogram::new(vec![10.0, 10.0]).is_none()); // not strictly ascending
        assert!(Histogram::new(vec![10.0, 5.0]).is_none()); // descending
        assert!(Histogram::new(vec![1.0, 2.0, 3.0]).is_some());
    }

    #[test]
    fn empty_histogram_reports_no_stats() {
        let h = Histogram::new(vec![10.0, 100.0]).unwrap();
        assert_eq!(h.count(), 0);
        assert!(h.max().is_none());
        assert!(h.mean().is_none());
        assert!(h.quantile(0.95).is_none());
        assert!(h.fraction_within(5.0).is_none());
    }

    #[test]
    fn records_count_max_mean() {
        let mut h = Histogram::new(vec![10.0, 50.0, 100.0]).unwrap();
        for v in [5.0, 15.0, 45.0, 200.0] {
            h.record(v);
        }
        assert_eq!(h.count(), 4);
        assert_eq!(h.max(), Some(200.0));
        assert_eq!(h.mean(), Some((5.0 + 15.0 + 45.0 + 200.0) / 4.0));
    }

    #[test]
    fn nan_is_ignored() {
        let mut h = Histogram::new(vec![10.0]).unwrap();
        h.record(f64::NAN);
        assert_eq!(h.count(), 0);
    }

    #[test]
    fn latency_summary_is_numbers_only_and_floors_overflow() {
        let mut h = Histogram::latency_ms();
        assert_eq!(h.summary(), "n=0 p50=- p95=- max=-");
        for _ in 0..19 {
            h.record(3.0);
        }
        h.record(60_000.0);
        let s = h.summary();
        assert!(s.starts_with("n=20 p50="), "{s}");
        assert!(s.ends_with("max=60000ms"), "{s}");
        h.record(60_000.0);
        h.record(60_000.0);
        assert!(h.summary().contains("p95=>=10000ms"), "{}", h.summary());
    }

    #[test]
    fn quantile_interpolates_within_bucket() {
        // 100 samples uniformly across (0,10] land in the first bucket [0,10].
        let mut h = Histogram::new(vec![10.0, 20.0, 100.0]).unwrap();
        for i in 1..=100 {
            h.record(i as f64 / 10.0); // 0.1 .. 10.0, all <= 10
        }
        let (p50, over) = h.p50().unwrap();
        assert!(!over);
        // Median should be ~ halfway into the [0,10] bucket.
        assert!((p50 - 5.0).abs() < 1.0, "p50={p50}");
    }

    #[test]
    fn quantile_flags_overflow() {
        let mut h = Histogram::new(vec![10.0, 100.0]).unwrap();
        // Most samples above 100 → p95 overflows.
        for _ in 0..99 {
            h.record(500.0);
        }
        h.record(5.0);
        let (p95, over) = h.p95().unwrap();
        assert!(over);
        assert_eq!(p95, 100.0); // floored at the top bound
    }

    #[test]
    fn fraction_within_is_conservative() {
        let mut h = Histogram::new(vec![1.0, 2.0, 5.0, 100.0]).unwrap();
        for _ in 0..95 {
            h.record(0.5); // bucket <=1
        }
        for _ in 0..5 {
            h.record(50.0); // bucket <=100
        }
        // 95 of 100 are <= 5 (whole buckets <=1, <=2, <=5).
        assert_eq!(h.fraction_within(5.0), Some(0.95));
    }

    #[test]
    fn slo_pass_when_under_budget() {
        let mut reg = SloRegistry::new();
        // Expand: 100 fast samples (~15ms) → p95 well under 150ms.
        for _ in 0..100 {
            reg.record(Slo::Expand, 15.0);
        }
        let s = reg.snapshot(Slo::Expand);
        assert!(s.measured);
        assert!(s.pass, "p95={} budget={}", s.p95, s.budget_p95);
        assert_eq!(s.budget_p95, 150.0);
    }

    #[test]
    fn slo_fail_when_over_budget() {
        let mut reg = SloRegistry::new();
        for _ in 0..100 {
            reg.record(Slo::CacheUpdate, 900.0); // way over 300ms
        }
        let s = reg.snapshot(Slo::CacheUpdate);
        assert!(s.measured);
        assert!(!s.pass);
    }

    #[test]
    fn unmeasured_slo_is_not_a_pass() {
        let reg = SloRegistry::new();
        for s in reg.snapshot_all() {
            assert!(!s.measured, "{} should be unmeasured", s.slo.id());
            assert!(!s.pass, "{} unmeasured must not pass", s.slo.id());
        }
    }

    #[test]
    fn all_slos_have_distinct_ids_and_budgets() {
        let ids: Vec<&str> = Slo::ALL.iter().map(|s| s.id()).collect();
        let mut uniq = ids.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(ids.len(), uniq.len());
        // Budgets match the CLAUDE.md / §7.1 table.
        assert_eq!(Slo::Expand.budget_p95(), 150.0);
        assert_eq!(Slo::IdleCpu.budget_p95(), 5.0);
    }

    #[test]
    fn core_side_mail_slos_surface_through_any_registry_snapshot() {
        record_global(Slo::MailTriageLocal, 2.0);
        let local = SloRegistry::with_core_overlay();
        let snap = local.snapshot(Slo::MailTriageLocal);
        assert!(snap.measured, "the shell registry reports the core-side sample");
        // Other tests in this process record real Stage L timings into the same global
        // registry, so only the presence of samples is asserted — never their values.
        assert!(snap.count >= 1);
        assert!(!SloRegistry::new().snapshot(Slo::MailTriageLocal).measured, "plain registries stay isolated");
        // A registry with its own samples keeps them.
        let mut own = SloRegistry::with_core_overlay();
        own.record(Slo::MailTriageLocal, 50.0);
        assert!(!own.snapshot(Slo::MailTriageLocal).pass);
    }

    #[test]
    fn first_answer_is_a_separate_guide_row_next_to_the_primary() {
        // Issue #750: its own row and id; the primary keeps NFR-SLO-03 unchanged.
        assert_eq!(Slo::FirstToken.id(), "NFR-SLO-03");
        assert_eq!(Slo::FirstAnswerToken.id(), "NFR-SLO-03-final");
        assert_eq!(Slo::FirstAnswerToken.budget_p95(), Slo::FirstToken.budget_p95());
        // Only the primary is an acceptance criterion (§7.1 NFR-SLO-03 definition).
        assert!(Slo::FirstToken.is_acceptance());
        assert!(!Slo::FirstAnswerToken.is_acceptance());
        assert!(Slo::ALL.iter().filter(|s| !s.is_acceptance()).eq([Slo::FirstAnswerToken].iter()));
        // Recorded from the shell's stream clock, never reported by the webview under a name.
        assert!(!["first_answer", "first_answer_token", "NFR-SLO-03-final"]
            .iter()
            .any(|n| Slo::from_ui_name(n).is_some()));

        let mut reg = SloRegistry::new();
        reg.record(Slo::FirstToken, 300.0);
        reg.record(Slo::FirstAnswerToken, 2400.0);
        assert_eq!(reg.snapshot(Slo::FirstToken).count, 1);
        let answer = reg.snapshot(Slo::FirstAnswerToken);
        assert!(answer.measured);
        // A multi-second tool turn lands in a real bucket, not the overflow floor.
        assert!(!answer.p95_overflowed);
        let json = render_snapshots_json(&reg.snapshot_all());
        assert!(json.contains(r#""slo":"NFR-SLO-03","count":1"#), "{json}");
        assert!(json.contains(r#""slo":"NFR-SLO-03-final","count":1"#), "{json}");
        assert!(json.contains(r#""p95_overflowed":false,"acceptance":false}"#), "{json}");
        assert!(json.contains(r#""acceptance":true}"#), "{json}");
    }

    #[test]
    fn plain_stream_answer_starts_with_the_first_visible_character() {
        let mut clock = FirstTokenClock::new();
        assert_eq!((clock.first_visible_ms(), clock.first_answer_ms()), (None, None));
        clock.plain_delta("", 50.0); // paints nothing
        clock.plain_delta("Hel", 320.0);
        clock.plain_delta("lo", 340.0);
        assert_eq!(clock.first_visible_ms(), Some(320.0));
        assert_eq!(clock.first_answer_ms(), Some(320.0));
    }

    #[test]
    fn tool_turn_answer_clock_skips_discarded_preamble() {
        let mut clock = FirstTokenClock::new();
        // Turn 0: "Let me check." then a tool call — visible, then discarded.
        clock.tool_delta(0, "Let me check.", 280.0);
        clock.tool_discard(0);
        // Turn 1: the answer, committed.
        clock.tool_delta(1, "Two ", 1900.0);
        clock.tool_delta(1, "meetings.", 1950.0);
        clock.tool_commit(1);
        assert_eq!(clock.first_visible_ms(), Some(280.0), "primary counts the preamble");
        assert_eq!(clock.first_answer_ms(), Some(1900.0), "secondary starts at the committed turn");
    }

    #[test]
    fn tool_turn_without_preamble_has_equal_clocks() {
        let mut clock = FirstTokenClock::new();
        clock.tool_delta(0, "", 100.0);
        clock.tool_delta(0, "Hi", 410.0);
        clock.tool_commit(0);
        assert_eq!(clock.first_visible_ms(), Some(410.0));
        assert_eq!(clock.first_answer_ms(), Some(410.0));
    }

    #[test]
    fn uncommitted_or_silent_turns_leave_the_answer_clock_unmeasured() {
        // Silent tool turn (no preamble) then a committed turn with no text: nothing to time.
        let mut clock = FirstTokenClock::new();
        clock.tool_discard(0);
        clock.tool_commit(1);
        assert_eq!((clock.first_visible_ms(), clock.first_answer_ms()), (None, None));

        // Cancelled mid-answer: visible text, but no commit ever came.
        let mut clock = FirstTokenClock::new();
        clock.tool_delta(0, "Part", 500.0);
        assert_eq!(clock.first_visible_ms(), Some(500.0));
        assert_eq!(clock.first_answer_ms(), None);

        // A commit for a different turn does not adopt another turn's time; a second commit does
        // not move the first.
        let mut clock = FirstTokenClock::new();
        clock.tool_delta(0, "a", 200.0);
        clock.tool_commit(1);
        assert_eq!(clock.first_answer_ms(), None);
        clock.tool_commit(0);
        clock.tool_delta(2, "b", 900.0);
        clock.tool_commit(2);
        assert_eq!(clock.first_answer_ms(), Some(200.0));

        // A non-finite stamp is dropped rather than recorded.
        let mut clock = FirstTokenClock::new();
        clock.plain_delta("x", f64::NAN);
        assert_eq!(clock.first_visible_ms(), None);
    }

    #[test]
    fn clocks_merge_per_metric() {
        let mut tool = FirstTokenClock::new();
        tool.tool_delta(0, "Let me check.", 300.0);
        tool.tool_discard(0);
        let mut plain = FirstTokenClock::new();
        plain.plain_delta("x", 700.0);
        let merged = tool.or(plain);
        assert_eq!(merged.first_visible_ms(), Some(300.0));
        assert_eq!(merged.first_answer_ms(), Some(700.0));
        assert_eq!(FirstTokenClock::new().or(FirstTokenClock::new()), FirstTokenClock::new());
    }

    #[test]
    fn ui_names_map_to_their_slos() {
        assert_eq!(Slo::from_ui_name("actions_present"), Some(Slo::ActionsPresented));
        assert_eq!(Slo::from_ui_name("local_search"), Some(Slo::Search));
        assert_eq!(Slo::from_ui_name("expand"), Some(Slo::Expand));
        assert_eq!(Slo::from_ui_name("first_token"), Some(Slo::FirstToken));
        assert_eq!(Slo::from_ui_name("cache_update"), Some(Slo::CacheUpdate));
        assert_eq!(Slo::from_ui_name("clarify_present"), Some(Slo::ClarifyPresented));
        assert_eq!(Slo::ClarifyPresented.budget_p95(), 150.0);
        // Unknown names and the out-of-process CPU sample must not silently land in a histogram.
        assert_eq!(Slo::from_ui_name("idle_cpu"), None);
        assert_eq!(Slo::from_ui_name(""), None);
        assert_eq!(Slo::from_ui_name("Actions_Present"), None);
    }

    #[test]
    fn render_json_marks_measured_and_unmeasured() {
        let mut reg = SloRegistry::new();
        // one measured SLO (a fast expand), the rest unmeasured
        reg.record(Slo::Expand, 40.0);
        let json = render_snapshots_json(&reg.snapshot_all());
        assert!(json.starts_with(r#"{"metrics":["#));
        // the measured Expand SLO reports NFR-SLO-01, a sample, and passes (40 ≤ 150)
        assert!(json.contains(r#""slo":"NFR-SLO-01","count":1"#), "{json}");
        assert!(json.contains(r#""measured":true,"p95_overflowed":false"#));
        // an unmeasured SLO is measured:false and pass:false (silence ≠ success)
        assert!(json.contains(r#""slo":"NFR-SLO-04","count":0,"p50":0,"p95":0,"budget_p95":500,"pass":false,"measured":false"#), "{json}");
    }

    #[test]
    fn pattern_counters_render_measured_or_flagged_unmeasured() {
        let reg = SloRegistry::new();
        let snaps = reg.snapshot_all();
        let json = render_snapshots_json_full(
            &snaps,
            None,
            None,
            SanitizerCounters::default(),
            Some(PatternCounters {
                offers_shown_7d: 40,
                offers_decided_7d: 12,
                offers_adopted_7d: 9,
                active_patterns: 5,
            }),
        );
        assert!(json.contains(r#""patterns":{"offers_shown_7d":40,"offers_decided_7d":12,"offers_adopted_7d":9,"active_patterns":5,"measured":true}"#), "{json}");
        // not computable → measured:false, never a fabricated zero (FR-PAT-07)
        let json = render_snapshots_json_full(&snaps, None, None, SanitizerCounters::default(), None);
        assert!(json.contains(r#""patterns":{"measured":false}"#), "{json}");
        // the narrower helper keeps its old shape plus the flagged patterns block
        let json = render_snapshots_json_with_lessons(&snaps, None);
        assert!(json.contains(r#""patterns":{"measured":false}"#), "{json}");
    }

    #[test]
    fn lesson_counters_render_measured_or_flagged_unmeasured() {
        let reg = SloRegistry::new();
        let snaps = reg.snapshot_all();
        // computable counters render with measured:true
        let json = render_snapshots_json_with_lessons(
            &snaps,
            Some(LessonCounters {
                active_lessons: 3,
                feedback_events_last_7d: 12,
                edit_before_approve_last_7d: 4,
                approve_unchanged_last_7d: 8,
            }),
        );
        assert!(json.contains(r#""lessons":{"active_lessons":3,"feedback_events_last_7d":12,"edit_before_approve_last_7d":4,"approve_unchanged_last_7d":8,"lesson_hit_rate":{"measured":false},"measured":true}"#), "{json}");
        assert!(json.starts_with(r#"{"metrics":["#));
        // not computable → measured:false, never a fabricated zero
        let json = render_snapshots_json_with_lessons(&snaps, None);
        assert!(json.contains(r#""lessons":{"measured":false}"#), "{json}");
        assert!(!json.contains("active_lessons"), "{json}");
        // harness rides on the same face; unknown is flagged, never a fabricated zero
        assert!(json.contains(r#""harness":{"measured":false}"#), "{json}");
        assert!(!json.contains("pre_tokens"), "{json}");
        assert!(!json.contains("\"uses\":"), "{json}");
        // sanitizer zeros are a real measurement (every persist goes through the strip)
        assert!(
            json.contains(r#""sanitizer":{"events_stripped":0,"chars_removed":0,"instruction_shaped_dropped":0}"#),
            "{json}"
        );
    }

    #[test]
    fn harness_counters_render_measured_or_flagged_unmeasured() {
        let snaps = SloRegistry::new().snapshot_all();
        let json = render_snapshots_json_with_lessons_and_harness(&snaps, None, None);
        assert!(json.contains(r#""harness":{"measured":false}"#), "{json}");
        assert!(!json.contains("pre_tokens"), "{json}");

        let json = render_snapshots_json_with_lessons_and_harness(
            &snaps,
            None,
            Some(HarnessCounters {
                compression: Some(CompressionHarness {
                    enabled: true,
                    pre_tokens: 200,
                    post_tokens: 80,
                    dropped: 3,
                }),
                tool_loop: None,
            }),
        );
        assert!(json.contains(r#""harness":{"measured":true"#), "{json}");
        assert!(
            json.contains(r#"{"pre_tokens":200,"post_tokens":80,"dropped":3,"enabled":true,"measured":true}"#),
            "{json}"
        );
        assert!(json.contains(r#""tool_loop":{"measured":false}"#), "{json}");
        assert!(!json.contains("\"uses\":"), "{json}");

        let json = render_snapshots_json_with_lessons_and_harness(
            &snaps,
            None,
            Some(HarnessCounters {
                compression: None,
                tool_loop: Some(ToolLoopHarness {
                    uses: 2,
                    refusals: 1,
                    proposes: 1,
                    timeouts: 0,
                    median_ms: Some(180),
                    slowest_ms: Some(420),
                }),
            }),
        );
        assert!(
            json.contains(
                r#""tool_loop":{"uses":2,"refusals":1,"proposes":1,"timeouts":0,"median_ms":180,"slowest_ms":420,"measured":true}"#
            ),
            "{json}"
        );
        assert!(json.contains(r#""compression":{"measured":false}"#), "{json}");
        // counts only — never a place for prompt / query text
        assert!(!json.contains("tool_use"), "{json}");
        assert!(!json.contains("prompt"), "{json}");
    }

    #[test]
    fn empty_harness_counters_render_as_unmeasured_not_zeros() {
        let snaps = SloRegistry::new().snapshot_all();
        let json = render_snapshots_json_with_lessons_and_harness(
            &snaps,
            None,
            Some(HarnessCounters::default()),
        );
        assert!(json.contains(r#""harness":{"measured":false}"#), "{json}");
        assert!(!json.contains("pre_tokens"), "{json}");
        assert!(!json.contains("\"uses\":0"), "{json}");
    }

    #[test]
    fn sanitizer_counters_render_live_counts_never_text() {
        let snaps = SloRegistry::new().snapshot_all();
        let json = render_snapshots_json_with_lessons_and_sanitizer(
            &snaps,
            None,
            SanitizerCounters {
                events_stripped: 4,
                chars_removed: 11,
                instruction_shaped_dropped: 2,
            },
        );
        assert!(
            json.contains(r#""sanitizer":{"events_stripped":4,"chars_removed":11,"instruction_shaped_dropped":2}"#),
            "{json}"
        );
        assert!(!json.to_lowercase().contains("ignore previous"), "{json}");
    }
}
