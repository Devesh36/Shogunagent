//! A value pre-assembled for one focus, read on the press (issue #355, NFR-SLO-02 ≤150ms).
//!
//! "context cacheは「押してから収集」禁止。常時プリアセンブル": the notch action row (commitments,
//! loops, people, projects, lessons, priors, fusion) is not assembled on every expand.
//! [`Prepared`] is the slot the capture poller fills when the focus changes and the expand reads
//! when the key still matches.
//!
//! Rules:
//! - A value is only served for the exact key it was built for, and only while younger than the
//!   caller's max age. Anything else is a miss and the caller assembles inline — the slot is an
//!   accelerator, never a source of truth.
//! - [`Prepared::invalidate`] (e.g. after an action ran and changed state) must win against an
//!   assembly that was already in flight: a build started before the invalidation is discarded
//!   on [`Prepared::put`] instead of reinstating the pre-invalidation answer. That is what the
//!   [`Ticket`] carries.
//!
//! Time-injected, pure std, unit-tested off a Mac.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Serve a pre-assembled action row for at most this long. State rows and ranking priors move
/// underneath a window the user stays on; past this the expand assembles inline.
pub const PREPARED_ACTIONS_MAX_AGE: Duration = Duration::from_secs(60);

/// The poller rebuilds the row for an unchanged focus once it is this old, so a user who stays on
/// one window still gets a warm expand. One local assembly per window per 45s — not a loop.
pub const PREPARED_ACTIONS_REFRESH_AGE: Duration = Duration::from_secs(45);

/// Proof that an assembly began at a given generation. Obtain with [`Prepared::begin`] *before*
/// reading anything the value is built from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ticket(u64);

struct Entry<K, V> {
    key: K,
    value: V,
    at: Instant,
}

struct Inner<K, V> {
    generation: u64,
    entry: Option<Entry<K, V>>,
}

/// One pre-assembled value and the key (focus) it belongs to.
pub struct Prepared<K, V> {
    inner: Mutex<Inner<K, V>>,
}

impl<K, V> Default for Prepared<K, V> {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner {
                generation: 0,
                entry: None,
            }),
        }
    }
}

impl<K: PartialEq, V: Clone> Prepared<K, V> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start an assembly. The returned ticket is checked on [`Self::put`].
    pub fn begin(&self) -> Ticket {
        Ticket(self.inner.lock().map(|g| g.generation).unwrap_or(u64::MAX))
    }

    /// Store `value` for `key`, unless the slot was invalidated after `ticket` was taken.
    /// Returns whether it was stored.
    pub fn put(&self, ticket: Ticket, key: K, value: V, now: Instant) -> bool {
        let Ok(mut g) = self.inner.lock() else {
            return false;
        };
        if g.generation != ticket.0 {
            return false;
        }
        g.entry = Some(Entry {
            key,
            value,
            at: now,
        });
        true
    }

    /// The value built for `key`, when it is younger than `max_age`.
    pub fn fresh(&self, key: &K, now: Instant, max_age: Duration) -> Option<V> {
        let g = self.inner.lock().ok()?;
        let e = g.entry.as_ref()?;
        (e.key == *key && now.saturating_duration_since(e.at) <= max_age).then(|| e.value.clone())
    }

    /// Whether the poller should (re)build for `key`: nothing held, a different key held, or the
    /// held value is at least `refresh_age` old.
    pub fn needs_build(&self, key: &K, now: Instant, refresh_age: Duration) -> bool {
        self.needs_build_where(|held| held == key, now, refresh_age)
    }

    /// [`Self::needs_build`] against a partial match: `same` says whether the held key is still
    /// the current focus. Lets a cheap per-tick check (app + window) decide whether the costlier
    /// parts of the key need reading at all; the serve side still compares the whole key.
    pub fn needs_build_where(
        &self,
        same: impl Fn(&K) -> bool,
        now: Instant,
        refresh_age: Duration,
    ) -> bool {
        let Ok(g) = self.inner.lock() else {
            return false;
        };
        match g.entry.as_ref() {
            Some(e) => !same(&e.key) || now.saturating_duration_since(e.at) >= refresh_age,
            None => true,
        }
    }

    /// Drop the held value and reject any assembly already in flight.
    pub fn invalidate(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.generation = g.generation.wrapping_add(1);
            g.entry = None;
        }
    }
}

/// How long an identical row counts as the same offer. Re-opening the panel on the same window
/// with the same buttons inside this window is not a new impression.
pub const IMPRESSION_REPEAT_WINDOW: Duration = Duration::from_secs(30 * 60);

/// How many recently logged rows are remembered, so switching back and forth between two windows
/// does not log both rows on every switch.
const IMPRESSION_MEMORY: usize = 8;

/// Offer impressions (FR-PAT-04) count once per row the panel showed, not once per expand. The
/// Work Patterns "avoid" rule reads them (five un-adopted impressions of a kind in an app), so an
/// impression per expand turned "opened the panel five times on one window" into "ignored this
/// kind five times" and suppressed it. Holds only signatures (hashes) and instants, in RAM.
#[derive(Default)]
pub struct ImpressionGate {
    recent: Mutex<Vec<(u64, Instant)>>,
}

impl ImpressionGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the row with `signature` should be logged at `now`: true unless the same row was
    /// logged less than `window` ago.
    pub fn admit(&self, signature: u64, now: Instant, window: Duration) -> bool {
        let Ok(mut recent) = self.recent.lock() else {
            // A poisoned gate must never stop offers being counted at all.
            return true;
        };
        recent.retain(|(_, at)| now.saturating_duration_since(*at) < window);
        if recent.iter().any(|(s, _)| *s == signature) {
            return false;
        }
        if recent.len() >= IMPRESSION_MEMORY {
            recent.remove(0);
        }
        recent.push((signature, now));
        true
    }
}

