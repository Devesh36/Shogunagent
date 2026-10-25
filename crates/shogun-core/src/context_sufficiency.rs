//! "Not enough context" as a first-class state (Issue #310, spec
//! `docs/specs/issue-310-context-insufficient-spec.md`).
//!
//! Without a verdict, a thin action row would fall back to the generic buttons without a word,
//! and chat would answer from a general prior in the same voice it uses for a grounded answer.
//! Both are the goldfish failure — the assistant that cannot tell you *that* it has forgotten,
//! only answers as if it never had to remember.
//!
//! This is the single place the verdict is made, and it is made **core-side** from the numbers the
//! core already holds (capture permission, pause state, whether anything was ever recorded, the
//! freshness of the live screen, how many related items retrieval found). The webview renders the
//! verdict; it never guesses one. Two rules shape the output:
//!
//! - **The reason is specific, in order of what the user can do about it.** No screen access is a
//!   Settings problem; a paused capture is a toggle; a stale screen is "look back at it"; thin
//!   history is "give me the text". Each variant carries only what its sentence needs.
//! - **No internals leak.** The verdict never carries captured text, confidence numbers or model
//!   details. The one number allowed out is a count of related items, and the UI is free to drop
//!   even that.
//!
//! The module is pure so the shell can judge on the press path without a build step (AR-08) and
//! the rules can be pinned by tests without a Mac.

/// The live-screen half of the inputs, as the shell reads it off the screen-context cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenState {
    /// Captured within the freshness window (`screen_context::DEFAULT_MAX_AGE`).
    Fresh { app: String },
    /// Held but older than the window: the user moved on, or capture stopped feeding it.
    Stale { app: String, age_secs: u64 },
    /// Nothing warm at all.
    Empty,
}

impl From<&crate::screen_context::ScreenSnapshot> for ScreenState {
    /// The cache's freshness verdict, reduced to what a reason line needs: the app's display
    /// name (bundle id when capture knew no name) and the age in whole seconds. The excerpt and
    /// selection never cross — this type is the boundary that keeps content out of the note.
    fn from(snapshot: &crate::screen_context::ScreenSnapshot) -> Self {
        use crate::screen_context::ScreenSnapshot;
        let app_of = |c: &crate::screen_context::ScreenContext| {
            if c.app_name.trim().is_empty() {
                c.app_bundle_id.trim().to_string()
            } else {
                c.app_name.trim().to_string()
            }
        };
        match snapshot {
            ScreenSnapshot::Fresh { context, .. } => ScreenState::Fresh {
                app: app_of(context),
            },
            ScreenSnapshot::Stale { context, age } => ScreenState::Stale {
                app: app_of(context),
                age_secs: age.as_secs(),
            },
            ScreenSnapshot::Empty => ScreenState::Empty,
        }
    }
}

/// Why the verdict is being asked for. The same inputs answer differently: a question can be
/// answered from the screen alone, an action cannot — it needs something *remembered* about this
/// context, otherwise the row is the generic fallback (FR-CF-04) and should say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// Chat / Ask: a fresh screen or any related memory is enough to answer with grounding.
    Ask,
    /// Context actions: only remembered state counts; the live screen is what produced the
    /// candidates, not what grounds them.
    Act,
}

/// Everything the verdict is derived from. All of it is already in the core's hands when the
/// question is asked — collecting any of it on the press would be the SLO violation the
/// pre-assembled cache exists to prevent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inputs {
    /// Accessibility trust is granted to this process (the capture source can read screens).
    pub capture_permitted: bool,
    /// The user paused passive capture.
    pub capture_paused: bool,
    /// At least one event was ever recorded. Distinguishes a first run from a quiet afternoon.
    pub has_ever_captured: bool,
    pub screen: ScreenState,
    /// How many related items retrieval found for this request: evidence hits for a question,
    /// confidence-gated state facts for the action row.
    pub related_items: usize,
    /// The floor `related_items` must reach to count as grounded.
    pub min_related: usize,
}

/// The verdict. `Sufficient` is the normal case; every other variant is a reason the UI can say
/// in one line, ordered from most to least actionable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sufficiency {
    Sufficient,
    /// Nothing has ever been recorded. `permission_missing` picks the sentence: "grant screen
    /// access" when it was never granted, "nothing seen yet" when it was but no focus has landed.
    NeverCaptured { permission_missing: bool },
    /// Capture ran before but screen access has since been revoked.
    PermissionMissing,
    /// Capture is paused right now and memory holds nothing related.
    Paused,
    /// The screen the user had open is no longer current and memory holds nothing related.
    Stale { app: String, age_secs: u64 },
    /// Capture is live but there is not enough history on this topic.
    Thin { related: usize },
}

