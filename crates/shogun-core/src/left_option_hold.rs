//! Hold the LEFT Option key alone to dictate; let go to stop (owner decision 2026-10-12: the old
//! ⌃⌥V default took too much effort). The macOS shell (`voice_shortcut.rs`) feeds NSEvent input
//! into [`LeftOptionHold`] and applies the [`Effect`]s it returns; the rules live here so they
//! are testable without a Mac.
//!
//! Left Option is an ordinary modifier all day long: ⌥+letter characters, ⌥⌘ shortcuts,
//! ⌥-click and ⌥-drag, ⌥-arrow word navigation. So a press only becomes dictation after left
//! Option has been held ALONE for [`HOLD_TO_TALK_THRESHOLD`], with no key, no other modifier
//! and no pointer event since it went down. The microphone is not touched before that moment:
//! a quick press, or any modifier use, never opens it. Once dictation has started, any other key
//! or modifier still means the user was typing after a pause, so that dictation is discarded
//! (nothing inserted, nothing copied). Pointer input after the start does not discard it.
//!
//! Right Option (key code 61) is a different key: it never starts dictation, and it keeps its own
//! tap-to-draft / double-tap Scribe gestures, which this module does not touch.
//! Customized left-modifier holds reuse this same press machine via [`ModifierHoldBinding`].

use std::time::{Duration, Instant};

/// How long left Option must be held alone before dictation starts.
///
/// 300 ms: long enough that the gap between pressing ⌥ and the next key of an ⌥ shortcut or
/// ⌥+letter character, which is well under it for practiced typing, poisons the press before the
/// microphone opens; short enough that the start does not feel late. It is also the window the
/// other Option gesture already uses (double-tap Scribe) and the minimum hold of the earlier
/// push-to-talk design, so one number governs how long "a held Option" is across the app.
pub const HOLD_TO_TALK_THRESHOLD: Duration = Duration::from_millis(300);

/// The default voice binding (shortcuts.json): left Option + left Command held together (owner
/// decision 2026-10-14). It follows the gesture grammar of "Tap+Alt" / "Dual+Super", with the side
/// spelled out because "Alt" alone means the right Option key in "Tap+Alt".
pub const VOICE_HOLD_BINDING: &str = "Hold+LeftAlt+LeftSuper";

/// macOS virtual key codes (`kVK_Option`, `kVK_RightOption`).
pub const LEFT_OPTION_KEY_CODE: u16 = 58;
pub const RIGHT_OPTION_KEY_CODE: u16 = 61;

/// NSEvent modifier flags (device-independent).
const FLAG_SHIFT: usize = 1 << 17;
const FLAG_CONTROL: usize = 1 << 18;
const FLAG_OPTION: usize = 1 << 19;
const FLAG_COMMAND: usize = 1 << 20;
const FLAG_FUNCTION: usize = 1 << 23;
/// Device-dependent bits (`NX_DEVICELALTKEYMASK` / `NX_DEVICERALTKEYMASK`). Both Option keys
/// share `FLAG_OPTION`; only these say which one is down.
const DEVICE_LEFT_OPTION: usize = 0x20;
const DEVICE_RIGHT_OPTION: usize = 0x40;

/// NSEvent mask of the pointer button, drag, scroll, tablet and gesture event families. Any of
/// them during the press means ⌥-click / ⌥-drag / ⌥-scroll, so the press is not dictation.
/// Plain pointer movement (mouseMoved, mouseEntered/Exited) is deliberately left out, unlike the
/// right Option tap's poison mask: a hand resting on the trackpad jitters the pointer, and that
/// must not stop a 300 ms hold from starting dictation.
pub const POINTER_EVENT_MASK: usize = (1 << 1)
    | (1 << 2)
    | (1 << 3)
    | (1 << 4)
    | (1 << 6)
    | (1 << 7)
    | (1 << 18) // rotate
    | (1 << 19) // begin gesture
    | (1 << 20) // end gesture
    | (1 << 22) // scroll wheel
    | (1 << 23) // tablet point
    | (1 << 24) // tablet proximity
    | (1 << 25)
    | (1 << 26)
    | (1 << 27)
    | (1 << 29) // gesture
    | (1 << 30) // magnify
    | (1 << 31) // swipe
    | (1usize << 32) // smart magnify
    | (1usize << 34) // pressure
    | (1usize << 37); // direct touch

