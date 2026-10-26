# @shogunagent/conformance

The MCP 2026-07-28 conformance suite for the ShogunAgent MCP face (E36a). It drives one endpoint
over Streamable HTTP and checks what the server claims: stateless transport (no `initialize`
needed, no session, `GET`/`DELETE` 405, batches refused, SSE-only clients served), the
`Mcp-Method` / `Mcp-Name` metering headers, `tools/list` shape and annotations by level (reads
`readOnlyHint`, L3 `destructiveHint` + `openWorldHint`, levels taken from
`apps/api/fixtures/tool-table.json`), MRTR `input_required` for L2 and `approval_required` with a
URL for L3, idempotent ingest and the `Mcp-Quantity` it settles, and error shapes (`code: sentence`,
`next_action`).

Each check passes, fails, warns (a repository rule or recommendation, not a protocol MUST; fails
only with `SHOGUN_CONFORMANCE_STRICT=1`) or is skipped with its reason.

```bash
# A throwaway local sidecar (temp store, random keys, loopback), the whole suite, writes included:
cargo build -p shogun-core --features daemon-server --bin shogun-api
pnpm --filter @shogunagent/conformance local          # add -- --build to build first

# Any endpoint (read-only checks unless SHOGUN_CONFORMANCE_WRITES=1):
SHOGUN_CONFORMANCE_URL=https://api.example/mcp SHOGUN_CONFORMANCE_TOKEN=sa_live_… \
  pnpm --filter @shogunagent/conformance conformance [-- --json]
```

Variables: see `src/config.mjs`. Write checks ingest one test turn, append one note and queue one
L3 approval in the target store; run them only against a store that may hold test rows.

The local fixture uses `--multi`, the server deployment mode. It needs no desktop trial or
hardware-bound licence, and creates its encrypted principal store only inside its temporary folder.
