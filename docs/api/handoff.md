# State-of-work handoff (A2A)

The object one agent hands to another together with a task: the people, projects, commitments and
open loops the task touches, each with its confidence band and the quotes that prove it, plus who
hands it to whom, what the task is and until when the handoff is valid. It travels as an A2A
`data` part. Its records are a strict subset of [export v1](export-v1.md): every field a handoff
record carries is the export v1 field of the same name and type, Low-confidence rows never travel,
and every row carries at least one provenance pointer. The JSON Schema is
[`handoff.schema.json`](handoff.schema.json) (`$id`
`https://shogunagent.dev/schemas/handoff.schema.json`); `apps/api/test/handoff.test.ts` validates
built objects against it and checks the subset relation field by field against
`export-v1.schema.json`. Implementation: `apps/api/src/handoff.ts`; plan E16.

## Getting one

| Face | Call |
|---|---|
| REST (hosted gateway) | `POST /v1/handoff` with `Authorization: Bearer sa_live_…` (scope `read`) |
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
| `from_agent` | no | Your agent id; default `agent:<key id>`. |
| `query` | no | What to recall for the task; default `task`. |
| `budget_tokens` | no | The recall's token budget, 100–32,000; default 2,000. |
| `ttl_seconds` | no | How long the handoff is valid, 60–86,400; default 3,600. |
| `thread_key` | no | Your task or conversation id, carried in `task.thread_key`. |

The gateway sends one `memory.recall` (query, budget) to the principal's core through the same
upstream it relays `/mcp` to, with the caller's own bearer: the core decides what the principal may
read and applies the confidence rules. The gateway maps the recall's state items onto the handoff
shape in memory and answers; it logs only the route, status and a fixed event word, and stores
nothing. It is a read: free, no receipt is billed (invariant 10).

The answer is the usual envelope:

```json
{
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
Refusals: `400` (`missing_task`, `invalid_to_agent`, `invalid_from_agent`, `invalid_budget_tokens`,
`invalid_ttl_seconds`, `invalid_json`; `next_action: fix_input`), `401` / `403` (key, scope), `502`
(the core refused the recall: `memory_update_pending`, `upstream_error`; `next_action` says
whether to retry), `503 upstream_unavailable` with `Retry-After`.

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

## Accepting one

The receiver validates the object (format, `to_agent`, `expires_at`) and ingests it into **its own
`agent_scratch` namespace**, never the principal's world: what another agent claims is working
context for the receiver, not something the user's store observed. The SDKs do this in one call:

```ts
const { handoff } = await sa.acceptHandoff(a2aMessageOrPartOrObject);   // TypeScript
```

```python
sa.accept_handoff(a2a_message_or_part_or_object)                        # Python
```

`acceptHandoff` sends `memory.ingest` with `namespace: "agent_scratch"`, `source: "handoff"`,
`actor` = the receiver, `thread_key: handoff:<handoff_id>`, one event for the task and one per
entity and fact (Medium ones keep the `possibly:` prefix, the quotes and event ids ride in the
text), and the idempotency key `handoff:<handoff_id>:<n>`, so accepting the same handoff twice
replays instead of appending. The receiver then reads it back with
`memory.recall({ include_scratch: true })`.