/// One input event, already classified by the shell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    /// A flagsChanged event for the left Option key itself.
    LeftOption {
        down: bool,
        /// No other modifier (right Option included) is down with it.
        alone: bool,
    },
    /// Any key down, or a flagsChanged event for any other key (another modifier, right Option).
    OtherKey,
    /// Any event in [`POINTER_EVENT_MASK`].
    Pointer,
}

/// What the shell must do after an input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    None,
    /// Call [`LeftOptionHold::threshold_reached`] with this press after [`HOLD_TO_TALK_THRESHOLD`].
    ArmTimer {
        press: u64,
    },
    /// Start dictation now (the existing start path and cue).
    StartDictation,
    /// Left Option was released: finish the dictation normally.
    StopDictation,
    /// Another key joined the hold: cancel the dictation and drop it, delivering nothing.
    DiscardDictation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    /// Down alone, waiting for the threshold.
    Armed {
        press: u64,
        down_at: Instant,
    },
    /// Something else happened during this press; nothing more until left Option goes up.
    Poisoned,
    Dictating,
}

/// The press state machine. One per process, owned by the shell under a lock.
#[derive(Debug)]
pub struct LeftOptionHold {
    phase: Phase,
    next_press: u64,
}

impl Default for LeftOptionHold {
    fn default() -> Self {
        Self::new()
    }
}

impl LeftOptionHold {
    pub const fn new() -> Self {
        Self {
            phase: Phase::Idle,
            next_press: 0,
        }
    }

    /// A press is waiting for its threshold. Cheap check for the hot pointer path.
    pub fn armed(&self) -> bool {
        matches!(self.phase, Phase::Armed { .. })
    }

    pub fn dictating(&self) -> bool {
        self.phase == Phase::Dictating
    }

    pub fn input(&mut self, input: Input, now: Instant) -> Effect {
        match input {
            Input::LeftOption { down: true, alone } => match self.phase {
                // A repeated down without an up in between changes nothing.
                Phase::Armed { .. } | Phase::Dictating => Effect::None,
                // From Idle, or Poisoned after a missed up: a fresh press.
                Phase::Idle | Phase::Poisoned => {
                    if !alone {
                        self.phase = Phase::Poisoned;
                        return Effect::None;
                    }
                    self.next_press = self.next_press.wrapping_add(1);
                    let press = self.next_press;
                    self.phase = Phase::Armed {
                        press,
                        down_at: now,
                    };
                    Effect::ArmTimer { press }
                }
            },
            Input::LeftOption { down: false, .. } => {
                let was = std::mem::replace(&mut self.phase, Phase::Idle);
                if was == Phase::Dictating {
                    Effect::StopDictation
                } else {
                    Effect::None
                }
            }
            Input::OtherKey => match self.phase {
                Phase::Armed { .. } => {
                    self.phase = Phase::Poisoned;
                    Effect::None
                }
                Phase::Dictating => {
                    self.phase = Phase::Poisoned;
                    Effect::DiscardDictation
                }
                Phase::Idle | Phase::Poisoned => Effect::None,
            },
            Input::Pointer => {
                if self.armed() {
                    self.phase = Phase::Poisoned;
                }
                Effect::None
            }
        }
    }

    /// The timer armed for `press` fired. Starts dictation only if that same press is still
    /// down, clean, and has really been held for the threshold.
    pub fn threshold_reached(&mut self, press: u64, now: Instant) -> Effect {
        match self.phase {
            Phase::Armed {
                press: armed,
                down_at,
            } if armed == press
                && now.saturating_duration_since(down_at) >= HOLD_TO_TALK_THRESHOLD =>
            {
                self.phase = Phase::Dictating;
                Effect::StartDictation
            }
            _ => Effect::None,
        }
    }

    /// A failed or obsolete threshold must not reset a newer press.
    pub fn abort_threshold(&mut self, press: u64) -> Effect {
        if matches!(self.phase, Phase::Armed { press: current, .. } if current == press) {
            self.reset()
        } else {
            Effect::None
        }
    }

