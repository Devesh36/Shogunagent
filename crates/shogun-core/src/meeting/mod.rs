//! Meeting notes (§6.16, FR-MT群): detect the meeting, offer once, listen, and hand back a
//! Recap that is already work rather than a record of work.
//!
//! The lane is off by default and opt-in (FR-MT-01), and every path out of Recording closes the
//! microphone (FR-MT-12) — both properties are enforced by the state machine rather than by
//! discipline at each call site.

pub mod anchors;
pub mod audio_health;
pub mod detect;
pub mod endwatch;
pub mod gate;
pub mod keyterms;
pub mod minutes;
#[cfg(feature = "db")]
pub mod export;
#[cfg(feature = "db")]
pub mod recap;
pub mod settings;
pub mod system_audio;
#[cfg(feature = "db")]
pub mod talk_share;
pub mod live_summary;
pub mod live_translation;
pub mod templates;
pub mod settings_store;
pub mod statemachine;
pub mod transcript_writer;

pub mod reminders;
