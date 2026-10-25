//! The frozen core tool set (plan §7.1, E4-T3 / E4-T4 / E4-T5; CLAUDE.md invariant 11).
//!
//! One table names every core tool with the description an agent reads in `tools/list`. The
//! descriptions are written for agents, not people: each names the task it answers, what it does
//! not do, and its cost class. The MCP annotations (`readOnlyHint`, `destructiveHint`,
//! `idempotentHint`, `openWorldHint`) are **generated** from the tool's level by
//! [`annotations`], never written per tool, so they cannot drift from what the gate enforces.
//! `docs/api/core-tools.md` is rendered from this module ([`render_core_tools_markdown`]) and the
//! snapshot test `tests/wire_freeze.rs` fails when a name, an input property or an annotation
//! changes: a changed tool gets a new name and a deprecation note
//! ([`crate::memory_api::tool_deprecation`]), never an in-place edit.

use serde_json::{json, Value};
use shogun_agents::permission::Level;

use crate::memory_api::{tool_deprecation, tool_group, tool_level, ApiLevel, Tool, ToolGroup, ALL_TOOLS};

/// The metering unit of a call, as the gateway classifies it from `Mcp-Method` / `Mcp-Name`
/// (`apps/api/src/pricing.ts`). Reads are free (invariant 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostClass {
    /// Counted, never billed: every read.
    Free,
    /// Per event appended: `memory.ingest`, `memory.append_note`.
    Ingest,
    /// A router / action run: `actions.execute`.
    Run,
    /// A model-invoking operation: any other write.
    ModelOp,
}

impl CostClass {
    /// The unit name the pricing table uses.
    pub fn as_str(self) -> &'static str {
        match self {
            CostClass::Free => "free",
            CostClass::Ingest => "ingest",
            CostClass::Run => "run",
            CostClass::ModelOp => "model_op",
        }
    }
}

/// The cost class of any tool, from its level and the two ingest writes.
pub fn cost_class(tool: Tool) -> CostClass {
    match tool_level(tool) {
        ApiLevel::Read => CostClass::Free,
        ApiLevel::PerAction => CostClass::Run,
        ApiLevel::Write(_) if matches!(tool, Tool::MemoryAppendNote | Tool::MemoryIngest) => CostClass::Ingest,
        ApiLevel::Write(_) => CostClass::ModelOp,
    }
}

/// One core tool: the wire name is [`Tool::wire_name`], the level [`tool_level`], the group
/// [`ToolGroup::Core`] by [`tool_group`]; only the agent-facing description lives here.
#[derive(Debug, Clone, Copy)]
pub struct CoreTool {
    pub tool: Tool,
    /// The `tools/list` description: the task, the exclusions, the cost class.
    pub description: &'static str,
}

