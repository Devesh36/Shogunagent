//! The three ways to say no (FR-MT-02), and the default that makes them meaningful (FR-MT-01).
//!
//! | tier | reached from | effect |
//! |---|---|---|
//! | (a) whole feature off | Settings → Meeting notes → Off | nothing is detected, nothing is listened to, the microphone is never touched |
//! | (b) not for this app | the Offered panel's secondary action / the exclusion list | that meeting app never offers again |
//! | (c) not this meeting | "Not now" / "Stop" | this one only; settings unchanged |
//!
//! The default is **on** (owner decision 2026-10-20, replacing FR-MT-01's off default). Tier (a)
//! stays the one switch that silences detection and listening; a saved `enabled: false` survives
//! every update.
//!
//! Deciding whether to offer is a pure function of these settings plus what was observed, so the
//! rule is testable on its own and cannot drift between the panel, the daemon and the settings
//! screen.

use std::collections::BTreeSet;

/// Which ASR engine transcribes meetings. Default is Deepgram (company-paid, 2026-08-05).
/// Whisper remains an offline/dev fallback (`SHOGUN_ASR_BACKEND=whisper` or settings).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AsrBackend {
    #[default]
    Deepgram,
    Whisper,
}

/// Which on-device ASR model to use when [`AsrBackend::Whisper`]. `Small` (bundled) or `Turbo`
/// (large-v3-turbo, opt-in, fetched on first use). Defaults to Small (§5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AsrModel {
    #[default]
    Small,
    Turbo,
}

/// How the in-meeting overlay presents speech while recording (issue #93).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MeetingMode {
    /// Live speech→text only; no translation.
    #[default]
    Transcription,
    /// Source (auto-detect or fixed) → target language subtitles.
    OneWay,
    /// Bidirectional: Me → other's language, Other → my language.
    TwoWay,
}

/// Meeting recognition language. Company speech uses Nova-3 Multilingual (#1101),
/// so new/missing settings use Auto without requiring a language selection.
/// Explicitly saved choices remain unchanged. The old English default addressed
/// Whisper's per-utterance detection; Whisper Auto now detects once per session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MeetingLanguage {
    English,
    Japanese,
    #[default]
    Auto,
}

impl MeetingLanguage {
    /// The whisper `set_language` code for a *fixed* language, or `None` for `Auto`. `None` is what
    /// the whisper backend reads as "detect" — for `Auto` that means detect-once-and-lock; for the
    /// fixed variants it hands whisper `"en"`/`"ja"` and never detects.
    pub fn whisper_code(self) -> Option<&'static str> {
        match self {
            MeetingLanguage::English => Some("en"),
            MeetingLanguage::Japanese => Some("ja"),
            MeetingLanguage::Auto => None,
        }
    }

    /// Deepgram `language` query value. Auto → multilingual Nova-3.
    pub fn deepgram_code(self) -> &'static str {
        match self {
            MeetingLanguage::English => "en",
            MeetingLanguage::Japanese => "ja",
            MeetingLanguage::Auto => "multi",
        }
    }
}

/// Which language the generated minutes and the live summary are written in
/// (docs/meeting-gap-analysis-2026-09.md B3; resolves design-doc §8-4).
///
/// `Auto` follows the meeting: a transcription-mode meeting in English gets English minutes, a
/// Japanese one Japanese, and a multilingual (two-way / auto-detect) meeting gets minutes in
/// whatever language most of the transcript is in. The fixed variants override that — the user
/// who takes notes in Japanese on English calls picks `Japanese` once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RecapLanguage {
    #[default]
    Auto,
    English,
    Japanese,
}

impl RecapLanguage {
    /// Parse the wire form (`auto` / `english` / `japanese`), the same strings serde uses.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "english" => Some(Self::English),
            "japanese" => Some(Self::Japanese),
            _ => None,
        }
    }

    /// The wire form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::English => "english",
            Self::Japanese => "japanese",
        }
    }
}

/// A scoped API request to show human retry controls for one live session.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SystemAudioRetryRequest {
    pub session_id: i64,
    pub generation: u64,
    pub expires_at_ms: i64,
}

