# State-of-work handoff (A2A)

The object one agent hands to another together with a task: the people, projects, commitments and
open loops the task touches, each with its confidence band and the quotes that prove it, plus who
hands it to whom, what the task is and until when the handoff is valid. It travels as an A2A
`data` part. Its records are a strict subset of [export v1](export-v1.md): every field a handoff
record carries is the export v1 field of the same name and type, Low-confidence rows never travel,
and every row carries at least one provenance pointer. The JSON Schema is
[`handoff.schema.json`](handoff.schema.json) (`$id`
`https://shogunagent.dev/schemas/handoff.schema.json`); `apps/api/test/handoff.test.ts` validates
the object against it and checks the subset relation field by field against
`export-v1.schema.json`, and `the_built_object_has_the_schema_fields` in the core checks that what
it builds carries exactly the schema's fields. Implementation: the Rust core
(`crates/shogun-mcp/src/handoff_api.rs`, tools `handoff.create` / `handoff.accept`); the hosted
gateway only relays (`apps/api/src/handoff.ts`). Plan E16, E35.

## Getting one: `handoff.create`

The same tool on every face, at the same level (a read) and with the same answer (invariant 4):

| Face | Call |
|---|---|
| MCP | `tools/call` `handoff.create` with the arguments below |
| REST (sidecar) | `POST /v1/handoff` with the arguments as the body |
| REST (hosted gateway) | `POST /v1/handoff` with `Authorization: Bearer sa_live_…` (scope `read`), relayed to the core |
| CLI | `shogun handoff create '{"task":"…","to_agent":"agent:…"}'` |
| TypeScript SDK | `await sa.handoff({ task, toAgent })` (`@shogunagent/sdk`) |
| Python SDK | `sa.handoff(task, to_agent=…)` (`shogunagent`) |

```sh
curl -s -X POST $BASE/v1/handoff -H "Authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"task":"Draft the Heron rollout note","to_agent":"agent:writer","query":"Heron rollout","ttl_seconds":3600}'
```

| Field | Required | Meaning |
|---|---|---|
| `task` | yes | What the receiving agent is asked to do (1–2,000 characters). |
| `to_agent` | yes | The receiving agent's id (`[A-Za-z0-9_.:@/-]`, ≤ 128). |
| `from_agent` | no | Your agent id; default the caller's own (`agent:<key id>` on the hosted service). |
| `query` | no | What to recall for the task (≤ 2,000 characters); default `task`. |
| `budget_tokens` | no | The recall's token budget, 100–32,000; default 2,000. |
| `ttl_seconds` | no | How long the handoff is valid, 60–86,400; default 3,600. |
| `thread_key` | no | Your task or conversation id, carried in `task.thread_key`. |

The core runs one budgeted recall (the `memory.recall` read, so the principal's confidence rules
and source filter apply unchanged) and maps its state items onto the handoff shape. The hosted
gateway hands the request body to the principal's sidecar as a stream and returns the sidecar's
answer as a stream: it reads neither, logs only the route, the status and a fixed event word, and
stores nothing (invariants 1 and 2). It is a read: free, no receipt is billed (invariant 10).

**Across principals.** A handoff reads the caller's own principal. On the sidecar's REST face,
`POST /v1/handoff?principal=<teammate>` builds it from a teammate's **world** rows only when the
two share a team and the teammate granted `world_read` (Team, E18); otherwise the same `403
cross_principal_forbidden` as every other cross-principal read. No other path crosses a principal
boundary.

The answer is the usual envelope:

```json
{
  "tool": "handoff.create",
  "result": {
    "handoff": { "format": "shogunagent-handoff/v1", "…": "…" },
    "a2a": {
      "part": { "kind": "data", "data": { "…": "the handoff" }, "metadata": { "schema": "https://shogunagent.dev/schemas/handoff.schema.json", "format": "shogunagent-handoff/v1" } },
      "message": { "kind": "message", "role": "agent", "messageId": "msg_…", "parts": [ { "…": "the part" } ], "metadata": { "handoff_id": "hof_…" } }
    }
  },
  "usage": { "unit": "read", "quantity": 1, "cost_micro_usd": 0 },
  "latency_ms": 41,
  "hints": [],
  "next_action": null
}
```

`next_action` is `narrow_query` when the recall was cut to its budget (`handoff.truncated`).
Refusals (REST status; MCP answers JSON-RPC `-32602` / `-32000` with the same `code: message`):
`400` (`missing_task`, `missing_to_agent`, `invalid_to_agent`, `invalid_from_agent`,
`invalid_budget_tokens`, `invalid_ttl_seconds`, `invalid_request`; `next_action: fix_input`), `401`
/ `403` (key, scope, cross-principal), `503` (`recall_unavailable`, `memory_update_pending`; the
gateway adds `upstream_unavailable` and `principal_not_provisioned`, and `421 wrong_shard`, each
with `next_action`).

## Shape

