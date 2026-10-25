//! When a running meeting ends on its own (FR-MT-11, audit meet-051..053).
//!
//! Granola-style: the user ends it, or it ends on a real signal. A calendar slot is never a
//! cutoff, and nothing ends silently except the Mac going to sleep.
//!
//! * the call is over (the call surface released the microphone / is gone), or
//! * [`IDLE_LIMIT_MS`] without new transcript audio **and** without a typed-note edit
//!
//! first raise a "Still in the meeting?" prompt. Unanswered for [`PROMPT_ANSWER_MS`] the meeting
//! ends; "Keep recording" resets the timers. Pure so the policy is unit-tested without a Mac.

use super::statemachine::EndReason;

/// No transcript audio and no note edit for this long raises the prompt.
pub const IDLE_LIMIT_MS: i64 = 10 * 60 * 1_000;
/// How long the prompt waits for an answer before the meeting ends.
pub const PROMPT_ANSWER_MS: i64 = 60_000;
/// After a sleep ends a meeting, a re-detected call must wait for a click this long, so waking
/// the Mac never silently starts a new recording through the offer's auto-start countdown.
pub const WAKE_HOLD_MS: i64 = 10 * 60 * 1_000;
/// Wall clock minus monotonic clock past this between two ticks means the Mac slept.
pub const SLEEP_DIVERGENCE_MS: i64 = 30_000;

/// Why the prompt is up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptCause {
    /// The call surface is gone or released the microphone.
    CallEnded,
    /// [`IDLE_LIMIT_MS`] with no audio and no note edit.
    Silence,
}

impl PromptCause {
    pub fn tag(self) -> &'static str {
        match self {
            PromptCause::CallEnded => "call_ended",
            PromptCause::Silence => "silence",
        }
    }

    fn end_reason(self) -> EndReason {
        match self {
            PromptCause::CallEnded => EndReason::AppGone,
            PromptCause::Silence => EndReason::Silence,
        }
    }
}

/// What one tick observed about the running meeting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Observation {
    pub now: i64,
    /// The call surface is still present (the adapter's existing presence signals).
    pub call_present: bool,
    /// When the meeting started; the floor for activity when nothing else was ever heard.
    pub started_at: i64,
    /// Last transcript audio (epoch ms), if the capture lane ever produced any.
    pub last_sound_at: Option<i64>,
    /// Last typed-note edit (epoch ms), if any.
    pub last_note_at: Option<i64>,
}

