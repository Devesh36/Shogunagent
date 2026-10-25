//! Reply-domain helpers.

use super::*;

/// Fraction of title words also present in the question, ignoring very short words.
pub(super) fn title_overlap(question_lower: &str, title_lower: &str) -> f64 {
    let words: Vec<&str> = title_lower
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| word.chars().count() > 2)
        .collect();
    if words.is_empty() {
        return 0.0;
    }
    let hits = words
        .iter()
        .filter(|word| question_lower.contains(**word))
        .count();
    hits as f64 / words.len() as f64
}

/// The pre-assembled reply context, kept warm so a press only starts generation.
///
/// The whole point is that reading it costs nothing: the focus path writes, the button reads.
/// A miss (focus moved to a thread that has not been built yet) returns `None` rather than
/// building inline — building on the press is exactly what the SLO forbids, and a caller that
/// silently fell back to it would hide the regression.
#[derive(Clone, Default)]
pub struct ReplyContextCache {
    inner: Arc<Mutex<Option<CachedReplyContext>>>,
}

struct CachedReplyContext {
    focus_key: String,
    context: ReplyContext,
    directives: String,
    assembled_at: std::time::Instant,
    focus_seen_at: std::time::Instant,
}

/// Exact-focus prompt inputs assembled by the background poller.
#[derive(Clone)]
pub struct PreparedReplyContext {
    pub focus_key: String,
    pub context: ReplyContext,
    pub directives: String,
    assembled_at: std::time::Instant,
}

impl PreparedReplyContext {
    /// Construct a detached prompt pack without allowing its assembly age to be rewritten later.
    pub fn new(
        focus_key: String,
        context: ReplyContext,
        directives: String,
        assembled_at: std::time::Instant,
    ) -> Self {
        Self {
            focus_key,
            context,
            directives,
            assembled_at,
        }
    }

    /// Strict assembly-age check for callers that retain this pack outside the live cache.
    pub fn assembly_is_fresh_at(
        &self,
        max_age: std::time::Duration,
        now: std::time::Instant,
    ) -> bool {
        age_at(now, self.assembled_at) < max_age
    }
}

/// Freshness-aware read of the preassembled reply cache.
pub enum ReplyContextCacheSnapshot {
    Fresh(ReplyContext),
    Empty,
    Stale,
}