/// A process-local signature of a shown row (the focus it was shown on and its buttons). Never
/// persisted or logged — it only tells one row from another for [`ImpressionGate`].
pub fn row_signature<'a>(parts: impl IntoIterator<Item = &'a str>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for p in parts {
        p.hash(&mut h);
        0xffu8.hash(&mut h);
    }
    h.finish()
}

#[cfg(test)]
mod impression_tests {
    use super::*;

    #[test]
    fn the_same_row_counts_once_per_window_and_a_different_row_counts_again() {
        let gate = ImpressionGate::new();
        let t0 = Instant::now();
        let a = row_signature(["com.apple.mail", "Re: plan", "update_state", "local_search"]);
        let b = row_signature(["com.apple.mail", "Re: plan", "local_search"]);
        assert_ne!(a, b);
        assert!(gate.admit(a, t0, IMPRESSION_REPEAT_WINDOW));
        for i in 1..5 {
            assert!(!gate.admit(a, t0 + Duration::from_secs(i * 60), IMPRESSION_REPEAT_WINDOW), "re-expand {i}");
        }
        assert!(gate.admit(b, t0 + Duration::from_secs(300), IMPRESSION_REPEAT_WINDOW), "a different row is a new offer");
        // Switching back to the first window inside the window is still the same offer.
        assert!(!gate.admit(a, t0 + Duration::from_secs(400), IMPRESSION_REPEAT_WINDOW));
        // Once the window has passed it counts again.
        assert!(gate.admit(a, t0 + IMPRESSION_REPEAT_WINDOW + Duration::from_secs(1), IMPRESSION_REPEAT_WINDOW));
    }

    #[test]
    fn signatures_separate_their_parts() {
        assert_ne!(row_signature(["ab", "c"]), row_signature(["a", "bc"]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Slot = Prepared<(&'static str, &'static str), Vec<&'static str>>;

    const AGE: Duration = Duration::from_secs(60);

    #[test]
    fn serves_only_the_key_it_was_built_for() {
        let slot = Slot::new();
        let now = Instant::now();
        let t = slot.begin();
        assert!(slot.put(t, ("mail", "Re: plan"), vec!["reply"], now));
        assert_eq!(
            slot.fresh(&("mail", "Re: plan"), now, AGE),
            Some(vec!["reply"])
        );
        assert_eq!(slot.fresh(&("mail", "Other"), now, AGE), None);
        assert_eq!(slot.fresh(&("code", "Re: plan"), now, AGE), None);
    }

    #[test]
    fn an_old_value_is_a_miss() {
        let slot = Slot::new();
        let then = Instant::now();
        let t = slot.begin();
        slot.put(t, ("a", "b"), vec!["x"], then);
        assert_eq!(
            slot.fresh(&("a", "b"), then + AGE + Duration::from_millis(1), AGE),
            None
        );
        assert!(slot.fresh(&("a", "b"), then + AGE, AGE).is_some());
    }

    #[test]
    fn needs_build_on_change_absence_or_age() {
        let slot = Slot::new();
        let now = Instant::now();
        let refresh = Duration::from_secs(45);
        assert!(slot.needs_build(&("a", "b"), now, refresh));
        let t = slot.begin();
        slot.put(t, ("a", "b"), vec!["x"], now);
        assert!(!slot.needs_build(&("a", "b"), now + Duration::from_secs(2), refresh));
        assert!(slot.needs_build(&("a", "c"), now, refresh));
        assert!(slot.needs_build(&("a", "b"), now + refresh, refresh));
    }

    #[test]
    fn a_partial_match_checks_only_what_the_caller_compares() {
        let slot = Slot::new();
        let now = Instant::now();
        let refresh = Duration::from_secs(45);
        let t = slot.begin();
        slot.put(t, ("mail", "Re: plan"), vec!["x"], now);
        assert!(!slot.needs_build_where(|k| k.0 == "mail", now, refresh));
        assert!(slot.needs_build_where(|k| k.0 == "code", now, refresh));
        assert!(slot.needs_build_where(|k| k.0 == "mail", now + refresh, refresh));
    }

    #[test]
    fn invalidation_beats_an_assembly_already_in_flight() {
        // The poller began building before a tap changed state; its (now stale) result must not
        // reinstate itself after the tap invalidated the slot.
        let slot = Slot::new();
        let now = Instant::now();
        let in_flight = slot.begin();
        slot.invalidate();
        assert!(!slot.put(in_flight, ("a", "b"), vec!["stale"], now));
        assert_eq!(slot.fresh(&("a", "b"), now, AGE), None);
        // A build begun after the invalidation lands normally.
        let t = slot.begin();
        assert!(slot.put(t, ("a", "b"), vec!["new"], now));
        assert_eq!(slot.fresh(&("a", "b"), now, AGE), Some(vec!["new"]));
    }

    #[test]
    fn invalidate_drops_the_held_value() {
        let slot = Slot::new();
        let now = Instant::now();
        let t = slot.begin();
        slot.put(t, ("a", "b"), vec!["x"], now);
        slot.invalidate();
        assert_eq!(slot.fresh(&("a", "b"), now, AGE), None);
        assert!(slot.needs_build(&("a", "b"), now, AGE));
    }
}
