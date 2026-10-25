//! `SHOGUN_MCP_TOOLSETS` (issue #410, `docs/specs/mcp-tool-surface.md`): which tool groups
//! the MCP `tools/list` response shows.
//!
//! This only narrows what a client **sees**. `tools/call` still resolves every wire name, levels
//! and approvals are untouched, and the REST / CLI faces never read it (invariant 6). Hidden is not
//! disabled.
//!
//! Value: a comma-separated list of group ids ([`crate::memory_api::ToolGroup::id`]) or `all`.
//! `core` is always listed. Unset or blank means `all` for the library; the stdio binary defaults to `core` (see [`Toolsets::for_stdio`]). `full` is a synonym of `all`.
//! Unknown ids are ignored with one stderr warning that names only how many there were, never
//! the text itself.

use std::sync::Once;

use crate::memory_api::{tool_group, tool_is_served, Tool, ToolGroup, ALL_GROUPS};

/// The environment variable the stdio binary reads.
pub const ENV_VAR: &str = "SHOGUN_MCP_TOOLSETS";

/// The groups `tools/list` shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toolsets {
    all: bool,
    /// With `all`: list every group that exists, served by this build or not (the desktop
    /// build's `all`). For the tool-table tests; no flag or env value selects it.
    every: bool,
    groups: Vec<ToolGroup>,
}

impl Default for Toolsets {
    /// Every tool, today's behaviour.
    fn default() -> Self {
        Self::all()
    }
}

impl Toolsets {
    /// Every group this build serves ([`ALL_GROUPS`], E4-T6).
    pub fn all() -> Self {
        Self {
            all: true,
            every: false,
            groups: Vec::new(),
        }
    }

    /// Every group that exists, served or not ([`crate::memory_api::EVERY_GROUP`]): what the
    /// desktop build's [`Toolsets::all`] lists. For tests of the whole tool table; a call to a
    /// listed-but-unserved tool answers what the backend answers, as a call by name always did.
    pub fn every() -> Self {
        Self {
            all: true,
            every: true,
            groups: Vec::new(),
        }
    }

    /// Only the core group.
    pub fn core() -> Self {
        Self {
            all: false,
            every: false,
            groups: vec![ToolGroup::Core],
        }
    }

    /// Parse a `SHOGUN_MCP_TOOLSETS` value. Returns the toolsets and how many ids were unknown
    /// (and so ignored). `None`, a value that names nothing (blank, only commas), or any `all`
    /// entry lists every tool.
    pub fn parse(raw: Option<&str>) -> (Self, usize) {
        let ids: Vec<&str> = raw
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .collect();
        if ids.is_empty() {
            return (Self::all(), 0);
        }
        let mut toolsets = Self::core();
        let mut unknown = 0;
        for id in ids {
            let id = id.to_ascii_lowercase();
            if id == "all" || id == "full" {
                toolsets.all = true;
            } else if let Some(group) = ToolGroup::from_id(&id) {
                if !toolsets.groups.contains(&group) {
                    toolsets.groups.push(group);
                }
            } else {
                unknown += 1;
            }
        }
        (toolsets, unknown)
    }

    /// Read [`ENV_VAR`]. Warns on stderr at most once per process if anything was ignored.
    pub fn from_env() -> Self {
        static WARN: Once = Once::new();
        let (toolsets, unknown) = match std::env::var(ENV_VAR) {
            Ok(value) => Self::parse(Some(&value)),
            Err(std::env::VarError::NotPresent) => (Self::all(), 0),
            // Not UTF-8: nothing in it can be a group id. Keep the default (all) and say so.
            Err(std::env::VarError::NotUnicode(_)) => (Self::all(), 1),
        };
        if unknown > 0 {
            WARN.call_once(|| {
                // Only a count and the fixed group ids: the value itself is never echoed.
                eprintln!(
                    "shogun-mcp: {ENV_VAR} has {unknown} unknown group id(s), ignored. Known: {}, all",
                    known_ids()
                );
            });
        }
        toolsets
    }

    /// Whether every tool is listed.
    pub fn is_all(&self) -> bool {
        self.all
    }

    /// Whether `tools/list` shows this tool. `all` lists the groups this build serves
    /// ([`crate::memory_api::ALL_GROUPS`], E4-T6): every group on the desktop build, the
    /// server's on the default build. A named group is always one this build parsed.
    pub fn lists(&self, tool: Tool) -> bool {
        (self.all && (self.every || tool_is_served(tool))) || self.groups.contains(&tool_group(tool))
    }
}

/// The stdio argument that picks the toolset: `--toolset core|full` (or a group list).
pub const ARG: &str = "--toolset";

/// Value of `--toolset <v>` / `--toolset=<v>`, validated. Unknown ids are an error: an entry that
/// asks for something unclear should not silently list a different surface.
pub fn toolset_arg(args: impl IntoIterator<Item = String>) -> Result<Option<String>, String> {
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let value = if arg == ARG {
            args.next().ok_or_else(|| format!("{ARG} needs core, full or group ids"))?
        } else if let Some(value) = arg.strip_prefix(&format!("{ARG}=")) {
            value.to_string()
        } else {
            continue;
        };
        let (_, unknown) = Toolsets::parse(Some(&value));
        if unknown > 0 || value.trim().is_empty() {
            return Err(format!("{ARG} must be core, full or group ids ({})", known_ids()));
        }
        return Ok(Some(value));
    }
    Ok(None)
}

impl Toolsets {
    /// What the stdio binary lists: `--toolset` wins, then `SHOGUN_MCP_TOOLSETS`, else `core`.
    /// Listing only: `tools/call` still answers any tool the grant permits.
    pub fn for_stdio(flag: Option<&str>) -> Self {
        match flag {
            Some(value) => Self::parse(Some(value)).0,
            None if std::env::var_os(ENV_VAR).is_some() => Self::from_env(),
            None => Self::core(),
        }
    }
}

