# Deprecation policy for the agent-facing API

Applies to every surface an agent reads or calls: the core tool set (`docs/api/core-tools.md`),
every other MCP tool name, the REST paths and the CLI subcommands that mirror them, the result
envelopes (`result`, `usage`, `latency_ms`, `next_action`), the MRTR / approval shapes
(`resultType`, `inputRequests`, `approval`), and the generated files (`server-card.json`,
`openapi.json`, `pricing.json`, `llms.txt`).

## Rules

1. **Wire names are frozen** (CLAUDE.md invariant 11). A core tool's name, the names of its input
   properties and its annotations never change in place. `tests/wire_freeze.rs` fails on any
   edit; a change ships as a **new name** plus a deprecation of the old one.
2. **Additive changes need no deprecation.** A new optional input property, a new field in a
   result, a new tool or a new group may land at any time. Agents must ignore fields they do not
   know.
3. **A deprecated tool keeps working for 12 months** from the release that deprecates it (the
   MCP 2026-07-28 window). During the window it answers exactly as before. It is removed in the
   first release after the window, and its name is never reused.
4. **A deprecation is visible on the wire.** The tool's `tools/list` descriptor carries a
   `deprecated` string (`shogun_mcp::memory_api::tool_deprecation`) of the form
   `"<since: YYYY-MM-DD>: use <replacement>; removed after <YYYY-MM-DD>"`. The description keeps
   its first sentence so an agent that already selected the tool still understands what it did.
   `docs/api/core-tools.md` lists deprecated core tools in its last line (generated).
5. **The replacement exists before the deprecation.** A deprecation note names a tool that is
   already listed, so an agent can switch in the same session.
6. **Semantics never change under a name.** When what a tool does changes (its level, its cost
   class, what it reads or writes), that is a new tool. A bug fix that makes a tool do what its
   description already said is not a change.
7. **Levels only tighten in place.** A tool may move from L1 to L2 or L3 (more confirmation)
   without a new name, because an agent that treated it as L1 is told by the MRTR / approval
   result and nothing runs without the confirmation. A tool never loosens in place: L3 → L1 is a
   new name.
8. **Error codes are additive.** A new `error.code` or `next_action` value may appear; an existing
   one keeps its meaning. Agents treat an unknown `next_action` as "stop and tell the human".
9. **REST and CLI follow the tool.** The REST path and CLI subcommand of a deprecated tool are
   deprecated and removed on the same dates; the CLI prints the note to stderr once per run.
10. **Generated surfaces regenerate.** `server-card.json`, `openapi.json` and `llms.txt` are built
    from the tool table, so a deprecation appears in all of them in the same release; none is
    edited by hand (CLAUDE.md invariant 12).

## How to deprecate a tool

1. Add the replacement tool (new `Tool` variant, wire name, level, group, description, REST path,
   CLI subcommand), with its tests.
2. Add a row to `DEPRECATED_TOOLS` in `crates/shogun-mcp/src/memory_api.rs`:
   `(Tool::Old, "2027-01-15: use new.tool; removed after 2028-01-15")`.
3. Run `WIRE_FREEZE_UPDATE=1 cargo test -p shogun-mcp --test wire_freeze` to regenerate the
   snapshot and `docs/api/core-tools.md`, and review the diff: only the `deprecated` field and the
   new tool may appear.
4. Note the deprecation in the release notes under "Deprecated", with the removal date.
5. After the removal date: remove the variant, the row, the REST path and the CLI subcommand in
   one commit; regenerate the snapshot; the name stays retired.

## Not covered

- `SHOGUN_MCP_TOOLSETS` group ids are a listing convenience, not a wire contract; a group may be
  renamed with one release of overlap.
- The handshake `instructions` text and tool descriptions are prose: they change freely, subject
  to the selection eval (E11) not regressing.
- Internal error messages after the `code:` prefix.
