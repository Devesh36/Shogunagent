//! The capture seam. A source pushes 16 kHz mono f32 frames tagged with a speaker. The real
//! backends (`mic` via cpal, `system_tap` via Core Audio) live behind the `audio` feature and
//! macOS; the fake drives worker tests on CI.

#[cfg(all(feature = "audio", target_os = "macos"))]
pub mod mic;
#[cfg(all(feature = "audio", target_os = "macos"))]
mod samples;
#[cfg(all(feature = "audio", target_os = "macos"))]
pub mod system_tap;

pub use shogun_microphone::find_named_device;

use super::Speaker;
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Clone, Debug)]
pub struct CaptureStartGate {
    state: Arc<Mutex<CaptureStartState>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CaptureStartState {
    Pending,
    Capturing,
    Cancelled,
}

/// Holds the start gate while work that must not race cancellation is performed.
pub struct PendingCaptureStart<'a> {
    guard: MutexGuard<'a, CaptureStartState>,
}

impl PendingCaptureStart<'_> {
    /// Commit the start before releasing the gate to a concurrent key-up handler. Dictation
    /// commits together with its ready cue, after the microphone delivers frames (#951).
    pub fn mark_capturing(mut self) {
        *self.guard = CaptureStartState::Capturing;
    }
}

impl CaptureStartGate {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(CaptureStartState::Pending)),
        }
    }

    /// Cancel a start that has not been committed.
    pub fn cancel(&self) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return true;
        };
        if *state == CaptureStartState::Pending {
            *state = CaptureStartState::Cancelled;
            true
        } else {
            false
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.state
            .lock()
            .map(|state| *state == CaptureStartState::Cancelled)
            .unwrap_or(true)
    }

    pub fn same_start(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }

    /// Lock the still-pending start across cue playback or microphone activation.
    pub fn lock_pending(&self) -> Option<PendingCaptureStart<'_>> {
        let guard = self.state.lock().ok()?;
        if *guard != CaptureStartState::Pending {
            return None;
        }
        Some(PendingCaptureStart { guard })
    }
}

impl Default for CaptureStartGate {
    fn default() -> Self {
        Self::new()
    }
}

/// Decides when a meeting microphone opened **without** a selected device should move to a new
/// macOS default input (issue #1005: AirPods connect, macOS moves the default, the call follows,
/// and SHOGUN kept recording the built-in mic). A selected device never uses this: switching
/// away from the user's explicit choice would capture the wrong microphone.
///
/// A change must be seen on two consecutive polls before it counts, so a default that flickers
/// while a Bluetooth headset negotiates does not reopen the stream twice. A missing default
/// (`None`, mid-switch) never counts. Holds device names in RAM only; never logged.
#[derive(Debug, Clone)]
pub struct DefaultInputFollow {
    current: Option<String>,
    candidate: Option<String>,
}

impl DefaultInputFollow {
    /// `opened` is the default input the stream was built on, if its name could be read.
    pub fn new(opened: Option<String>) -> Self {
        Self {
            current: opened,
            candidate: None,
        }
    }

    /// Feed the current default input's name; `true` means reopen on it now.
    pub fn observe(&mut self, default_now: Option<&str>) -> bool {
        let Some(name) = default_now else {
            self.candidate = None;
            return false;
        };
        // The opening name was unreadable: adopt what the stream is most likely on, no reopen.
        if self.current.is_none() {
            self.current = Some(name.to_string());
            return false;
        }
        if self.current.as_deref() == Some(name) {
            self.candidate = None;
            return false;
        }
        if self.candidate.as_deref() == Some(name) {
            self.current = Some(name.to_string());
            self.candidate = None;
            return true;
        }
        self.candidate = Some(name.to_string());
        false
    }
}

/// A frame of already-resampled 16 kHz mono audio from one source.
pub struct Frame {
    pub speaker: Speaker,
    pub samples: Vec<f32>,
}

/// Terminal failures of identified capture channels. Empty queues and silent samples do not
/// contribute: only backend failures/disconnection can set these flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CaptureFailures {
    pub microphone: bool,
    pub system_audio: bool,
}

/// A running capture source. `try_recv` is non-blocking so the worker can poll mic and tap on one
/// thread without either starving the other. `None` means "no data right now", not "closed".
pub trait AudioSource: Send {
    fn try_recv(&mut self) -> Option<Frame>;
    /// A terminal device error, distinct from an empty queue or ordinary silence.
    fn capture_failed(&self) -> bool {
        false
    }
    fn capture_failures(&self) -> CaptureFailures {
        CaptureFailures::default()
    }
    /// Per-source sample position including bounded-ingress loss, when available.
    fn try_recv_with_offset(&mut self) -> Option<(Frame, Option<u64>)> {
        self.try_recv().map(|frame| (frame, None))
    }
    /// Stop capture and release the device. Idempotent.
    fn stop(&mut self);
    /// Samples discarded by bounded capture ingress since the last poll.
    fn take_dropped_samples(&self) -> u64 {
        0
    }
}

