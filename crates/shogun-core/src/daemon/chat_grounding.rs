//! The memory half of a chat prompt (owner request 2026-10-14, answer accuracy): what SHOGUN
//! tracks about the user's work as a compact structured block, the turn's clock, and the
//! retrieved evidence as citable, dated lines. Pure rendering lives in
//! [`crate::chat_grounding`]; this half reads the store.
//!
//! The structured block stands in for the commitment / open-loop fact lines inside the same
//! budget ([`crate::chat_grounding::MEMORY_PROMPT_BUDGET_CHARS`]). Every row passes the one
//! confidence gate (`shogun_fusion::confidence::treat_fact`): High is stated, Medium is
//! `possibly:`, Low never appears (FR-ST-20). The block carries absolute dates and no clock, so it
//! stays in the cacheable stable layer; "Now:" rides in the turn's own context.

use super::*;
pub use crate::chat_grounding::ChatGrounding;
use crate::chat_grounding::{local_date, local_time, local_weekday, now_line, STRUCTURED_BLOCK_MAX_CHARS};
use crate::chat_prompt::HistoryTurn;
use shogun_fusion::confidence::{treat_fact, wording_confidence, Treatment};

/// Rows per section of the structured block.
const COMMITMENTS_MAX: usize = 6;
const OPEN_LOOPS_MAX: usize = 5;
const PEOPLE_MAX: usize = 6;
const PROJECTS_MAX: usize = 5;
const CALENDAR_MAX: usize = 8;
/// One structured line at most (a description is a sentence, not a document).
const LINE_MAX_CHARS: usize = 140;
/// Identity profile lines in the user's section (About You: one per section).
const IDENTITY_MAX: usize = 6;

/// How much retrieved evidence the chat prompt carries: six excerpts of ~600 chars (unchanged
/// by the 2026-10-14 work; the owner kept the budget).
pub const CHAT_EVIDENCE_HITS: usize = 6;
pub const CHAT_EVIDENCE_CHARS: usize = 600;

/// What retrieval decided for a chat turn.
#[derive(Debug)]
pub enum ChatRetrieval {
    /// Ask which thread a reference means (the titles, best first). [`Db::chat_retrieve`] no
    /// longer settles a tie this way (Goldfish parity): it answers about the top thread and the
    /// prompt has the model name it. Kept for callers that still match on it.
    Clarify(Vec<String>),
    /// The pack retrieval produced, ready for [`Db::chat_grounding`].
    Ready(ContextPack),
}