/// The core set, in `tools/list` order. Frozen by `tests/wire_freeze.rs`.
pub const CORE: &[CoreTool] = &[
    CoreTool {
        tool: Tool::MemorySearch,
        description: "Find where something appears in the user's memory (screen text, mail, meetings, notes, people, projects, summaries) by keyword or topic. Returns ranked hits with source ids and confidence: an index, not evidence; open hits with memory.get_events before quoting. Not for \"what is true now\" (memory.recall) and not the web. Narrow with a distinctive query, from_ms/to_ms (unix ms) and source_kind; limit 1-50 (default 20); depth all needs both bounds. Free.",
    },
    CoreTool {
        tool: Tool::MemoryGetContext,
        description: "What the user is doing right now on their device: the current app/window session with its latest captured text, the recent sessions, key facts and notes. Empty when no capture source feeds this store. Not a search (memory.recall) and not an earlier span (memory.read_session). Free.",
    },
    CoreTool {
        tool: Tool::MemoryGetContextPack,
        description: "One grounded pack for a person, project or task: confidence-gated facts plus dated, attributed evidence lines with event ids, the same assembly the in-app chat uses. Moderate size; prefer memory.recall when you have a token budget. Not the current screen (memory.get_context). Free.",
    },
    CoreTool {
        tool: Tool::MemoryGetEvents,
        description: "Open the source text behind a search hit, a pack line or recall evidence: up to 20 event ids, each with time, app/window, source and its stored text (max_chars each, default 8000). Unknown ids come back under missing. Free.",
    },
    CoreTool {
        tool: Tool::MemoryReadSession,
        description: "What happened over a span of at most 12 hours: captured rows merged into app/window sessions with start, end, duration, latest text and event ids to open. Optional app filter (name or bundle id). Not a summary (memory.get_periods). Free.",
    },
    CoreTool {
        tool: Tool::MemoryGetWrap,
        description: "Today's wrap-up as the user sees it: outcome counts, still-open items, tomorrow's first items and loose ends. Small. For the narrative of the day use memory.get_periods. Free.",
    },
    CoreTool {
        tool: Tool::MemoryGetPeriods,
        description: "Stored hour/day/week/month summaries of a time window (from_ms/to_ms, unix ms; default the last 24 h): use for \"today\", \"yesterday\", \"this week\", \"between 2 and 4pm\". Summaries are an index: open sources with memory.search or memory.get_events before quoting. Up to page_size periods (default 24, max 48). Free.",
    },
    CoreTool {
        tool: Tool::MemoryRecall,
        description: "What memory holds on a question, cut to budget_tokens: High-confidence facts as facts, Medium ones prefixed \"possibly:\", then dated evidence with event ids until the budget is spent. The right first call before acting on the user's work. Reads only this principal's store, never the web; include_scratch adds your own scratch namespace. Free.",
    },
    CoreTool {
        tool: Tool::CalendarUpcoming,
        description: "The user's upcoming calendar from now through the next 7 days (default 10 events, page_size up to 100): title, start/end (unix ms), all_day, location, attendee count, local day. Calendar facts, never hedged. Empty when no calendar is connected. Free.",
    },
    CoreTool {
        tool: Tool::ProfileWhoami,
        description: "Who the user is: identity (name, email, role, tools, habits, style; read first), the declared profile, the About You projection and a work summary, with guesses under work.inferred. Free.",
    },
    CoreTool {
        tool: Tool::StateCommitmentsList,
        description: "What the user committed to: open commitments with description, due date, status, confidence and the quote that proves each one. Rows marked possibly are Medium confidence; say so, never act on them externally. Newest first; limit/offset. Free.",
    },
    CoreTool {
        tool: Tool::StateOpenLoopsList,
        description: "What the user is waiting on or has left unresolved: open loops with description, status, confidence and the proving quote. Rows marked possibly are Medium confidence. Newest first; limit/offset. Free.",
    },
    CoreTool {
        tool: Tool::StatePeopleList,
        description: "The people the user works with, ranked by engagement: id, display name, confidence. Follow with state.people.get for one person's evidence sources. Free.",
    },
    CoreTool {
        tool: Tool::StateProjectsList,
        description: "The projects the user is on, ranked by engagement: id, name, confidence. Follow with state.projects.get for one project's evidence sources. Free.",
    },
    CoreTool {
        tool: Tool::StatePeopleGet,
        description: "One person by id: display name, confidence and where the evidence comes from (sources and counts, never content). not_found for an unknown id. Free.",
    },
    CoreTool {
        tool: Tool::StateProjectsGet,
        description: "One project by id: name, confidence and where the evidence comes from (sources and counts, never content). not_found for an unknown id. Free.",
    },
    CoreTool {
        tool: Tool::WikiEvidence,
        description: "The source quotes behind a person or project page (query: the name or key): dated excerpts with event ids. Use before quoting a fact from a page or a pack. Free.",
    },
    CoreTool {
        tool: Tool::ActionsStatus,
        description: "Poll one L3 approval by approval_id: pending, sent, rejected, timed_out, send_failed, draft_saved or outcome_unknown. Never claim an action happened before this answers sent. Free.",
    },
    CoreTool {
        tool: Tool::MemoryAppendNote,
        description: "Append one note worth remembering (text, at most 2000 characters): a durable L1 write to the user's own memory, deduplicated, nothing sent anywhere. Not for conversation transcripts or batches (memory.ingest). Metered per event (ingest).",
    },
    CoreTool {
        tool: Tool::MemoryIngest,
        description: "Hand over what you learned so the next agent starts from it: events[] (ts_ms, kind, title, content) or turns[] (role, content) under a source, an actor and a thread_key, with a required idempotency_key; the same key replays the first result and adds nothing. Text is sanitized, secrets are masked, exact duplicates collapse, then the rows join the event log for the hourly review that updates state. Local L1 write; nothing leaves the device. Not for verbatim transcripts of whole sessions: ingest the turns that changed something. Metered per event (ingest).",
    },
    CoreTool {
        tool: Tool::ActionsExecute,
        description: "Run an action for the user. kind local_search runs now (L1) and returns results; open_app, reveal_file, show_notification and copy_to_clipboard act on the user's Mac and run only from the ShogunAI app (refused here, nothing done); send_email, post_message and post_comment never run here: they queue for the user's explicit approval (L3) and answer resultType approval_required with an approval URL and approval_id to poll with actions.status. Never report a send as done before actions.status says sent. Metered as a run.",
    },
];

