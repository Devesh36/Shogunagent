//! The Memory API docs quote how many tools a client sees (issue #779): all of them, the reads a
//! read-only token is shown, and the `core` toolset. A user checking a read-only mount against
//! those numbers concludes the scope filter is broken when they drift, so they are checked
//! against the tool table here rather than kept by hand.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use shogun_mcp::memory_api::{scope_allows, TokenScope, ALL_TOOLS};
use shogun_mcp::toolsets::Toolsets;

fn doc(relative: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/docs").join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

struct Counts {
    all: usize,
    reads: usize,
    core: usize,
    core_reads: usize,
}

fn counts() -> Counts {
    let core = Toolsets::core();
    let reads = |tool: &&shogun_mcp::memory_api::Tool| scope_allows(TokenScope::Read, **tool);
    Counts {
        all: ALL_TOOLS.len(),
        reads: ALL_TOOLS.iter().filter(reads).count(),
        core: ALL_TOOLS.iter().filter(|tool| core.lists(**tool)).count(),
        core_reads: ALL_TOOLS.iter().filter(|tool| core.lists(**tool)).filter(reads).count(),
    }
}

#[test]
fn the_overview_quotes_the_real_tool_and_read_counts() {
    let c = counts();
    let text = doc("memory-api/overview.mdx");
    let expected = format!("of the {} tools, it lists the {} reads", c.all, c.reads);
    assert!(text.contains(&expected), "overview.mdx should say \"{expected}\"");
}

#[test]
fn the_mcp_page_quotes_the_real_toolset_counts() {
    let c = counts();
    let text = doc("memory-api/mcp.mdx");
    for expected in [
        format!("`core` (always included) is {} everyday tools", c.core),
        format!("Under a read-only token it is {}:", c.core_reads),
        format!("lists everything: {} tools, or the {}\n  reads", c.all, c.reads),
    ] {
        assert!(text.contains(&expected), "mcp.mdx should say \"{expected}\"");
    }
}