/// Poll several sources as one. Round-robins `try_recv` so neither mic nor system tap starves the
/// other; `stop` stops all. This is how the desktop feeds one `Worker` from mic + system tap
/// without the core worker needing to know how many sources there are.
pub struct MultiSource {
    sources: Vec<Box<dyn AudioSource>>,
    next: usize,
}

impl MultiSource {
    pub fn new(sources: Vec<Box<dyn AudioSource>>) -> Self {
        MultiSource { sources, next: 0 }
    }
}

impl AudioSource for MultiSource {
    fn capture_failed(&self) -> bool {
        self.sources.iter().any(|source| source.capture_failed())
    }
    fn capture_failures(&self) -> CaptureFailures {
        self.sources
            .iter()
            .fold(CaptureFailures::default(), |mut failures, source| {
                let channel = source.capture_failures();
                failures.microphone |= channel.microphone;
                failures.system_audio |= channel.system_audio;
                failures
            })
    }
    fn take_dropped_samples(&self) -> u64 {
        self.sources.iter().map(|s| s.take_dropped_samples()).sum()
    }
    fn try_recv(&mut self) -> Option<Frame> {
        self.try_recv_with_offset().map(|(frame, _)| frame)
    }
    fn try_recv_with_offset(&mut self) -> Option<(Frame, Option<u64>)> {
        let n = self.sources.len();
        if n == 0 {
            return None;
        }
        for _ in 0..n {
            let i = self.next % n;
            self.next = self.next.wrapping_add(1);
            if let Some(f) = self.sources.get_mut(i).and_then(|s| s.try_recv_with_offset()) {
                return Some(f);
            }
        }
        None
    }
    fn stop(&mut self) {
        for s in self.sources.iter_mut() {
            s.stop();
        }
    }
}

/// A scripted source for tests: hands out pre-canned frames, then reports empty.
pub struct FakeSource {
    frames: std::collections::VecDeque<Frame>,
    pub stopped: bool,
}

impl FakeSource {
    pub fn new(frames: Vec<Frame>) -> Self {
        FakeSource { frames: frames.into(), stopped: false }
    }
}

impl AudioSource for FakeSource {
    fn try_recv(&mut self) -> Option<Frame> {
        self.frames.pop_front()
    }
    fn stop(&mut self) {
        self.stopped = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn devices(names: &[&str]) -> Vec<(String, String)> {
        names.iter().map(|n| ((*n).into(), (*n).into())).collect()
    }

    #[test]
    fn capture_start_gate_cancellation_blocks_pending_work() {
        let gate = CaptureStartGate::new();

        assert!(gate.cancel());
        assert!(gate.is_cancelled());
        assert!(gate.lock_pending().is_none());
    }

    #[test]
    fn capture_start_gate_serializes_playback_and_release() {
        let gate = CaptureStartGate::new();
        let permit = gate
            .lock_pending()
            .expect("fresh start should allow playback");
        let release_gate = gate.clone();
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let release = std::thread::spawn(move || {
            let _ = started_tx.send(());
            release_gate.cancel()
        });

        assert!(started_rx.recv().is_ok());
        std::thread::yield_now();
        assert!(
            !release.is_finished(),
            "release must wait until in-progress playback leaves its critical section"
        );
        drop(permit);
        assert!(release.join().is_ok_and(|cancelled| cancelled));
        assert!(gate.is_cancelled());
    }

    #[test]
    fn missing_selected_device_is_an_error_not_a_fallback() {
        let error = find_named_device(devices(&["Built-in Microphone"]).into_iter(), "Studio Mic")
            .expect_err("an unplugged selection must not resolve to another input");
        assert!(error.contains("Studio Mic"), "error names the device: {error}");
    }

    #[test]
    fn selected_device_resolves_by_exact_name() {
        let picked = find_named_device(
            devices(&["Built-in Microphone", "Studio Mic"]).into_iter(),
            "Studio Mic",
        )
        .expect("a connected selection resolves");
        assert_eq!(picked, "Studio Mic");
    }

    #[test]
    fn duplicate_names_are_rejected_instead_of_selecting_arbitrarily() {
        let candidates = vec![
            ("Studio Mic".to_string(), "first"),
            ("Studio Mic".to_string(), "second"),
        ];
        let error = find_named_device(candidates.into_iter(), "Studio Mic")
            .expect_err("an ambiguous device name must not select an arbitrary microphone");
        assert!(error.contains("ambiguous"), "error explains the ambiguity: {error}");
    }

    #[test]
    fn fake_yields_frames_then_empty() {
        let mut s = FakeSource::new(vec![Frame { speaker: Speaker::Me, samples: vec![0.1] }]);
        assert!(s.try_recv().is_some());
        assert!(s.try_recv().is_none());
        s.stop();
        assert!(s.stopped);
    }

    #[test]
    fn multisource_drains_both_and_stops_all() {
        // Uneven lengths on purpose: the round-robin must not drop the mic's extra frame just
        // because the tap ran dry first.
        let a = FakeSource::new(vec![
            Frame { speaker: Speaker::Me, samples: vec![0.1] },
            Frame { speaker: Speaker::Me, samples: vec![0.2] },
        ]);
        let b = FakeSource::new(vec![Frame { speaker: Speaker::Other, samples: vec![0.9] }]);
        let mut multi = MultiSource::new(vec![Box::new(a), Box::new(b)]);

        let mut me = 0;
        let mut other = 0;
        while let Some(f) = multi.try_recv() {
            match f.speaker {
                Speaker::Me => me += 1,
                Speaker::Other => other += 1,
            }
        }
        assert_eq!(me, 2, "both mic frames come out");
        assert_eq!(other, 1, "the tap frame comes out too");
        assert!(multi.try_recv().is_none(), "empty once both sources are drained");
    }

    #[test]
    fn multisource_preserves_each_sources_capture_clock() {
        struct TimedSource(Option<(Frame, Option<u64>)>);
        impl AudioSource for TimedSource {
            fn try_recv(&mut self) -> Option<Frame> {
                self.try_recv_with_offset().map(|(frame, _)| frame)
            }
            fn try_recv_with_offset(&mut self) -> Option<(Frame, Option<u64>)> {
                self.0.take()
            }
            fn stop(&mut self) {}
        }
        let mut source = MultiSource::new(vec![
            Box::new(TimedSource(Some((Frame { speaker: Speaker::Me, samples: vec![0.1] }, Some(16000))))),
            Box::new(TimedSource(Some((Frame { speaker: Speaker::Other, samples: vec![0.2] }, Some(32000))))),
        ]);
        assert!(source.try_recv_with_offset().is_some_and(|(f, offset)| f.speaker == Speaker::Me && offset == Some(16000)));
        assert!(source.try_recv_with_offset().is_some_and(|(f, offset)| f.speaker == Speaker::Other && offset == Some(32000)));
        assert!(source.try_recv_with_offset().is_none());
    }

    #[test]
    fn multisource_stop_propagates_to_all() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        // A source whose only job is to record that `stop` reached it.
        struct StopSpy(Arc<AtomicUsize>);
        impl AudioSource for StopSpy {
            fn try_recv(&mut self) -> Option<Frame> {
                None
            }
            fn stop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let stops = Arc::new(AtomicUsize::new(0));
        let mut multi = MultiSource::new(vec![
            Box::new(StopSpy(stops.clone())),
            Box::new(StopSpy(stops.clone())),
        ]);
        multi.stop();
        assert_eq!(stops.load(Ordering::SeqCst), 2, "stop reaches every source");
    }

    #[test]
    fn multisource_empty_never_panics() {
        let mut multi = MultiSource::new(vec![]);
        assert!(multi.try_recv().is_none());
        multi.stop();
    }
}

#[cfg(test)]
mod health_tests {
    use super::*;

