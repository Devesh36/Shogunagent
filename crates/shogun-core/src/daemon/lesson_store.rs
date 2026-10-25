//! Feedback, lesson lifecycle, and generation directives.

use super::*;

impl Db {
    // L5 lessons
    // Db wrappers over `shogun_memory::lessons`, next to the brief wrappers: the
    // LessonDistillation Dream job, Context Fusion injection, and the Memory API lessons tools
    // all read/write through these. Feedback text never leaves the DB through any of them —
    // the only wrapper that returns it ([`Self::feedback_after`]) feeds the local distiller.

    /// The distillation watermark: highest `feedback_events.id` already consumed (0 = none).
    pub fn lesson_distill_watermark(&self) -> i64 {
        self.try_lesson_distill_watermark().unwrap_or(0)
    }

    pub fn try_lesson_distill_watermark(&self) -> Result<i64, String> {
        self.with_conn_reported("lessons.distill_watermark", |conn| {
            lessons::distill_watermark(conn)
                .map_err(|_| "dream lesson-watermark read failed".to_string())
        })
    }

    /// Advance the distillation watermark (monotonic). Returns false on a write failure so the
    /// job can report the night as failed and stay resumable.
    pub fn set_lesson_distill_watermark(&self, last_processed_feedback_id: i64) -> bool {
        self.try_set_lesson_distill_watermark(last_processed_feedback_id)
            .is_ok()
    }

    pub fn try_set_lesson_distill_watermark(
        &self,
        last_processed_feedback_id: i64,
    ) -> Result<(), String> {
        self.with_conn_reported("lessons.set_distill_watermark", |conn| {
            lessons::set_distill_watermark(conn, last_processed_feedback_id)
                .map_err(|_| "dream lesson-watermark write failed".to_string())
        })
    }

    /// Unprocessed feedback (id strictly above the watermark), oldest first — the distillation
    /// job's input. Local-only data: the rows carry the user's before/after text, so this must
    /// never feed a log or an egress path.
    pub fn feedback_after(&self, after_id: i64) -> Vec<lessons::FeedbackRow> {
        self.try_feedback_after(after_id).unwrap_or_default()
    }

    pub fn try_feedback_after(&self, after_id: i64) -> Result<Vec<lessons::FeedbackRow>, String> {
        self.with_conn_reported("lessons.list_feedback_after", |conn| {
            lessons::list_feedback_after(conn, after_id)
                .map_err(|_| "dream feedback read failed".to_string())
        })
    }

    /// Record one feedback signal (the D-2 approval hooks call this). Returns the new row id.
    pub fn record_feedback(
        &self,
        kind: lessons::FeedbackKind,
        scope: lessons::LessonScope,
        f: &lessons::NewFeedback<'_>,
    ) -> Option<i64> {
        self.with_conn("lessons.record_feedback", |c| {
            lessons::record_feedback(c, kind, scope, f)
        })
        .ok()
    }

    /// Insert-or-merge a distilled lesson with its evidence ids (provenance mandatory).
    /// `None` on a lock/write failure (the job reports failure without echoing any text).
    pub fn upsert_lesson(&self, candidate: &lessons::LessonCandidate, now_ms: i64) -> Option<i64> {
        self.try_upsert_lesson(candidate, now_ms).ok()
    }

    pub fn try_upsert_lesson(
        &self,
        candidate: &lessons::LessonCandidate,
        now_ms: i64,
    ) -> Result<i64, String> {
        self.with_conn_mut_reported("lessons.upsert", |conn| {
            lessons::upsert_lesson(conn, candidate, &candidate.evidence, now_ms)
                .map_err(|_| "dream lesson upsert failed".to_string())
        })
    }

    /// Run the lesson lifecycle pass (decay, contradiction, floor, cap) at `now_ms`.
    pub fn decay_lessons(&self, now_ms: i64) -> Option<lessons::LifecycleOutcome> {
        self.try_decay_lessons(now_ms).ok()
    }

    pub fn try_decay_lessons(&self, now_ms: i64) -> Result<lessons::LifecycleOutcome, String> {
        self.with_conn_mut_reported("lessons.decay", |conn| {
            lessons::decay_and_deactivate(conn, now_ms)
                .map_err(|_| "dream lesson lifecycle failed".to_string())
        })
    }