fn clip(text: &str, max: usize) -> String {
    let text = text.trim();
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

/// `text` through the confidence gate as one block line, or `None` for a Low row.
fn gated(text: &str, confidence: f64) -> Option<String> {
    match treat_fact(&clip(text, LINE_MAX_CHARS), confidence) {
        Treatment::Fact(s) | Treatment::Possible(s) => Some(s),
        Treatment::Excluded => None,
    }
}

impl Db {
    /// The retrieval query for a chat turn: the message, plus the previous turn's topic when the
    /// message names nothing on its own (a follow-up). Time words are recognised by the same
    /// parser the search window uses, so "yesterday" in an earlier turn never narrows this one.
    pub fn chat_retrieval_query(&self, message: &str, history: &[HistoryTurn]) -> String {
        let now = self.now_ms();
        let days = local_day_bounds(now);
        crate::chat_grounding::retrieval_query(message, history, |term| {
            shogun_memory::search::query_time_window(term, now, days).is_some()
        })
    }

    /// Retrieval for one chat turn. The query is [`Self::chat_retrieval_query`] (the message,
    /// plus the previous turn's topic for a follow-up). A referring question with no
    /// conversation behind it ("how's that going?") is resolved to a thread first; when two
    /// threads tie, the top one is answered and the answer names it (the chat preamble), rather
    /// than a "which one?" question. With a conversation, the conversation is the referent.
    /// Local only, inside the retrieval budget.
    pub fn chat_retrieve(&self, message: &str, history: &[HistoryTurn]) -> ChatRetrieval {
        use shogun_memory::thread::Referent;
        let mut query = self.chat_retrieval_query(message, history);
        let mut resolved_threads: Vec<String> = Vec::new();
        let has_conversation = history.iter().any(|t| t.role == crate::chat_prompt::Role::User);
        if !has_conversation && shogun_memory::thread::is_referring(message) {
            let outcome = self.resolve_referent(message, None);
            match outcome.verdict {
                // A tie answers about the top thread; the preamble has the model name it.
                Referent::Ambiguous | Referent::Resolved => {
                    // Fold the resolved thread's own words into the query so retrieval has
                    // something to match on.
                    if let Some(t) = outcome.candidates.first().and_then(|c| c.title.as_deref()) {
                        query.push(' ');
                        query.push_str(t);
                    }
                    if let Some(c) = outcome.candidates.first() {
                        resolved_threads.push(c.thread_key.clone());
                    }
                }
                Referent::None => {}
            }
        }
        let pack = match self.compression_config() {
            Some(cfg) if cfg.enabled => {
                self.assemble_context_compressed(&query, CHAT_EVIDENCE_HITS, CHAT_EVIDENCE_CHARS, &resolved_threads, cfg).0
            }
            _ => self.assemble_context(&query, CHAT_EVIDENCE_HITS, CHAT_EVIDENCE_CHARS),
        };
        ChatRetrieval::Ready(self.with_window_items(message, pack))
    }

    /// "What happened yesterday?" names a time and nothing to match: search keeps only lexical
    /// matches, so such a question retrieves nothing on its own. When the message itself names a
    /// past window (the search window parser, `query_time_window`), fill the free evidence slots
    /// with what happened in it, newest first. Never beyond [`CHAT_EVIDENCE_HITS`].
    fn with_window_items(&self, message: &str, mut pack: ContextPack) -> ContextPack {
        let now = self.now_ms();
        let Some((from, to)) = shogun_memory::search::query_time_window(message, now, local_day_bounds(now)) else {
            return pack;
        };
        let to = to.min(now);
        if from >= to || pack.evidence.len() >= CHAT_EVIDENCE_HITS {
            return pack;
        }
        for item in self.events_in_window(from, to, None) {
            if pack.evidence.len() >= CHAT_EVIDENCE_HITS {
                break;
            }
            if pack.evidence.iter().any(|e| e.event_id == item.event_id) {
                continue;
            }
            pack.evidence.push(item);
        }
        pack
    }

    /// "Now: 2026-10-14 (Wednesday) 09:00 UTC+09:00 …" for this turn.
    pub fn chat_now_line(&self) -> String {
        let now = self.now_ms();
        now_line(now, os_gmt_offset_secs(now))
    }

    /// The structured state block: the user's own details (never gated), then commitments with due
    /// dates, open loops, people, projects and the next days of the synced calendar. ≥ 0.5
    /// confidence only; Medium rows `possibly:`.
    /// Empty when nothing is tracked. Deterministic for the same rows (cache-friendly).
    pub fn chat_structured_block(&self) -> String {
        let now = self.now_ms();
        let offset = |ms: i64| os_gmt_offset_secs(ms);
        let rows = self.with_conn("chat.structured_block", |conn| {
            Ok::<_, rusqlite::Error>((
                state::list_commitments(conn)?,
                state::list_open_loops(conn)?,
                state::list_people(conn)?,
                state::list_projects(conn)?,
                summaries_by_id(conn, "people", "relationship_summary")?,
                summaries_by_id(conn, "projects", "summary")?,
                shogun_memory::self_identity::own_details(conn)?,
            ))
        });
        let Ok((commitments, open_loops, people, projects, person_notes, project_notes, own)) = rows else {
            return String::new();
        };
        // First and outside the budget, so it is never cut. The user's own name and addresses are
        // facts, never hedged (owner 2026-10-16: "whats my email" must not answer "I can't
        // verify"); then their identity profile (About You), source-backed across days.
        let mut user = own_lines(&own);
        let about = self.generated_about_profile().unwrap_or_default();
        user.extend(about.identity_lines().iter().take(IDENTITY_MAX).map(|line| clip(line, LINE_MAX_CHARS)));
        let mut sections: Vec<(&str, Vec<String>)> = Vec::new();

        let mut lines = Vec::new();
        for c in commitments.iter().filter(|c| c.status != "done" && c.status != "cancelled" && !c.is_deferred(now)) {
            let mut text = format!("you committed: {}", c.description);
            if let Some(due) = c.due_at {
                text.push_str(&format!(" — due {} ({})", local_date(due, offset(due)), local_weekday(due, offset(due))));
            }
            if c.status == "overdue" {
                text.push_str(" — overdue");
            }
            lines.extend(gated(&text, c.confidence));
            if lines.len() == COMMITMENTS_MAX {
                break;
            }
        }
        sections.push(("Commitments", lines));

        let mut lines = Vec::new();
        for l in open_loops.iter().filter(|l| l.status != "closed" && !l.is_deferred(now)) {
            let text = format!("open loop ({}): {}", l.kind.replace('_', " "), l.description);
            lines.extend(gated(&text, wording_confidence(l.confidence, l.inferred)));
            if lines.len() == OPEN_LOOPS_MAX {
                break;
            }
        }
        sections.push(("Open loops", lines));

        let mut lines = Vec::new();
        for p in &people {
            let mut text = p.display_name.clone();
            if let Some(note) = person_notes.get(&p.id).filter(|n| !n.trim().is_empty()) {
                text.push_str(" — ");
                text.push_str(note);
            }
            lines.extend(gated(&text, p.confidence));
            if lines.len() == PEOPLE_MAX {
                break;
            }
        }
        sections.push(("People", lines));

        let mut lines = Vec::new();
        for p in projects.iter().filter(|p| p.status != "done") {
            let mut text = format!("{} ({})", p.name, p.status);
            if let Some(note) = project_notes.get(&p.id).filter(|n| !n.trim().is_empty()) {
                text.push_str(" — ");
                text.push_str(note);
            }
            lines.extend(gated(&text, p.confidence));
            if lines.len() == PROJECTS_MAX {
                break;
            }
        }
        sections.push(("Projects", lines));

        // The user's own calendar is a fact, never hedged (as on Today's "Coming up").
        let lines = self
            .calendar_agenda(now)
            .unwrap_or_default()
            .into_iter()
            .take(CALENDAR_MAX)
            .map(|e| calendar_line(e.title.as_deref(), e.start_ms, e.end_ms, e.all_day, &offset))
            .collect();
        sections.push(("Calendar, next 7 days", lines));

        let render = |title: &str, lines: Vec<String>| {
            let mut section = format!("{title}:\n");
            for line in lines {
                section.push_str("- ");
                section.push_str(&line);
                section.push('\n');
            }
            section
        };
        let mut out = String::new();
        if !user.is_empty() {
            out.push_str(&render("The user (their own details)", user));
        }
        let mut used = 0;
        for (title, lines) in sections.into_iter().filter(|(_, lines)| !lines.is_empty()) {
            let section = render(title, lines);
            if used + section.chars().count() > STRUCTURED_BLOCK_MAX_CHARS {
                break;
            }
            used += section.chars().count();
            out.push_str(&section);
        }
        if out.is_empty() {
            return out;
        }
        format!("What SHOGUN tracks about their work (\"possibly:\" = unconfirmed):\n{out}")
    }

    /// Assemble the memory half of a chat prompt from the pack retrieval produced: the
    /// structured block in place of the commitment / open-loop fact lines, the turn's clock, and
    /// the evidence numbered `E1…` and dated, all within [`MEMORY_PROMPT_BUDGET_CHARS`].
    pub fn chat_grounding(&self, pack: &ContextPack) -> ChatGrounding {
        ChatGrounding::assemble(pack, &self.chat_structured_block(), self.chat_now_line(), os_gmt_offset_secs)
    }
}

/// `name: Ada Example (also goes by adaex)`, `email: …`, `handles: …`; at most five of each.
fn own_lines(own: &shogun_memory::self_identity::OwnDetails) -> Vec<String> {
    let list = |v: &[String]| v.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
    let mut lines = Vec::new();
    if let Some(name) = own.name.as_deref() {
        let mut line = format!("name: {name}");
        if !own.aliases.is_empty() {
            line.push_str(&format!(" (also goes by {})", list(own.aliases.as_slice())));
        }
        lines.push(clip(&line, LINE_MAX_CHARS));
    }
    for (label, values) in [("email", &own.emails), ("handles", &own.handles)] {
        if !values.is_empty() {
            lines.push(clip(&format!("{label}: {}", list(values.as_slice())), LINE_MAX_CHARS));
        }
    }
    lines
}

/// `2026-10-15 (Thursday) 10:00–11:00 Design review` (an all-day event has no times).
pub(super) fn calendar_line(
    title: Option<&str>,
    start_ms: i64,
    end_ms: i64,
    all_day: bool,
    offset: &impl Fn(i64) -> i32,
) -> String {
    let title = title.map(|t| clip(t, LINE_MAX_CHARS)).unwrap_or_else(|| "(untitled)".to_string());
    if all_day {
        // All-day rows are UTC midnights: their date is the date, whatever the zone.
        return format!("{} ({}) all day {title}", local_date(start_ms, 0), local_weekday(start_ms, 0));
    }
    let off = offset(start_ms);
    format!(
        "{} ({}) {}–{} {title}",
        local_date(start_ms, off),
        local_weekday(start_ms, off),
        local_time(start_ms, off),
        local_time(end_ms, offset(end_ms)),
    )
}

/// `id → note` for one of the state tables' free-text columns (the row structs omit them).
fn summaries_by_id(
    conn: &Connection,
    table: &str,
    column: &str,
) -> Result<std::collections::HashMap<i64, String>, rusqlite::Error> {
    // Both names are compile-time constants from this file, never user input.
    let mut stmt = conn.prepare(&format!("SELECT id, {column} FROM {table} WHERE {column} IS NOT NULL"))?;
    // A page's first line is its summary; the claim bullets below it would eat the block's budget.
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?.lines().next().unwrap_or_default().to_string()))
    })?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use shogun_memory::state::{
        CommitmentDirection, CommitmentStatus, NewCommitment, NewPerson, Provenance,
    };

    fn db() -> Db {
        Db::open_in_memory(std::sync::Arc::new(|| 20_740 * 86_400_000 + 9 * 3_600_000)).expect("db")
    }

    fn event(db: &Db, body: &str) -> i64 {
        db.capture(&shogun_memory::event_log::NewEvent {
            ts: 20_740 * 86_400_000,
            source: "slack",
            kind: "message",
            app_bundle_id: None,
            window_title: Some("#t"),
            content: body,
            content_hash: body,
            dwell_ms: 0,
            display_id: None,
            window_bounds: None,
        })
        .expect("capture")
        .0
    }

    #[test]
    fn structured_block_gates_every_row_by_confidence() {
        let db = db();
        let e = event(&db, "grounding event");
        let commit = |description: &str, confidence: f64| {
            db.insert_commitment(
                &NewCommitment { direction: CommitmentDirection::Mine, counterparty_id: None, description,
                    due_at: Some(20_741 * 86_400_000), status: CommitmentStatus::Open, project_id: None,
                    confidence, now: 1 },
                &[Provenance::new(e)],
            )
            .expect("commitment");
        };
        commit("send the checklist", 0.9);
        commit("review the budget", 0.6);
        commit("call the bank", 0.3);
        db.insert_person(
            &NewPerson { display_name: "Ghost", aliases_json: None, emails_json: None, handles_json: None,
                relationship_summary: None, confidence: 0.2, now: 1 },
            &[Provenance::new(e)],
        )
        .expect("person");
        let block = db.chat_structured_block();
        assert!(block.contains("- you committed: send the checklist — due 2026-10-15 (Thursday)"), "{block}");
        assert!(block.contains("- possibly: you committed: review the budget"), "{block}");
        assert!(!block.contains("call the bank"), "{block}");
        assert!(!block.contains("Ghost"), "{block}");
        assert!(block.chars().count() <= STRUCTURED_BLOCK_MAX_CHARS + 80);
        assert_eq!(block, db.chat_structured_block(), "deterministic for the prompt cache");
    }

    #[test]
    fn the_users_own_details_come_first_and_unhedged() {
        let db = db();
        assert!(!db.chat_structured_block().contains("The user"), "nothing known, no section");
        db.with_conn("t", |c| {
            c.execute(
                "INSERT INTO people (display_name, aliases, handles, person_class, confidence, base_confidence, created_at, updated_at, last_evidence_at)
                 VALUES ('Ada Example', '[\"adaex\"]', '[\"@ada\"]', 'self', 0.6, 0.6, 1, 1, 1)",
                [],
            )
        })
        .expect("self row");
        db.add_mail_identity("ada@example.com");
        let block = db.chat_structured_block();
        let user = "The user (their own details):\n- name: Ada Example (also goes by adaex)\n- email: ada@example.com\n- handles: @ada\n";
        assert!(block.contains(user), "{block}");
        assert!(!block.contains("possibly: name"), "{block}");
        assert_eq!(block.find("The user"), block.find('\n').map(|i| i + 1), "first section: {block}");
        assert!(!block.contains("People:"), "the user is not one of their own contacts: {block}");
    }

    #[test]
    fn the_identity_profile_follows_the_users_details_once() {
        let db = db();
        db.with_conn("t", |c| {
            c.execute(
                "INSERT INTO people (display_name, person_class, confidence, base_confidence, created_at, updated_at, last_evidence_at)
                 VALUES ('Ada Example', 'self', 0.6, 0.6, 1, 1, 1)",
                [],
            )?;
            c.execute(
                "INSERT INTO about_profile_generation (id, summary, sources_json, state, active_days, created_at, updated_at)
                 VALUES (1, ?1, '[]', 'complete', 5, 1, 1)",
                [r#"{"about":[{"section":"tools_stack","text":"Works in Rust and TypeScript.","basis":"fact","evidence":[{"source_id":1,"quote":"Rust and TypeScript"}]},{"section":"role_focus","text":"Builds the Harbor relay.","basis":"fact","evidence":[{"source_id":1,"quote":"Harbor relay"}]}]}"#],
            )
        })
        .expect("profile");
        let block = db.chat_structured_block();
        let user = "The user (their own details):\n- name: Ada Example\n- Role & focus: Builds the Harbor relay.\n- Tools & stack: Works in Rust and TypeScript.\n";
        assert!(block.contains(user), "{block}");
        assert_eq!(block.matches("Harbor relay").count(), 1, "{block}");
    }

    #[test]
    fn a_reply_saved_as_a_note_is_not_read_back_as_the_users_words() {
        use shogun_mcp::memory_notes_api::NoteChange;
        let db = db();
        for text in [
            "I can use these details, but I can\u{2019}t confirm they\u{2019}ve been saved to memory. To save them, send: my email is old@example.com",
            "User confirms ada@example.com is their email address.",
        ] {
            db.memory_notes_change(NoteChange::Save { text: text.into() }).expect("note");
        }
        let facts = db.assemble_context("email", CHAT_EVIDENCE_HITS, CHAT_EVIDENCE_CHARS).facts;
        assert!(facts.iter().any(|f| f == "user-authored memory: User confirms ada@example.com is their email address."), "{facts:?}");
        assert!(!facts.iter().any(|f| f.contains("old@example.com")), "{facts:?}");
        assert!(!db.recent_note_texts().iter().any(|t| t.contains("old@example.com")));
    }

    #[test]
    fn grounding_numbers_dates_and_replaces_state_fact_lines() {
        let db = db();
        let pack = ContextPack {
            facts: vec!["you committed: x".into(), "possibly: open loop: y".into(), "user-authored memory: z".into()],
            evidence: vec![Evidence { event_id: 4, ts: 20_739 * 86_400_000, source: "gmail".into(),
                title: Some("Re: plan".into()), excerpt: "moved".into(), frame_id: None }],
            screen_frames: Vec::new(),
        };
        let g = db.chat_grounding(&pack);
        assert!(g.context().starts_with("Now: 2026-10-14 (Wednesday)"), "{}", g.context());
        assert!(g.context().contains("- [E1 · mail · 2026-10-13 · Re: plan] moved"), "{}", g.context());
        assert_eq!(g.ledger.len(), 1);
        assert_eq!(g.retrieved, 1);
        // With nothing tracked, the fact lines stay: they are all the state there is.
        assert!(g.stable_context.contains("you committed: x"));
        // With a structured block, the block carries state and the fact lines go.
        let g = ChatGrounding::assemble(&pack, "Commitments:\n- you committed: x\n", String::new(), |_| 0);
        assert_eq!(g.stable_context.matches("you committed: x").count(), 1);
        assert!(!g.stable_context.contains("open loop: y"));
        assert!(g.stable_context.contains("user-authored memory: z"));
    }

    #[test]
    fn calendar_lines_are_local_and_all_day_lines_keep_their_date() {
        let start = 20_741 * 86_400_000 + 3_600_000;
        let line = calendar_line(Some("Design review"), start, start + 3_600_000, false, &|_| 9 * 3600);
        assert_eq!(line, "2026-10-15 (Thursday) 10:00–11:00 Design review");
        let line = calendar_line(Some("Holiday"), 20_741 * 86_400_000, 20_742 * 86_400_000, true, &|_| -8 * 3600);
        assert_eq!(line, "2026-10-15 (Thursday) all day Holiday");
    }
}
