//! Encrypted writing reference storage and the egress consent boundary. Learning is local and
//! always on (bounded, 30-day); `enabled` is the consent for sending examples to the provider.
use super::*;
use shogun_memory::writing_voice;

impl Db {
    /// Counts for the Writing Style dashboard. Raw draft text and recipient keys stay local.
    pub fn writing_voice_scopes(&self) -> Result<Vec<writing_voice::Scope>, String> {
        self.with_conn_reported("writing_voice.scopes", |conn| {
            writing_voice::scopes(conn, (self.clock)())
                .map_err(|_| "Could not load writing references".to_string())
        })
    }

    pub fn voice_learning_enabled(&self) -> bool {
        self.with_conn("writing_voice.enabled", writing_voice::enabled)
            .unwrap_or(false)
    }

    pub fn set_voice_learning_enabled(&self, enabled: bool) -> Result<(), String> {
        self.with_conn_reported("writing_voice.set_enabled", |conn| {
            writing_voice::set_enabled(conn, enabled)
                .map_err(|_| "Could not save writing preference".into())
        })
    }

    /// Apply a confirmed L3 approval that changes a local setting instead of sending anything.
    /// All are queued by MCP / REST / CLI: turning writing-style sharing on (`writing_voice.set`
    /// enable, issue #654), a `capture_exclusions.set` that removes exclusions (issue #657), an
    /// `agent.source.set` that selects a subscription CLI (issue #110), turning dictionary
    /// auto-add on, and turning Visual recall on or keeping saved screens longer (issue #782).
    /// An API caller's Gmail archive request (issue #779) is recorded here too: the Gmail write
    /// itself is left to the mail write-back tick. `None` for every real send: those go to a
    /// transport. The desktop calls this right after the user's dedicated-button confirm.
    ///
    /// `capture_exclusions` is the exclusion store the desktop enforces. A capture change is
    /// re-checked against the lists on disk at this moment: it applies only if everything it
    /// would stop excluding is what the approval card showed, so a list the user changed after
    /// the request fails closed instead of silently resuming capture of something else.
    ///
    /// `llm_settings` is the desktop's `llm.json`. A confirmed subscription selects that delegate
    /// and records the opt-in the card disclosed; the desktop then reloads its Agent lane.
    ///
    /// `visual_recall` is the desktop's `visual_recall.json`. A confirmed Visual recall widening
    /// (issue #782) applies exactly what the card showed and never more: see
    /// `apply_confirmed_visual_recall` below.
    pub fn apply_confirmed_setting(
        &self,
        action: &shogun_agents::permission::SendAction,
        capture_exclusions: Option<&std::path::Path>,
        llm_settings: Option<&std::path::Path>,
        visual_recall: Option<&std::path::Path>,
    ) -> Option<Result<(), String>> {
        match action {
            shogun_agents::permission::SendAction::ShareWritingStyle => {
                Some(self.set_voice_learning_enabled(true))
            }
            shogun_agents::permission::SendAction::EnableDictionaryAutoAdd => {
                Some(self.enable_dictionary_autoadd_after_approval())
            }
            shogun_agents::permission::SendAction::ResumeCapture { change } => Some(
                capture_exclusions
                    .ok_or_else(|| "capture exclusions unavailable".to_string())
                    .and_then(|path| {
                        crate::capture::exclusion::replace_policy_resuming_at_most(
                            path,
                            &change.apps,
                            &change.websites,
                            &change.resumed_apps,
                            &change.resumed_websites,
                        )
                    })
                    .map(|_| ()),
            ),
            shogun_agents::permission::SendAction::WidenVisualRecall { change } => Some(
                visual_recall
                    .ok_or_else(|| "visual recall settings unavailable".to_string())
                    .and_then(|path| apply_confirmed_visual_recall(path, change)),
            ),
            // Issue #779: the user approved an API caller's Gmail archive / unarchive. Recording
            // it re-checks the thread as it is now (still known, still in the Inbox for an
            // archive, still archived for an unarchive); the mail write-back tick then executes
            // it at the next mail sync, traced and undoable.
            shogun_agents::permission::SendAction::ArchiveMailThread { thread_key, unarchive } => {
                Some(if *unarchive {
                    self.mail_request_unarchive(thread_key)
                } else {
                    self.mail_request_archive(thread_key)
                })
            }
            shogun_agents::permission::SendAction::UseSubscriptionAgent { delegate } => Some(
                llm_settings
                    .ok_or_else(|| "agent source settings unavailable".to_string())
                    .and_then(|path| crate::agent_source::apply_confirmed_subscription(path, delegate))
                    .map(|_| ()),
            ),
            _ => None,
        }
    }

