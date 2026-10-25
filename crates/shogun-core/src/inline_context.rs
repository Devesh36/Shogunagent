//! Ephemeral AX handoffs and local relevance selection for inline writing.
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

/// One policy-admitted Accessibility snapshot. Never populated from OCR or disk retrieval.
#[derive(Clone)]
pub struct InlineSource {
    pub app: String,
    pub title: String,
    /// Local policy metadata; never rendered into the model prompt.
    pub website_host: Option<String>,
    pub text: String,
    pub observed_at: Instant,
}

impl InlineSource {
    pub fn is_recent_at(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.observed_at) < Duration::from_secs(120)
    }

    pub fn render(&self) -> String {
        format!("Window: {}\n{}", self.title, self.text)
    }
}

/// The latest AX snapshot for each of the two most recently observed apps.
#[derive(Default)]
pub struct InlineSourceCache {
    entries: Vec<InlineSource>,
}

impl InlineSourceCache {
    pub fn observe(&mut self, mut source: InlineSource) {
        source.text = source.text.chars().take(32_768).collect();
        source.title = source.title.chars().take(256).collect();
        self.entries.retain(|entry| entry.app != source.app);
        self.entries.insert(0, source);
        self.entries.truncate(2);
    }

    pub fn refresh_seen(&mut self, app: &str, at: Instant) -> bool {
        let Some(source) = self.entries.iter_mut().find(|source| source.app == app) else {
            return false;
        };
        source.observed_at = at;
        true
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn retain_allowed(&mut self, allowed: impl Fn(&InlineSource) -> bool) {
        self.entries.retain(allowed);
    }

    pub fn recent_for(&self, app: &str, now: Instant) -> Option<InlineSource> {
        self.entries
            .iter()
            .find(|source| source.app != app && source.is_recent_at(now))
            .cloned()
    }
}

/// Normalize AX whitespace while retaining each normalized byte's original position.
fn normalized_offsets(text: &str) -> (String, Vec<usize>) {
    let mut normalized = String::new();
    let mut offsets = Vec::new();
    let mut space = false;
    for (byte, ch) in text.char_indices() {
        if ch.is_whitespace() {
            if !space {
                normalized.push(' ');
                offsets.push(byte);
            }
            space = true;
        } else {
            normalized.push(ch);
            offsets.extend(byte..byte + ch.len_utf8());
            space = false;
        }
    }
    offsets.push(text.len());
    (normalized, offsets)
}

/// Remove substantive draft lines before clipping, including adjacent AX nodes without separators.
/// Short replies are ambiguous and remain in the conversation unchanged.
pub fn visible_text_near(text: &str, draft: &str) -> String {
    const MAX_CHARS: usize = 6_000;
    let (normalized, offsets) = normalized_offsets(text);
    let mut ranges = Vec::new();
    let mut anchor = None;
    for line in draft
        .lines()
        .filter(|line| line.trim().chars().count() >= 12)
    {
        let (needle, _) = normalized_offsets(line.trim());
        for (at, _) in normalized.match_indices(&needle) {
            let start = offsets[at];
            let end = offsets[at + needle.len()];
            ranges.push((start, end));
            anchor = Some(start);
        }
    }
    ranges.sort_unstable();
    let strip = |from: usize, to: usize| {
        let mut out = String::new();
        let mut cursor = from;
        for &(start, end) in &ranges {
            if end <= from || start >= to {
                continue;
            }
            let start = start.max(from);
            if start > cursor {
                out.push_str(&text[cursor..start]);
            }
            cursor = cursor.max(end.min(to));
        }
        if cursor < to {
            out.push_str(&text[cursor..to]);
        }
        out
    };
    let tail = |value: String, count| {
        let chars: Vec<_> = value.chars().collect();
        chars[chars.len().saturating_sub(count)..]
            .iter()
            .collect::<String>()
    };
    if let Some(anchor) = anchor {
        let mut excerpt = tail(strip(0, anchor), MAX_CHARS - 1_800);
        excerpt.extend(strip(anchor, text.len()).chars().take(1_800));
        excerpt
    } else {
        tail(strip(0, text.len()), MAX_CHARS)
    }
}

/// The last visible message candidate, bounded independently of the whole conversation.
fn latest_message(visible: &str) -> String {
    let visible = visible.trim();
    let message = if visible.contains("\n\n") {
        visible.rsplit("\n\n").next().unwrap_or_default()
    } else {
        visible.lines().last().unwrap_or_default()
    };
    let chars: Vec<_> = message.chars().collect();
    chars[chars.len().saturating_sub(400)..].iter().collect()
}

fn terms(text: &str) -> BTreeSet<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .map(|word| {
            let lower = word.to_lowercase();
            if lower.len() > 5 && lower.ends_with('s') {
                lower[..lower.len() - 1].to_string()
            } else {
                lower
            }
        })
        .filter(|word| word.chars().count() >= 4 && word.chars().any(char::is_alphabetic))
        .filter(|word| {
            !matches!(
                word.as_str(),
                "this"
                    | "that"
                    | "these"
                    | "those"
                    | "about"
                    | "there"
                    | "their"
                    | "them"
                    | "they"
                    | "what"
                    | "when"
                    | "where"
                    | "which"
                    | "with"
                    | "from"
                    | "have"
                    | "will"
                    | "would"
                    | "could"
                    | "should"
                    | "think"
                    | "thanks"
                    | "thank"
                    | "please"
                    | "tell"
                    | "send"
                    | "reply"
                    | "write"
                    | "draft"
                    | "message"
                    | "subject"
                    | "window"
                    | "just"
                    | "saw"
                    | "copied"
                    | "other"
                    | "screen"
                    | "app"
                    | "works"
                    | "work"
                    | "friday"
                    | "monday"
                    | "tuesday"
                    | "wednesday"
                    | "thursday"
                    | "saturday"
                    | "sunday"
                    | "today"
                    | "tomorrow"
                    | "update"
                    | "hello"
                    | "good"
                    | "some"
                    | "your"
                    | "much"
                    | "keep"
                    | "small"
                    | "need"
                    | "more"
                    | "only"
                    | "after"
                    | "before"
                    | "fine"
                    | "okay"
                    | "great"
                    | "sure"
                    | "done"
                    | "agree"
                    | "sounds"
                    | "right"
            )
        })
        .collect()
}

