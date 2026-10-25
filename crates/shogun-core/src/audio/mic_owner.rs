//! Who owns the microphone: meeting notes, dictation, or both (AUDIT-004, issue #934).
//!
//! Dictation is allowed during a meeting. While a dictation holds the mic, the meeting's `Me`
//! lane is fed silence in place of the user's voice, so dictated words never land in the notes,
//! and the `Other` lane (system tap) keeps transcribing the call. This is the single source of
//! truth for both directions: dictation never reads the meeting's private state, and the meeting
//! never reads dictation's.
//!
//! A hold is an RAII [`DictationClaim`]: whatever ends the dictation (release, error, panic,
//! cancellation) drops it, so the meeting can never stay muted after the dictation is gone.
//! Pure state, no audio is kept or written anywhere.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::LazyLock;
use std::time::Instant;

use super::capture::{AudioSource, Frame};
use crate::meeting::statemachine::State;

/// After the last hold ends, the meeting stays silent this much longer. Frames the meeting lane
/// had already queued when dictation closed its mic were captured during the hold; this covers
/// them so the dictation's tail cannot leak into the notes (E3).
pub const RELEASE_GRACE_MS: u64 = 250;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MicOwner {
    Free,
    Meeting,
    Dictation,
    MeetingWithDictationHold,
}

impl MicOwner {
    pub fn tag(self) -> &'static str {
        match self {
            MicOwner::Free => "free",
            MicOwner::Meeting => "meeting",
            MicOwner::Dictation => "dictation",
            MicOwner::MeetingWithDictationHold => "meeting_with_dictation_hold",
        }
    }
}

pub struct MicOwnership {
    meeting: AtomicBool,
    holds: AtomicUsize,
    /// Monotonic ms until which a just-released hold still silences the meeting; 0 = never.
    silent_until: AtomicU64,
}

/// The process-wide owner. Both lanes go through this one value.
pub static MIC_OWNER: MicOwnership = MicOwnership::new();

static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

fn mono_ms() -> u64 {
    // +1 so a release at the very first instant still sets a non-zero deadline.
    EPOCH.elapsed().as_millis() as u64 + 1
}

impl MicOwnership {
    pub const fn new() -> Self {
        Self {
            meeting: AtomicBool::new(false),
            holds: AtomicUsize::new(0),
            silent_until: AtomicU64::new(0),
        }
    }

    /// Follow the meeting lifecycle. Only Recording owns the mic: Offered is dictation as if no
    /// meeting existed (E13), and pause keeps the interval, so ownership survives it (E6). Every
    /// start path (UI, detected, MCP/REST/CLI) reaches Recording through the same machine (E15).
    pub fn follow_meeting(&self, state: State) {
        self.meeting
            .store(state == State::Recording, Ordering::Release);
    }

    /// Take the mic for one dictation capture. Never refused: macOS lets both clients read the
    /// mic, and the meeting goes silent for the hold instead.
    pub fn claim_dictation(&self) -> DictationClaim<'_> {
        self.holds.fetch_add(1, Ordering::AcqRel);
        DictationClaim { owner: self }
    }

    pub fn owner(&self) -> MicOwner {
        match (
            self.meeting.load(Ordering::Acquire),
            self.holds.load(Ordering::Acquire) > 0,
        ) {
            (false, false) => MicOwner::Free,
            (true, false) => MicOwner::Meeting,
            (false, true) => MicOwner::Dictation,
            (true, true) => MicOwner::MeetingWithDictationHold,
        }
    }

    /// Whether the meeting's `Me` lane must hear silence right now.
    pub fn meeting_mic_silenced(&self) -> bool {
        self.silenced_at(mono_ms())
    }

    fn silenced_at(&self, now_ms: u64) -> bool {
        self.holds.load(Ordering::Acquire) > 0 || now_ms < self.silent_until.load(Ordering::Acquire)
    }

    fn release(&self, now_ms: u64) {
        self.silent_until
            .fetch_max(now_ms + RELEASE_GRACE_MS, Ordering::AcqRel);
        self.holds.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Default for MicOwnership {
    fn default() -> Self {
        Self::new()
    }
}

/// One dictation's hold on the mic. Dropping it hands the mic back to the meeting.
#[must_use = "dropping the claim ends the hold immediately"]
pub struct DictationClaim<'a> {
    owner: &'a MicOwnership,
}