/// Whether `tool` is in the core set.
pub fn is_core(tool: Tool) -> bool {
    CORE.iter().any(|core| core.tool == tool)
}

/// The agent-facing description of a core tool; `None` outside the core set.
pub fn description(tool: Tool) -> Option<&'static str> {
    CORE.iter().find(|core| core.tool == tool).map(|core| core.description)
}

/// The MCP tool annotations for a level (E4-T4): a pure function of [`ApiLevel`], the same for
/// every tool at that level, so the hints a client sees before calling say exactly what the gate
/// will do.
///
/// | level | readOnly | destructive | idempotent | openWorld |
/// |---|---|---|---|---|
/// | read | true | false | true | false |
/// | write L1 / L2 | false | false | false | false |
/// | write L3 / per action | false | true | false | true |
///
/// L3 is the only tier whose effect leaves the device (a send, a post, a privacy boundary
/// removed), so it is the only one marked destructive and open-world; `actions.execute` carries
/// its ceiling. L1 and L2 writes stay local and reversible.
pub fn annotations(level: ApiLevel) -> Value {
    let (read_only, destructive, idempotent, open_world) = match level {
        ApiLevel::Read => (true, false, true, false),
        ApiLevel::Write(Level::L1) | ApiLevel::Write(Level::L2) => (false, false, false, false),
        ApiLevel::Write(Level::L3) | ApiLevel::PerAction => (false, true, false, true),
    };
    json!({
        "readOnlyHint": read_only,
        "destructiveHint": destructive,
        "idempotentHint": idempotent,
        "openWorldHint": open_world,
    })
}

/// The level label the docs use.
pub fn level_label(level: ApiLevel) -> &'static str {
    match level {
        ApiLevel::Read => "read",
        ApiLevel::Write(Level::L1) => "write L1",
        ApiLevel::Write(Level::L2) => "write L2",
        ApiLevel::Write(Level::L3) => "write L3",
        ApiLevel::PerAction => "per action (L1 / L3 by kind)",
    }
}