/// Meeting-notes settings as persisted.
///
/// Every field defaults, so a file written before this feature existed — or a half-written one —
/// reads as the shipped default rather than failing to parse. The shipped default is on
/// (2026-10-20), so only a saved `enabled: false` keeps listening off.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Calendar reminder lead time: 0 (off), 1, 5, or 10 minutes; independent of capture.
    pub calendar_reminder_minutes: u8,
    /// Tier (a). `true` is the shipped default (2026-10-20).
    pub enabled: bool,
    /// First online meeting explanation accepted; this is not an OS permission grant.
    pub system_audio_explained: bool,
    /// Remember an explicit captions close across meetings and launches.
    pub captions_hidden: bool,
    /// Last call capture was unavailable; remind on the next online meeting.
    pub system_audio_needs_help: bool,
    /// A scoped API request to show human retry controls, never permission consent.
    pub system_audio_retry_request: Option<SystemAudioRetryRequest>,
    /// Input used for meeting capture. `None` follows the current macOS default. A selected
    /// device that disappears is rejected at capture time; SHOGUN never silently falls back.
    #[serde(default)]
    pub microphone: Option<String>,
    /// Tier (b): bundle ids (and meeting hosts) that never offer.
    pub excluded_apps: BTreeSet<String>,
    /// Tier (b), the calendar half: occurrence external ids that never offer — the recurring 1:1
    /// a user never wants noted (FR-MT-02).
    pub excluded_occurrences: BTreeSet<String>,
    /// Which ASR engine. Defaults to Deepgram (company-paid Nova-3).
    #[serde(default)]
    pub asr_backend: AsrBackend,
    /// Whisper model when `asr_backend == Whisper`. Defaults to Small.
    #[serde(default)]
    pub asr_model: AsrModel,
    /// Recognition language. New and missing settings use Auto (multilingual); explicit choices survive.
    #[serde(default)]
    pub language: MeetingLanguage,
    /// When `true` (shipped default, 2026-10-20), sustained microphone use alone can open an
    /// offer, for Discord-style calls in apps SHOGUN does not recognise (FR-MT-04). When `false`,
    /// a Meet URL, Zoom, or meeting controls must corroborate. No field-level serde default: a
    /// missing key takes the container default above.
    pub allow_mic_only_detect: bool,
    /// In-meeting overlay mode while recording (issue #93).
    #[serde(default)]
    pub meeting_mode: MeetingMode,
    /// One-way source language (auto-detect or fixed). Also drives ASR when not `Auto`.
    #[serde(default)]
    pub source_lang: MeetingLanguage,
    /// One-way target language (English or Japanese for v1).
    #[serde(default = "default_target_lang")]
    pub target_lang: MeetingLanguage,
    /// Two-way: the language *I* speak (and what Other's speech is translated into).
    #[serde(default)]
    pub my_lang: MeetingLanguage,
    /// Two-way: the language the *other* speaks (and what My speech is translated into).
    #[serde(default = "default_other_lang")]
    pub other_lang: MeetingLanguage,
    /// Which language the minutes / live summary are written in. Defaults to `Auto` (follow the
    /// meeting) — see [`RecapLanguage`].
    #[serde(default)]
    pub recap_language: RecapLanguage,
    /// The user's standing instruction for how minutes and the live canvas should be shaped
    /// (`"group next actions by team"`), at most [`crate::meeting::minutes::MAX_INSTRUCTION_CHARS`]
    /// characters. `None` / blank = the fixed rules only. Read through
    /// [`Settings::minutes_instruction`], which trims and drops a blank value.
    #[serde(default)]
    pub minutes_instruction: Option<String>,
}

fn default_target_lang() -> MeetingLanguage {
    MeetingLanguage::Japanese
}

fn default_other_lang() -> MeetingLanguage {
    MeetingLanguage::Japanese
}

// Written out rather than derived, though it is derivable. `#[derive(Default)]` would leave the
// most important property of this type — whether it ships listening — resting on the reader
// knowing what `bool` defaults to. It ships on (owner decision 2026-10-20), so it is stated.
// `language` is English here for the policy reason given on `MeetingLanguage`.
#[allow(clippy::derivable_impls)]
impl Default for Settings {
    fn default() -> Self {
        Self {
            calendar_reminder_minutes: 1,
            enabled: true,
            system_audio_explained: false,
            captions_hidden: false,
            system_audio_needs_help: false,
            system_audio_retry_request: None,
            microphone: None,
            excluded_apps: BTreeSet::new(),
            excluded_occurrences: BTreeSet::new(),
            asr_backend: AsrBackend::Deepgram,
            asr_model: AsrModel::Small,
            language: MeetingLanguage::Auto,
            allow_mic_only_detect: true,
            meeting_mode: MeetingMode::Transcription,
            source_lang: MeetingLanguage::Auto,
            target_lang: MeetingLanguage::Japanese,
            my_lang: MeetingLanguage::English,
            other_lang: MeetingLanguage::Japanese,
            recap_language: RecapLanguage::Auto,
            minutes_instruction: None,
        }
    }
}

