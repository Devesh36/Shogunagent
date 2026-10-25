//! The meeting-notes lifecycle (FR-MT-07).
//!
//! ```text
//!   Idle
//!    │  detection (FR-MT-04)
//!    ▼
//!   Offered ──"Not now"───────────────────────────────────┐
//!    │  10s grace (start if nothing happens)              │
//!    ▼                                                    │
//!   Recording ──"Stop"──→ Wrapping ──→ Recap ──────────────┤
//!    │                                                    │
//!    └─ call over / 10min idle (prompted) / sleep ┘
//!                                                         ▼
//!                                                       Idle
//! ```
//!
//! **Offered is not a formality for automatic starts.** Detection never jumps Idle → Recording
//! (FR-MT-08): the feature starts on its own only after showing itself once, and refusal is
//! always one tap away. [`Input::ManualStart`] is the explicit exception — Record now is the
//! consent, so the offer card is skipped. A test holds both lines.

/// Timers the adapter schedules on request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Timer {
    /// The 10s countdown shown in Offered (FR-MT-08).
    OfferGrace,
    /// Idle watchdog — 10 minutes without audio or note edits (see `endwatch`) (FR-MT-11).
    Silence,
}

/// Named states of FR-MT-07. `Wrapping` is the interval closing: audio has stopped, Recap has not
/// yet been assembled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    Offered,
    Recording,
    Wrapping,
}

impl State {
    pub fn tag(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Offered => "offered",
            State::Recording => "recording",
            State::Wrapping => "wrapping",
        }
    }
}

/// Why an interval ended. Kept on the transition because Recap and the health metrics want to know
/// whether the user stopped it or the machine gave up on it (FR-MT-11).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndReason {
    /// The user pressed Stop.
    UserStopped,
    /// The meeting app quit, or its window/tab disappeared.
    AppGone,
    /// 10 minutes with no audio and no note edit, "Still in the meeting?" unanswered.
    Silence,
    /// The Mac went to sleep; the meeting ends at the last moment before it.
    Sleep,
}

/// Inputs. Detection signals arrive from [`super::detect`]; the rest from the UI and the adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    /// Detection crossed the offer threshold (FR-MT-04).
    MeetingDetected,
    /// The 10s grace expired with no answer → start (FR-MT-08).
    GraceExpired,
    /// "Start" pressed during the grace.
    Start,
    /// "Record now" — the user started notes by hand. Consent is the press, so Idle may
    /// open a session without the 10s offer (FR-MT-08 still forbids every other Idle edge).
    ManualStart,
    /// "Not now" — this meeting only, settings untouched (FR-MT-02c).
    NotNow,
    /// "Stop" pressed while recording. No confirmation dialog (FR-MT-09).
    Stop,
    /// Adapter validated Undo inside the user Stop window; reuse the open interval.
    UndoStop,
    /// An automatic end condition fired (FR-MT-11).
    AutoEnd(EndReason),
    /// Recap finished (or degraded Recap was shown) → back to Idle.
    Wrapped,
    /// The whole feature was switched off (FR-MT-02a) — must stop everything, everywhere.
    FeatureDisabled,
}

/// Side effects for the adapter. The machine never touches the database or the audio device
/// itself; it says what must happen and the caller does it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    Transition(State),
    StartTimer {
        timer: Timer,
        ms: u64,
    },
    CancelTimer(Timer),
    /// Open the interval (`sessions` row) — FR-MT-05.
    OpenSession,
    /// Close the interval at the current time, recording why.
    CloseSession(EndReason),
    /// Begin/stop capturing audio. **The only two effects that touch the microphone**, so the
    /// audio lane cannot be opened from anywhere but the Recording state (FR-MT-12).
    StartAudio,
    StopAudio,
    /// Assemble the Recap (FR-MT-16), degraded if need be (FR-MT-19).
    BuildRecap,
}

/// Timing constants. Named rather than inlined so the spec value is greppable from the code.
#[derive(Clone, Copy, Debug)]
pub struct Params {
    /// FR-MT-08: how long Offered waits before starting on its own.
    pub offer_grace_ms: u64,
    /// FR-MT-11: silence that ends a meeting.
    pub silence_ms: u64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            offer_grace_ms: 10_000,
            silence_ms: 10 * 60 * 1_000,
        }
    }
}

