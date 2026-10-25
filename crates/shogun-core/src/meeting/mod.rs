//! Meeting notes (§6.16, FR-MT群): detect the meeting, offer once, listen, and hand back a
//! Recap that is already work rather than a record of work.
//!
//! The lane is off by default and opt-in (FR-MT-01), and every path out of Recording closes the
//! microphone (FR-MT-12) — both properties are enforced by the state machine rather than by
//! discipline at each call site.

// Feature `desktop` (E1-T7): the live lane — detection gating, the state machine, end-of-meeting
// watch, system audio, audio health, live summary / translation, the transcript writer, the
// Deepgram key terms and the app's settings file — exists only for the Mac app. The stored
// notes' shape (minutes, export, recap, templates, talk share, anchors, reminders, settings and
// the detection rules the reminders use) stays in every build: the Memory API serves it.

pub mod anchors;
#[cfg(feature = "desktop")]
pub mod audio_health;
pub mod detect;
#[cfg(feature = "desktop")]
pub mod endwatch;
#[cfg(feature = "desktop")]
pub mod gate;
#[cfg(feature = "desktop")]
pub mod keyterms;
pub mod minutes;
#[cfg(feature = "db")]
pub mod export;
#[cfg(feature = "db")]
pub mod recap;
pub mod settings;
#[cfg(feature = "desktop")]
pub mod system_audio;
#[cfg(feature = "db")]
pub mod talk_share;
#[cfg(feature = "desktop")]
pub mod live_summary;
#[cfg(feature = "desktop")]
pub mod live_translation;
pub mod templates;
#[cfg(feature = "desktop")]
pub mod settings_store;
#[cfg(feature = "desktop")]
pub mod statemachine;
#[cfg(feature = "desktop")]
pub mod transcript_writer;

pub mod reminders;
