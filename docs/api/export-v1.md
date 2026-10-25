# Export format v1

The portable form of one principal's world model: people, projects, commitments and open loops,
with the confidence, the validity interval, the writer, the namespace and the quote that proves
each one, plus the rows that were deleted. It is how a user takes their state to another store
(or another product) and how a sync peer keeps up. The JSON Schema for every line is
[`export-v1.schema.json`](export-v1.schema.json); the implementation is
`crates/shogun-memory/src/export/` and a test validates every exported line against the schema.

## Getting one

| Face | Call |
|---|---|
| MCP | `state.export` with `{"format": "export-v1"}`, plus `"differential": true` or `"since_cursor": <ms>` |
| REST | `POST /v1/export` with the same body (`format` defaults to `export-v1`); `GET /v1/state/export` stays the Markdown export |
| CLI | `shogun state export --format export-v1 [--differential \| --since-cursor <ms>]` |

The result is the usual envelope: `result.jsonl` (the export), `result.cursor`,
`result.since_cursor`, `result.records`, a free `usage` (exports are reads, invariant 10) and
`latency_ms`.

- **Full** (no option): every row of the four state tables, in every namespace and every
  confidence band — the export belongs to the user — and every tombstone.
- **`since_cursor: <ms>`**: only rows changed after that time (`updated_at > since`) and the
  tombstones written after it. The stored cursor is not touched.
- **`differential: true`**: `since_cursor` taken from the store's own `export_cursor`
  (`principal_meta`, V96), then advanced to this export's `cursor` in the same transaction. The
  first differential export is a full one. Advancing the cursor is the only write and holds no
  content.

## Shape

JSON Lines, UTF-8, one compact object per line. The **first line is the manifest**; the rest are
records in this order: entities, facts, tombstones.

### `manifest`

```json
{"type":"manifest","format":"shogunagent-export/v1","schema_version":100,"generated_at":1792000000000,
 "since_cursor":null,"cursor":1792000000000,"counts":{"entities":2,"facts":3,"tombstones":1}}
```

`schema_version` is the store's migration version. `cursor` is the export's time, or the newest
change it carries when that is later; pass it back as `since_cursor` to continue.

### `entity` — a person or a project

```json
{"type":"entity","kind":"person","id":12,"name":"Oren Vale","aliases":["Oren"],
 "summary":"Leads the Heron rollout.","status":null,"confidence":0.9,"band":"high",
 "valid_from":1791990000000,"valid_to":null,"updated_at":1791995000000,
 "actor":"agent:claude-code","namespace":"world",
 "provenance":[{"event_id":481,"quote":"Oren Vale: I'll send the rollout plan by Friday."}]}
```

`status` is the project status (`active`, `waiting`, `paused`, `done`) and `null` for a person.
`valid_to` is set when a project is `done`.

### `fact` — a commitment or an open loop

```json
{"type":"fact","kind":"commitment","id":7,"subtype":"theirs","text":"send the rollout plan by Friday",
 "status":"open","due_at":1792300000000,"counterparty_id":12,"project_id":null,
 "confidence":0.9,"band":"high","valid_from":1791990000000,"valid_to":null,
 "updated_at":1791995000001,"actor":"system:loop_consolidator","namespace":"world",
 "provenance":[{"event_id":481,"quote":"I'll send the rollout plan by Friday."}]}
```

`subtype` is the commitment direction (`mine`, `theirs`) or the open-loop kind
(`reply_needed`, `waiting_on_them`, `review_pending`, `decision_pending`, `follow_up`, `other`).
`valid_to` is set when the fact stopped being current (`done`, `cancelled`, `closed`).
`counterparty_id` / `project_id` name entity ids of the same export.

### `tombstone` — a deleted row

```json
{"type":"tombstone","kind":"commitments","id":9,"deleted_at":1791999000000,"reason":"deleted_by:human:owner"}
```

The deleted content is never kept or exported. A consumer that holds a copy of `(kind, id)`
retires it.

## Fields every record shares

| Field | Meaning |
|---|---|
| `confidence` | The stored confidence, 0–1. |
| `band` | `high` (≥ 0.8, a fact), `medium` (≥ 0.5, render as `possibly:`), `low` (never render as fact). Same bands every face applies. |
| `valid_from` / `valid_to` | When the row became current and when it stopped (`null` while current), unix ms. |
| `updated_at` | The last change, unix ms; what a differential export filters on. |
| `actor` | The last writer (`human:owner`, `agent:<id>`, `connector:<source>`, `system:<job>`); `null` for rows written before V93. |
| `namespace` | `world` or `agent_scratch:<agent_id>`. |
| `provenance` | `{event_id, quote}` pointers: the event in the exporting store and the exact span of it that proves the row (`quote` is `null` when none was retained). Event bodies are not exported: the export is the world model, not the log. |

## Importing one

`memory.import` (MCP), `POST /v1/memory/import` (REST) and `shogun memory import` (CLI) accept an
export v1 in the `package` slot, as its JSON Lines text. Preview reports what it holds
(`"action": "restore"` and the counts); commit restores it, no `select` needed:

- every entity and fact becomes a new row of this store, with new ids; facts keep pointing at the
  entities they name;
- each provenance pointer becomes one event of its own (source `import`) whose text is the quote,
  or the row's text when there was no quote, so every row keeps its provenance and its quote;
- confidence is capped at 0.6 (Medium): an imported claim is not something this store observed,
  the same rule as every other import, until the review confirms it;
- the importer is the writer (`actor`) of every restored row;
- tombstones name ids of the exporting store; they are counted and not applied.

Export → import into a fresh store gives the same row counts and the same quotes (tested).