/// What to feed the machine when the Offered grace elapses.
///
/// Detected meetings auto-start (FR-MT-08). An MCP/REST/CLI queue is L2: no tap means decline,
/// not consent to open the microphone.
pub fn grace_elapsed_input(queued_by_api: bool) -> Input {
    offer_expiry_input(queued_by_api, false)
}

/// An unanswered audio choice expires as a decline, just like an API approval.
pub fn offer_expiry_input(queued_by_api: bool, audio_choice_pending: bool) -> Input {
    if queued_by_api || audio_choice_pending {
        Input::NotNow
    } else {
        Input::GraceExpired
    }
}

/// How many CloseSession attempts (the first, then one per Wrapping tick, about a second apart)
/// may fail before the lane stops waiting and returns to Idle (issue #1005). Wrapping ignores
/// detection and Record now, so a database that never accepts the close would otherwise block
/// every later meeting until the app restarts. The interval stays open in the database with its
/// transcript and notes; the next launch closes it as abandoned and indexes it.
pub const CLOSE_RETRY_LIMIT: u32 = 30;

/// What a failed CloseSession leads to, given how many attempts have failed in a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseRetry {
    /// Stay in Wrapping and try again on the next tick.
    Retry,
    /// Stop retrying: feed [`Input::Wrapped`] so the lane returns to Idle.
    GiveUp,
}

pub fn close_retry(failed_attempts: u32) -> CloseRetry {
    if failed_attempts >= CLOSE_RETRY_LIMIT {
        CloseRetry::GiveUp
    } else {
        CloseRetry::Retry
    }
}

/// Pure identity/deadline fence for the desktop's deferred user Stop.
#[derive(Debug, Clone, Copy)]
pub struct StopUndo {
    pub session_id: i64,
    undo_until: std::time::Instant,
}

impl StopUndo {
    pub const WINDOW_MS: i64 = 10_000;

    pub fn new(session_id: i64, now: std::time::Instant) -> Self {
        Self { session_id, undo_until: now + std::time::Duration::from_millis(Self::WINDOW_MS as u64) }
    }

    pub fn remaining_ms(self, now: std::time::Instant) -> i64 {
        self.undo_until.saturating_duration_since(now).as_millis().min(Self::WINDOW_MS as u128) as i64
    }

    pub fn accepts(self, session_id: i64, now: std::time::Instant) -> bool {
        self.session_id == session_id && self.remaining_ms(now) > 0
    }
}

/// The lifecycle machine. Holds only the current state — everything else lives in the caller.
#[derive(Debug)]
pub struct Machine {
    state: State,
    params: Params,
}

