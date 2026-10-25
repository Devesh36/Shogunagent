//! Notch UI behavioural core (spec §3.3 + §3.4 wiring).
//!
//! - [`geometry`] — screen/notch measurements → hit regions (R_enter/R_stay/R_exp), plus the
//!   CGEvent→NSScreen coordinate conversion.
//! - [`hover`] — raw mouse samples → hover signals (early-reject, coalescing, speed/fast-dwell,
//!   menu/drag suppression).
//! - [`statemachine`] — the deterministic T1..T6 state machine (time-injected, no real timers).
//! - [`engine`] — routes hover signals and timer expiries through the tracker + machine and
//!   emits the concrete effects the macOS adapter applies.
//! - [`power`] — sleep/wake: force-collapse before sleep, health-check 1000ms after wake.
//! - [`prepared`] — the pre-assembled action row slot the expand reads (#355, NFR-SLO-02).
//! - [`dock_hover`] — when the pointer over the Dock is worth asking the Dock about (the icon's
//!   heart beat on hover).

pub mod dock_hover;
pub mod engine;
pub mod geometry;
pub mod hover;
pub mod power;
pub mod prepared;
pub mod statemachine;