impl ReplyContextCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the warm pack (called off the focus path).
    pub fn put(&self, ctx: ReplyContext) {
        let focus_key = ctx.thread_key.clone();
        self.put_for_focus_at(&focus_key, ctx, std::time::Instant::now());
    }

    /// Replace the warm pack with its capture time. Hosts use this seam for deterministic
    /// freshness checks; it performs no collection or persistence.
    pub fn put_at(&self, ctx: ReplyContext, captured_at: std::time::Instant) {
        let focus_key = ctx.thread_key.clone();
        self.put_for_focus_at(&focus_key, ctx, captured_at);
    }

    /// Store fetched/linked memory under the desktop surface that was actually captured.
    pub fn put_for_focus(&self, focus_key: &str, ctx: ReplyContext) {
        self.put_for_focus_at(focus_key, ctx, std::time::Instant::now());
    }

    /// Deterministic capture-time seam for exact-focus freshness tests.
    pub fn put_for_focus_at(
        &self,
        focus_key: &str,
        ctx: ReplyContext,
        captured_at: std::time::Instant,
    ) {
        self.put_prepared_for_focus_at(focus_key, ctx, String::new(), captured_at);
    }

    /// Store memory and trusted standing directives assembled for the same desktop focus.
    pub fn put_prepared_for_focus(&self, focus_key: &str, ctx: ReplyContext, directives: String) {
        self.put_prepared_for_focus_at(focus_key, ctx, directives, std::time::Instant::now());
    }

    /// Deterministic assembly-time seam for exact-focus tests.
    pub fn put_prepared_for_focus_at(
        &self,
        focus_key: &str,
        ctx: ReplyContext,
        directives: String,
        assembled_at: std::time::Instant,
    ) {
        self.put_prepared_for_focus_observed_at(
            focus_key,
            ctx,
            directives,
            assembled_at,
            assembled_at,
        );
    }

    /// Store separately measured assembly and exact-focus observation times.
    pub fn put_prepared_for_focus_observed_at(
        &self,
        focus_key: &str,
        ctx: ReplyContext,
        directives: String,
        assembled_at: std::time::Instant,
        focus_seen_at: std::time::Instant,
    ) {
        if let Ok(mut g) = self.inner.lock() {
            *g = Some(CachedReplyContext {
                focus_key: focus_key.to_string(),
                context: ctx,
                directives,
                assembled_at,
                focus_seen_at,
            });
        }
    }

    /// Confirm only that the cached surface was seen again. This never changes assembly age.
    pub fn mark_fresh(&self) {
        if let Ok(mut g) = self.inner.lock() {
            if let Some(cached) = g.as_mut() {
                cached.focus_seen_at = std::time::Instant::now();
            }
        }
    }

    /// Confirm only that this exact surface was seen again. This never changes assembly age.
    pub fn mark_focus_fresh(&self, focus_key: &str) {
        self.mark_focus_seen_at(focus_key, std::time::Instant::now());
    }

    /// Deterministic observation-time seam. Observing focus cannot refresh assembled content.
    pub fn mark_focus_seen_at(&self, focus_key: &str, seen_at: std::time::Instant) {
        if let Ok(mut g) = self.inner.lock() {
            if let Some(cached) = g.as_mut().filter(|cached| cached.focus_key == focus_key) {
                cached.focus_seen_at = seen_at;
            }
        }
    }

    /// Drop warm context when the capture source can no longer vouch for the focused surface.
    pub fn clear(&self) {
        if let Ok(mut g) = self.inner.lock() {
            *g = None;
        }
    }

    /// The warm pack for `thread_key`, if that is the one currently held.
    pub fn get(&self, thread_key: &str) -> Option<ReplyContext> {
        let g = self.inner.lock().ok()?;
        g.as_ref()
            .map(|cached| &cached.context)
            .filter(|context| context.thread_key == thread_key)
            .cloned()
    }

    /// The warm pack, whatever thread it is for.
    pub fn current(&self) -> Option<ReplyContext> {
        self.inner
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|cached| cached.context.clone()))
    }

    /// Clone only an exact-focus pack whose capture age is still within `max_age`.
    pub fn get_fresh_for_focus(
        &self,
        focus_key: &str,
        max_age: std::time::Duration,
    ) -> Option<ReplyContext> {
        self.get_fresh_prepared_for_focus(focus_key, max_age)
            .map(|prepared| prepared.context)
    }

    /// Clone memory and directives only when both assembly and exact-focus observation are fresh.
    pub fn get_fresh_prepared_for_focus(
        &self,
        focus_key: &str,
        max_age: std::time::Duration,
    ) -> Option<PreparedReplyContext> {
        self.get_fresh_prepared_for_focus_at(focus_key, max_age, std::time::Instant::now())
    }

    /// Clone exact-focus prompt inputs without treating standing directives as expiring memory.
    /// Callers must independently freshness-gate `context` before provider egress.
    pub fn get_prepared_for_focus(&self, focus_key: &str) -> Option<PreparedReplyContext> {
        let guard = self.inner.lock().ok()?;
        let cached = guard
            .as_ref()
            .filter(|cached| cached.focus_key == focus_key)?;
        Some(PreparedReplyContext::new(
            cached.focus_key.clone(),
            cached.context.clone(),
            cached.directives.clone(),
            cached.assembled_at,
        ))
    }

    fn get_fresh_prepared_for_focus_at(
        &self,
        focus_key: &str,
        max_age: std::time::Duration,
        now: std::time::Instant,
    ) -> Option<PreparedReplyContext> {
        let guard = self.inner.lock().ok()?;
        let cached = guard
            .as_ref()
            .filter(|cached| cached.focus_key == focus_key)?;
        cached_is_fresh(cached, max_age, now).then(|| {
            PreparedReplyContext::new(
                cached.focus_key.clone(),
                cached.context.clone(),
                cached.directives.clone(),
                cached.assembled_at,
            )
        })
    }

    /// Whether the exact-focus pack itself is young enough to reuse without rebuilding.
    pub fn assembly_is_fresh_for_focus(
        &self,
        focus_key: &str,
        max_age: std::time::Duration,
    ) -> bool {
        let Ok(guard) = self.inner.lock() else {
            return false;
        };
        guard.as_ref().is_some_and(|cached| {
            cached.focus_key == focus_key
                && age_at(std::time::Instant::now(), cached.assembled_at) < max_age
        })
    }

    /// Read only a fresh preassembled pack. Stale text is deliberately not cloned into the
    /// result, so callers cannot accidentally use it while focus collection is paused.
    pub fn snapshot(&self, max_age: std::time::Duration) -> ReplyContextCacheSnapshot {
        let Ok(g) = self.inner.lock() else {
            return ReplyContextCacheSnapshot::Empty;
        };
        let Some(cached) = g.as_ref() else {
            return ReplyContextCacheSnapshot::Empty;
        };
        if !cached_is_fresh(cached, max_age, std::time::Instant::now()) {
            ReplyContextCacheSnapshot::Stale
        } else {
            ReplyContextCacheSnapshot::Fresh(cached.context.clone())
        }
    }

    /// Drop the warm pack: something changed underneath it (an integration sync landed new items),
    /// so whatever was assembled before is stale. The next `get` is an honest miss until the focus
    /// path re-assembles via [`Self::put`] — invalidation never rebuilds inline, because building
    /// on the press path is exactly what this cache exists to prevent.
    pub fn invalidate(&self) {
        self.clear();
    }
}