    /// The lessons eligible for injection right now (active, at/above the Low-band floor,
    /// scope-filtered, strongest first, at most `top_k`) — the Fusion/D-5 supply.
    pub fn active_lessons(
        &self,
        scopes: &[lessons::ScopeFilter<'_>],
        top_k: usize,
    ) -> Vec<lessons::Lesson> {
        self.with_conn("lessons.active_lessons", |c| {
            lessons::active_lessons(c, scopes, top_k)
        })
        .unwrap_or_default()
    }

    /// Every lesson row, sleeping included — the Learned UI / `lessons.list` supply. Instructions
    /// and bookkeeping only; never `feedback_events` text.
    pub fn lessons_all(&self) -> Vec<lessons::Lesson> {
        self.with_conn("lessons.list", lessons::list_lessons)
            .unwrap_or_default()
    }

    /// Flip one lesson's active switch (`lessons.set_active`). `false` when the row is missing or
    /// the write failed.
    pub fn set_lesson_active(&self, lesson_id: i64, active: bool) -> bool {
        self.try_set_lesson_active(lesson_id, active).unwrap_or(false)
    }

    /// [`Self::set_lesson_active`] that keeps a storage fault apart from a missing row: the API
    /// answers a busy or locked DB with a retryable 5xx, never "lesson not found" (issue #779).
    pub fn try_set_lesson_active(&self, lesson_id: i64, active: bool) -> MemoryResult<bool> {
        let now = self.now_ms();
        self.with_conn("lessons.set_lesson_active", |c| {
            lessons::set_lesson_active(c, lesson_id, active, now)
        })
    }

    /// Record one surface's offered candidates (FR-PAT-04). Best-effort and fire-and-forget:
    /// the impression log can never block or fail an offer, and the writer purges expired rows
    /// itself, so retention needs no separate maintenance hook.
    pub fn record_offer_impressions(
        &self,
        surface: &str,
        context_app: Option<&str>,
        offered: &[(String, Option<i64>)],
    ) {
        let now = self.now_ms();
        let borrowed: Vec<shogun_memory::work_patterns::OfferedKind<'_>> = offered
            .iter()
            .map(|(kind, rank)| (kind.as_str(), *rank))
            .collect();
        let _ = self.with_conn("patterns.record_impressions", |c| {
            shogun_memory::work_patterns::record_impressions(
                c,
                now,
                surface,
                context_app,
                &borrowed,
            )
        });
    }

    /// The Patterns overview (FR-PAT-06): live 7-day activity aggregates plus every distilled
    /// profile row. Empty on failure — the card renders its empty state, never a fabricated view.
    #[allow(clippy::type_complexity)]
    pub fn patterns_overview(
        &self,
    ) -> (
        Vec<shogun_memory::work_patterns::ActivityAgg>,
        Vec<shogun_memory::work_patterns::PatternRow>,
    ) {
        self.try_patterns_overview().unwrap_or_default()
    }

    #[allow(clippy::type_complexity)]
    pub fn try_patterns_overview(
        &self,
    ) -> Result<
        (
            Vec<shogun_memory::work_patterns::ActivityAgg>,
            Vec<shogun_memory::work_patterns::PatternRow>,
        ),
        String,
    > {
        let now = self.now_ms();
        self.with_conn_reported("patterns.overview", |conn| {
            Ok((
                shogun_memory::work_patterns::activity_overview(conn, now)
                    .map_err(|_| "dream pattern-overview read failed".to_string())?,
                shogun_memory::work_patterns::list_patterns(conn)
                    .map_err(|_| "dream pattern-list read failed".to_string())?,
            ))
        })
    }

    /// The user's per-pattern switch (FR-PAT-06). OFF stays visible but is never consumed.
    pub fn set_work_pattern_active(&self, id: i64, active: bool) -> bool {
        self.try_set_work_pattern_active(id, active).unwrap_or(false)
    }

    /// [`Self::set_work_pattern_active`] with a storage fault kept apart from a missing row
    /// (issue #779).
    pub fn try_set_work_pattern_active(&self, id: i64, active: bool) -> MemoryResult<bool> {
        let now = self.now_ms();
        self.with_conn("patterns.set_active", |c| {
            shogun_memory::work_patterns::set_pattern_active(c, id, active, now)
        })
    }

    /// Remove one profile row ("that's not me"). Fresh behavior may re-derive it; a durable
    /// silence is `set_work_pattern_active(false)`.
    pub fn delete_work_pattern(&self, id: i64) -> bool {
        self.try_delete_work_pattern(id).unwrap_or(false)
    }

    /// [`Self::delete_work_pattern`] with a storage fault kept apart from a missing row
    /// (issue #779).
    pub fn try_delete_work_pattern(&self, id: i64) -> MemoryResult<bool> {
        self.with_conn("patterns.delete", |c| {
            shogun_memory::work_patterns::delete_pattern(c, id)
        })
    }

    /// FR-PAT-07 counters for `shogun metrics`. `None` when the DB is unreadable — rendered as
    /// `measured:false`, never a fabricated zero. Local face only; no telemetry path.
    pub fn pattern_counters(&self) -> Option<crate::metrics::PatternCounters> {
        let now = self.now_ms();
        self.with_conn("patterns.counters", |conn| {
            let overview = shogun_memory::work_patterns::activity_overview(conn, now)?;
            let active_patterns: i64 = conn.query_row(
                "SELECT count(*) FROM work_patterns WHERE active = 1",
                [],
                |r| r.get(0),
            )?;
            Ok::<_, rusqlite::Error>(crate::metrics::PatternCounters {
                offers_shown_7d: overview.iter().map(|a| a.shown_7d).sum(),
                offers_decided_7d: overview.iter().map(|a| a.decided_7d).sum(),
                offers_adopted_7d: overview.iter().map(|a| a.adopted_7d).sum(),
                active_patterns,
            })
        })
        .ok()
    }

    /// D-6 counters for `shogun metrics`: (active lessons, feedback events in the last 7 days).
    /// `None` when the DB is unreadable — rendered as `measured:false`, never a fabricated zero.
    pub fn lesson_counters(&self) -> Option<crate::metrics::LessonCounters> {
        const WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1000;
        let now = self.now_ms();
        self.with_conn("lessons.counters", |conn| {
            Ok::<_, rusqlite::Error>(crate::metrics::LessonCounters {
                active_lessons: lessons::count_active_lessons(conn)?,
                feedback_events_last_7d: lessons::count_feedback_since(conn, now - WEEK_MS)?,
                edit_before_approve_last_7d: lessons::count_feedback_kind_since(
                    conn,
                    lessons::FeedbackKind::EditBeforeApprove,
                    now - WEEK_MS,
                )?,
                approve_unchanged_last_7d: lessons::count_feedback_kind_since(
                    conn,
                    lessons::FeedbackKind::ApproveUnchanged,
                    now - WEEK_MS,
                )?,
            })
        })
        .ok()
    }

    pub(super) fn record_harness_compression(&self, snap: crate::metrics::CompressionHarness) {
        if let Ok(mut g) = self.harness.lock() {
            g.compression = Some(snap);
        }
    }

    /// Last tool-loop counters for `shogun metrics`. Overwrites the previous snapshot (last loop,
    /// not a process-lifetime sum). Counts only — never tool names or result text.
    pub fn record_tool_loop(&self, snap: crate::metrics::ToolLoopHarness) {
        if let Ok(mut g) = self.harness.lock() {
            g.tool_loop = Some(snap);
        }
    }

    /// H1 counters for `shogun metrics`: last compression assemble and last tool loop, if any.
    /// `None` when neither has run — rendered as `harness.measured:false`, never fabricated zeros.
    pub fn harness_counters(&self) -> Option<crate::metrics::HarnessCounters> {
        self.harness.lock().ok().and_then(|g| (*g).measured())
    }

    /// The active lessons relevant to the current screen (this app + global scope), mapped into
    /// Fusion's input view — instruction + confidence only (D-5).
    pub fn lessons_for_screen(
        &self,
        app_bundle_id: &str,
        top_k: usize,
    ) -> Vec<shogun_fusion::assemble::LessonInput> {
        use lessons::{LessonScope, ScopeFilter};
        let scopes = [
            ScopeFilter {
                scope: LessonScope::App,
                scope_ref: Some(app_bundle_id),
            },
            ScopeFilter {
                scope: LessonScope::Global,
                scope_ref: None,
            },
        ];
        self.active_lessons(&scopes, top_k)
            .into_iter()
            .map(|l| shogun_fusion::assemble::LessonInput {
                id: l.id,
                instruction: l.instruction,
                confidence: l.confidence,
            })
            .collect()
    }

    /// Active, scope-matching lessons mapped into the directive-render view (D-5a). The desktop
    /// joins these into the Shougun.md system-prompt block via
    /// [`crate::user_config::render_directives_with_lessons`].
    pub fn learned_lessons(
        &self,
        context: GenerationContext<'_>,
        top_k: usize,
    ) -> Vec<crate::user_config::LearnedLesson> {
        self.active_lessons(&context.lesson_scopes(), top_k)
            .into_iter()
            .map(|l| crate::user_config::LearnedLesson {
                instruction: l.instruction,
                confidence: l.confidence,
            })
            .collect()
    }

    /// File directives plus active lessons — the standing generation prompt (issue #104).
    pub fn generation_directives(
        &self,
        cfg: &crate::user_config::ShougunConfig,
        context: GenerationContext<'_>,
    ) -> String {
        crate::user_config::render_directives_with_lessons(
            cfg,
            &self.learned_lessons(context, LESSON_TOP_K),
        )
    }
}
