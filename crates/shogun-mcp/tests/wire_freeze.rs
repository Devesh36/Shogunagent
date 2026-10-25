//! `wire-freeze` (CLAUDE.md invariant 11, plan §8): the core tool set's wire names, input
//! property names, required lists and annotations are a snapshot under `tests/snapshots/`, and
//! `docs/api/core-tools.md` is generated from the same table. Either drifting fails here.
//!
//! A deliberate change (a new tool, a deprecation) regenerates both with
//! `WIRE_FREEZE_UPDATE=1 cargo test -p shogun-mcp --test wire_freeze`; the diff is the review.
//! Renaming a tool, removing a property or changing an annotation in place is never a deliberate
//! change: see `docs/api/deprecation-policy.md`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use serde_json::{json, Value};
use shogun_mcp::core_tools::{self, CORE};
use shogun_mcp::mcp::tool_descriptor;
use shogun_mcp::memory_api::{tool_group, tool_level, ToolGroup, ALL_TOOLS};

fn repo_path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn update_requested() -> bool {
    std::env::var_os("WIRE_FREEZE_UPDATE").is_some_and(|v| v == "1")
}

/// What is frozen per core tool: the wire name, its input property names (sorted), its required
/// list, its annotations, its level and cost class, and its deprecation note.
fn snapshot() -> Value {
    let tools: Vec<Value> = CORE
        .iter()
        .map(|core| {
            let descriptor = tool_descriptor(core.tool);
            let mut properties: Vec<String> = descriptor["inputSchema"]["properties"]
                .as_object()
                .map(|p| p.keys().cloned().collect())
                .unwrap_or_default();
            properties.sort();
            json!({
                "name": core.tool.wire_name(),
                "level": core_tools::level_label(tool_level(core.tool)),
                "cost": core_tools::cost_class(core.tool).as_str(),
                "properties": properties,
                "required": descriptor["inputSchema"].get("required").cloned().unwrap_or(Value::Array(Vec::new())),
                "additionalProperties": descriptor["inputSchema"].get("additionalProperties").cloned().unwrap_or(Value::Null),
                "annotations": descriptor["annotations"],
                "deprecated": descriptor.get("deprecated").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    json!({ "schema": "shogunagent/wire-freeze/v1", "core_count": tools.len(), "tools": tools })
}

fn compare_or_update(relative: &str, current: &str, what: &str) {
    let path = repo_path(relative);
    if update_requested() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, current).unwrap();
        eprintln!("wire-freeze: wrote {}", path.display());
        return;
    }
    let committed = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {} ({e}): regenerate with WIRE_FREEZE_UPDATE=1", path.display()));
    assert_eq!(
        committed, current,
        "{what} drifted from {}: a core wire name, input property, required list or annotation changed. \
         Changing one in place is forbidden (docs/api/deprecation-policy.md); a deliberate addition or \
         deprecation regenerates it with WIRE_FREEZE_UPDATE=1 cargo test -p shogun-mcp --test wire_freeze",
        path.display()
    );
}

#[test]
fn core_tool_names_schemas_and_annotations_are_frozen() {
    let mut current = serde_json::to_string_pretty(&snapshot()).unwrap();
    current.push('\n');
    compare_or_update("tests/snapshots/core_tools.json", &current, "the core tool snapshot");
}

#[test]
fn core_tools_doc_is_generated_from_the_table() {
    compare_or_update("../../docs/api/core-tools.md", &core_tools::render_core_tools_markdown(), "docs/api/core-tools.md");
}

#[test]
fn the_core_group_is_exactly_the_frozen_table_on_every_build() {
    let in_group: Vec<&str> = ALL_TOOLS
        .iter()
        .filter(|tool| tool_group(**tool) == ToolGroup::Core)
        .map(|tool| tool.wire_name())
        .collect();
    let mut in_table: Vec<&str> = CORE.iter().map(|core| core.tool.wire_name()).collect();
    let mut sorted_group = in_group.clone();
    sorted_group.sort_unstable();
    in_table.sort_unstable();
    assert_eq!(sorted_group, in_table);
    // The meeting reads left core (plan §7.1); the two new tools joined it.
    for name in ["meeting.list", "meeting.search", "meeting.recap.get", "meeting.transcript.get"] {
        assert!(!in_table.contains(&name), "{name} is not core");
    }
    for name in ["memory.ingest", "memory.recall"] {
        assert!(in_table.contains(&name), "{name} is core");
    }
}

#[test]
fn every_core_annotation_is_the_level_function() {
    for core in CORE {
        assert_eq!(
            tool_descriptor(core.tool)["annotations"],
            core_tools::annotations(tool_level(core.tool)),
            "{}",
            core.tool.wire_name()
        );
    }
}
