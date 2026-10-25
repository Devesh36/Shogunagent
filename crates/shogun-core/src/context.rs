//! Typed grounded memory shared by retrieval and text-generation lanes.

use std::collections::HashSet;

/// Maximum confidence-gated fact lines allowed into an Option/Scribe prompt.
pub const PROMPT_FACT_LIMIT: usize = 6;
/// Maximum provenance-carrying evidence lines allowed into an Option/Scribe prompt.
pub const PROMPT_EVIDENCE_LIMIT: usize = 8;

/// One retrieved piece of evidence behind an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    pub event_id: i64,
    pub ts: i64,
    pub source: String,
    pub title: Option<String>,
    pub excerpt: String,
    /// Linked `screen_frames` row when a finite-retention encrypted JPEG is stored.
    pub frame_id: Option<i64>,
}

/// A stored screen capture available for visual recall. Text-generation lanes never consume it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenFrameRef {
    pub frame_id: i64,
    /// OCR event read from the focused window; `None` for a preview taken while Accessibility
    /// text was enough (#969).
    pub event_id: Option<i64>,
    pub evidence_event_id: i64,
    pub ts: i64,
    pub app_bundle_id: Option<String>,
    pub window_title: Option<String>,
    pub width: u32,
    pub height: u32,
    pub ocr_excerpt: String,
    pub accessibility_excerpt: String,
    pub text_excerpt: String,
    pub text_source: String,
    pub needs_rescan: bool,
    pub source: String,
}

/// Grounded context for one question: confidence-gated facts plus typed evidence.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ContextPack {
    pub facts: Vec<String>,
    pub evidence: Vec<Evidence>,
    pub screen_frames: Vec<ScreenFrameRef>,
}

impl ContextPack {
    /// Bound a pack for text generation. Visual-recall metadata is intentionally discarded.
    pub fn into_prompt_pack(self) -> Self {
        let mut bounded = Self::default();
        bounded.merge_prompt_pack(self);
        bounded
    }

    /// Merge grounded memory while preserving caller order, deduplicating, and enforcing prompt
    /// limits. Existing items win so current-thread/warm evidence stays ahead of query expansion.
    pub fn merge_prompt_pack(&mut self, incoming: Self) {
        self.facts.truncate(PROMPT_FACT_LIMIT);
        let mut seen_facts: HashSet<String> = self.facts.iter().cloned().collect();
        for fact in incoming.facts {
            if self.facts.len() == PROMPT_FACT_LIMIT {
                break;
            }
            if seen_facts.insert(fact.clone()) {
                self.facts.push(fact);
            }
        }

        self.evidence.retain(text_prompt_evidence);
        self.evidence.truncate(PROMPT_EVIDENCE_LIMIT);
        for evidence in &mut self.evidence {
            evidence.frame_id = None;
        }
        let mut seen_evidence: HashSet<i64> = self
            .evidence
            .iter()
            .map(|evidence| evidence.event_id)
            .collect();
        for mut evidence in incoming.evidence.into_iter().filter(text_prompt_evidence) {
            if self.evidence.len() == PROMPT_EVIDENCE_LIMIT {
                break;
            }
            // The link to a saved screen is visual-recall metadata; the text itself is ordinary
            // memory and stays (#283).
            evidence.frame_id = None;
            if seen_evidence.insert(evidence.event_id) {
                self.evidence.push(evidence);
            }
        }

        self.screen_frames.clear();
    }

    /// Build Scribe's prompt pack with submit-time retrieval first while reserving room for the
    /// exact-focus warm context. Typed intent does not exist when the warm pack is assembled, so a
    /// full warm pack must never starve the evidence retrieved for the submitted instruction.
    pub fn for_scribe(warm: Self, retrieved: Self) -> Self {
        let warm = warm.into_prompt_pack();
        let retrieved = retrieved.into_prompt_pack();
        let warm_fact_reserve = usize::from(!warm.facts.is_empty());
        let warm_evidence_reserve = warm.evidence.len().min(2);

        let mut facts = Vec::new();
        let mut seen_facts = HashSet::new();
        for fact in retrieved
            .facts
            .into_iter()
            .take(PROMPT_FACT_LIMIT - warm_fact_reserve)
            .chain(warm.facts)
        {
            if facts.len() == PROMPT_FACT_LIMIT {
                break;
            }
            if seen_facts.insert(fact.clone()) {
                facts.push(fact);
            }
        }

        let mut evidence = Vec::new();
        let mut seen_evidence = HashSet::new();
        for item in retrieved
            .evidence
            .into_iter()
            .take(PROMPT_EVIDENCE_LIMIT - warm_evidence_reserve)
            .chain(warm.evidence)
        {
            if evidence.len() == PROMPT_EVIDENCE_LIMIT {
                break;
            }
            if seen_evidence.insert(item.event_id) {
                evidence.push(item);
            }
        }

        Self {
            facts,
            evidence,
            screen_frames: Vec::new(),
        }
    }
}