impl Settings {
    /// The language the minutes are written in, as the prompt builder wants it: a code
    /// (`"en"` / `"ja"`), or [`crate::meeting::minutes::LANG_AUTO`] when the meeting itself is
    /// multilingual and no fixed choice was made.
    pub fn recap_language_code(&self) -> &'static str {
        match self.recap_language {
            RecapLanguage::English => "en",
            RecapLanguage::Japanese => "ja",
            RecapLanguage::Auto => match self.asr_language() {
                MeetingLanguage::English => "en",
                MeetingLanguage::Japanese => "ja",
                MeetingLanguage::Auto => crate::meeting::minutes::LANG_AUTO,
            },
        }
    }

    /// The custom minutes instruction, trimmed; `None` when unset, blank, or over the ceiling
    /// (an oversized value written around the setters is ignored rather than truncated into
    /// something the user never wrote).
    pub fn minutes_instruction(&self) -> Option<&str> {
        self.minutes_instruction
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .filter(|s| s.chars().count() <= crate::meeting::minutes::MAX_INSTRUCTION_CHARS)
    }
}

impl Settings {
    /// ASR language for the audio lane.
    ///
    /// - Transcription: selected `language` (multilingual by default).
    /// - One-way: `source_lang`, or `Auto` (Deepgram `multi`) when source is Auto — do **not**
    ///   fall back to English-only ASR or Japanese speech never reaches the translator.
    /// - Two-way: always `Auto` / multilingual so both sides can speak different languages.
    pub fn asr_language(&self) -> MeetingLanguage {
        match self.meeting_mode {
            MeetingMode::Transcription => self.language,
            MeetingMode::OneWay => {
                if self.source_lang == MeetingLanguage::Auto {
                    MeetingLanguage::Auto
                } else {
                    self.source_lang
                }
            }
            MeetingMode::TwoWay => MeetingLanguage::Auto,
        }
    }

    /// Target language for a finished line, if translation mode is active. `speaker_me` is true for
    /// mic (`Me`) and false for system tap (`Other`).
    pub fn translation_target(&self, speaker_me: bool) -> Option<MeetingLanguage> {
        match self.meeting_mode {
            MeetingMode::Transcription => None,
            MeetingMode::OneWay => {
                if self.target_lang == MeetingLanguage::Auto || self.source_lang == self.target_lang {
                    None
                } else {
                    Some(self.target_lang)
                }
            }
            MeetingMode::TwoWay => {
                if self.my_lang == self.other_lang {
                    return None;
                }
                if speaker_me {
                    Some(self.other_lang)
                } else {
                    Some(self.my_lang)
                }
            }
        }
    }

    /// Whose line this is, for the translation direction. Online the capture channel says (the
    /// microphone is the user). In a room (issue #262 FR-MC-01) the microphone carries everyone,
    /// so the channel says nothing and the language does: a line in the user's language is
    /// theirs and is translated outward; anything else — or undetectable — is the other side's
    /// and is translated into the user's language, which is what Two-way is for.
    pub fn line_is_mine(&self, in_person: bool, channel_me: bool, text: &str) -> bool {
        if !in_person {
            return channel_me;
        }
        match crate::translate::detect_language(text) {
            Some(crate::translate::Language::Japanese) => self.my_lang == MeetingLanguage::Japanese,
            Some(crate::translate::Language::English) => self.my_lang == MeetingLanguage::English,
            None => false,
        }
    }
}

#[cfg(test)]
mod in_person_direction_tests {
    use super::*;

