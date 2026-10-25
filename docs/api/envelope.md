# Response envelope (MCP, REST and CLI)

Every face of the core answers with the same envelope fields (CLAUDE.md invariant 4: three faces,
one result). The values come from one module, `crates/shogun-mcp/src/envelope.rs`, so the faces
cannot drift; `crates/shogun-core/tests/envelope_parity.rs` calls the same tools on all three and
compares the fields. Envelope keys are only ever added, and only when the answer does not already
carry its own (an approval's `approve_at_url` / `poll_status`, a recall's own `usage`, a rate
limit's `wait`, a refusal that sets `next_action: null`).

## Where the fields are

| Face | Refusal: `next_action` | Read envelope (`usage`, `latency_ms`, `next_action: null`) |
|---|---|---|
| MCP (`stdio`, `POST /mcp`) | JSON-RPC error: `error.data.next_action`. Tool that ran without an answer (`isError: true`): in the JSON text block. | In the JSON text block of the result. |
| REST (`/v1/...`) | Top level of the error body (`{"error": "<code>", ..., "next_action": "..."}`), any 4xx/5xx. | Top level of the 200 body. |
| CLI (`shogun ...`) | The CLI prints the REST body it receives, so the same keys. Exit 1. | Same keys as REST. |

The read envelope is on the state reads (`state.*` reads) and `profile.whoami`:
`"usage": {"unit": "read", "quantity": 1, "cost_micro_usd": 0}` (reads are free, invariant 10),
`"latency_ms": <server time>` and `"next_action": null`. `memory.search` and `memory.get_events`
carry no read envelope on any face: their answers are compared byte for byte across the faces.
`memory.recall`, `memory.ingest`, `handoff.*` and exports render their own envelope.

## `next_action` values

Branch on `error` (REST) or the JSON-RPC code (MCP), then on `next_action`; never parse the
message. New values may appear (`docs/api/deprecation-policy.md` rule 8): treat an unknown one as
"stop and tell the human".

| `next_action` | Refusals (MCP code / REST status and `error`) | What to do |
|---|---|---|
| `fix_input` | `-32700`, `-32600`, `-32601`, `-32602`; `isError` `not_found`, `invalid_*`, `missing_*`, `bad_*`. REST `400`, `404 not_found`, `405`, `409` (other than `revision_changed`), `413`, `422` | Change the request; the same request fails again. |
| `reauthenticate` | `-32001`; REST `401 unauthorized` | The bearer is unknown or revoked: get a valid one. |
| `upgrade_plan` | `-32003`; REST `403 plan_required` | The plan does not include this; tell the user. |
| `request_scope` | `-32004`; REST `403 scope_insufficient` | A read token called a write; ask for a full-scope token. |
| `reread` | `isError` / REST `409 revision_changed` | Read the record again, then retry the change. |
| `retry_after` | `-32005`, `-32029`, `-32000`; REST `429 rate_limited`, `503 memory_unavailable` / `memory_update_pending` / `memory_newer_than_app`, `409 principal_busy`, any other 5xx | Wait (`retry_after_ms` when given) and retry. |
| `ask_human` | `-32006 source_not_allowed`; REST `410 principal_purged`; any refusal the table does not name | Stop and tell the human. |

Other values a specific route documents: `approve_at_url`, `poll_status`, `execute_unavailable`
(approvals), `narrow_query` (recall and handoff cut to budget), `not_allowed` (handoff), `wait`
(rate limit on stdio).