/// Every group id, comma-separated (for the warning and docs).
pub fn known_ids() -> String {
    ALL_GROUPS
        .iter()
        .map(|group| group.id())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_api::ALL_TOOLS;

    #[test]
    fn unset_or_blank_means_all() {
        for raw in [None, Some(""), Some("  "), Some(" , ,")] {
            let (toolsets, unknown) = Toolsets::parse(raw);
            assert!(toolsets.is_all(), "{raw:?}");
            assert_eq!(unknown, 0);
        }
        assert!(Toolsets::default().is_all());
    }

    #[cfg(feature = "desktop")]
    #[test]
    fn groups_add_to_core_and_all_wins() {
        let (toolsets, unknown) = Toolsets::parse(Some(" Mail , meeting,mail"));
        assert_eq!(unknown, 0);
        assert!(!toolsets.is_all());
        assert!(toolsets.lists(Tool::MemorySearch), "core is always listed");
        assert!(toolsets.lists(Tool::MailTriageList));
        assert!(toolsets.lists(Tool::MeetingRecordNow));
        assert!(!toolsets.lists(Tool::VoiceDictionaryList));
        let (toolsets, _) = Toolsets::parse(Some("core,all"));
        assert!(ALL_TOOLS.iter().all(|t| toolsets.lists(*t)));
    }

    #[test]
    fn served_groups_add_to_core_and_all_lists_exactly_the_served_tools() {
        let (toolsets, unknown) = Toolsets::parse(Some(" Import , state,import"));
        assert_eq!(unknown, 0);
        assert!(!toolsets.is_all());
        assert!(toolsets.lists(Tool::MemorySearch), "core is always listed");
        assert!(toolsets.lists(Tool::MemoryImportPreview));
        assert!(toolsets.lists(Tool::StateExport));
        assert!(!toolsets.lists(Tool::WikiList));
        let (toolsets, _) = Toolsets::parse(Some("core,all"));
        for &tool in ALL_TOOLS {
            assert_eq!(toolsets.lists(tool), tool_is_served(tool), "{}", tool.wire_name());
        }
    }

    /// E4-T6: the server lists core, import, state, skills and wiki and nothing the Mac app
    /// alone serves; the desktop build lists every group.
    #[test]
    fn all_lists_only_the_groups_this_build_serves() {
        let listed: Vec<ToolGroup> = ALL_TOOLS
            .iter()
            .filter(|t| Toolsets::all().lists(**t))
            .map(|t| tool_group(*t))
            .fold(Vec::new(), |mut groups, group| {
                if !groups.contains(&group) {
                    groups.push(group);
                }
                groups
            });
        for group in &listed {
            assert!(ALL_GROUPS.contains(group), "{}", group.id());
        }
        for group in [ToolGroup::Core, ToolGroup::Files, ToolGroup::Handoff, ToolGroup::Governance, ToolGroup::Import, ToolGroup::State, ToolGroup::Skills, ToolGroup::Wiki] {
            assert!(listed.contains(&group), "{}", group.id());
        }
        let desktop_only = [
            ToolGroup::Meeting,
            ToolGroup::Mail,
            ToolGroup::VisualRecall,
            ToolGroup::Voice,
            ToolGroup::Web,
            ToolGroup::Export,
            ToolGroup::Translate,
            ToolGroup::Settings,
            ToolGroup::Forms,
        ];
        for group in desktop_only {
            assert_eq!(listed.contains(&group), cfg!(feature = "desktop"), "{}", group.id());
            assert_eq!(ToolGroup::from_id(group.id()).is_some(), cfg!(feature = "desktop"), "{}", group.id());
        }
        // Every wire name still resolves on every build: hidden is not disabled.
        for &tool in ALL_TOOLS {
            assert_eq!(Tool::from_wire(tool.wire_name()), Some(tool));
        }
    }

    #[test]
    fn unknown_ids_are_counted_and_ignored() {
        let (toolsets, unknown) = Toolsets::parse(Some("import,nope,settngs"));
        assert_eq!(unknown, 2);
        assert!(toolsets.lists(Tool::MemoryImportPreview));
        assert!(!toolsets.lists(Tool::UiLanguageGet));
        let (toolsets, unknown) = Toolsets::parse(Some("nope"));
        assert_eq!((toolsets, unknown), (Toolsets::core(), 1));
    }

    #[test]
    fn every_group_id_round_trips() {
        for &group in ALL_GROUPS {
            assert_eq!(ToolGroup::from_id(group.id()), Some(group));
            let (toolsets, unknown) = Toolsets::parse(Some(group.id()));
            assert_eq!(unknown, 0, "{}", group.id());
            assert!(ALL_TOOLS
                .iter()
                .filter(|t| tool_group(**t) == group)
                .all(|t| toolsets.lists(*t)));
        }
        assert_eq!(ToolGroup::from_id("all"), None, "all is a keyword, not a group");
    }

    fn args_of(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn toolset_flag_is_validated_and_full_means_all() {
        assert_eq!(toolset_arg(args_of(&["--client", "x"])), Ok(None));
        assert_eq!(toolset_arg(args_of(&["--toolset", "full"])), Ok(Some("full".into())));
        assert_eq!(toolset_arg(args_of(&["--toolset=core,state"])), Ok(Some("core,state".into())));
        assert!(toolset_arg(args_of(&["--toolset"])).is_err());
        assert!(toolset_arg(args_of(&["--toolset", "nope"])).is_err());
        assert!(Toolsets::for_stdio(Some("full")).is_all());
        assert_eq!(Toolsets::for_stdio(Some("core")), Toolsets::core());
    }
}