fn age_at(now: std::time::Instant, then: std::time::Instant) -> std::time::Duration {
    now.checked_duration_since(then).unwrap_or_default()
}

fn cached_is_fresh(
    cached: &CachedReplyContext,
    max_age: std::time::Duration,
    now: std::time::Instant,
) -> bool {
    age_at(now, cached.assembled_at) < max_age && age_at(now, cached.focus_seen_at) < max_age
}

/// The daemon-owned bus subscription that keeps the reply-context cache honest across integration
/// syncs (design §2.2 / E-49): an [`crate::bus::BusEvent::IntegrationSynced`] means new items just
/// landed in the event log, so a pack assembled before the sync may answer with a stale thread.
///
/// No thread of its own: like the daemon's other background work (embedding, local maintenance,
/// the dream driver's `tick`) this is tick-driven — the host's existing loop calls [`Self::pump`],
/// which drains whatever the bus has buffered via the non-blocking `try_recv` and returns
/// immediately when the bus is quiet (no busy loop, no waiting). Invalidation only clears; the
/// rebuild stays on the focus path ([`ReplyContextCache::put`]), preserving the cache's
/// "None on miss" contract.
pub struct SyncInvalidator {
    sub: crate::bus::Subscriber,
    cache: ReplyContextCache,
}

impl SyncInvalidator {
    /// Subscribe to `bus` on behalf of `cache`. Only events published after this call are seen,
    /// so wire it up before the first sync can run.
    pub fn new(bus: &crate::bus::Bus, cache: ReplyContextCache) -> Self {
        Self {
            sub: bus.subscribe(),
            cache,
        }
    }

    /// Drain every buffered bus event, invalidating the cache for each `IntegrationSynced` seen
    /// (other event kinds pass through untouched). Non-blocking — an empty bus returns 0 at once.
    /// Returns how many sync events were handled, so the caller's tick can log/meter quietly.
    pub fn pump(&mut self) -> usize {
        let mut handled = 0;
        while let Some(ev) = self.sub.try_recv() {
            if let crate::bus::BusEvent::IntegrationSynced { .. } = *ev {
                self.cache.invalidate();
                handled += 1;
            }
        }
        handled
    }
}
