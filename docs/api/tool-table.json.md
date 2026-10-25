# `tool-table.json` — the machine-readable tool table

The one input every agent-readable surface is rendered from (CLAUDE.md invariant 12):
`/.well-known/mcp/server-card.json` (E10-T1), the MCP `tools/list` the core answers, the
`llms.txt` tool index, the read-only filter of `npx shogunagent mcp` (E10-T10), and the OpenAPI
document's tool-call section (E10-T6).

## Where it comes from

The Rust tool catalog (`crates/shogun-mcp/src/tool_catalog.rs`, E4-T4) is the source. Once E4
lands, `shogun-mcp --export-tool-table` (name to be confirmed by E4) prints this JSON and CI
regenerates `apps/api/fixtures/tool-table.json` from it; a test fails when the committed file
differs from the export. **Until then** the committed fixture is hand-maintained to the plan's
§7.1 list and is marked `"source": "fixture"`. Nothing else in the repository may list tool names
or annotations by hand.

## Shape (`shogunagent/tool-table/v1`)

```json
{
  "schema": "shogunagent/tool-table/v1",
  "source": "shogun-mcp --export-tool-table" | "fixture",
  "protocol_version": "2026-07-28",
  "toolset": "core",
  "tools": [
    {
      "name": "state.commitments.list",
      "title": "List commitments",
      "description": "…written for agents: the task, the exclusions, the cost class…",
      "level": "read" | "L1" | "L2" | "L3",
      "cost_unit": "read" | "ingest" | "model_op" | "run",
      "input_schema": { "type": "object", "properties": { … }, "required": [ … ] },
      "output_schema": { … } | null,
      "deprecated": null | { "since": "0.2.0", "replacement": "…", "removal_after": "2027-10-01" }
    }
  ]
}
```

| Field | Meaning |
|---|---|
| `name` | The frozen wire name (invariant 11). Never renamed in place. |
| `title` | Short human label (MCP `title`). |
| `description` | The agent-facing description E4 writes: task, exclusions, cost class. |
| `level` | `read` for a read; otherwise the permission tier of the write (`L1` / `L2` / `L3`). For `actions.execute` the tier depends on the action type at run time, so the table carries the **highest** tier the tool can reach (`L3`). |
| `cost_unit` | The metering unit of `apps/api/src/pricing.ts` (`classifyCall`). A test checks the two agree. |
| `input_schema` | JSON Schema of the tool arguments, as `tools/list` returns it. |
| `output_schema` | JSON Schema of `structuredContent`, or `null`. |
| `deprecated` | `null`, or the entry the deprecation policy (`docs/api/deprecation-policy.md`, E4-T5) requires. |

## Annotations are derived, never stored

MCP tool annotations are computed from `level` by one rule (`apps/api/src/wellknown/tool-table.ts`
`annotationsFor`):

| `level` | `readOnlyHint` | `destructiveHint` | `idempotentHint` | `openWorldHint` |
|---|---|---|---|---|
| `read` | `true` | `false` | `true` | `false` |
| `L1` | `false` | `false` | `true` | `false` |
| `L2` | `false` | `false` | `false` | `false` |
| `L3` | `false` | `true` | `false` | `true` |

`L3` is the only tier that leaves the device (invariant 3), so it is the only one with
`openWorldHint: true`. The SKILL.md sentence "`readOnlyHint: true` is L1; `destructiveHint: true`
with `openWorldHint: true` is L3" is a consequence of this table, not a second source.

## Validation

`validateToolTable` (same module) rejects: a missing or wrong `schema`, a duplicate name, a name
that is not `<group>.<verb>` in lowercase, a `level` or `cost_unit` outside the enums, and an
`input_schema` that is not an object schema. The CI test `wellknown.test.ts` also asserts the
fixture's names equal the §7.1 list of `docs/implementation-plan.md`.
