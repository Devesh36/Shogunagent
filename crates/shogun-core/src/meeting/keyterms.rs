//! The dictionary hints a meeting's ASR lane sends (docs/meeting-gap-analysis-2026-09.md C1).
//!
//! The dictation lane already hands the user's vocabulary to the ASR provider as keyterm hints,
//! bounded and only with the user's explicit consent; the meeting lane had the same dictionary
//! and the same `keyterms` plumbing but never connected them, so a company name that dictation
//! spells right was still misheard in every meeting. This is the selection rule, pure and
//! Linux-tested; the desktop's `meeting_config()` hands the result to `with_keyterms`.
//!
//! Consent is the same switch dictation uses (`allow_personal_dictionary_keyterms`): without it,
//! only the built-in product names go out — the user's terms stay local (invariant 3 / 7).

use crate::meeting::settings::MeetingLanguage;
use crate::voice_dictionary::{DictionaryContext, VoiceDictionary};

/// The `surface` scope value a term can be limited to so it applies in meetings only.
pub const MEETING_SURFACE: &str = "meeting";

/// The keyterm hints for one meeting: the dictionary's eligible canonical terms for the meeting
/// app (bundle scope), the meeting surface, and the meeting's language (a locale-tagged term
/// applies only when the meeting is transcribed in that language; `Auto` matches every locale
/// tag by carrying none). Bounded by the dictionary's own hint cap.
///
/// `allow_personal` false → built-ins only, exactly as dictation behaves before consent.
pub fn meeting_keyterms(
    dictionary: &VoiceDictionary,
    app_bundle_id: Option<&str>,
    language: MeetingLanguage,
    allow_personal: bool,
) -> Vec<String> {
    let context = DictionaryContext {
        locale: match language {
            MeetingLanguage::English => Some("en".to_string()),
            MeetingLanguage::Japanese => Some("ja".to_string()),
            MeetingLanguage::Auto => None,
        },
        bundle_id: app_bundle_id.map(str::to_string),
        surface: Some(MEETING_SURFACE.to_string()),
    };
    if allow_personal {
        dictionary.keyterms_for(&context)
    } else {
        VoiceDictionary::with_defaults().keyterms_for(&context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice_dictionary::TermScope;

    fn dictionary() -> VoiceDictionary {
        VoiceDictionary::with_user_entries(vec![
            VoiceDictionary::user_entry(
                "Select KK".into(),
                vec![],
                None,
                TermScope::Global,
                0,
                true,
            ),
            VoiceDictionary::user_entry(
                "Kamon".into(),
                vec![],
                None,
                TermScope::Surface(MEETING_SURFACE.into()),
                0,
                true,
            ),
            VoiceDictionary::user_entry(
                "Zoom-only term".into(),
                vec![],
                None,
                TermScope::Bundle("us.zoom.xos".into()),
                0,
                true,
            ),
            VoiceDictionary::user_entry(
                "田中".into(),
                vec![],
                Some("ja".into()),
                TermScope::Global,
                0,
                true,
            ),
            VoiceDictionary::user_entry(
                "Switched off".into(),
                vec![],
                None,
                TermScope::Global,
                0,
                false,
            ),
        ])
    }

    #[test]
    fn without_consent_only_built_ins_leave_the_device() {
        let hints = meeting_keyterms(
            &dictionary(),
            Some("us.zoom.xos"),
            MeetingLanguage::English,
            false,
        );
        assert!(
            hints.iter().any(|h| h == "ShogunAI"),
            "built-ins stay as speech hints"
        );
        assert!(
            !hints.iter().any(|h| h == "Select KK"),
            "a personal term must not go out"
        );
        assert!(!hints.iter().any(|h| h == "Kamon"));
    }

    #[test]
    fn with_consent_global_meeting_surface_and_this_apps_terms_are_sent() {
        let hints = meeting_keyterms(
            &dictionary(),
            Some("us.zoom.xos"),
            MeetingLanguage::English,
            true,
        );
        assert!(hints.contains(&"Select KK".to_string()));
        assert!(
            hints.contains(&"Kamon".to_string()),
            "surface = meeting applies in meetings"
        );
        assert!(hints.contains(&"Zoom-only term".to_string()));
        assert!(
            !hints.contains(&"Switched off".to_string()),
            "disabled terms never go out"
        );
        // A term scoped to another app does not ride along.
        let other_app = meeting_keyterms(
            &dictionary(),
            Some("com.google.Chrome"),
            MeetingLanguage::English,
            true,
        );
        assert!(!other_app.contains(&"Zoom-only term".to_string()));
    }

    #[test]
    fn a_locale_tagged_term_follows_the_meeting_language() {
        let en = meeting_keyterms(&dictionary(), None, MeetingLanguage::English, true);
        assert!(
            !en.contains(&"田中".to_string()),
            "a ja-only term is not an English hint"
        );
        let ja = meeting_keyterms(&dictionary(), None, MeetingLanguage::Japanese, true);
        assert!(ja.contains(&"田中".to_string()));
        // Auto (multilingual) carries no locale, so the locale-tagged term is left out rather
        // than guessed — the same rule dictation applies to an unknown locale.
        let auto = meeting_keyterms(&dictionary(), None, MeetingLanguage::Auto, true);
        assert!(!auto.contains(&"田中".to_string()));
        assert!(auto.contains(&"Select KK".to_string()));
    }
}