    #[test]
    fn in_a_room_the_language_decides_the_direction_not_the_channel() {
        let s = Settings {
            meeting_mode: MeetingMode::TwoWay,
            my_lang: MeetingLanguage::Japanese,
            other_lang: MeetingLanguage::English,
            ..Settings::default()
        };
        // Online: the mic is me, whatever I said.
        assert!(s.line_is_mine(false, true, "Sounds good, see you Friday"));
        // In person: the same mic line in English is the other side's → translated into mine.
        assert!(!s.line_is_mine(true, true, "Sounds good, see you Friday"));
        assert_eq!(
            s.translation_target(s.line_is_mine(true, true, "Sounds good, see you Friday")),
            Some(MeetingLanguage::Japanese)
        );
        // …and a Japanese line is mine → translated outward.
        assert!(s.line_is_mine(true, true, "金曜日に確認します"));
        assert_eq!(
            s.translation_target(s.line_is_mine(true, true, "金曜日に確認します")),
            Some(MeetingLanguage::English)
        );
    }
}

/// What the detector saw, reduced to the identifiers the settings are expressed in.
#[derive(Debug, Clone, Copy, Default)]
pub struct OfferContext<'a> {
    pub app_bundle_id: Option<&'a str>,
    pub occurrence_external_id: Option<&'a str>,
}

impl Settings {
    /// Whether an offer may be shown at all. Consulted before detection acts on anything.
    ///
    /// Tier (a) is checked first and answers on its own: with the feature off, no exclusion list,
    /// calendar or observation can produce an offer. Tier (c) is not represented here — declining
    /// one meeting is deliberately not persisted (FR-MT-02c).
    pub fn may_offer(&self, ctx: &OfferContext<'_>) -> bool {
        if !self.enabled {
            return false;
        }
        if ctx.app_bundle_id.is_some_and(|id| self.excluded_apps.contains(id)) {
            return false;
        }
        if ctx
            .occurrence_external_id
            .is_some_and(|id| self.excluded_occurrences.contains(id))
        {
            return false;
        }
        true
    }