    struct FailedMic;
    impl AudioSource for FailedMic {
        fn try_recv(&mut self) -> Option<Frame> {
            None
        }
        fn stop(&mut self) {}
        fn capture_failed(&self) -> bool {
            true
        }
        fn capture_failures(&self) -> CaptureFailures {
            CaptureFailures {
                microphone: true,
                system_audio: false,
            }
        }
    }

    #[test]
    fn empty_capture_is_healthy_but_terminal_channel_failure_is_identified() {
        let silent = MultiSource::new(vec![Box::new(FakeSource::new(vec![]))]);
        assert!(!silent.capture_failed());
        assert_eq!(silent.capture_failures(), CaptureFailures::default());
        let failed = MultiSource::new(vec![Box::new(FailedMic), Box::new(FakeSource::new(vec![]))]);
        assert!(failed.capture_failed());
        assert_eq!(
            failed.capture_failures(),
            CaptureFailures {
                microphone: true,
                system_audio: false
            }
        );
    }
}

#[cfg(test)]
mod default_input_follow_tests {
    use super::DefaultInputFollow;

    #[test]
    fn a_new_default_is_followed_once_it_is_stable() {
        let mut follow = DefaultInputFollow::new(Some("Built-in".into()));
        assert!(!follow.observe(Some("Built-in")));
        assert!(!follow.observe(Some("Headset")), "one sighting is not yet a change");
        assert!(follow.observe(Some("Headset")), "seen twice: reopen");
        assert!(!follow.observe(Some("Headset")), "already following it");
    }

    #[test]
    fn a_flickering_or_missing_default_does_not_reopen() {
        let mut follow = DefaultInputFollow::new(Some("Built-in".into()));
        assert!(!follow.observe(Some("Headset")));
        assert!(!follow.observe(None), "mid-switch");
        assert!(!follow.observe(Some("Headset")), "the gap restarted the count");
        assert!(!follow.observe(Some("Built-in")), "back where we were");
        assert!(!follow.observe(Some("Headset")));
        assert!(!follow.observe(Some("Other")), "a different candidate restarts the count");
        assert!(follow.observe(Some("Other")));
    }

    #[test]
    fn an_unreadable_opening_name_is_adopted_without_a_reopen() {
        let mut follow = DefaultInputFollow::new(None);
        assert!(!follow.observe(Some("Built-in")));
        assert!(!follow.observe(Some("Built-in")));
        assert!(!follow.observe(Some("Headset")));
        assert!(follow.observe(Some("Headset")));
    }
}
