//! Readback acknowledgement for editors that normalize pasted text.

/// Normalize editor whitespace and the zero-width artifacts seen in web readback.
/// This is for post-write acknowledgement, never permission to modify a field.
pub fn normalize_editor_text(text: &str) -> String {
    let without_artifacts: String = text
        .chars()
        .filter(|character| !matches!(character, '\u{200b}' | '\u{feff}'))
        .collect();
    without_artifacts
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Prepare expected readback once, before polling an editor.
pub struct PasteVerification {
    before: Option<String>,
    before_words: Option<String>,
    expected: String,
    alternative: Option<String>,
    expected_changed: bool,
}

impl PasteVerification {
    /// Prepare a replacement while retaining inline rewriting's formatting-only changes.
    pub fn new(before: Option<&str>, expected: &str) -> Self {
        Self {
            before: before.map(str::to_owned),
            before_words: None,
            // Raw emptiness: a whitespace-only replacement is still a real inline edit.
            expected_changed: !expected.is_empty(),
            expected: normalize_editor_text(expected),
            alternative: None,
        }
    }

    /// Dictation must add meaningful text; formatting alone cannot acknowledge speech.
    pub fn requiring_word_change(mut self) -> Self {
        self.before_words = self.before.as_deref().map(normalize_editor_text);
        self.expected_changed &= !self.expected.is_empty()
            && self.before_words.as_deref() != Some(self.expected.as_str());
        self
    }

    /// Permit one specific alternate result, such as an editor dropping the inserted leading
    /// space. This does not remove word boundaries from the rest of the document.
    pub fn with_alternative(mut self, alternative: &str) -> Self {
        self.alternative = Some(normalize_editor_text(alternative));
        self
    }

    /// Missing, unchanged, partial, duplicated, or unrelated text cannot acknowledge delivery.
    pub fn matches(&self, now: Option<&str>) -> bool {
        let Some(now) = now else {
            return false;
        };
        if !self.expected_changed || self.before.as_deref() == Some(now) {
            return false;
        }
        let now = normalize_editor_text(now);
        self.before_words.as_deref() != Some(now.as_str())
            && (now == self.expected || self.alternative.as_deref() == Some(now.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting_alone_is_not_dictation() {
        let verification =
            PasteVerification::new(Some("existing text"), "existing text ").requiring_word_change();
        for observed in [
            "existing text\n",
            "existing\u{a0}text",
            "existing\u{200b} text",
        ] {
            assert!(!verification.matches(Some(observed)));
        }
        assert!(!PasteVerification::new(Some(""), " \n")
            .requiring_word_change()
            .matches(Some("\n")));
        assert!(PasteVerification::new(Some("hello"), "hello\n").matches(Some("hello\n")));
        // Inline rewrites may legitimately replace text with whitespace only.
        assert!(PasteVerification::new(Some("x"), "\n").matches(Some(" ")));
    }

    #[test]
    fn accepts_editor_whitespace_and_zero_width_artifacts() {
        let verification = PasteVerification::new(Some("before after"), "before hello world after");
        assert!(verification.matches(Some("before\u{a0}hello\u{200b} world\u{feff} after\n")));
    }

    #[test]
    fn dropped_space_is_allowed_only_at_the_specific_insertion_boundary() {
        let verification =
            PasteVerification::new(Some("text"), "text more").with_alternative("textmore");
        assert!(verification.matches(Some("textmore\u{feff}")));
        let verification = PasteVerification::new(Some("two words"), "two words more")
            .with_alternative("two wordsmore");
        assert!(!verification.matches(Some("twowordsmore")));
    }

    #[test]
    fn rejects_missing_partial_duplicate_and_unrelated_content() {
        let verification = PasteVerification::new(Some("before after"), "before hello world after");
        for observed in [
            "before after",
            "before hello after",
            "before hello worldhello world after",
            "before other words after",
            "different hello world after",
        ] {
            assert!(!verification.matches(Some(observed)));
        }
        assert!(!verification.matches(None));
    }
}