    /// Tier (b) from the Offered panel: never offer for this app again.
    pub fn exclude_app(&mut self, bundle_id: &str) {
        self.excluded_apps.insert(bundle_id.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_language_pairs_do_not_translate() {
        let mut settings = super::Settings {
            meeting_mode: super::MeetingMode::OneWay,
            source_lang: super::MeetingLanguage::Japanese,
            target_lang: super::MeetingLanguage::Japanese,
            ..Default::default()
        };
        assert_eq!(settings.translation_target(true), None);
        settings.meeting_mode = super::MeetingMode::TwoWay;
        settings.my_lang = settings.other_lang;
        assert_eq!(settings.translation_target(true), None);
        assert_eq!(settings.translation_target(false), None);
    }

    fn zoom() -> OfferContext<'static> {
        OfferContext { app_bundle_id: Some("us.zoom.xos"), occurrence_external_id: None }
    }

    #[test]
    fn the_feature_ships_switched_on() {
        // Owner decision 2026-10-20, asserted rather than trusted.
        assert!(Settings::default().enabled);
    }

    #[test]
    fn mic_only_detect_ships_on() {
        assert!(Settings::default().allow_mic_only_detect);
    }

    #[test]
    fn nothing_is_offered_while_the_feature_is_off() {
        let s = Settings { enabled: false, ..Default::default() };
        assert!(!s.may_offer(&zoom()));
    }

    #[test]
    fn an_enabled_feature_offers_for_an_ordinary_meeting() {
        let s = Settings { enabled: true, ..Default::default() };
        assert!(s.may_offer(&zoom()));
    }

    #[test]
    fn an_excluded_app_never_offers() {
        let mut s = Settings { enabled: true, ..Default::default() };
        s.exclude_app("us.zoom.xos");
        assert!(!s.may_offer(&zoom()));
    }

    #[test]
    fn excluding_one_app_leaves_the_others_alone() {
        let mut s = Settings { enabled: true, ..Default::default() };
        s.exclude_app("us.zoom.xos");

        let other = OfferContext { app_bundle_id: Some("com.google.Chrome"), ..Default::default() };
        assert!(s.may_offer(&other));
    }

    #[test]
    fn an_excluded_recurring_meeting_never_offers() {
        // The 1:1 a user does not want noted stays un-noted every week, without them having to
        // decline it every week (FR-MT-02).
        let mut s = Settings { enabled: true, ..Default::default() };
        s.excluded_occurrences.insert("evt-1on1".into());

        let ctx = OfferContext {
            app_bundle_id: Some("us.zoom.xos"),
            occurrence_external_id: Some("evt-1on1"),
        };
        assert!(!s.may_offer(&ctx));
    }

    #[test]
    fn the_off_switch_outranks_every_other_setting() {
        // Tier (a) is absolute: an empty exclusion list must not make an off feature offer.
        let s = Settings { enabled: false, ..Default::default() };
        assert!(!s.may_offer(&OfferContext::default()));
        assert!(!s.may_offer(&zoom()));
    }

    #[test]
    fn excluding_the_same_app_twice_is_harmless() {
        let mut s = Settings { enabled: true, ..Default::default() };
        s.exclude_app("us.zoom.xos");
        s.exclude_app("us.zoom.xos");
        assert_eq!(s.excluded_apps.len(), 1);
    }

    #[test]
    fn settings_survive_a_save_and_load_round_trip() {
        let mut s = Settings { enabled: true, ..Default::default() };
        s.exclude_app("us.zoom.xos");
        s.excluded_occurrences.insert("evt-1on1".into());

        let restored: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();

        assert_eq!(restored, s);
    }

    #[test]
    fn a_settings_file_that_predates_this_feature_reads_as_the_shipped_default() {
        // An install from before meeting notes existed has no such key: it takes the shipped
        // default (on, 2026-10-20), including mic-only detection.
        let restored: Settings = serde_json::from_str("{}").unwrap();

        assert!(restored.enabled);
        assert!(restored.allow_mic_only_detect);
        assert!(restored.excluded_apps.is_empty());
        assert_eq!(restored.microphone, None);
    }

    #[test]
    fn a_saved_off_switch_survives_a_reload() {
        let restored: Settings =
            serde_json::from_str(r#"{"enabled":false,"allow_mic_only_detect":false,"excluded_apps":["us.zoom.xos"]}"#)
                .unwrap();

        assert!(!restored.enabled);
        assert!(!restored.allow_mic_only_detect);
        assert_eq!(restored.excluded_apps.len(), 1);
    }

    #[test]
    fn new_meetings_use_multilingual_speech_without_configuration() {
        assert_eq!(Settings::default().asr_language(), MeetingLanguage::Auto);
        assert_eq!(Settings::default().asr_language().deepgram_code(), "multi");
    }

    #[test]
    fn asr_backend_defaults_to_deepgram() {
        assert_eq!(AsrBackend::default(), AsrBackend::Deepgram);
        assert_eq!(Settings::default().asr_backend, AsrBackend::Deepgram);
    }

    #[test]
    fn asr_model_defaults_to_small() {
        assert_eq!(AsrModel::default(), AsrModel::Small);
    }

    #[test]
    fn asr_model_round_trips_json() {
        let json = serde_json::to_string(&AsrModel::Turbo).unwrap();
        assert_eq!(json, "\"turbo\"");
        let back: AsrModel = serde_json::from_str(&json).unwrap();
        assert_eq!(back, AsrModel::Turbo);
    }

    #[test]
    fn missing_language_uses_multilingual_but_saved_choices_survive() {
        let restored: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(restored.language, MeetingLanguage::Auto);
        let saved: Settings = serde_json::from_str(r#"{"language":"english"}"#).unwrap();
        assert_eq!(saved.language, MeetingLanguage::English);
        let japanese: Settings = serde_json::from_str(r#"{"language":"japanese"}"#).unwrap();
        assert_eq!(japanese.asr_language().deepgram_code(), "ja");
        assert_eq!(restored.asr_language().deepgram_code(), "multi");
    }

    #[test]
    fn language_round_trips_json() {
        // The wire form is lowercase, matching AsrModel's style, and every variant survives a trip.
        assert_eq!(serde_json::to_string(&MeetingLanguage::English).unwrap(), "\"english\"");
        assert_eq!(serde_json::to_string(&MeetingLanguage::Auto).unwrap(), "\"auto\"");
        let english: MeetingLanguage = serde_json::from_str("\"english\"").unwrap();
        assert_eq!(english, MeetingLanguage::English);
        let auto: MeetingLanguage = serde_json::from_str("\"auto\"").unwrap();
        assert_eq!(auto, MeetingLanguage::Auto);
        let japanese: MeetingLanguage = serde_json::from_str("\"japanese\"").unwrap();
        assert_eq!(japanese, MeetingLanguage::Japanese);
    }

    #[test]
    fn language_maps_to_the_right_whisper_code() {
        // The fixed languages pin whisper to a code; Auto yields None (detect-then-lock downstream).
        assert_eq!(MeetingLanguage::English.whisper_code(), Some("en"));
        assert_eq!(MeetingLanguage::Japanese.whisper_code(), Some("ja"));
        assert_eq!(MeetingLanguage::Auto.whisper_code(), None);
    }

    #[test]
    fn recap_language_ships_as_auto_and_reads_as_auto_from_an_older_file() {
        assert_eq!(Settings::default().recap_language, RecapLanguage::Auto);
        let restored: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(restored.recap_language, RecapLanguage::Auto);
        assert_eq!(restored.minutes_instruction, None);
        assert_eq!(serde_json::to_string(&RecapLanguage::Japanese).unwrap(), "\"japanese\"");
        assert_eq!(RecapLanguage::parse(" English "), Some(RecapLanguage::English));
        assert_eq!(RecapLanguage::parse("klingon"), None);
    }

    #[test]
    fn auto_recap_language_follows_the_meeting_and_a_fixed_one_overrides_it() {
        // Transcription in English → English minutes; in Japanese → Japanese minutes.
        assert_eq!(Settings::default().recap_language_code(), crate::meeting::minutes::LANG_AUTO);
        let en = Settings { language: MeetingLanguage::English, ..Default::default() };
        assert_eq!(en.recap_language_code(), "en");
        let ja = Settings { language: MeetingLanguage::Japanese, ..Default::default() };
        assert_eq!(ja.recap_language_code(), "ja");
        // A multilingual meeting (two-way) has no single language: ask for the transcript's.
        let two_way = Settings { meeting_mode: MeetingMode::TwoWay, ..Default::default() };
        assert_eq!(two_way.recap_language_code(), crate::meeting::minutes::LANG_AUTO);
        // The fixed choice wins over the meeting language (§8-4: Japanese notes on English calls).
        let fixed = Settings { recap_language: RecapLanguage::Japanese, ..Default::default() };
        assert_eq!(fixed.recap_language_code(), "ja");
    }

    #[test]
    fn the_minutes_instruction_is_trimmed_and_an_oversized_one_is_ignored() {
        let s = Settings { minutes_instruction: Some("  Group by team  ".into()), ..Default::default() };
        assert_eq!(s.minutes_instruction(), Some("Group by team"));
        let blank = Settings { minutes_instruction: Some("   ".into()), ..Default::default() };
        assert_eq!(blank.minutes_instruction(), None);
        let huge = Settings {
            minutes_instruction: Some("x".repeat(crate::meeting::minutes::MAX_INSTRUCTION_CHARS + 1)),
            ..Default::default()
        };
        assert_eq!(huge.minutes_instruction(), None);
    }

    #[test]
    fn asr_language_transcription_uses_meeting_language() {
        let s = Settings::default();
        assert_eq!(s.meeting_mode, MeetingMode::Transcription);
        assert_eq!(s.asr_language(), MeetingLanguage::Auto);
        // Preserve an explicitly saved language choice during upgrades.
        let fixed = Settings { language: MeetingLanguage::English, ..s };
        assert_eq!(fixed.asr_language(), MeetingLanguage::English);
    }

    #[test]
    fn asr_language_one_way_auto_source_is_multilingual() {
        // Must not fall back to English-only ASR — Japanese speech would never reach translate.
        let s = Settings {
            meeting_mode: MeetingMode::OneWay,
            source_lang: MeetingLanguage::Auto,
            ..Default::default()
        };
        assert_eq!(s.asr_language(), MeetingLanguage::Auto);
    }

    #[test]
    fn asr_language_one_way_fixed_source() {
        let s = Settings {
            meeting_mode: MeetingMode::OneWay,
            source_lang: MeetingLanguage::Japanese,
            ..Default::default()
        };
        assert_eq!(s.asr_language(), MeetingLanguage::Japanese);
    }

    #[test]
    fn asr_language_two_way_is_multilingual() {
        let s = Settings {
            meeting_mode: MeetingMode::TwoWay,
            ..Default::default()
        };
        assert_eq!(s.asr_language(), MeetingLanguage::Auto);
    }
}
