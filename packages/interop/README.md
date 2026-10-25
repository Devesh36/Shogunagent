# @shogunagent/interop

ShogunAgent [export v2](../../docs/api/export-v2.md) and converters between it and other memory
shapes. Every converter returns the converted value **and a lossiness report**: what had no place
in the target (`dropped`), what was filled with a stand-in (`approximated`), and what input had no
home in a typed world model (`unmapped`). Counts only, never content. No runtime dependencies, no
network, no file access.

Export v2 is a draft proposal for the W3C AI Agent Memory Interoperability Community Group; the
CG has not adopted it.

```ts
import { parseExportV2, v2ToFlat, flatToV2, v2ToMemoriesTree, memoriesTreeToV2, exportV2ToV1 } from "@shogunagent/interop";

const exp = parseExportV2(jsonl);                 // result.jsonl of state.export {format: "export-v2"}
const { list, report } = v2ToFlat(exp);           // a flat memories list
const { tree } = v2ToMemoriesTree(exp);           // { "/memories/INDEX.md": "…", … }
const back = memoriesTreeToV2(tree, { ns: "imported-1", now: Date.now() });
```

## Shapes

| Shape | Functions | Defined as |
|---|---|---|
| export v1 | `exportV2ToV1`, `exportV1ToV2`, `parseExportV1` | [`docs/api/export-v1.md`](../../docs/api/export-v1.md) |
| flat memories list | `v2ToFlat`, `flatToV2` | `{ results: [{ id, memory, user_id, agent_id, metadata, created_at, updated_at }] }` — one text string per memory with flat scalar metadata and ISO times, modelled on the list endpoints of memory APIs that store one string per memory (Mem0-style `results[]`, supermemory-style document lists). Defined here precisely; not claimed to be any vendor's exact wire format. |
| `/memories` tree | `v2ToMemoriesTree`, `memoriesTreeToV2` | `Record<path, content>` in the layout the engine serves to Anthropic's memory tool (`memory_20250818`): `INDEX.md`, `commitments.md`, `open_loops.md`, `people/<slug>.md`, `projects/<slug>.md` (`crates/shogun-core/src/memfs/`). |

## What each conversion loses

| Conversion | Dropped | Approximated | Unmapped |
|---|---|---|---|
| v2 → v1 | relations, `superseded_by`, `recorded_at` (when it differs from `valid_from`), pointer `event_uri` / `observed_at` / `source` / `content_hash` | — | — |
| v1 → v2 | — | `recorded_at` (set to `valid_from`); the manifest declares relations, supersession, transaction time and hashes unsupported | — |
| v2 → flat | relations, tombstones, provenance pointers after the first | — | — |
| flat → v2 | provenance when a memory has no `shogun_event_uri` | confidence (0.5) and `valid_from` when the metadata lacks them | memories without `shogun_type`: free text is not a typed record and is left out (send it through `memory.import`'s `shogun-memory-import/v1` package instead, which reviews it) |
| v2 → tree | relations, tombstones, low-confidence and agent-scratch records, closed and superseded facts, timestamps, fact links, pointers without a quote, hashes | confidence becomes a band and a `possibly:` prefix | — |
| tree → v2 | counterparty / project links, closed history | confidence of list lines (band floor: 0.8 / 0.5), every time (`now`), commitment direction (`mine`) | files outside the layout (an agent's own notes) |

`v2 → flat → v2` is exact for every typed field, the first quote and its hash, and both time
axes (tested); `v2 → v1 → v2` is exact for every v1 field (tested). Every output is validated
against the published JSON Schemas in the tests.

## Scripts

```bash
pnpm --filter @shogunagent/interop typecheck
pnpm --filter @shogunagent/interop test
pnpm --filter @shogunagent/interop build   # dist/esm, dist/cjs, dist/types
```
