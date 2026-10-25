//! Meeting templates: named shapes for the generated minutes (Recap).
//!
//! A template does one thing — it tells the minutes generator which *sections* this kind of
//! meeting should come back with, on top of the invariant trio (summary / decisions / next
//! actions) that every Recap keeps. It never changes what is recorded, only how the minutes are
//! organised; and it is pure data here so the registry is the same for the UI picker, the prompt
//! builder, and the API face (invariant 6).
//!
//! The registry is deliberately static and small: a handful of shapes that cover most work
//! meetings, with `general` as the default that reproduces the untemplated Recap exactly.

/// One section a template asks the minutes to include.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TemplateSection {
    /// The heading, as it should appear in the minutes.
    pub title: &'static str,
    /// What belongs under this heading — handed to the model as guidance.
    pub guidance: &'static str,
}

/// A meeting template. `id` is the stable wire/storage name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeetingTemplate {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub sections: &'static [TemplateSection],
}

/// The default template: no extra sections — the plain summary/decisions/next-actions Recap.
pub const GENERAL: &str = "general";

/// Every built-in template. `general` first — it is the default and the fallback.
pub const TEMPLATES: &[MeetingTemplate] = &[
    MeetingTemplate {
        id: GENERAL,
        name: "General",
        description: "Summary, decisions and next actions — the default shape.",
        sections: &[],
    },
    MeetingTemplate {
        id: "one_on_one",
        name: "1:1",
        description: "Recurring one-on-one: updates, feedback, growth, follow-ups.",
        sections: &[
            TemplateSection { title: "Updates", guidance: "What each side reported since last time." },
            TemplateSection { title: "Feedback", guidance: "Feedback exchanged, in either direction." },
            TemplateSection { title: "Growth & blockers", guidance: "Career/growth topics and anything blocking them." },
        ],
    },
    MeetingTemplate {
        id: "sales_call",
        name: "Sales call",
        description: "Discovery/deal call: needs, objections, pricing, next steps.",
        sections: &[
            TemplateSection { title: "Customer needs", guidance: "Problems, goals and requirements the customer stated." },
            TemplateSection { title: "Objections & risks", guidance: "Concerns, objections and competitive mentions." },
            TemplateSection { title: "Pricing & terms", guidance: "Anything said about budget, pricing or contract terms." },
        ],
    },
    MeetingTemplate {
        id: "standup",
        name: "Standup",
        description: "Team sync: progress, plans, blockers per person.",
        sections: &[
            TemplateSection { title: "Progress", guidance: "What was completed, by whom." },
            TemplateSection { title: "Plans", guidance: "What each person will do next." },
            TemplateSection { title: "Blockers", guidance: "Anything blocking anyone, and who can unblock it." },
        ],
    },
    MeetingTemplate {
        id: "interview",
        name: "Interview",
        description: "Candidate interview: background, signals, concerns, follow-ups.",
        sections: &[
            TemplateSection { title: "Background", guidance: "Relevant experience and context the candidate gave." },
            TemplateSection { title: "Signals", guidance: "Strengths and evidence observed, tied to what was actually said." },
            TemplateSection { title: "Concerns", guidance: "Gaps or doubts raised — only ones grounded in the conversation." },
        ],
    },
    MeetingTemplate {
        id: "customer_research",
        name: "Customer research",
        description: "User/customer interview: context, pains, quotes, requests.",
        sections: &[
            TemplateSection { title: "Context", guidance: "Who the customer is and how they work today." },
            TemplateSection { title: "Pain points", guidance: "Problems described, in the customer's own framing." },
            TemplateSection { title: "Notable quotes", guidance: "Short verbatim quotes worth keeping, attributed." },
            TemplateSection { title: "Requests", guidance: "Feature asks and explicit needs." },
        ],
    },
];

/// Look up a template by its stable id. Unknown or empty → `None` (callers fall back to
/// [`GENERAL`] rather than failing the Recap over a template name).
pub fn by_id(id: &str) -> Option<&'static MeetingTemplate> {
    TEMPLATES.iter().find(|t| t.id == id)
}

/// The template to actually use for a stored (possibly absent / stale) id.
pub fn resolve(id: Option<&str>) -> &'static MeetingTemplate {
    // TEMPLATES[0] is `general` by construction (pinned by test); no lookup that could miss.
    id.and_then(by_id).unwrap_or(&TEMPLATES[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn general_is_first_default_and_sectionless() {
        assert_eq!(TEMPLATES[0].id, GENERAL);
        assert!(TEMPLATES[0].sections.is_empty(), "general must reproduce the untemplated Recap");
    }

    #[test]
    fn ids_are_unique_and_stable_shaped() {
        let mut seen = std::collections::BTreeSet::new();
        for t in TEMPLATES {
            assert!(seen.insert(t.id), "duplicate template id {}", t.id);
            assert!(t.id.chars().all(|c| c.is_ascii_lowercase() || c == '_'), "{} not wire-safe", t.id);
            assert!(!t.name.is_empty() && !t.description.is_empty());
        }
    }

    #[test]
    fn resolve_falls_back_to_general_never_fails() {
        assert_eq!(resolve(None).id, GENERAL);
        assert_eq!(resolve(Some("no_such_template")).id, GENERAL);
        assert_eq!(resolve(Some("standup")).id, "standup");
        assert_eq!(resolve(Some("")).id, GENERAL);
    }

    #[test]
    fn every_templated_section_has_guidance() {
        for t in TEMPLATES {
            for s in t.sections {
                assert!(!s.title.trim().is_empty());
                assert!(!s.guidance.trim().is_empty());
            }
        }
    }
}