impl Sufficiency {
    /// Stable, machine-readable tag for the shell → webview boundary and for analytics buckets.
    /// Never content.
    pub fn tag(&self) -> &'static str {
        match self {
            Sufficiency::Sufficient => "sufficient",
            Sufficiency::NeverCaptured { .. } => "never_captured",
            Sufficiency::PermissionMissing => "permission_missing",
            Sufficiency::Paused => "paused",
            Sufficiency::Stale { .. } => "stale",
            Sufficiency::Thin { .. } => "thin",
        }
    }

    pub fn is_sufficient(&self) -> bool {
        matches!(self, Sufficiency::Sufficient)
    }

    /// Whether an action that reads context (L2 / L3) should refuse rather than run on
    /// guesswork. `Thin` deliberately does NOT refuse: the generic fallback row (FR-CF-04) is
    /// built for exactly that case and its buttons must stay usable. What refuses is the absence
    /// of the *means* to have context — no permission, never captured, paused.
    pub fn blocks_context_actions(&self) -> bool {
        matches!(
            self,
            Sufficiency::NeverCaptured { .. } | Sufficiency::PermissionMissing | Sufficiency::Paused
        )
    }

    /// The serializable note the shell hands to the panel, or `None` when there is nothing to say.
    pub fn note(&self) -> Option<ContextNote> {
        match self {
            Sufficiency::Sufficient => None,
            Sufficiency::NeverCaptured { permission_missing } => Some(ContextNote {
                tag: self.tag(),
                permission_missing: *permission_missing,
                ..ContextNote::default()
            }),
            Sufficiency::PermissionMissing => Some(ContextNote {
                tag: self.tag(),
                permission_missing: true,
                ..ContextNote::default()
            }),
            Sufficiency::Paused => Some(ContextNote {
                tag: self.tag(),
                ..ContextNote::default()
            }),
            Sufficiency::Stale { app, age_secs } => Some(ContextNote {
                tag: self.tag(),
                app: Some(app.clone()).filter(|a| !a.trim().is_empty()),
                age_secs: Some(*age_secs),
                ..ContextNote::default()
            }),
            Sufficiency::Thin { related } => Some(ContextNote {
                tag: self.tag(),
                related: Some(*related),
                ..ContextNote::default()
            }),
        }
    }
}

/// What crosses to the panel. Fields are the minimum each sentence needs; nothing here is captured
/// text or an internal score.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct ContextNote {
    /// One of [`Sufficiency::tag`]'s non-`sufficient` values.
    pub tag: &'static str,
    /// Set for the permission variants so the panel can offer the Settings route.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub permission_missing: bool,
    /// The app whose context went stale (display name, as the cache labelled it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_secs: Option<u64>,
    /// How many related items were found (the one count the spec allows out).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub related: Option<usize>,
}