    /// The gesture stopped applying (rebound, or shortcut recording began). A running
    /// dictation ends normally; a waiting press is forgotten.
    pub fn reset(&mut self) -> Effect {
        let was = std::mem::replace(&mut self.phase, Phase::Idle);
        if was == Phase::Dictating {
            Effect::StopDictation
        } else {
            Effect::None
        }
    }
}

/// Classify a flagsChanged event for the default left-Option hold. Right Option (61) and every
/// other modifier are "another key".
pub fn classify_flags_changed(key_code: u16, flags: usize) -> Input {
    ModifierHoldBinding::left_option().classify_flags_changed(key_code, flags)
}

// Side bits and key codes from IOKit's IOLLEvent.h and macOS virtual key codes.
const HOLD_MODIFIERS: [(&str, u16, usize, usize, usize); 4] = [
    ("LeftControl", 59, FLAG_CONTROL, 0x01, 0x2001),
    ("LeftShift", 56, FLAG_SHIFT, 0x02, 0x06),
    (
        "LeftAlt",
        LEFT_OPTION_KEY_CODE,
        FLAG_OPTION,
        DEVICE_LEFT_OPTION,
        DEVICE_LEFT_OPTION | DEVICE_RIGHT_OPTION,
    ),
    ("LeftSuper", 55, FLAG_COMMAND, 0x08, 0x18),
];

/// A user-selected combination of left modifiers, reusing the hold press state machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModifierHoldBinding(u8);

impl ModifierHoldBinding {
    /// Reject unknown keys, repeated modifiers and lone keys at the persistence boundary. A lone
    /// modifier starts dictation by accident: ⌘-Tab and ⌃-arrow key presses never reach the
    /// monitor, and ⌥ / ⇧ are held while typing.
    pub fn parse(combo: &str) -> Option<Self> {
        let mut selected = 0u8;
        for token in combo.strip_prefix("Hold+")?.split('+') {
            let index = HOLD_MODIFIERS
                .iter()
                .position(|(name, ..)| *name == token)?;
            let bit = 1 << index;
            if selected & bit != 0 {
                return None;
            }
            selected |= bit;
        }
        (selected.count_ones() >= 2).then_some(Self(selected))
    }

    /// The default binding, [`VOICE_HOLD_BINDING`].
    pub const fn default_hold() -> Self {
        Self((1 << 2) | (1 << 3))
    }

    /// Left Option alone: the press machine's single-key reference, no longer a valid binding.
    pub const fn left_option() -> Self {
        Self(1 << 2)
    }

    /// Physical keys to poll if macOS misses a modifier release.
    pub fn key_codes(self) -> impl Iterator<Item = u16> {
        HOLD_MODIFIERS
            .into_iter()
            .enumerate()
            .filter_map(move |(index, (_, key, ..))| (self.0 & (1 << index) != 0).then_some(key))
    }

    /// The device-independent flags required throughout dictation.
    pub fn modifier_flags(self) -> usize {
        HOLD_MODIFIERS
            .iter()
            .enumerate()
            .filter(|(index, _)| self.0 & (1 << index) != 0)
            .fold(0, |flags, (_, (_, _, flag, ..))| flags | flag)
    }

    fn pressed(self, flags: usize) -> bool {
        HOLD_MODIFIERS
            .iter()
            .enumerate()
            .all(|(index, (_, _, flag, left, sides))| {
                self.0 & (1 << index) == 0
                    || (flags & flag != 0 && (flags & sides == 0 || flags & left != 0))
            })
    }

    /// The complete chord, with no extra modifier or opposite-side key joining it.
    pub fn matches(self, flags: usize) -> bool {
        self.pressed(flags)
            && flags & (FLAG_SHIFT | FLAG_CONTROL | FLAG_OPTION | FLAG_COMMAND | FLAG_FUNCTION)
                == self.modifier_flags()
            && HOLD_MODIFIERS
                .iter()
                .enumerate()
                .all(|(index, (_, _, _, left, sides))| {
                    flags & sides == 0 || (self.0 & (1 << index) != 0 && flags & sides == *left)
                })
    }

