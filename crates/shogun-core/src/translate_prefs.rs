//! Translation preferences (issue #279): the register a user wants, remembered **per app**
//! (FR-TL-07), so Slack stays casual and mail stays formal after one choice each — and the
//! language pair (FR-TL-03), which side of it the user reads.
//!
//! Pure: a small document the shell keeps in `translate.json` (app data, not a secret). The
//! shell supplies the frontmost app's bundle id; this module answers which register applies and
//! rewrites the document when the user changes their mind. Unknown apps fall back to the
//! default, and the default falls back to matching the source's own register — the only choice
//! that is never wrong.

use std::collections::BTreeMap;

use crate::translate::{Formality, Language, LanguagePair};

/// The wire / file spelling of a register.
pub fn formality_tag(f: Formality) -> &'static str {
    match f {
        Formality::Match => "match",
        Formality::Casual => "casual",
        Formality::Polite => "polite",
        Formality::Formal => "formal",
    }
}

pub fn formality_from_tag(tag: &str) -> Option<Formality> {
    match tag.trim().to_ascii_lowercase().as_str() {
        "match" => Some(Formality::Match),
        "casual" => Some(Formality::Casual),
        "polite" => Some(Formality::Polite),
        "formal" => Some(Formality::Formal),
        _ => None,
    }
}

/// The document. Bundle ids are the keys; a per-app entry wins over the default.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct TranslatePrefs {
    /// The register when the app has no entry of its own. `match` by default.
    #[serde(default = "default_tag")]
    pub default_formality: String,
    #[serde(default)]
    pub per_app: BTreeMap<String, String>,
    /// The language pair the user chose (FR-TL-03). Absent = follow the system language. Kept as
    /// tags so a file from a later build with a language this one lacks degrades to "follow the
    /// system", never to a panic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pair: Option<StoredPair>,
}

/// The pair as `translate.json` spells it: `{"mine": "ja", "theirs": "en"}`. `mine` is the
/// language the user reads (where back-translations land); `theirs` is the other side.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredPair {
    pub mine: String,
    pub theirs: String,
}

/// Why a pair was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairError {
    /// A tag outside the languages there are prompts for (`en`, `ja`).
    UnknownLanguage,
    /// Both sides the same: nothing would ever be translated.
    SameLanguage,
}

impl PairError {
    pub fn code(self) -> &'static str {
        match self {
            PairError::UnknownLanguage => "unknown_language",
            PairError::SameLanguage => "same_language",
        }
    }
}

/// Parse a pair from two tags. `theirs` may be omitted when there is only one other language.
pub fn pair_from_tags(mine: &str, theirs: Option<&str>) -> Result<LanguagePair, PairError> {
    let mine = Language::from_tag(mine.trim()).ok_or(PairError::UnknownLanguage)?;
    let theirs = match theirs.map(str::trim).filter(|t| !t.is_empty()) {
        Some(tag) => Language::from_tag(tag).ok_or(PairError::UnknownLanguage)?,
        // Two languages today: the other side is the one that is not `mine`.
        None => LanguagePair::from_locale(mine.tag()).theirs,
    };
    if mine == theirs {
        return Err(PairError::SameLanguage);
    }
    Ok(LanguagePair { mine, theirs })
}

fn default_tag() -> String {
    "match".to_string()
}

