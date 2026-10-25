//! Observed call-audio health. Silence is not proof of permission denial.

use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Unknown,
    Unconfirmed,
    Unsupported,
    Checking,
    Active,
    Unavailable,
    Silent,
    MicrophoneOnly,
    InPerson,
}

impl Status {
    pub fn needs_help(self) -> bool {
        matches!(
            self,
            Self::Unconfirmed | Self::Unavailable | Self::Silent | Self::MicrophoneOnly
        )
    }
}

/// Session mode is separate from microphone diarization: choosing mic-only for a call must
/// not label the user's microphone as a room full of speakers.
#[derive(Clone, Copy, Debug)]
pub enum CaptureMode {
    Online,
    MicrophoneOnly,
    InPerson,
}

pub fn needs_choice(in_person: bool, explained: bool, needs_help: bool, chosen: bool) -> bool {
    !in_person && !chosen && (!explained || needs_help)
}

/// Only a failed tap or an explicit mic-only choice needs repair next time. Silence and
/// unfinished startup are not permission evidence; unsupported systems cannot be repaired.
pub fn durable_help(status: Status) -> Option<bool> {
    match status {
        Status::Unavailable | Status::MicrophoneOnly => Some(true),
        Status::Active | Status::Unsupported => Some(false),
        _ => None,
    }
}

/// The event and read API must project the same evidence, including on older macOS.
pub fn effective_status(
    status: Status,
    supported: bool,
    recording: bool,
    needs_help: bool,
    in_person: bool,
) -> Status {
    if recording && in_person {
        return Status::InPerson;
    }
    if !supported {
        return Status::Unsupported;
    }
    if !recording {
        return if needs_help {
            Status::Unconfirmed
        } else {
            Status::Unknown
        };
    }
    status
}

pub struct Health {
    last_heard: Instant,
    status: Status,
}

impl Health {
    pub fn new(now: Instant) -> Self {
        Self {
            last_heard: now,
            status: Status::Checking,
        }
    }

    /// A conservative 60s reminder also catches a revoked tap returning zeroes while the mic
    /// still works. Copy must say "if others are speaking", never "permission denied".
    pub fn observe(&mut self, now: Instant, heard_system_audio: bool) -> Option<Status> {
        let next = if heard_system_audio {
            self.last_heard = now;
            Status::Active
        } else if now.saturating_duration_since(self.last_heard) >= Duration::from_secs(60) {
            Status::Silent
        } else {
            self.status
        };
        if next == self.status {
            return None;
        }
        self.status = next;
        Some(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_silence_and_unsupported_are_not_persistent_permission_failures() {
        assert_eq!(durable_help(Status::Checking), None);
        assert_eq!(durable_help(Status::Silent), None);
        assert_eq!(durable_help(Status::Unsupported), Some(false));
        assert_eq!(durable_help(Status::Unavailable), Some(true));
        assert_eq!(durable_help(Status::Active), Some(false));
        assert_eq!(
            effective_status(Status::Checking, true, true, true, false),
            Status::Checking
        );
        assert_eq!(
            effective_status(Status::Silent, true, false, false, false),
            Status::Unknown
        );
        assert_eq!(
            effective_status(Status::Unknown, false, false, true, false),
            Status::Unsupported
        );
    }

    #[test]
    fn first_online_start_waits_but_in_person_and_explained_starts_do_not() {
        assert!(needs_choice(false, false, false, false));
        assert!(!needs_choice(true, false, true, false));
        assert!(!needs_choice(false, true, false, false));
    }

    #[test]
    fn unresolved_capture_requires_a_new_choice_each_meeting() {
        assert!(needs_choice(false, true, true, false));
        assert!(!needs_choice(false, true, true, true));
    }

    #[test]
    fn silent_tap_is_not_active_and_recovers_when_audio_arrives() {
        let start = Instant::now();
        let mut health = Health::new(start);
        assert_eq!(
            health.observe(start + Duration::from_millis(59_999), false),
            None
        );
        assert_eq!(
            health.observe(start + Duration::from_millis(60_000), false),
            Some(Status::Silent)
        );
        assert_eq!(
            health.observe(start + Duration::from_millis(61_000), true),
            Some(Status::Active)
        );
        assert_eq!(
            health.observe(start + Duration::from_millis(62_000), true),
            None
        );
    }

    #[test]
    fn a_previously_working_tap_can_become_silent() {
        let start = Instant::now();
        let mut health = Health::new(start);
        assert_eq!(
            health.observe(start + Duration::from_millis(1000), true),
            Some(Status::Active)
        );
        assert_eq!(
            health.observe(start + Duration::from_millis(61_000), false),
            Some(Status::Silent)
        );
    }
}