/// The tick's verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing to do (a prompt may still be counting down; see [`EndWatch::prompt`]).
    Continue,
    /// The prompt just appeared.
    Prompt(PromptCause),
    /// The prompt went unanswered.
    End(EndReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prompt {
    pub cause: PromptCause,
    pub since_ms: i64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EndWatch {
    prompt: Option<Prompt>,
    /// "Keep recording" counts as activity from this moment.
    kept_at: Option<i64>,
    /// "Keep recording" on a call-ended prompt mutes it until the call is seen present again.
    call_ended_muted: bool,
}

impl EndWatch {
    pub fn prompt(&self) -> Option<Prompt> {
        self.prompt
    }

    /// Milliseconds the prompt has left, or `None` with no prompt up.
    pub fn remaining_ms(&self, now: i64) -> Option<i64> {
        self.prompt
            .map(|p| (PROMPT_ANSWER_MS - now.saturating_sub(p.since_ms)).max(0))
    }

    /// "Keep recording" / Enter / Esc. A press with no prompt up changes nothing, so a stale or
    /// repeated press cannot mute a real signal. Returns whether it applied.
    pub fn keep(&mut self, now: i64) -> bool {
        if self.prompt.take().is_none() {
            return false;
        }
        self.kept_at = Some(now);
        self.call_ended_muted = true;
        true
    }

    pub fn tick(&mut self, o: &Observation) -> Verdict {
        if o.call_present {
            // A new call-ended signal needs the call to have been seen present first.
            self.call_ended_muted = false;
        }
        let call_ended = !o.call_present && !self.call_ended_muted;
        // A calendar slot is deliberately not an input: meetings overrun and it is not a signal.
        let last_activity = [
            Some(o.started_at),
            o.last_sound_at,
            o.last_note_at,
            self.kept_at,
        ]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or(o.started_at);
        let silent = o.now.saturating_sub(last_activity) >= IDLE_LIMIT_MS;

        if let Some(p) = self.prompt {
            let still_true = match p.cause {
                PromptCause::CallEnded => call_ended,
                PromptCause::Silence => silent,
            };
            if !still_true {
                // Audio, a note edit or the call coming back answers it.
                self.prompt = None;
                return Verdict::Continue;
            }
            if o.now.saturating_sub(p.since_ms) >= PROMPT_ANSWER_MS {
                self.prompt = None;
                return Verdict::End(p.cause.end_reason());
            }
            return Verdict::Continue;
        }

        let cause = if call_ended {
            PromptCause::CallEnded
        } else if silent {
            PromptCause::Silence
        } else {
            return Verdict::Continue;
        };
        self.prompt = Some(Prompt {
            cause,
            since_ms: o.now,
        });
        Verdict::Prompt(cause)
    }
}

/// Whether a wake hold set at `until` still blocks auto-start at `now`.
pub fn wake_hold_active(until: Option<i64>, now: i64) -> bool {
    until.is_some_and(|until| now < until)
}

/// Whether the Mac slept between two ticks: the wall clock kept running, the monotonic one
/// (which stops in sleep) did not. A busy tick moves both together and never trips this.
pub fn slept_between(wall_gap_ms: i64, monotonic_gap_ms: i64) -> bool {
    wall_gap_ms.saturating_sub(monotonic_gap_ms) >= SLEEP_DIVERGENCE_MS
}

/// Sleep ends the meeting at the last tick before it; no prompt is possible on a sleeping Mac.
pub fn sleep_verdict(wall_gap_ms: i64, monotonic_gap_ms: i64) -> Option<EndReason> {
    slept_between(wall_gap_ms, monotonic_gap_ms).then_some(EndReason::Sleep)
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_000_000;

    fn obs(now: i64) -> Observation {
        Observation {
            now,
            call_present: true,
            started_at: T0,
            last_sound_at: Some(now),
            last_note_at: None,
        }
    }

    #[test]
    fn a_meeting_in_progress_is_left_alone() {
        let mut w = EndWatch::default();
        assert_eq!(w.tick(&obs(T0 + 5_000)), Verdict::Continue);
        assert_eq!(w.prompt(), None);
    }

    #[test]
    fn a_meeting_running_hours_past_any_slot_with_audio_is_never_cut_off() {
        // meet-051: there is no calendar input at all, so a slot cannot end a live meeting.
        let mut w = EndWatch::default();
        for hour in 1..=6 {
            let o = obs(T0 + hour * 60 * 60_000);
            assert_eq!(w.tick(&o), Verdict::Continue, "hour {hour}");
            assert_eq!(w.prompt(), None);
        }
    }

    #[test]
    fn a_wake_hold_lasts_ten_minutes_from_the_wake_tick() {
        assert!(!wake_hold_active(None, T0));
        assert!(wake_hold_active(Some(T0 + WAKE_HOLD_MS), T0 + WAKE_HOLD_MS - 1));
        assert!(!wake_hold_active(Some(T0 + WAKE_HOLD_MS), T0 + WAKE_HOLD_MS));
    }

    #[test]
    fn call_ended_prompts_then_ends_after_sixty_seconds() {
        let mut w = EndWatch::default();
        let now = T0 + 60_000;
        let gone = Observation {
            call_present: false,
            ..obs(now)
        };
        assert_eq!(w.tick(&gone), Verdict::Prompt(PromptCause::CallEnded));
        assert_eq!(w.remaining_ms(now), Some(PROMPT_ANSWER_MS));
        let later = Observation {
            now: now + PROMPT_ANSWER_MS - 1,
            ..gone
        };
        assert_eq!(w.tick(&later), Verdict::Continue);
        let expired = Observation {
            now: now + PROMPT_ANSWER_MS,
            ..gone
        };
        assert_eq!(w.tick(&expired), Verdict::End(EndReason::AppGone));
    }

    #[test]
    fn ten_minutes_without_audio_or_notes_prompts() {
        let mut w = EndWatch::default();
        let now = T0 + IDLE_LIMIT_MS;
        let o = Observation {
            last_sound_at: Some(T0),
            ..obs(now)
        };
        assert_eq!(w.tick(&o), Verdict::Prompt(PromptCause::Silence));
        let just_short = Observation {
            now: T0 + IDLE_LIMIT_MS - 1,
            ..o
        };
        assert_eq!(EndWatch::default().tick(&just_short), Verdict::Continue);
    }

    #[test]
    fn a_note_edit_resets_the_silence_clock() {
        let mut w = EndWatch::default();
        let now = T0 + IDLE_LIMIT_MS + 30_000;
        let o = Observation {
            last_sound_at: Some(T0),
            last_note_at: Some(now - 1_000),
            ..obs(now)
        };
        assert_eq!(w.tick(&o), Verdict::Continue);
    }

    #[test]
    fn a_note_edit_while_the_prompt_is_up_dismisses_it() {
        let mut w = EndWatch::default();
        let now = T0 + IDLE_LIMIT_MS;
        let quiet = Observation {
            last_sound_at: Some(T0),
            ..obs(now)
        };
        assert_eq!(w.tick(&quiet), Verdict::Prompt(PromptCause::Silence));
        let typed = Observation {
            now: now + 10_000,
            last_note_at: Some(now + 5_000),
            ..quiet
        };
        assert_eq!(w.tick(&typed), Verdict::Continue);
        assert_eq!(w.prompt(), None);
    }

    #[test]
    fn keep_resets_the_silence_timer() {
        let mut w = EndWatch::default();
        let now = T0 + IDLE_LIMIT_MS;
        let quiet = Observation {
            last_sound_at: Some(T0),
            ..obs(now)
        };
        assert_eq!(w.tick(&quiet), Verdict::Prompt(PromptCause::Silence));
        assert!(w.keep(now + 5_000));
        let after = Observation {
            now: now + 5_000 + IDLE_LIMIT_MS - 1,
            ..quiet
        };
        assert_eq!(w.tick(&after), Verdict::Continue, "a full window starts over");
        let again = Observation {
            now: now + 5_000 + IDLE_LIMIT_MS,
            ..quiet
        };
        assert_eq!(w.tick(&again), Verdict::Prompt(PromptCause::Silence));
    }

    #[test]
    fn keep_on_call_ended_mutes_until_the_call_is_seen_again() {
        let mut w = EndWatch::default();
        let now = T0 + 60_000;
        let gone = Observation {
            call_present: false,
            ..obs(now)
        };
        assert_eq!(w.tick(&gone), Verdict::Prompt(PromptCause::CallEnded));
        assert!(w.keep(now + 1_000));
        for step in 2..200 {
            let o = Observation {
                now: now + step * 1_000,
                last_sound_at: Some(now + step * 1_000),
                ..gone
            };
            assert_eq!(w.tick(&o), Verdict::Continue, "muted, step {step}");
        }
        let back = obs(now + 300_000);
        assert_eq!(w.tick(&back), Verdict::Continue);
        let gone_again = Observation {
            call_present: false,
            ..back
        };
        assert_eq!(w.tick(&gone_again), Verdict::Prompt(PromptCause::CallEnded));
    }

    #[test]
    fn keep_without_a_prompt_changes_nothing() {
        let mut w = EndWatch::default();
        assert!(!w.keep(T0));
        let gone = Observation {
            call_present: false,
            ..obs(T0 + 1_000)
        };
        assert_eq!(w.tick(&gone), Verdict::Prompt(PromptCause::CallEnded));
    }

    #[test]
    fn no_audio_at_all_but_active_typing_never_ends() {
        // meet-052: manual or in-person meeting, no key / no mic, so audio never started.
        let mut w = EndWatch::default();
        let mut last_note = None;
        for minute in 1..=90 {
            let now = T0 + minute * 60_000;
            if minute % 5 == 0 {
                last_note = Some(now);
            }
            let o = Observation {
                last_sound_at: None,
                last_note_at: last_note,
                ..obs(now)
            };
            assert_eq!(w.tick(&o), Verdict::Continue, "minute {minute}");
            assert_eq!(w.prompt(), None);
        }
        // Typing stops: a prompt after the window, never a silent end.
        let quiet_at = last_note.map_or(T0, |n| n + IDLE_LIMIT_MS);
        let o = Observation {
            last_sound_at: None,
            last_note_at: last_note,
            ..obs(quiet_at)
        };
        assert_eq!(w.tick(&o), Verdict::Prompt(PromptCause::Silence));
        let end = Observation {
            now: quiet_at + PROMPT_ANSWER_MS,
            ..o
        };
        assert_eq!(w.tick(&end), Verdict::End(EndReason::Silence));
    }

    #[test]
    fn no_audio_and_no_notes_prompts_ten_minutes_after_start() {
        let mut w = EndWatch::default();
        let o = Observation {
            last_sound_at: None,
            ..obs(T0 + IDLE_LIMIT_MS)
        };
        assert_eq!(w.tick(&o), Verdict::Prompt(PromptCause::Silence));
    }

    #[test]
    fn unanswered_silence_prompt_ends_after_sixty_seconds() {
        let mut w = EndWatch::default();
        let now = T0 + IDLE_LIMIT_MS;
        let quiet = Observation {
            last_sound_at: Some(T0),
            ..obs(now)
        };
        assert_eq!(w.tick(&quiet), Verdict::Prompt(PromptCause::Silence));
        let end = Observation {
            now: now + PROMPT_ANSWER_MS,
            ..quiet
        };
        assert_eq!(w.tick(&end), Verdict::End(EndReason::Silence));
    }

    #[test]
    fn sleep_ends_the_meeting_but_a_slow_tick_does_not() {
        assert_eq!(
            sleep_verdict(8 * 60 * 60_000, 1_000),
            Some(EndReason::Sleep)
        );
        // A stalled process moves both clocks together.
        assert_eq!(sleep_verdict(45_000, 45_000), None);
        assert_eq!(sleep_verdict(1_000, 1_000), None);
    }
}
