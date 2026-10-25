//! Factual live meeting health. No inference from silence, no audio retention, no user content.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureState {
    Ready,
    Failed,
    NotRequested,
    Unsupported,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderState {
    Connecting,
    Streaming,
    Reconnecting,
    CatchingUp,
    Stopped,
    KeyRejected,
    /// The provider refused the stream for payment: the account is out of credit (#1005).
    QuotaExhausted,
    Offline,
}

impl ProviderState {
    pub fn observed(
        connected: bool,
        auth_rejected: bool,
        quota_exhausted: bool,
        stopped: bool,
        reconnecting: bool,
        catching_up: bool,
    ) -> Self {
        if auth_rejected {
            Self::KeyRejected
        } else if quota_exhausted {
            Self::QuotaExhausted
        } else if stopped {
            Self::Stopped
        } else if reconnecting {
            Self::Reconnecting
        } else if !connected {
            Self::Connecting
        } else if catching_up {
            Self::CatchingUp
        } else {
            Self::Streaming
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistenceState {
    Saving,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AudioHealth {
    pub microphone: CaptureState,
    pub system_audio: CaptureState,
    pub provider: ProviderState,
    pub persistence: PersistenceState,
}

impl AudioHealth {
    pub fn capture_message(self) -> Option<&'static str> {
        if self.persistence == PersistenceState::Failed {
            Some("Some transcript lines could not be saved. Keep your typed notes; retrying audio cannot recover those lines.")
        } else if self.microphone == CaptureState::Failed {
            Some("Your microphone disconnected. Check the selected device and retry audio.")
        } else {
            match self.system_audio {
                CaptureState::Failed => Some("Call audio capture stopped. Retry audio to restore the other participants' transcription."),
                CaptureState::Unavailable => Some("Call audio is unavailable. Check Audio Recording permission and retry audio; only microphone capture is available."),
                // Permanent OS limitations belong in the factual status line, not a retry alert.
                CaptureState::Unsupported => None,
                CaptureState::Ready | CaptureState::NotRequested => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_state_is_from_connection_facts_and_has_explicit_recovery() {
        assert_eq!(
            ProviderState::observed(false, false, false, false, false, false),
            ProviderState::Connecting
        );
        assert_eq!(
            ProviderState::observed(false, true, true, true, true, false),
            ProviderState::KeyRejected
        );
        assert_eq!(
            ProviderState::observed(false, false, true, true, false, false),
            ProviderState::QuotaExhausted
        );
        assert_eq!(
            ProviderState::observed(false, false, false, true, false, false),
            ProviderState::Stopped
        );
        assert_eq!(
            ProviderState::observed(false, false, false, false, true, true),
            ProviderState::Reconnecting
        );
        assert_eq!(
            ProviderState::observed(true, false, false, false, false, true),
            ProviderState::CatchingUp
        );
        assert_eq!(
            ProviderState::observed(true, false, false, false, false, false),
            ProviderState::Streaming
        );
    }

    #[test]
    fn intentional_room_microphone_only_has_no_missing_remote_audio_warning() {
        let mut health = AudioHealth {
            microphone: CaptureState::Ready,
            system_audio: CaptureState::NotRequested,
            provider: ProviderState::Streaming,
            persistence: PersistenceState::Saving,
        };
        assert_eq!(health.capture_message(), None);
        health.system_audio = CaptureState::Unavailable;
        assert!(health
            .capture_message()
            .is_some_and(|message| message.contains("only microphone")));
        health.system_audio = CaptureState::Failed;
        assert!(health
            .capture_message()
            .is_some_and(|message| message.contains("stopped")));
        health.microphone = CaptureState::Failed;
        assert!(health
            .capture_message()
            .is_some_and(|message| message.contains("selected device")));
        health.persistence = PersistenceState::Failed;
        assert!(health
            .capture_message()
            .is_some_and(|message| message.contains("cannot recover")));
    }
}