    /// Partial presses wait; releasing any member stops; an unrelated modifier poisons the hold.
    pub fn classify_flags_changed(self, key_code: u16, flags: usize) -> Input {
        if self.key_codes().any(|key| key == key_code) {
            Input::LeftOption {
                down: self.pressed(flags),
                alone: self.matches(flags),
            }
        } else {
            Input::OtherKey
        }
    }
}

/// Whether a stored voice binding names a supported modifier-only hold.
pub fn is_voice_hold_binding(combo: &str) -> bool {
    ModifierHoldBinding::parse(combo).is_some()
}

/// `combo` is the previous voice default, ⌃⌥V, in any modifier order or alias spelling.
pub fn is_legacy_voice_default(combo: &str) -> bool {
    let mut control = false;
    let mut option = false;
    let mut key_v = false;
    for part in combo.split('+') {
        match part {
            "Control" | "Ctrl" => control = true,
            "Alt" | "Option" => option = true,
            "KeyV" | "V" => key_v = true,
            _ => return false,
        }
    }
    control && option && key_v
}

/// The one-time upgrade of a saved voice binding to the hold-left-Option default. `saved` is the
/// binding in a shortcuts file written before that default existed (`None` when the file had no
/// voice entry). Only the old default (or no value) moves; a customized shortcut is kept.
pub fn upgraded_voice_binding(
    saved: Option<&str>,
    written_before_hold_default: bool,
) -> Option<&'static str> {
    if !written_before_hold_default {
        return None;
    }
    match saved {
        None => Some(VOICE_HOLD_BINDING),
        Some(combo) if is_legacy_voice_default(combo) => Some(VOICE_HOLD_BINDING),
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEFT_DOWN: usize = FLAG_OPTION | DEVICE_LEFT_OPTION;
    const ALONE: Input = Input::LeftOption {
        down: true,
        alone: true,
    };
    const UP: Input = Input::LeftOption {
        down: false,
        alone: true,
    };

    fn armed(hold: &mut LeftOptionHold, at: Instant) -> u64 {
        match hold.input(ALONE, at) {
            Effect::ArmTimer { press } => press,
            other => panic!("expected a timer, got {other:?}"),
        }
    }

    #[test]
    fn quick_press_and_release_does_nothing_and_never_opens_the_microphone() {
        let start = Instant::now();
        let mut hold = LeftOptionHold::new();
        let press = armed(&mut hold, start);
        assert_eq!(
            hold.input(UP, start + Duration::from_millis(120)),
            Effect::None
        );
        // The timer for that press fires after the release: still nothing.
        assert_eq!(
            hold.threshold_reached(press, start + HOLD_TO_TALK_THRESHOLD),
            Effect::None
        );
        assert!(!hold.dictating());
    }

    #[test]
    fn held_alone_past_threshold_starts_and_release_stops() {
        let start = Instant::now();
        let mut hold = LeftOptionHold::new();
        let press = armed(&mut hold, start);
        assert_eq!(
            hold.threshold_reached(press, start + HOLD_TO_TALK_THRESHOLD),
            Effect::StartDictation
        );
        assert!(hold.dictating());
        // A repeated flagsChanged down while dictating is not a second start.
        assert_eq!(
            hold.input(ALONE, start + Duration::from_secs(1)),
            Effect::None
        );
        assert_eq!(
            hold.input(UP, start + Duration::from_secs(2)),
            Effect::StopDictation
        );
        assert!(!hold.dictating());
    }

    #[test]
    fn early_timer_does_not_start() {
        let start = Instant::now();
        let mut hold = LeftOptionHold::new();
        let press = armed(&mut hold, start);
        assert_eq!(
            hold.threshold_reached(press, start + Duration::from_millis(299)),
            Effect::None
        );
        assert!(hold.armed());
    }

    #[test]
    fn another_key_before_threshold_poisons_the_press() {
        let start = Instant::now();
        let mut hold = LeftOptionHold::new();
        let press = armed(&mut hold, start);
        // ⌥e typed quickly.
        assert_eq!(
            hold.input(Input::OtherKey, start + Duration::from_millis(80)),
            Effect::None
        );
        assert_eq!(
            hold.threshold_reached(press, start + HOLD_TO_TALK_THRESHOLD),
            Effect::None
        );
        assert_eq!(hold.input(UP, start + Duration::from_secs(1)), Effect::None);
    }

    #[test]
    fn another_modifier_before_threshold_poisons_the_press() {
        let start = Instant::now();
        let mut hold = LeftOptionHold::new();
        let press = armed(&mut hold, start);
        // ⌘ joins: an ⌥⌘ shortcut.
        let command = classify_flags_changed(55, LEFT_DOWN | FLAG_COMMAND);
        assert_eq!(command, Input::OtherKey);
        assert_eq!(hold.input(command, start), Effect::None);
        assert_eq!(
            hold.threshold_reached(press, start + HOLD_TO_TALK_THRESHOLD),
            Effect::None
        );
    }

    #[test]
    fn pointer_before_threshold_poisons_the_press() {
        let start = Instant::now();
        let mut hold = LeftOptionHold::new();
        let press = armed(&mut hold, start);
        assert_eq!(
            hold.input(Input::Pointer, start + Duration::from_millis(50)),
            Effect::None
        );
        assert!(!hold.armed());
        assert_eq!(
            hold.threshold_reached(press, start + HOLD_TO_TALK_THRESHOLD),
            Effect::None
        );
    }

    #[test]
    fn another_key_after_start_discards_the_dictation() {
        let start = Instant::now();
        let mut hold = LeftOptionHold::new();
        let press = armed(&mut hold, start);
        hold.threshold_reached(press, start + HOLD_TO_TALK_THRESHOLD);
        assert_eq!(
            hold.input(Input::OtherKey, start + Duration::from_millis(400)),
            Effect::DiscardDictation
        );
        // The release after a discard does not deliver anything either.
        assert_eq!(hold.input(UP, start + Duration::from_secs(1)), Effect::None);
    }

    #[test]
    fn pointer_after_start_keeps_the_dictation() {
        let start = Instant::now();
        let mut hold = LeftOptionHold::new();
        let press = armed(&mut hold, start);
        hold.threshold_reached(press, start + HOLD_TO_TALK_THRESHOLD);
        assert_eq!(
            hold.input(Input::Pointer, start + Duration::from_millis(500)),
            Effect::None
        );
        assert!(hold.dictating());
    }

    #[test]
    fn a_press_with_another_modifier_already_down_never_arms() {
        let start = Instant::now();
        let mut hold = LeftOptionHold::new();
        let with_shift = classify_flags_changed(58, LEFT_DOWN | FLAG_SHIFT);
        assert_eq!(hold.input(with_shift, start), Effect::None);
        assert!(!hold.armed());
        // Released, the next clean press works again.
        assert_eq!(hold.input(UP, start), Effect::None);
        assert!(matches!(
            hold.input(ALONE, start),
            Effect::ArmTimer { press: 1 }
        ));
    }

    #[test]
    fn stale_timer_from_an_earlier_press_cannot_start_a_newer_one_early() {
        let start = Instant::now();
        let mut hold = LeftOptionHold::new();
        let first = armed(&mut hold, start);
        hold.input(UP, start + Duration::from_millis(100));
        let second = armed(&mut hold, start + Duration::from_millis(200));
        assert_ne!(first, second);
        assert_eq!(
            hold.threshold_reached(first, start + HOLD_TO_TALK_THRESHOLD),
            Effect::None
        );
        assert_eq!(
            hold.threshold_reached(second, start + Duration::from_millis(500)),
            Effect::StartDictation
        );
    }

    #[test]
    fn right_option_never_starts_dictation() {
        let start = Instant::now();
        let mut hold = LeftOptionHold::new();
        let right =
            classify_flags_changed(RIGHT_OPTION_KEY_CODE, FLAG_OPTION | DEVICE_RIGHT_OPTION);
        assert_eq!(right, Input::OtherKey);
        assert_eq!(hold.input(right, start), Effect::None);
        assert!(!hold.armed());
        // Right Option going down after left was pressed poisons the left press too (⌥⌥).
        let press = armed(&mut hold, start);
        hold.input(right, start + Duration::from_millis(50));
        assert_eq!(
            hold.threshold_reached(press, start + HOLD_TO_TALK_THRESHOLD),
            Effect::None
        );
    }

    #[test]
    fn left_option_with_right_option_already_down_is_not_alone() {
        let both = FLAG_OPTION | DEVICE_LEFT_OPTION | DEVICE_RIGHT_OPTION;
        assert_eq!(
            classify_flags_changed(LEFT_OPTION_KEY_CODE, both),
            Input::LeftOption {
                down: true,
                alone: false
            }
        );
    }

    #[test]
    fn left_release_is_read_from_the_device_bit_while_right_option_stays_down() {
        let down = |flags| {
            matches!(
                classify_flags_changed(LEFT_OPTION_KEY_CODE, flags),
                Input::LeftOption { down: true, .. }
            )
        };
        // Right Option still holds the shared Option flag.
        assert!(!down(FLAG_OPTION | DEVICE_RIGHT_OPTION));
        assert!(down(FLAG_OPTION | DEVICE_LEFT_OPTION));
        assert!(!down(0));
        // Synthetic events without device bits fall back to the shared flag.
        assert!(down(FLAG_OPTION));
    }

    #[test]
    fn reset_ends_a_running_dictation_and_forgets_a_waiting_press() {
        let start = Instant::now();
        let mut hold = LeftOptionHold::new();
        let press = armed(&mut hold, start);
        assert_eq!(hold.reset(), Effect::None);
        assert_eq!(
            hold.threshold_reached(press, start + HOLD_TO_TALK_THRESHOLD),
            Effect::None
        );
        let press = armed(&mut hold, start);
        hold.threshold_reached(press, start + HOLD_TO_TALK_THRESHOLD);
        assert_eq!(hold.reset(), Effect::StopDictation);
    }

    #[test]
    fn clicks_drags_scrolls_and_gestures_poison_but_plain_movement_does_not() {
        for bit in [
            1usize, 2, 3, 4, 6, 7, 18, 19, 20, 22, 23, 24, 25, 26, 27, 29, 30, 31, 32, 34, 37,
        ] {
            assert_ne!(POINTER_EVENT_MASK & (1usize << bit), 0, "missing bit {bit}");
        }
        // Plain movement (mouseMoved, mouseEntered, mouseExited) never cancels a hold.
        for bit in [5usize, 8, 9] {
            assert_eq!(
                POINTER_EVENT_MASK & (1usize << bit),
                0,
                "movement bit {bit}"
            );
        }
        // Key down / key up / flagsChanged are classified separately.
        for bit in [10usize, 11, 12] {
            assert_eq!(POINTER_EVENT_MASK & (1usize << bit), 0, "bit {bit}");
        }
    }

    #[test]
    fn custom_modifier_hold_arms_only_when_both_left_keys_are_down_and_stops_on_either_release() {
        let binding = ModifierHoldBinding::parse("Hold+LeftAlt+LeftSuper").expect("valid hold");
        let option = FLAG_OPTION | 0x20;
        let command = FLAG_COMMAND | 0x08;
        for (first_key, first_flags, second_key, release_flags) in
            [(58, option, 55, option), (55, command, 58, command)]
        {
            let now = Instant::now();
            let mut hold = LeftOptionHold::new();
            assert_eq!(
                hold.input(binding.classify_flags_changed(first_key, first_flags), now),
                Effect::None
            );
            let effect = hold.input(
                binding.classify_flags_changed(second_key, option | command),
                now,
            );
            let Effect::ArmTimer { press } = effect else {
                panic!("full hold must arm")
            };
            assert_eq!(
                hold.threshold_reached(press, now + HOLD_TO_TALK_THRESHOLD),
                Effect::StartDictation
            );
            assert_eq!(
                hold.input(
                    binding.classify_flags_changed(second_key, release_flags),
                    now
                ),
                Effect::StopDictation
            );
        }
    }

    #[test]
    fn custom_hold_rejects_wrong_sides_extras_and_invalid_tokens() {
        let binding = ModifierHoldBinding::parse("Hold+LeftAlt+LeftSuper").expect("valid hold");
        let flags = FLAG_OPTION | FLAG_COMMAND | 0x20 | 0x08;
        assert!(binding.matches(flags));
        assert!(!binding.matches((flags & !0x08) | 0x10));
        assert!(!binding.matches(flags | FLAG_SHIFT | 0x02));
        assert!(!binding.matches(flags | 0x40));
        assert_eq!(binding.key_codes().collect::<Vec<_>>(), [58, 55]);
        assert_eq!(
            ModifierHoldBinding::parse("Hold+LeftSuper+LeftAlt"),
            Some(binding)
        );
        for value in [
            "Hold+",
            "Hold+Alt",
            "Hold+RightAlt",
            "Hold+LeftAlt+LeftAlt",
            "Hold+LeftAlt+KeyV",
            "Hold+LeftAlt",
            "Hold+LeftSuper",
            "Hold+LeftControl",
            "Hold+LeftShift",
        ] {
            assert!(ModifierHoldBinding::parse(value).is_none(), "{value}");
        }
    }

    #[test]
    fn obsolete_custom_hold_timer_does_not_clear_a_newer_press() {
        let now = Instant::now();
        let mut hold = LeftOptionHold::new();
        let first = armed(&mut hold, now);
        hold.reset();
        let second = armed(&mut hold, now);
        assert_eq!(hold.abort_threshold(first), Effect::None);
        assert!(hold.armed());
        assert_eq!(
            hold.threshold_reached(second, now + HOLD_TO_TALK_THRESHOLD),
            Effect::StartDictation
        );
    }

    #[test]
    fn custom_hold_still_discards_typing_and_poisoned_presses_do_not_start() {
        let binding = ModifierHoldBinding::parse("Hold+LeftAlt+LeftSuper").expect("valid hold");
        let now = Instant::now();
        let full = binding.classify_flags_changed(55, FLAG_OPTION | FLAG_COMMAND | 0x28);
        let mut hold = LeftOptionHold::new();
        let Effect::ArmTimer { press } = hold.input(full, now) else {
            panic!("must arm")
        };
        hold.input(Input::OtherKey, now);
        assert_eq!(
            hold.threshold_reached(press, now + HOLD_TO_TALK_THRESHOLD),
            Effect::None
        );
        hold.reset();
        let Effect::ArmTimer { press } = hold.input(full, now) else {
            panic!("must arm")
        };
        hold.threshold_reached(press, now + HOLD_TO_TALK_THRESHOLD);
        assert_eq!(hold.input(Input::OtherKey, now), Effect::DiscardDictation);
    }

    #[test]
    fn hold_binding_parses_only_in_its_exact_form() {
        assert!(is_voice_hold_binding(VOICE_HOLD_BINDING));
        assert_eq!(
            ModifierHoldBinding::parse(VOICE_HOLD_BINDING),
            Some(ModifierHoldBinding::default_hold())
        );
        for other in [
            "Hold+LeftAlt",
            "Hold+Alt",
            "Tap+Alt",
            "Hold+RightAlt",
            "LeftAlt",
            "Control+Alt+KeyV",
        ] {
            assert!(!is_voice_hold_binding(other), "{other}");
        }
    }

    #[test]
    fn upgrade_moves_only_the_old_default_or_a_missing_value() {
        for old in [
            "Control+Alt+KeyV",
            "Alt+Control+KeyV",
            "Control+Option+KeyV",
            "Ctrl+Alt+V",
        ] {
            assert_eq!(
                upgraded_voice_binding(Some(old), true),
                Some(VOICE_HOLD_BINDING),
                "{old}"
            );
        }
        assert_eq!(upgraded_voice_binding(None, true), Some(VOICE_HOLD_BINDING));
        for custom in [
            "Control+Shift+KeyV",
            "Control+Alt+Shift+KeyV",
            "Control+Alt+KeyB",
            "Super+KeyV",
            "Control+Alt+Space",
        ] {
            assert_eq!(upgraded_voice_binding(Some(custom), true), None, "{custom}");
        }
        // A file written after the change keeps even an explicit ⌃⌥V: the user chose it again.
        assert_eq!(
            upgraded_voice_binding(Some("Control+Alt+KeyV"), false),
            None
        );
        assert_eq!(upgraded_voice_binding(None, false), None);
    }
}