impl Drop for DictationClaim<'_> {
    fn drop(&mut self) {
        self.owner.release(mono_ms());
    }
}

/// A source with the user's voice replaced by silence while dictation holds the mic, decided when
/// a frame is **read**. Frames keep their length and offset, so `Me` and `Other` stay aligned and
/// the `Me` live stream keeps flowing (no idle timeout or reconnect during a long hold). Zeros
/// carry no speech, so no `Me` line is invented for the held span (E14).
///
/// The real meeting mic does not use this: it silences in the capture callback
/// (`capture::mic::Mic::open_meeting`), because a reader that lags behind capture would apply
/// the hold to the wrong frames. This adapter is for sources that are read as they are produced
/// (fakes in tests, the same zero-fill rule).
pub struct HeldMic<S> {
    inner: S,
    owner: &'static MicOwnership,
}

impl<S: AudioSource> HeldMic<S> {
    pub fn new(inner: S, owner: &'static MicOwnership) -> Self {
        Self { inner, owner }
    }

    fn hold(&self, mut frame: Frame) -> Frame {
        if self.owner.meeting_mic_silenced() {
            frame.samples.fill(0.0);
        }
        frame
    }
}

impl<S: AudioSource> AudioSource for HeldMic<S> {
    fn try_recv(&mut self) -> Option<Frame> {
        self.inner.try_recv().map(|frame| self.hold(frame))
    }
    fn try_recv_with_offset(&mut self) -> Option<(Frame, Option<u64>)> {
        self.inner
            .try_recv_with_offset()
            .map(|(frame, offset)| (self.hold(frame), offset))
    }
    fn stop(&mut self) {
        self.inner.stop();
    }
    fn take_dropped_samples(&self) -> u64 {
        self.inner.take_dropped_samples()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::asr::FakeTranscriber;
    use crate::audio::capture::{FakeSource, MultiSource};
    use crate::audio::worker::{SegmentSink, Worker};
    use crate::audio::{Speaker, Utterance, SAMPLE_RATE};
    use crate::meeting::statemachine::{Effect, Input, Machine, Params};

    fn tone(n: usize) -> Vec<f32> {
        (0..n).map(|i| 0.3 * ((i as f32) * 0.3).sin()).collect()
    }

    fn me(samples: Vec<f32>) -> Frame {
        Frame {
            speaker: Speaker::Me,
            samples,
        }
    }

    fn after_grace(owner: &MicOwnership) -> bool {
        owner.silenced_at(mono_ms() + RELEASE_GRACE_MS + 1)
    }

    /// A mic that reports per-source offsets, like the real bounded capture.
    struct OffsetMic {
        frames: std::collections::VecDeque<(Frame, u64)>,
    }
    impl AudioSource for OffsetMic {
        fn try_recv(&mut self) -> Option<Frame> {
            self.try_recv_with_offset().map(|(frame, _)| frame)
        }
        fn try_recv_with_offset(&mut self) -> Option<(Frame, Option<u64>)> {
            self.frames.pop_front().map(|(frame, at)| (frame, Some(at)))
        }
        fn stop(&mut self) {}
    }

    #[derive(Default)]
    struct Lines(Vec<(Speaker, i64, String)>);
    impl SegmentSink for Lines {
        fn emit(&mut self, u: &Utterance, text: &str, _c: f64, _t: Option<&str>) {
            self.0.push((u.speaker, u.started_at, text.to_string()));
        }
    }

    #[test]
    fn owner_reports_all_four_states() {
        let owner = MicOwnership::new();
        assert_eq!(owner.owner(), MicOwner::Free);
        owner.follow_meeting(State::Recording);
        assert_eq!(owner.owner(), MicOwner::Meeting);
        let claim = owner.claim_dictation();
        assert_eq!(owner.owner(), MicOwner::MeetingWithDictationHold);
        owner.follow_meeting(State::Wrapping);
        assert_eq!(owner.owner(), MicOwner::Dictation);
        drop(claim);
        assert_eq!(owner.owner(), MicOwner::Free);
    }

    #[test]
    fn e1_hold_then_meeting_start_starts_silent() {
        let owner = MicOwnership::new();
        let claim = owner.claim_dictation();
        owner.follow_meeting(State::Recording);
        assert_eq!(owner.owner(), MicOwner::MeetingWithDictationHold);
        assert!(owner.meeting_mic_silenced());
        drop(claim);
        assert_eq!(owner.owner(), MicOwner::Meeting);
        assert!(!after_grace(&owner));
    }

    #[test]
    fn e2_meeting_then_hold_is_granted_and_silences_me() {
        let owner = MicOwnership::new();
        owner.follow_meeting(State::Recording);
        assert!(!owner.meeting_mic_silenced());
        let _claim = owner.claim_dictation();
        assert_eq!(owner.owner(), MicOwner::MeetingWithDictationHold);
        assert!(owner.meeting_mic_silenced());
    }

    #[test]
    fn e3_silence_outlasts_the_hold_by_the_release_grace() {
        let owner = MicOwnership::new();
        owner.follow_meeting(State::Recording);
        let claim = owner.claim_dictation();
        drop(claim);
        // Frames queued before the dictation closed its mic still come out silent.
        assert!(owner.meeting_mic_silenced());
        assert!(!after_grace(&owner));
    }

    #[test]
    fn e4_cancelled_tap_never_leaves_a_permanent_mute() {
        let owner = MicOwnership::new();
        owner.follow_meeting(State::Recording);
        // The start gate cancelled before capture: the claim lived only for the attempt.
        drop(owner.claim_dictation());
        assert_eq!(owner.owner(), MicOwner::Meeting);
        assert!(!after_grace(&owner));
    }

    #[test]
    fn e5_meeting_stop_mid_hold_keeps_the_tail_silent_and_dictation_unaffected() {
        let owner = MicOwnership::new();
        owner.follow_meeting(State::Recording);
        let claim = owner.claim_dictation();
        owner.follow_meeting(State::Wrapping);
        // The tail flush after StopAudio still runs through the held mic.
        assert!(owner.meeting_mic_silenced());
        assert_eq!(owner.owner(), MicOwner::Dictation);
        owner.follow_meeting(State::Idle);
        drop(claim);
        assert_eq!(owner.owner(), MicOwner::Free);
    }

    #[test]
    fn e6_resumed_generation_starts_silent_while_the_hold_lasts() {
        static OWNER: MicOwnership = MicOwnership::new();
        OWNER.follow_meeting(State::Recording);
        let claim = OWNER.claim_dictation();
        // Pause drops the capture worker; resume opens a new one. Neither touches ownership.
        let mut resumed = HeldMic::new(FakeSource::new(vec![me(tone(160))]), &OWNER);
        assert!(resumed
            .try_recv()
            .is_some_and(|f| f.samples.iter().all(|s| *s == 0.0)));
        drop(claim);
        assert!(!after_grace(&OWNER));
    }

    #[test]
    fn e7_dictation_vanishing_by_panic_resumes_me() {
        static OWNER: MicOwnership = MicOwnership::new();
        OWNER.follow_meeting(State::Recording);
        let crashed = std::thread::spawn(|| {
            let _claim = OWNER.claim_dictation();
            panic!("voice lane failed mid-hold");
        })
        .join();
        assert!(crashed.is_err());
        assert_eq!(OWNER.owner(), MicOwner::Meeting);
        assert!(!after_grace(&OWNER));
    }

    #[test]
    fn e8_overlapping_dictations_release_only_when_the_last_ends() {
        let owner = MicOwnership::new();
        owner.follow_meeting(State::Recording);
        // A cancelled capture can still be stopping while the next hold opens.
        let stopping = owner.claim_dictation();
        let next = owner.claim_dictation();
        drop(stopping);
        assert!(after_grace(&owner));
        drop(next);
        assert!(!after_grace(&owner));
        assert_eq!(owner.owner(), MicOwner::Meeting);
    }

    #[test]
    fn e13_offered_meeting_does_not_own_the_mic_and_accepting_follows_e1() {
        let owner = MicOwnership::new();
        let mut machine = Machine::new(Params::default());
        let claim = owner.claim_dictation();
        for input in [Input::MeetingDetected, Input::Start] {
            for effect in machine.step(input) {
                if let Effect::Transition(state) = effect {
                    owner.follow_meeting(state);
                    if state == State::Offered {
                        assert_eq!(owner.owner(), MicOwner::Dictation);
                    }
                }
            }
        }
        assert_eq!(owner.owner(), MicOwner::MeetingWithDictationHold);
        assert!(owner.meeting_mic_silenced());
        drop(claim);
    }

    #[test]
    fn e15_api_and_record_now_starts_take_the_same_ownership_path() {
        // An MCP/REST/CLI request is an Offered start the user accepts; Record now opens directly.
        for inputs in [
            vec![Input::MeetingDetected, Input::Start],
            vec![Input::ManualStart],
        ] {
            let owner = MicOwnership::new();
            let mut machine = Machine::new(Params::default());
            for input in inputs {
                for effect in machine.step(input) {
                    if let Effect::Transition(state) = effect {
                        owner.follow_meeting(state);
                    }
                }
            }
            assert_eq!(owner.owner(), MicOwner::Meeting);
        }
    }

    #[test]
    fn held_mic_keeps_length_and_offset_and_leaves_other_untouched() {
        static OWNER: MicOwnership = MicOwnership::new();
        let loud = tone(320);
        let mic = OffsetMic {
            frames: [(me(loud.clone()), 0), (me(loud.clone()), 320)].into(),
        };
        let tap = FakeSource::new(vec![Frame {
            speaker: Speaker::Other,
            samples: loud.clone(),
        }]);
        let claim = OWNER.claim_dictation();
        let mut source = MultiSource::new(vec![Box::new(HeldMic::new(mic, &OWNER)), Box::new(tap)]);
        let mut frames = Vec::new();
        while let Some(frame) = source.try_recv_with_offset() {
            frames.push(frame);
        }
        drop(claim);
        assert_eq!(frames.len(), 3);
        let me_frames: Vec<_> = frames
            .iter()
            .filter(|(f, _)| f.speaker == Speaker::Me)
            .collect();
        assert_eq!(
            me_frames.iter().map(|(_, at)| *at).collect::<Vec<_>>(),
            [Some(0), Some(320)]
        );
        for (frame, _) in &me_frames {
            assert_eq!(frame.samples.len(), loud.len());
            assert!(frame.samples.iter().all(|s| *s == 0.0));
        }
        let other = frames.iter().find(|(f, _)| f.speaker == Speaker::Other);
        assert_eq!(other.map(|(f, _)| &f.samples), Some(&loud));
    }

    #[test]
    fn e14_held_span_yields_no_me_line_and_other_keeps_its_time() {
        fn run(owner: &'static MicOwnership, held: bool) -> Vec<(Speaker, i64, String)> {
            let mic = FakeSource::new(vec![
                me(tone(SAMPLE_RATE as usize)),
                me(vec![0.0; SAMPLE_RATE as usize]),
            ]);
            let tap = FakeSource::new(vec![
                Frame {
                    speaker: Speaker::Other,
                    samples: tone(SAMPLE_RATE as usize),
                },
                Frame {
                    speaker: Speaker::Other,
                    samples: vec![0.0; SAMPLE_RATE as usize],
                },
            ]);
            let claim = held.then(|| owner.claim_dictation());
            let source = MultiSource::new(vec![Box::new(HeldMic::new(mic, owner)), Box::new(tap)]);
            let mut worker = Worker::new(source, FakeTranscriber::default());
            let mut lines = Lines::default();
            worker.poll(1_000, &mut lines);
            drop(claim);
            lines.0
        }
        static FREE: MicOwnership = MicOwnership::new();
        static HELD: MicOwnership = MicOwnership::new();
        let normal = run(&FREE, false);
        let held = run(&HELD, true);
        assert!(normal.iter().any(|line| line.0 == Speaker::Me));
        assert_eq!(
            held.len(),
            1,
            "only the call audio becomes a line: {held:?}"
        );
        let other = normal.iter().find(|line| line.0 == Speaker::Other);
        assert_eq!(
            held.first().map(|line| (line.0, &line.2)),
            other.map(|line| (line.0, &line.2))
        );
        assert_eq!(held.first().map(|line| line.1), Some(1_000));
    }
}