```json
{
  "format": "shogunagent-handoff/v1",
  "handoff_id": "hof_3f0c…",
  "created_at": 1792000000000,
  "expires_at": 1792003600000,
  "from_agent": "agent:planner",
  "to_agent": "agent:writer",
  "task": { "summary": "Draft the Heron rollout note", "query": "Heron rollout", "thread_key": "task-42" },
  "entities": [
    { "type": "entity", "kind": "person", "id": 12, "name": "Oren Vale", "confidence": 0.92, "band": "high",
      "provenance": [{ "event_id": 481, "quote": "Oren Vale: I'll send the rollout plan by Friday." }] }
  ],
  "facts": [
    { "type": "fact", "kind": "commitment", "id": 7, "text": "send the rollout plan by Friday", "confidence": 0.9, "band": "high",
      "provenance": [{ "event_id": 481, "quote": "I'll send the rollout plan by Friday." }] },
    { "type": "fact", "kind": "open_loop", "id": 3, "text": "waiting on Oren's budget numbers", "confidence": 0.6, "band": "medium",
      "provenance": [{ "event_id": 482, "quote": "Oren: budget numbers next week" }] }
  ],
  "truncated": false
}
```

| Field | Meaning |
|---|---|
| `format` | Always `shogunagent-handoff/v1`. |
| `handoff_id` | `hof_<uuid>`; the receiver's idempotency key derives from it. |
| `created_at` / `expires_at` | Unix ms. A receiver refuses an expired handoff. |
| `from_agent` / `to_agent` | Agent ids. A receiver refuses a handoff addressed to someone else (unless it opts out). |
| `task` | `summary` (the task), `query` (what was recalled), `thread_key` (or `null`). |
| `entities[]` | export v1 `entity` records (`person`, `project`) cut to `type`, `kind`, `id`, `name`, `confidence`, `band`, `provenance`; the other export v1 entity fields may appear and then mean exactly what they mean there. |
| `facts[]` | export v1 `fact` records (`commitment`, `open_loop`) cut to `type`, `kind`, `id`, `text`, `confidence`, `band`, `provenance`; likewise optional export v1 fields. |
| `band` | `high` (a fact) or `medium` (render as `possibly:`). Never `low`. |
| `provenance` | At least one `{event_id, quote}`: the event in the **sender's** store and the exact span that proves the row (`quote` is `null` when none was retained). The ids mean nothing in the receiver's store; the quotes are what travels. |
| `truncated` | The recall was cut to its token budget, or a list hit its cap (200 each). |

Ids (`id`, `event_id`) are the sender principal's. Duplicate `(kind, id)` rows from the recall are
merged, their provenance concatenated. A row the recall returned without an event id is left out:
it cannot be proven to the receiver.

## Accepting one: `handoff.accept`

The receiver hands the object back to the core, which validates it (format, row shapes, that it is
addressed to the receiver and not expired) and stores it in **the receiver's own `agent_scratch`
namespace**, never the principal's world: what another agent claims is working context for the
receiver, not something the user's store observed.

| Face | Call |
|---|---|
| MCP | `tools/call` `handoff.accept` `{"handoff": <object, A2A data part or message>, "actor": "<your agent id>"}` |
| REST (sidecar) | `POST /v1/handoff/accept` with the same body |
| CLI | `shogun handoff accept '{"handoff": …, "actor": "agent:…"}'` |
| SDKs | `await sa.acceptHandoff(a2aMessageOrPartOrObject)` / `sa.accept_handoff(…)` |

| Field | Required | Meaning |
|---|---|---|
| `handoff` | yes | The `shogunagent-handoff/v1` object, its A2A data part, the A2A message carrying it, or a `handoff.create` result. |
| `actor` | yes | Who accepts: your agent id. Must equal `to_agent` unless `check_addressee` is `false`. On the hosted service the rows are written as the key's agent whatever this says. |
| `idempotency_key` | no | Default `handoff:<handoff_id>:<actor>`: accepting the same handoff again replays and appends nothing. |
| `check_addressee` | no | Default `true`. |

It is the `memory.ingest` path: one event for the task and one per entity and fact (kinds
`handoff_task` / `handoff_entity` / `handoff_fact`), source `handoff`, `thread_key
handoff:<handoff_id>`, Medium rows prefixed `possibly:`, the quotes and the sender's event ids in
the text. A Low row or one without provenance is skipped and counted (`skipped`). An L1 write
(like `memory.ingest`), metered per event stored (`usage.unit: events`; a replay is 0, and the
sidecar's `Mcp-Quantity` says what to settle). On the hosted service it is a `tools/call` over the
metered `/mcp`; there is no gateway REST route for it. Cross-principal accepts are refused (a
write).

```json
{
  "tool": "handoff.accept",
  "result": { "handoff_id": "hof_…", "from_agent": "agent:planner", "to_agent": "agent:writer", "task": "…",
              "accepted": 4, "inserted": 4, "deduplicated": 0, "rejected": 0, "skipped": 0, "replayed": false,
              "event_ids": [91, 92, 93, 94], "thread_key": "…", "namespace": "agent_scratch", "idempotency_key": "handoff:hof_…:agent:writer" },
  "usage": { "unit": "events", "quantity": 4 },
  "latency_ms": 6,
  "hints": ["Read it back with memory.recall and include_scratch: true."],
  "next_action": null
}
```

Refusals: `400 invalid_handoff` / `missing_actor` (`fix_input`), `400 handoff_expired`
(`retry_after`: ask the sender for a fresh one), `403 handoff_not_for_you` (`not_allowed`), `503
ingest_unavailable`. The receiver then reads it back with `memory.recall({ include_scratch: true })`.