    pub fn clear_voice_learning(&self) -> Result<(), String> {
        self.with_conn_mut_reported("writing_voice.clear", |conn| {
            writing_voice::clear(conn).map_err(|_| "Could not clear learned writing".into())
        })
    }

    /// Capture before inference so a clear/toggle that lands mid-rewrite invalidates that
    /// rewrite's late observation. Independent of the sharing consent: learning is local.
    pub fn voice_learning_revision(&self) -> Option<i64> {
        self.with_conn("writing_voice.revision", |conn| {
            conn.query_row(
                "SELECT revision FROM writing_voice_settings WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .optional()
        })
        .ok()
        .flatten()
    }

    /// Whether the one-time "use my writing style in rewrites?" offer should be shown now.
    pub fn writing_voice_offer_due(&self) -> bool {
        self.with_conn("writing_voice.offer_due", writing_voice::offer_due)
            .unwrap_or(false)
    }

    /// Answer the one-time offer (`true` = start sharing). Never asked again either way.
    pub fn decide_writing_voice_offer(&self, enable: bool) -> Result<(), String> {
        self.with_conn_reported("writing_voice.decide_offer", |conn| {
            writing_voice::decide_offer(conn, enable)
                .map_err(|_| "Could not save writing preference".into())
        })
    }

    pub fn observe_writing_draft(
        &self,
        app: &str,
        source: &str,
        output: &str,
        revision: Option<i64>,
    ) {
        self.observe_writing_draft_for_person(app, None, source, output, revision);
    }

    pub fn observe_writing_draft_for_person(
        &self,
        app: &str,
        person: Option<&str>,
        source: &str,
        output: &str,
        revision: Option<i64>,
    ) {
        let Some(revision) = revision else {
            return;
        };
        let _ = self.with_conn_mut("writing_voice.observe", |conn| {
            writing_voice::observe_for_person(
                conn,
                app,
                person,
                source,
                output,
                (self.clock)(),
                revision,
            )
        });
    }

    /// Only inline and Scribe text requests consume this data. Reads and writes fail closed on
    /// missing consent/storage errors. Keep out of shared context caches and trusted directives.
    pub fn writing_voice_prompt_data(&self, app: &str) -> String {
        self.writing_voice_prompt_data_for_person(app, None)
    }

    pub fn writing_voice_prompt_data_for_person(&self, app: &str, person: Option<&str>) -> String {
        self.with_conn("writing_voice.prompt", |conn| {
            writing_voice::prompt_data_for_person(conn, app, person, (self.clock)())
        })
        .unwrap_or_default()
    }
}

/// Apply a confirmed Visual recall widening (issue #782) to `visual_recall.json`, re-checked
/// against the settings as they are now, under their lock:
/// - `enable` turns Visual recall on. It fails closed when the retention in force is now longer
///   than the card said saved screens would be kept, so confirming never starts capture under a
///   retention the user did not see.
/// - `retention_days` sets that retention when it is longer than the one in force. A retention
///   the user has since set at least as long is left alone: confirming never goes beyond the
///   request and never overrides the user's own later choice with a shorter one.
fn apply_confirmed_visual_recall(
    path: &std::path::Path,
    change: &shogun_agents::permission::VisualRecallWidening,
) -> Result<(), String> {
    if !change.is_valid() {
        return Err("invalid visual recall change".to_string());
    }
    let retention = change
        .retention_days
        .map(crate::capture::visual_recall::RetentionPolicy::try_days)
        .transpose()?;
    crate::capture::visual_recall::try_update_settings_with_commit(
        path,
        |settings| {
            let current = settings.retention.days();
            if change.enable {
                let shown = change.retention_days.unwrap_or(change.previous_retention_days);
                let after = retention.map_or(current, |requested| requested.days().max(current));
                if after > shown {
                    return Err("visual recall retention changed after this request".to_string());
                }
                settings.enabled = true;
            }
            if let Some(requested) = retention {
                if requested.days() > current {
                    settings.retention = requested;
                }
            }
            Ok(())
        },
        |_| {},
    )
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recipient_methods_preserve_scope_and_consent_revision(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(Arc::new(|| 100))?;
        db.set_voice_learning_enabled(true)?;
        let revision = db.voice_learning_revision();
        for n in 0..3 {
            db.observe_writing_draft_for_person(
                "mail",
                Some("person:a"),
                &format!("hey lets catch up tomorrow {n}"),
                "Tomorrow is fine.",
                revision,
            );
        }
        assert!(!db
            .writing_voice_prompt_data_for_person("mail", Some("person:a"))
            .is_empty());
        assert!(db
            .writing_voice_prompt_data_for_person("mail", Some("person:b"))
            .is_empty());
        assert!(db.writing_voice_prompt_data("mail").is_empty());
        db.clear_voice_learning()?;
        for n in 0..3 {
            db.observe_writing_draft_for_person(
                "mail",
                Some("person:a"),
                &format!("stale recipient draft tomorrow {n}"),
                "Tomorrow is fine.",
                revision,
            );
        }
        assert!(db
            .writing_voice_prompt_data_for_person("mail", Some("person:a"))
            .is_empty());
        Ok(())
    }

    #[test]
    fn clearing_or_disabling_invalidates_in_flight_learning(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(Arc::new(|| 100))?;
        db.set_voice_learning_enabled(true)?;
        let revision = db.voice_learning_revision();
        db.clear_voice_learning()?;
        for n in 0..3 {
            db.observe_writing_draft(
                "slack",
                &format!("yeah lets do it tomorrow {n}"),
                "Tomorrow works.",
                revision,
            );
        }
        assert!(db.writing_voice_prompt_data("slack").is_empty());
        let revision = db.voice_learning_revision();
        db.set_voice_learning_enabled(false)?;
        db.set_voice_learning_enabled(true)?;
        for n in 0..3 {
            db.observe_writing_draft(
                "slack",
                &format!("yeah lets do it tomorrow {n}"),
                "Tomorrow works.",
                revision,
            );
        }
        assert!(db.writing_voice_prompt_data("slack").is_empty());
        let revision = db.voice_learning_revision();
        for n in 0..3 {
            db.observe_writing_draft(
                "slack",
                &format!("yeah lets do it tomorrow {n}"),
                "Tomorrow works.",
                revision,
            );
        }
        assert!(!db.writing_voice_prompt_data("slack").is_empty());
        Ok(())
    }

    #[test]
    fn only_a_confirmed_writing_style_approval_turns_sharing_on(
    ) -> Result<(), Box<dyn std::error::Error>> {
        use shogun_agents::approval::{
            writing_style_request, ApprovalOrigin, ApprovalQueue, ConfirmIntent, Decision,
        };
        use shogun_agents::permission::SendAction;
        let db = Db::open_in_memory(Arc::new(|| 100))?;
        assert!(db.voice_learning_enabled(), "V79: on by default for undecided users");
        db.set_voice_learning_enabled(false)?; // an explicit off is respected
        // A real send is not a setting: left to its transport, nothing changes here.
        assert_eq!(db.apply_confirmed_setting(&SendAction::PostMessage { channel: "#x".into() }, None, None, None), None);
        assert!(!db.voice_learning_enabled());

        // The queued enable (issue #654) changes nothing while it is pending…
        let mut queue = ApprovalQueue::new();
        let (action, preview) = writing_style_request();
        let id = queue.try_request(action, preview, ApprovalOrigin::Api, 0).map_err(|e| e.to_string())?;
        assert!(!db.voice_learning_enabled());
        // …and is applied only once the user confirms it with the dedicated button.
        let Decision::Confirmed(confirmed) = queue.confirm(id, ConfirmIntent::DedicatedButton, 1) else {
            return Err("the dedicated button confirms".into());
        };
        assert_eq!(db.apply_confirmed_setting(&confirmed.action, None, None, None), Some(Ok(())));
        assert!(db.voice_learning_enabled());
        Ok(())
    }

    #[test]
    fn examples_learned_before_consent_are_used_the_moment_sharing_is_enabled(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(Arc::new(|| 100))?;
        assert!(db.voice_learning_enabled(), "V79: on by default");
        assert!(!db.writing_voice_offer_due(), "nothing left to ask");
        db.set_voice_learning_enabled(false)?;
        let revision = db.voice_learning_revision();
        assert!(revision.is_some(), "learning has a revision even while sharing is off");
        for n in 0..3 {
            db.observe_writing_draft("slack", &format!("yeah lets do it tomorrow {n}"), "Tomorrow works.", revision);
        }
        assert!(db.writing_voice_prompt_data("slack").is_empty(), "no egress while the user has it off");
        db.set_voice_learning_enabled(true)?;
        assert!(db.voice_learning_enabled());
        assert!(!db.writing_voice_prompt_data("slack").is_empty());
        Ok(())
    }
}