impl Machine {
    pub fn new(params: Params) -> Self {
        Self {
            state: State::Idle,
            params,
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// OpenSession failed after we had already moved to Recording. There is no interval and
    /// no audio, so Idle is the only honest state.
    pub fn revert_failed_open(&mut self) {
        if self.state == State::Recording {
            self.state = State::Idle;
        }
    }

    /// Apply an input, returning the effects to perform in order.
    pub fn step(&mut self, input: Input) -> Vec<Effect> {
        use Input as I;
        use State as S;

        match (self.state, input) {
            // Detection offers; it never starts. Automatic Idle → Recording is forbidden
            // (FR-MT-08) — see `recording_is_unreachable_without_passing_through_offered`.
            (S::Idle, I::MeetingDetected) => {
                self.state = S::Offered;
                vec![
                    Effect::Transition(S::Offered),
                    Effect::StartTimer {
                        timer: Timer::OfferGrace,
                        ms: self.params.offer_grace_ms,
                    },
                ]
            }
            (S::Idle, I::ManualStart) => self.begin_recording(false),

            // Pressing Start, Record now, and letting the countdown run out are the same
            // transition: the grace is a chance to refuse, not a different way of consenting.
            (S::Offered, I::Start | I::ManualStart | I::GraceExpired) => self.begin_recording(true),
            // "Not now" declines this meeting and nothing else: no session was opened, so there
            // is nothing to close, and no audio was started, so there is nothing to stop.
            (S::Offered, I::NotNow | I::FeatureDisabled) => {
                self.state = S::Idle;
                vec![
                    Effect::CancelTimer(Timer::OfferGrace),
                    Effect::Transition(S::Idle),
                ]
            }

            (S::Recording, I::Stop) => self.end(EndReason::UserStopped),
            (S::Recording, I::AutoEnd(why)) => self.end(why),
            // Switching the feature off mid-meeting ends it like any other stop, on the same
            // path — so no route out of Recording can leave the microphone open.
            (S::Recording, I::FeatureDisabled) => self.end(EndReason::UserStopped),

            (S::Wrapping, I::UndoStop) => {
                self.state = S::Recording;
                vec![Effect::Transition(S::Recording)]
            }

            (S::Wrapping, I::Wrapped) => {
                self.state = S::Idle;
                vec![Effect::Transition(S::Idle)]
            }

            // A late timer or a repeated Stop must not move the
            // machine, and the daemon cannot afford to panic on an unexpected input (CLAUDE.md).
            _ => Vec::new(),
        }
    }

    /// How long the Recap may sit on screen before the lane returns to Idle on its own.
    pub const RECAP_DISMISS_MS: i64 = 90_000;
    /// Shorter auto-dismiss when the user has already left the call (FR-MT-11).
    pub const RECAP_DISMISS_LEFT_MS: i64 = 60_000;

    fn begin_recording(&mut self, cancel_offer: bool) -> Vec<Effect> {
        self.state = State::Recording;
        let mut effects = Vec::with_capacity(6);
        if cancel_offer {
            effects.push(Effect::CancelTimer(Timer::OfferGrace));
        }
        effects.extend([
            Effect::Transition(State::Recording),
            Effect::OpenSession,
            Effect::StartAudio,
            Effect::StartTimer {
                timer: Timer::Silence,
                ms: self.params.silence_ms,
            },
        ]);
        effects
    }

    /// Recording → Wrapping. Audio stops first, so that the instant the interval is over the
    /// microphone is already closed — before any slower work (closing the row, building Recap).
    fn end(&mut self, why: EndReason) -> Vec<Effect> {
        self.state = State::Wrapping;
        vec![
            Effect::StopAudio,
            Effect::CancelTimer(Timer::Silence),
            Effect::Transition(State::Wrapping),
            Effect::CloseSession(why),
            Effect::BuildRecap,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undo_stop_is_scoped_and_expires_at_ten_seconds() {
        let now = std::time::Instant::now();
        let window = StopUndo::new(42, now);
        assert_eq!(window.remaining_ms(now), 10_000);
        assert!(window.accepts(42, now + std::time::Duration::from_millis(9_999)));
        assert!(!window.accepts(43, now));
        assert!(!window.accepts(42, now + std::time::Duration::from_secs(10)));
        assert_eq!(window.remaining_ms(now + std::time::Duration::from_secs(20)), 0);
    }

    #[test]
    fn undo_reuses_the_interval_without_closing_or_building_a_recap() {
        let mut m = machine();
        assert!(m.step(Input::UndoStop).is_empty());
        m.step(Input::ManualStart);
        m.step(Input::Stop);
        assert_eq!(m.step(Input::UndoStop), vec![Effect::Transition(State::Recording)]);
        assert!(m.step(Input::UndoStop).is_empty());
        assert!(m.step(Input::Stop).contains(&Effect::BuildRecap));
    }

    fn machine() -> Machine {
        Machine::new(Params::default())
    }

    #[test]
    fn detection_offers_rather_than_starting() {
        let mut m = machine();
        let fx = m.step(Input::MeetingDetected);

        assert_eq!(m.state(), State::Offered);
        assert!(
            !fx.contains(&Effect::StartAudio),
            "detection alone must never open the microphone (FR-MT-08)"
        );
        assert!(fx.contains(&Effect::StartTimer {
            timer: Timer::OfferGrace,
            ms: 10_000
        }));
    }

    #[test]
    fn ignoring_the_offer_starts_recording_after_the_grace() {
        let mut m = machine();
        m.step(Input::MeetingDetected);

        let fx = m.step(Input::GraceExpired);

        assert_eq!(m.state(), State::Recording);
        assert!(fx.contains(&Effect::OpenSession));
        assert!(fx.contains(&Effect::StartAudio));
    }

    #[test]
    fn not_now_declines_without_opening_a_session() {
        let mut m = machine();
        m.step(Input::MeetingDetected);

        let fx = m.step(Input::NotNow);

        assert_eq!(m.state(), State::Idle);
        assert!(!fx.contains(&Effect::OpenSession));
        assert!(!fx.contains(&Effect::StartAudio));
    }

    #[test]
    fn recording_is_unreachable_without_passing_through_offered() {
        // Automatic path: no input applied to Idle produces audio or a session (FR-MT-08).
        // ManualStart is the explicit Record-now exception, covered separately.
        for input in [
            Input::Start,
            Input::GraceExpired,
            Input::Stop,
            Input::Wrapped,
            Input::AutoEnd(EndReason::Silence),
            Input::NotNow,
            Input::FeatureDisabled,
        ] {
            let mut m = machine();
            let fx = m.step(input);
            assert_eq!(m.state(), State::Idle, "{input:?} must leave Idle alone");
            assert!(fx.is_empty(), "{input:?} must have no effect from Idle");
        }
    }

    #[test]
    fn record_now_from_idle_opens_the_session_without_an_offer() {
        let mut m = machine();
        let fx = m.step(Input::ManualStart);

        assert_eq!(m.state(), State::Recording);
        assert!(fx.contains(&Effect::OpenSession));
        assert!(fx.contains(&Effect::StartAudio));
        assert!(
            !fx.iter().any(|e| matches!(
                e,
                Effect::StartTimer {
                    timer: Timer::OfferGrace,
                    ..
                }
            )),
            "Record now must not show the 10s offer"
        );
    }

    #[test]
    fn failed_open_returns_recording_to_idle() {
        let mut m = machine();
        m.step(Input::ManualStart);
        m.revert_failed_open();
        assert_eq!(m.state(), State::Idle);
        m.revert_failed_open();
        assert_eq!(m.state(), State::Idle, "Idle is already the honest state");
    }

    #[test]
    fn an_api_start_from_idle_offers_instead_of_recording() {
        let mut m = machine();
        let fx = m.step(Input::MeetingDetected);
        assert_eq!(m.state(), State::Offered);
        assert!(!fx.contains(&Effect::OpenSession));
        assert!(!fx.contains(&Effect::StartAudio));
        assert!(fx.iter().any(|e| matches!(
            e,
            Effect::StartTimer {
                timer: Timer::OfferGrace,
                ..
            }
        )));
    }

    #[test]
    fn an_api_offer_expires_as_a_decline() {
        assert_eq!(grace_elapsed_input(true), Input::NotNow);
        assert_eq!(grace_elapsed_input(false), Input::GraceExpired);

        let mut m = machine();
        m.step(Input::MeetingDetected);
        let fx = m.step(grace_elapsed_input(true));
        assert_eq!(m.state(), State::Idle);
        assert!(!fx.contains(&Effect::OpenSession));
        assert!(!fx.contains(&Effect::StartAudio));
    }

    #[test]
    fn stop_closes_the_interval_and_the_microphone() {
        let mut m = machine();
        m.step(Input::MeetingDetected);
        m.step(Input::Start);

        let fx = m.step(Input::Stop);

        assert_eq!(m.state(), State::Wrapping);
        assert_eq!(fx.first(), Some(&Effect::StopAudio), "audio stops first");
        assert!(fx.contains(&Effect::CloseSession(EndReason::UserStopped)));
        assert!(fx.contains(&Effect::BuildRecap));
    }

    #[test]
    fn every_automatic_end_condition_stops_the_microphone() {
        // FR-MT-11: the user forgetting to stop must not leave it running.
        for why in [
            EndReason::AppGone,
            EndReason::Sleep,
            EndReason::Silence,
        ] {
            let mut m = machine();
            m.step(Input::MeetingDetected);
            m.step(Input::Start);

            let fx = m.step(Input::AutoEnd(why));

            assert_eq!(m.state(), State::Wrapping);
            assert!(fx.contains(&Effect::StopAudio), "{why:?} must stop audio");
            assert!(
                fx.contains(&Effect::CloseSession(why)),
                "{why:?} must be recorded as the reason"
            );
        }
    }

    #[test]
    fn disabling_the_feature_mid_meeting_stops_the_microphone() {
        // FR-MT-02a: the global off switch must reach a meeting already in progress.
        let mut m = machine();
        m.step(Input::MeetingDetected);
        m.step(Input::Start);

        let fx = m.step(Input::FeatureDisabled);

        assert!(fx.contains(&Effect::StopAudio));
        assert_eq!(m.state(), State::Wrapping);
    }

    #[test]
    fn a_repeated_stop_does_not_close_the_interval_twice() {
        let mut m = machine();
        m.step(Input::MeetingDetected);
        m.step(Input::Start);
        m.step(Input::Stop);

        let fx = m.step(Input::Stop);

        assert!(fx.is_empty(), "a second Stop while wrapping is a no-op");
        assert_eq!(m.state(), State::Wrapping);
    }

    #[test]
    fn wrapping_returns_to_idle_ready_for_the_next_meeting() {
        let mut m = machine();
        m.step(Input::MeetingDetected);
        m.step(Input::Start);
        m.step(Input::Stop);

        m.step(Input::Wrapped);
        assert_eq!(m.state(), State::Idle);

        // and the next meeting is offered again, not resumed
        let fx = m.step(Input::MeetingDetected);
        assert_eq!(m.state(), State::Offered);
        assert!(!fx.contains(&Effect::StartAudio));
    }

    /// Issue #1005: a close that keeps failing is retried a bounded number of times, then the
    /// lane leaves Wrapping, so the next meeting can be offered or recorded again.
    #[test]
    fn a_close_that_keeps_failing_gives_up_and_frees_the_lane() {
        let mut m = machine();
        m.step(Input::ManualStart);
        let fx = m.step(Input::Stop);
        assert!(fx.contains(&Effect::CloseSession(EndReason::UserStopped)));
        // While retrying, Wrapping holds: a new meeting cannot start over the open interval.
        let mut failures = 1;
        while close_retry(failures) == CloseRetry::Retry {
            assert!(m.step(Input::ManualStart).is_empty());
            assert!(m.step(Input::MeetingDetected).is_empty());
            assert_eq!(m.state(), State::Wrapping);
            failures += 1;
        }
        assert_eq!(failures, CLOSE_RETRY_LIMIT, "bounded, not forever");
        assert_eq!(close_retry(CLOSE_RETRY_LIMIT + 5), CloseRetry::GiveUp);
        // Giving up feeds Wrapped; nothing in it touches the session row or the microphone.
        let fx = m.step(Input::Wrapped);
        assert_eq!(fx, vec![Effect::Transition(State::Idle)]);
        assert_eq!(m.state(), State::Idle);
        let fx = m.step(Input::ManualStart);
        assert!(fx.contains(&Effect::OpenSession), "Record now works again");
    }

    #[test]
    fn every_ending_can_be_dismissed_back_to_idle() {
        // Detection only acts from Idle, so a machine parked in Wrapping has stopped noticing
        // meetings. Whichever way a meeting ended, dismissing the Recap must revive the lane.
        for ending in [
            Input::Stop,
            Input::FeatureDisabled,
            Input::AutoEnd(EndReason::AppGone),
            Input::AutoEnd(EndReason::Sleep),
            Input::AutoEnd(EndReason::Silence),
        ] {
            let mut m = machine();
            m.step(Input::MeetingDetected);
            m.step(Input::Start);

            m.step(ending);
            assert_eq!(
                m.state(),
                State::Wrapping,
                "{ending:?}: the Recap needs a state to live in"
            );

            m.step(Input::Wrapped);
            assert_eq!(m.state(), State::Idle, "{ending:?} left the lane stuck");
        }
    }
}