impl TranslatePrefs {
    /// Parse the stored document; anything unreadable is the empty document (a bad file must
    /// never take translation down — it just forgets the preference).
    pub fn from_json(text: &str) -> TranslatePrefs {
        serde_json::from_str(text).unwrap_or_default()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// The register for `app` (a bundle id, or `None` when the frontmost app is unknown).
    pub fn formality_for(&self, app: Option<&str>) -> Formality {
        app.and_then(|a| self.per_app.get(a))
            .and_then(|t| formality_from_tag(t))
            .or_else(|| formality_from_tag(&self.default_formality))
            .unwrap_or_default()
    }

    /// The pair the user chose, when they chose one and it is still a valid pair.
    pub fn stored_pair(&self) -> Option<LanguagePair> {
        let stored = self.pair.as_ref()?;
        pair_from_tags(&stored.mine, Some(&stored.theirs)).ok()
    }

    /// The pair in force: the stored choice, else `system` (the pair derived from the system
    /// language, which only the desktop can read).
    pub fn pair_or(&self, system: LanguagePair) -> LanguagePair {
        self.stored_pair().unwrap_or(system)
    }

    /// Remember a pair, or with `None` go back to following the system language.
    pub fn set_pair(&mut self, pair: Option<LanguagePair>) {
        self.pair = pair.map(|p| StoredPair {
            mine: p.mine.tag().to_string(),
            theirs: p.theirs.tag().to_string(),
        });
    }

    /// Read `translate.json` at `path`; a missing or unreadable file is the empty document.
    pub fn load(path: &std::path::Path) -> TranslatePrefs {
        std::fs::read_to_string(path)
            .map(|text| TranslatePrefs::from_json(&text))
            .unwrap_or_default()
    }

    /// Whether `app` has its own entry (the UI says "remembered for Mail" only when it is).
    pub fn has_entry(&self, app: Option<&str>) -> bool {
        app.is_some_and(|a| self.per_app.contains_key(a))
    }

    /// Remember `formality` for `app`; with no app, change the default. Choosing `match` for an
    /// app removes its entry rather than storing a no-op, so the file stays a list of real
    /// choices.
    pub fn remember(&mut self, app: Option<&str>, formality: Formality) {
        match app {
            Some(a) if !a.trim().is_empty() => {
                if formality == Formality::Match {
                    self.per_app.remove(a);
                } else {
                    self.per_app.insert(a.to_string(), formality_tag(formality).to_string());
                }
            }
            _ => self.default_formality = formality_tag(formality).to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_round_trip_and_reject_garbage() {
        for f in [Formality::Match, Formality::Casual, Formality::Polite, Formality::Formal] {
            assert_eq!(formality_from_tag(formality_tag(f)), Some(f));
        }
        assert_eq!(formality_from_tag(" Formal "), Some(Formality::Formal));
        assert_eq!(formality_from_tag("keigo"), None);
    }

    #[test]
    fn an_app_entry_wins_over_the_default_which_wins_over_match() {
        let mut prefs = TranslatePrefs::default();
        assert_eq!(prefs.formality_for(Some("com.apple.mail")), Formality::Match);
        prefs.remember(None, Formality::Polite);
        assert_eq!(prefs.formality_for(Some("com.apple.mail")), Formality::Polite);
        assert_eq!(prefs.formality_for(None), Formality::Polite);
        prefs.remember(Some("com.apple.mail"), Formality::Formal);
        prefs.remember(Some("com.tinyspeck.slackmacgap"), Formality::Casual);
        assert_eq!(prefs.formality_for(Some("com.apple.mail")), Formality::Formal);
        assert_eq!(prefs.formality_for(Some("com.tinyspeck.slackmacgap")), Formality::Casual);
        assert_eq!(prefs.formality_for(Some("com.other")), Formality::Polite);
        assert!(prefs.has_entry(Some("com.apple.mail")));
        assert!(!prefs.has_entry(Some("com.other")));
    }

    #[test]
    fn choosing_match_for_an_app_forgets_its_entry() {
        let mut prefs = TranslatePrefs::default();
        prefs.remember(Some("com.apple.mail"), Formality::Formal);
        prefs.remember(Some("com.apple.mail"), Formality::Match);
        assert!(!prefs.has_entry(Some("com.apple.mail")));
        prefs.remember(Some("  "), Formality::Casual);
        assert_eq!(prefs.default_formality, "casual", "a blank app means the default");
    }

    #[test]
    fn the_pair_is_absent_until_chosen_then_wins_over_the_system_language() {
        let system = LanguagePair::from_locale("en_US");
        let mut prefs = TranslatePrefs::default();
        assert_eq!(prefs.stored_pair(), None);
        assert_eq!(prefs.pair_or(system), system);
        assert!(!prefs.to_json().contains("pair"), "an unchosen pair is not written");

        let chosen = pair_from_tags("ja", None).unwrap();
        assert_eq!(chosen, LanguagePair { mine: Language::Japanese, theirs: Language::English });
        prefs.set_pair(Some(chosen));
        assert_eq!(prefs.pair_or(system), chosen);
        let text = prefs.to_json();
        assert!(text.contains(r#""mine": "ja""#), "{text}");
        assert_eq!(TranslatePrefs::from_json(&text).stored_pair(), Some(chosen));

        prefs.set_pair(None);
        assert_eq!(prefs.pair_or(system), system, "reset follows the system again");
    }

    #[test]
    fn a_bad_pair_is_refused_on_the_way_in_and_ignored_on_the_way_out() {
        assert_eq!(pair_from_tags("ja", Some("ja")), Err(PairError::SameLanguage));
        assert_eq!(pair_from_tags("fr", None), Err(PairError::UnknownLanguage));
        assert_eq!(pair_from_tags("en", Some("klingon")), Err(PairError::UnknownLanguage));
        assert_eq!(pair_from_tags(" EN-us ", Some("ja_JP")).unwrap().mine, Language::English);
        // A file edited by hand (or written by a later build) never breaks translation.
        let odd = r#"{"pair":{"mine":"fr","theirs":"en"}}"#;
        let system = LanguagePair::default();
        assert_eq!(TranslatePrefs::from_json(odd).pair_or(system), system);
    }

    #[test]
    fn the_register_and_the_pair_survive_each_others_writes() {
        // The desktop rewrites the whole document from this struct: the pair must survive a
        // register change and the register a pair change.
        let mut prefs = TranslatePrefs::default();
        prefs.set_pair(Some(pair_from_tags("en", None).unwrap()));
        let mut reread = TranslatePrefs::from_json(&prefs.to_json());
        reread.remember(Some("com.apple.mail"), Formality::Formal);
        let again = TranslatePrefs::from_json(&reread.to_json());
        assert_eq!(again.stored_pair().map(|p| p.mine), Some(Language::English));
        assert_eq!(again.formality_for(Some("com.apple.mail")), Formality::Formal);
    }

    #[test]
    fn the_document_survives_a_round_trip_and_shrugs_at_a_bad_file() {
        let mut prefs = TranslatePrefs::default();
        prefs.remember(Some("com.apple.mail"), Formality::Formal);
        let text = prefs.to_json();
        assert_eq!(TranslatePrefs::from_json(&text), prefs);
        assert_eq!(TranslatePrefs::from_json("not json"), TranslatePrefs::default());
        // Unknown tags in the file degrade to match, never to a panic.
        let odd = r#"{"default_formality":"weird","per_app":{"a":"stranger"}}"#;
        assert_eq!(TranslatePrefs::from_json(odd).formality_for(Some("a")), Formality::Match);
    }
}