/// `docs/api/core-tools.md`, rendered from the table above and the live descriptors so the human
/// doc derives from the code (invariant 12). `tests/wire_freeze.rs` fails when the file drifts.
pub fn render_core_tools_markdown() -> String {
    let mut out = String::new();
    out.push_str("# Core tools (frozen wire names)\n\n");
    out.push_str("<!-- Generated by `shogun_mcp::core_tools::render_core_tools_markdown`; `cargo test -p shogun-mcp --test wire_freeze` fails when this file is edited by hand. -->\n\n");
    out.push_str(&format!(
        "The core set is the {} tools every ShogunAgent face lists under `SHOGUN_MCP_TOOLSETS=core`, in this order. Their wire names, input property names and annotations are frozen (CLAUDE.md invariant 11): a change is a new name plus a note in `docs/api/deprecation-policy.md`, never an edit in place. Annotations are generated from the level (`shogun_mcp::core_tools::annotations`); the cost class is what the gateway meters from `Mcp-Method` / `Mcp-Name` (reads are free, invariant 10).\n\n",
        CORE.len()
    ));
    out.push_str("| # | Tool | Level | Cost | Annotations | Input properties | Description |\n");
    out.push_str("|---|---|---|---|---|---|---|\n");
    for (index, core) in CORE.iter().enumerate() {
        let level = tool_level(core.tool);
        let hints = annotations(level);
        let descriptor = crate::mcp::tool_descriptor(core.tool);
        let mut props: Vec<&str> = descriptor["inputSchema"]["properties"]
            .as_object()
            .map(|properties| properties.keys().map(String::as_str).collect())
            .unwrap_or_default();
        props.sort_unstable();
        let props = if props.is_empty() { "—".to_string() } else { props.join(", ") };
        let flags = ["readOnlyHint", "destructiveHint", "idempotentHint", "openWorldHint"]
            .iter()
            .filter(|key| hints[*key] == Value::Bool(true))
            .map(|key| key.to_string())
            .collect::<Vec<_>>();
        let flags = if flags.is_empty() { "—".to_string() } else { flags.join(", ") };
        out.push_str(&format!(
            "| {} | `{}` | {} | {} | {} | {} | {} |\n",
            index + 1,
            core.tool.wire_name(),
            level_label(level),
            cost_class(core.tool).as_str(),
            flags,
            props,
            core.description.replace('|', "\\|"),
        ));
    }
    out.push_str("\nDeprecated core tools: ");
    let deprecated: Vec<String> = CORE
        .iter()
        .filter_map(|core| tool_deprecation(core.tool).map(|note| format!("`{}` ({note})", core.tool.wire_name())))
        .collect();
    if deprecated.is_empty() {
        out.push_str("none.\n");
    } else {
        out.push_str(&deprecated.join("; "));
        out.push_str(".\n");
    }
    out
}

/// Every tool `tool_group` puts in core, for the tests that check the table and the group agree.
pub fn core_group_tools() -> Vec<Tool> {
    ALL_TOOLS.iter().copied().filter(|tool| tool_group(*tool) == ToolGroup::Core).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_and_the_core_group_name_the_same_tools() {
        let table: Vec<Tool> = CORE.iter().map(|core| core.tool).collect();
        for tool in &table {
            assert_eq!(tool_group(*tool), ToolGroup::Core, "{}", tool.wire_name());
        }
        for tool in core_group_tools() {
            assert!(is_core(tool), "{} is in the core group but not in the table", tool.wire_name());
        }
        let mut names: Vec<&str> = table.iter().map(|tool| tool.wire_name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), table.len(), "a core tool is listed twice");
    }

    #[test]
    fn annotations_are_a_pure_function_of_the_level_for_every_tool() {
        for &tool in ALL_TOOLS {
            let level = tool_level(tool);
            let hints = annotations(level);
            assert_eq!(hints, annotations(level), "{}", tool.wire_name());
            assert_eq!(hints["readOnlyHint"], Value::Bool(level == ApiLevel::Read), "{}", tool.wire_name());
            let l3 = matches!(level, ApiLevel::Write(Level::L3) | ApiLevel::PerAction);
            assert_eq!(hints["destructiveHint"], Value::Bool(l3), "{}", tool.wire_name());
            assert_eq!(hints["openWorldHint"], Value::Bool(l3), "{}", tool.wire_name());
            assert_eq!(hints["idempotentHint"], Value::Bool(level == ApiLevel::Read), "{}", tool.wire_name());
        }
    }

    #[test]
    fn every_core_description_names_its_cost_class() {
        for core in CORE {
            let expected = match cost_class(core.tool) {
                CostClass::Free => "Free.",
                CostClass::Ingest => "(ingest).",
                CostClass::Run => "as a run.",
                CostClass::ModelOp => "model",
            };
            assert!(core.description.ends_with(expected), "{}: {}", core.tool.wire_name(), core.description);
        }
    }

    #[test]
    fn cost_classes_match_the_gateway_table() {
        assert_eq!(cost_class(Tool::MemoryRecall), CostClass::Free);
        assert_eq!(cost_class(Tool::StateCommitmentsList), CostClass::Free);
        assert_eq!(cost_class(Tool::MemoryIngest), CostClass::Ingest);
        assert_eq!(cost_class(Tool::MemoryAppendNote), CostClass::Ingest);
        assert_eq!(cost_class(Tool::ActionsExecute), CostClass::Run);
        assert_eq!(cost_class(Tool::StateUpdate), CostClass::ModelOp);
    }
}