/// Text lanes take ordinary memory only. A saved screen's OCR text and anything carrying frame
/// metadata stay out; a window's accessibility text is ordinary memory even when a saved screen
/// links to it (#283) — Visual recall must never make the text lanes remember less.
fn text_prompt_evidence(evidence: &Evidence) -> bool {
    let ocr_text = matches!(evidence.source.as_str(), "screen_ocr" | "user_screenshot")
        || evidence.source.starts_with("visual_recall_");
    let frame_marker =
        evidence.excerpt.contains("[screen frame ") && evidence.excerpt.contains(" stored]");
    !ocr_text && !frame_marker
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(event_id: i64) -> Evidence {
        Evidence {
            event_id,
            ts: event_id,
            source: "capture".into(),
            title: None,
            excerpt: format!("evidence {event_id}"),
            frame_id: None,
        }
    }

    #[test]
    fn screen_text_is_ordinary_memory_for_text_lanes_but_screen_ocr_is_not() {
        let mut item = evidence(1);
        item.source = "screen_text".into();
        assert!(text_prompt_evidence(&item));
        item.source = "screen_ocr".into();
        assert!(!text_prompt_evidence(&item));
    }

    #[test]
    fn prompt_pack_merge_deduplicates_and_enforces_caps() {
        let mut warm = ContextPack {
            facts: (0..4).map(|index| format!("fact {index}")).collect(),
            evidence: (0..6).map(evidence).collect(),
            screen_frames: Vec::new(),
        };
        warm.merge_prompt_pack(ContextPack {
            facts: (3..10).map(|index| format!("fact {index}")).collect(),
            evidence: (5..14).map(evidence).collect(),
            screen_frames: Vec::new(),
        });

        assert_eq!(warm.facts.len(), PROMPT_FACT_LIMIT);
        assert_eq!(warm.evidence.len(), PROMPT_EVIDENCE_LIMIT);
        assert_eq!(
            warm.facts.iter().filter(|fact| *fact == "fact 3").count(),
            1
        );
        assert_eq!(
            warm.evidence
                .iter()
                .filter(|item| item.event_id == 5)
                .count(),
            1
        );
    }

    #[test]
    fn prompt_pack_excludes_frame_derived_evidence() {
        let pack = ContextPack {
            facts: Vec::new(),
            evidence: vec![Evidence {
                event_id: 41,
                ts: 99,
                source: "visual_recall_ocr".into(),
                title: Some("Private canvas".into()),
                excerpt: "visible text [screen frame 733 stored]".into(),
                frame_id: Some(733),
            }],
            screen_frames: Vec::new(),
        }
        .into_prompt_pack();

        assert!(pack.evidence.is_empty());
    }

    #[test]
    fn prompt_pack_keeps_window_text_that_a_saved_screen_links_to() {
        let pack = ContextPack {
            facts: Vec::new(),
            evidence: vec![
                Evidence {
                    event_id: 7,
                    ts: 1,
                    source: "capture".into(),
                    title: Some("Orchid launch".into()),
                    excerpt: "Orchid launch checklist".into(),
                    frame_id: Some(733),
                },
                Evidence {
                    event_id: 8,
                    ts: 1,
                    source: "screen_ocr".into(),
                    title: Some("Orchid launch".into()),
                    excerpt: "Orchid launch checklist (pixels)".into(),
                    frame_id: Some(733),
                },
            ],
            screen_frames: Vec::new(),
        }
        .into_prompt_pack();

        assert_eq!(pack.evidence.len(), 1, "{:?}", pack.evidence);
        assert_eq!(pack.evidence[0].event_id, 7);
        assert_eq!(pack.evidence[0].frame_id, None, "frame metadata never reaches text lanes");
    }

    #[test]
    fn scribe_pack_prioritizes_retrieval_without_starving_warm_context() {
        let warm = ContextPack {
            facts: (0..PROMPT_FACT_LIMIT)
                .map(|index| format!("warm fact {index}"))
                .collect(),
            evidence: (0..PROMPT_EVIDENCE_LIMIT)
                .map(|index| evidence(100 + index as i64))
                .collect(),
            screen_frames: Vec::new(),
        };
        let retrieved = ContextPack {
            facts: (0..PROMPT_FACT_LIMIT)
                .map(|index| format!("retrieved fact {index}"))
                .collect(),
            evidence: (0..4).map(evidence).collect(),
            screen_frames: Vec::new(),
        };

        let pack = ContextPack::for_scribe(warm, retrieved);

        assert_eq!(pack.facts.len(), PROMPT_FACT_LIMIT);
        assert_eq!(pack.evidence.len(), PROMPT_EVIDENCE_LIMIT);
        assert!(pack.facts.iter().any(|fact| fact == "retrieved fact 0"));
        assert!(pack.facts.iter().any(|fact| fact == "warm fact 0"));
        assert!(pack.evidence.iter().any(|item| item.event_id == 0));
        assert!(pack.evidence.iter().any(|item| item.event_id == 100));
    }
}