/// The verdict for `inputs` under `purpose`.
///
/// Order matters and is the whole design: the first reason that explains the shortfall wins,
/// from the one the user can fix in Settings down to the one only more work can fix.
pub fn judge(inputs: &Inputs, purpose: Purpose) -> Sufficiency {
    let grounded_by_memory = inputs.related_items >= inputs.min_related;
    let grounded_by_screen =
        purpose == Purpose::Ask && matches!(inputs.screen, ScreenState::Fresh { .. });
    if grounded_by_memory || grounded_by_screen {
        return Sufficiency::Sufficient;
    }
    if !inputs.has_ever_captured {
        return Sufficiency::NeverCaptured {
            permission_missing: !inputs.capture_permitted,
        };
    }
    if !inputs.capture_permitted {
        return Sufficiency::PermissionMissing;
    }
    if inputs.capture_paused {
        return Sufficiency::Paused;
    }
    match (&inputs.screen, purpose) {
        (ScreenState::Stale { app, age_secs }, Purpose::Ask) => Sufficiency::Stale {
            app: app.clone(),
            age_secs: *age_secs,
        },
        _ => Sufficiency::Thin {
            related: inputs.related_items,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live() -> Inputs {
        Inputs {
            capture_permitted: true,
            capture_paused: false,
            has_ever_captured: true,
            screen: ScreenState::Fresh {
                app: "Mail".into(),
            },
            related_items: 0,
            min_related: 1,
        }
    }

    #[test]
    fn a_fresh_screen_grounds_a_question_but_not_an_action() {
        let inputs = live();
        assert_eq!(judge(&inputs, Purpose::Ask), Sufficiency::Sufficient);
        assert_eq!(judge(&inputs, Purpose::Act), Sufficiency::Thin { related: 0 });
    }

    #[test]
    fn related_memory_grounds_either_purpose() {
        let inputs = Inputs {
            screen: ScreenState::Empty,
            related_items: 1,
            ..live()
        };
        assert_eq!(judge(&inputs, Purpose::Ask), Sufficiency::Sufficient);
        assert_eq!(judge(&inputs, Purpose::Act), Sufficiency::Sufficient);
    }

    #[test]
    fn first_run_without_permission_points_at_settings() {
        let inputs = Inputs {
            capture_permitted: false,
            has_ever_captured: false,
            screen: ScreenState::Empty,
            ..live()
        };
        let verdict = judge(&inputs, Purpose::Ask);
        assert_eq!(
            verdict,
            Sufficiency::NeverCaptured {
                permission_missing: true
            }
        );
        assert!(verdict.blocks_context_actions());
        let note = verdict.note().expect("a reason");
        assert_eq!(note.tag, "never_captured");
        assert!(note.permission_missing);
    }

    #[test]
    fn first_run_with_permission_is_nothing_seen_yet() {
        let inputs = Inputs {
            has_ever_captured: false,
            screen: ScreenState::Empty,
            ..live()
        };
        assert_eq!(
            judge(&inputs, Purpose::Act),
            Sufficiency::NeverCaptured {
                permission_missing: false
            }
        );
    }

    #[test]
    fn revoked_permission_after_history_is_its_own_reason() {
        let inputs = Inputs {
            capture_permitted: false,
            screen: ScreenState::Empty,
            ..live()
        };
        let verdict = judge(&inputs, Purpose::Ask);
        assert_eq!(verdict, Sufficiency::PermissionMissing);
        assert!(verdict.note().expect("note").permission_missing);
    }

    #[test]
    fn paused_capture_outranks_a_stale_screen() {
        // Pausing is what made the screen go stale; naming the pause is the honest reason and
        // the one the user can undo.
        let inputs = Inputs {
            capture_paused: true,
            screen: ScreenState::Stale {
                app: "Safari".into(),
                age_secs: 40,
            },
            ..live()
        };
        assert_eq!(judge(&inputs, Purpose::Ask), Sufficiency::Paused);
        assert!(judge(&inputs, Purpose::Ask).blocks_context_actions());
    }

    #[test]
    fn a_stale_screen_with_no_memory_is_stale_for_a_question() {
        let inputs = Inputs {
            screen: ScreenState::Stale {
                app: "Safari".into(),
                age_secs: 40,
            },
            ..live()
        };
        let verdict = judge(&inputs, Purpose::Ask);
        assert_eq!(
            verdict,
            Sufficiency::Stale {
                app: "Safari".into(),
                age_secs: 40
            }
        );
        let note = verdict.note().expect("note");
        assert_eq!(note.app.as_deref(), Some("Safari"));
        assert_eq!(note.age_secs, Some(40));
        assert!(!verdict.blocks_context_actions());
    }

    #[test]
    fn the_action_row_never_reports_stale() {
        // Candidates are assembled from the live frontmost window, so "the screen moved on" is
        // not a state the row can be in; with no facts it is simply thin.
        let inputs = Inputs {
            screen: ScreenState::Stale {
                app: "Safari".into(),
                age_secs: 40,
            },
            ..live()
        };
        assert_eq!(judge(&inputs, Purpose::Act), Sufficiency::Thin { related: 0 });
    }

    #[test]
    fn thin_history_never_blocks_the_generic_row() {
        let verdict = Sufficiency::Thin { related: 0 };
        assert!(!verdict.blocks_context_actions());
        assert_eq!(verdict.note().expect("note").related, Some(0));
    }

    #[test]
    fn sufficient_has_no_note() {
        assert!(Sufficiency::Sufficient.note().is_none());
        assert_eq!(Sufficiency::Sufficient.tag(), "sufficient");
    }

    #[test]
    fn notes_carry_no_content_fields() {
        // The boundary type is the contract: a count, an app label, seconds, a flag and a tag.
        // If a field for text ever appears here, the spec's "no captured content" rule broke.
        let json = serde_json::to_value(
            Sufficiency::Stale {
                app: "Mail".into(),
                age_secs: 3,
            }
            .note()
            .expect("note"),
        )
        .expect("serializes");
        let mut keys: Vec<&str> = json
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["age_secs", "app", "tag"]);
    }

    #[test]
    fn a_snapshot_reduces_to_app_and_age_only() {
        use crate::screen_context::{ScreenContext, ScreenSnapshot};
        use std::time::Duration;
        let context = ScreenContext {
            app_bundle_id: "com.apple.Safari".into(),
            app_name: String::new(),
            window_title: "secret title".into(),
            text_excerpt: "secret body".into(),
            selection: Some("secret selection".into()),
            website_host: None,
        };
        let stale = ScreenSnapshot::Stale {
            context: context.clone(),
            age: Duration::from_millis(41_900),
        };
        assert_eq!(
            ScreenState::from(&stale),
            ScreenState::Stale {
                app: "com.apple.Safari".into(),
                age_secs: 41
            }
        );
        let fresh = ScreenSnapshot::Fresh {
            context: ScreenContext {
                app_name: "Safari".into(),
                ..context
            },
            age: Duration::from_secs(1),
        };
        assert_eq!(
            ScreenState::from(&fresh),
            ScreenState::Fresh {
                app: "Safari".into()
            }
        );
        assert_eq!(ScreenState::from(&ScreenSnapshot::Empty), ScreenState::Empty);
    }

    #[test]
    fn min_related_is_honoured() {
        let inputs = Inputs {
            screen: ScreenState::Empty,
            related_items: 2,
            min_related: 3,
            ..live()
        };
        assert_eq!(judge(&inputs, Purpose::Ask), Sufficiency::Thin { related: 2 });
    }
}