/// Select a processing chunk locally. A deictic reference needs matching subject matter;
/// an empty field needs a topic/header match. Ordinary edits cannot attach ambient app text.
pub fn select_inline_source(draft: &str, field_label: &str, visible: &str, source: &str) -> String {
    const MAX_CHARS: usize = 4_000;
    if source.trim().is_empty() {
        return String::new();
    }
    let draft_lower = draft.to_lowercase();
    let explicit = [
        "just saw",
        "just copied",
        "from the other app",
        "in the other app",
        "from my screen",
        "acabo de ver",
        "acabo de copiar",
    ]
    .iter()
    .any(|phrase| draft_lower.contains(phrase));
    let refers = draft_lower
        .split(|ch: char| !ch.is_alphanumeric())
        .any(|word| {
            matches!(
                word,
                "this"
                    | "that"
                    | "these"
                    | "those"
                    | "eso"
                    | "esa"
                    | "este"
                    | "esta"
                    | "ça"
                    | "cela"
            )
        });
    let title = source.lines().next().unwrap_or_default();
    let draft_terms = terms(draft);
    let named_topic = terms(title).intersection(&draft_terms).any(|term| {
        !matches!(
            term.as_str(),
            "chart" | "file" | "page" | "document" | "search"
        )
    });
    let mut query = draft_terms;
    query.extend(terms(field_label));
    query.extend(terms(&latest_message(visible)));
    let title_match = terms(title).intersection(&query).next().is_some();
    if !explicit && !named_topic && (!(refers || draft.trim().is_empty()) || !title_match) {
        return String::new();
    }
    // The title is context, not a license to send the whole document. Carry at most two
    // matching neighborhoods, in source order, so a long bank portal cannot ride along wholesale.
    let lines: Vec<&str> = source.lines().collect();
    let mut matches: Vec<(usize, usize)> = lines
        .iter()
        .enumerate()
        .skip(1)
        .map(|(index, line)| (index, terms(line).intersection(&query).count()))
        .filter(|(_, score)| *score > 0)
        .collect();
    matches.sort_by_key(|(index, score)| (std::cmp::Reverse(*score), *index));
    matches.truncate(2);
    if matches.is_empty() {
        matches.push((1, 0));
    }
    let mut selected = BTreeSet::new();
    for (index, _) in matches {
        for nearby in
            index.saturating_sub(1).max(1)..=(index + 1).min(lines.len().saturating_sub(1))
        {
            selected.insert(nearby);
        }
    }
    let mut result: String = title.chars().take(256).collect();
    for index in selected {
        let line = lines[index];
        let lower = line.to_lowercase();
        let anchor = query
            .iter()
            .filter_map(|term| lower.find(term))
            .min()
            .map(|byte| lower[..byte].chars().count())
            .unwrap_or(0);
        let chunk: String = line
            .chars()
            .skip(anchor.saturating_sub(300))
            .take(1_500)
            .collect();
        result.push('\n');
        result.push_str(&chunk);
    }
    result.chars().take(MAX_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn edits_do_not_export_unrelated_recent_activity() {
        let bank = "Window: Medical bank statement\nAccount balance 5000 and medical records";
        for draft in [
            "ok see you",
            "what do you think about this?",
            "send me that file",
            "",
        ] {
            assert!(
                select_inline_source(draft, "Message", "Sam: thanks for the update", bank)
                    .is_empty()
            );
        }
    }
    #[test]
    fn long_conversation_and_numbers_cannot_opt_in_to_a_bank_snapshot() {
        let bank = "Window: Bank statement 2026\nAccount balance 5000\nPharmacy transfer; account ending 1234";
        let history = format!(
            "{}\nSam: see you tomorrow",
            "account team asked about the transfer 2026\n".repeat(100)
        );
        for draft in ["", "that's fine", "what do you think about that?"] {
            assert!(select_inline_source(draft, "Message", &history, bank).is_empty());
        }
        assert!(select_inline_source("", "Message", "Sam: how did 2026 go?", bank).is_empty());
        let body_only = "Window: Private records\nConversions and weekly chart numbers 12%";
        assert!(
            select_inline_source("", "Message", "Sam: weekly conversions?", body_only).is_empty()
        );
        assert!(select_inline_source(
            "that's fine",
            "Message",
            "Sam: weekly conversions?",
            body_only
        )
        .is_empty());
    }

    #[test]
    fn normalized_draft_removal_preserves_boundaries_unicode_and_short_replies() {
        // No separator between the AX text nodes, on purpose.
        let text = format!(
            "{}focused editor text{}END-OF-WINDOW",
            "old ".repeat(1_200),
            "new ".repeat(1_200)
        );
        let excerpt = visible_text_near(&text, "focused editor text");
        assert!(!excerpt.contains("focused editor text"));
        assert!(!excerpt.contains("END-OF-WINDOW"));
        assert!(excerpt.chars().count() <= 6_000);
        assert_eq!(
            visible_text_near("Me: ok\nSam: ok", "ok"),
            "Me: ok\nSam: ok"
        );
        assert_eq!(
            visible_text_near(
                "前文\n予定は  明日の 午後です後文",
                "予定は 明日の 午後です"
            ),
            "前文\n後文"
        );
        let long = "draft word ".repeat(300);
        let draft = format!("first draft line\n{long}\nfirst draft line\nlast draft line");
        let text = format!(
            "Sam: please respond\nfirst   draft line\n{}\nlast draft line\nSam: latest question",
            long.replace(' ', "  ")
        );
        let excerpt = visible_text_near(&text, &draft);
        assert!(!excerpt.contains("draft"));
        assert!(excerpt.contains("latest question"));
    }

    #[test]
    fn implicit_chart_and_named_empty_composer_select_source_facts() {
        let chart = "Window: Weekly conversions\nConversions rose 12%. Mobile stayed flat.";
        assert!(select_inline_source(
            "tell them about that chart",
            "Message",
            "Sam: how did conversions do?",
            chart
        )
        .contains("12%"));
        assert!(select_inline_source("summarize conversions", "", "", chart).contains("12%"));
        assert!(select_inline_source(
            "",
            "Subject: Pilot scope",
            "",
            "Window: Pilot scope\n20 users; budget $500"
        )
        .contains("20 users"));
        assert!(select_inline_source(
            "responde a eso",
            "",
            "conversiones semanales",
            "Window: Conversiones semanales\nSubieron un 12%"
        )
        .contains("12%"));
    }
    #[test]
    fn selection_is_bounded_and_keeps_matching_tail_facts() {
        let source = format!(
            "Window: Conversion chart\n{}\nConversions rose 12%",
            "unrelated account statement\n".repeat(3_000)
        );
        let chunk = select_inline_source("that conversion chart", "", "", &source);
        assert!(chunk.contains("12%"));
        assert!(chunk.chars().count() <= 4_000);
        assert!(chunk.chars().count() < source.chars().count());
    }
    #[test]
    fn cached_browser_host_preserves_allowed_handoffs_under_website_rules() {
        let mut policy = crate::capture::exclusion::ExclusionPolicy::new();
        policy.add_website("bank.example");
        let allowed = |host| {
            policy
                .is_excluded_with_website("com.google.Chrome", Some("Chart"), host)
                .is_none()
        };
        assert!(allowed(Some("charts.example")));
        assert!(!allowed(Some("bank.example")));
        assert!(!allowed(None));
    }
    #[test]
    fn cache_uses_latest_complete_live_ax_snapshot_and_expires() {
        let now = Instant::now();
        let mut cache = InlineSourceCache::default();
        for (app, text) in [
            ("browser", "old chart"),
            ("browser", "latest chart"),
            ("chat", "message"),
        ] {
            cache.observe(InlineSource {
                app: app.into(),
                title: "Chart".into(),
                website_host: Some("charts.example".into()),
                text: text.into(),
                observed_at: now,
            });
        }
        let source = cache.recent_for("chat", now).unwrap();
        assert_eq!(source.text, "latest chart");
        assert_eq!(source.website_host.as_deref(), Some("charts.example"));
        assert!(cache
            .recent_for("chat", now + Duration::from_secs(120))
            .is_none());
        assert!(cache.refresh_seen("browser", now + Duration::from_secs(120)));
        assert_eq!(
            cache
                .recent_for("chat", now + Duration::from_secs(121))
                .unwrap()
                .text,
            "latest chart"
        );
        assert!(!cache.refresh_seen("missing", now));
        cache.retain_allowed(|entry| entry.app != "browser");
        assert!(cache.recent_for("chat", now).is_none());
    }
}
