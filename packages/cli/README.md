# shogunagent (CLI)

The CLI starts a local ShogunAgent sidecar and can connect AI apps to its read-only MCP face (plan E10-T10). ShogunAgent is the state layer for AI agents: people, projects, commitments and open loops, with a quote and a confidence on every fact.

The [repository status](../../README.md) describes the packages and hosted service as unreleased. The commands below distinguish using a checkout from using published packages and release assets. A configured URL such as `https://api.shogunagent.dev/mcp` is not evidence that the service is deployed; use an operator-confirmed endpoint and credentials for hosted access.

## Prerequisites

- Node.js ≥22.13.0; pnpm and a Rust toolchain when building from source.
- Trusted `shogun-api` and `shogun-mcp` binaries. The release workflow targets **Linux x86_64 and arm64**. macOS release downloads are not supported; use an existing ShogunAI Mac app connection or build the binaries from source. Other platforms have no release download in this CLI.
- An **existing encrypted `memory.db` at the supported schema version**, with its matching 64-hex-character store key. The single-store server opens an existing store; it never creates or migrates one. Installing or building the binaries does not initialize a database. Have the store's owner provision or upgrade it; do not create an empty file or substitute a new key.

The data directory contains `memory.db`, `api.token`, `db.key` and `memory_api.json`. Its defaults are `~/.local/share/shogunagent` on Linux (or `$XDG_DATA_HOME/shogunagent`), `~/Library/Application Support/shogunagent` on macOS, and `%APPDATA%\shogunagent` on Windows; `SHOGUNAGENT_DATA_DIR` overrides it. Choose the directory containing the existing store.

`SHOGUN_DB_KEY` and `SHOGUN_API_TOKEN` take precedence over `db.key` and `api.token`. Otherwise the CLI uses those files and generates missing secrets with mode 0600. A newly generated key cannot decrypt an existing store: provide the matching key before starting. Supply secrets through the owner's secure environment or existing protected files, never through chat or MCP configuration. The CLI creates `memory_api.json` with `enabled: true` only when absent; it preserves an existing setting.

## Run from a checkout

From the repository root, with the existing store and key already available:

```sh
pnpm install --frozen-lockfile
pnpm --filter shogunagent build
cargo build --release --locked -p shogun-core --features daemon-server --bin shogun-api --bin shogun-mcp

export SHOGUNAGENT_BIN_DIR="$(pwd)/target/release"
export SHOGUNAGENT_DATA_DIR="/absolute/path/to/existing-store-directory"
node packages/cli/bin/shogunagent.mjs help --json
node packages/cli/bin/shogunagent.mjs serve
```

If `CARGO_TARGET_DIR` is set, point `SHOGUNAGENT_BIN_DIR` at that directory's `release` subdirectory instead. It must be absolute. The CLI locates binaries there or in an explicitly installed release, never on PATH or by searching the current directory. Keep `serve` running while using local HTTP at `http://127.0.0.1:7464`; `--port` selects another port.

For a source-only MCP setup, configure the app's stdio server to run an absolute Node executable with arguments `["/absolute/path/to/Shogunagent/packages/cli/bin/shogunagent.mjs", "mcp"]`, and provide the same `SHOGUNAGENT_BIN_DIR` and `SHOGUNAGENT_DATA_DIR` environment paths. Each MCP process must also have access to the matching store key. Use your app's MCP configuration format. The automatic `connect` command writes an **npx-based** entry, so it requires the published npm package even when you invoke `connect` from a checkout; it does not write the source-build binary directory into that entry.

## Use published packages and releases

After the npm package and matching Linux release assets have been published, and the store prerequisites above are satisfied:

```sh
npx shogunagent@0.1 install
npx shogunagent@0.1 serve --connect
npx shogunagent@0.1 help --json
```

| Command | Behavior |
|---|---|
| No command, or `serve` | Start the HTTP sidecar using available binaries; no download or MCP configuration change. |
| `install` | Download and verify release binaries; does not start the sidecar. |
| `serve --install` | Download binaries, then start the sidecar. |
| `connect` | Show proposed MCP configuration changes and ask before writing; `--yes` applies them non-interactively. Does not start HTTP. |
| `serve --connect` | Apply MCP configuration changes and start HTTP. The flag explicitly authorizes those configuration writes. |
| `mcp` | Run the stdio MCP server behind the read-only filter. |
| `status --json` | Report paths, located binaries, the recorded sidecar port and client configuration status. |

Downloads come from `Torutesu/Shogunagent` release assets named `shogunagent-<tag>-<target>.tar.gz` with a `.sha256` file. The CLI verifies the checksum before extraction and installs under the data directory.

Connecting adds `"shogunagent": { "command": "npx", "args": ["-y", "shogunagent@0.1", "mcp"] }` to detected Claude Code, Cursor, Codex or Claude Desktop configurations. An explicitly set data directory is included as an environment path. Existing unrelated entries are preserved; changed files get a backup and are checked for concurrent edits. Restart the app, then call `profile.whoami` to verify the connection. On a Mac with an existing ShogunAI app connection, use that connection and its granted permissions instead of setting up another store.

## First authenticated write

The CLI's MCP connection filters `tools/list` and rejects write calls with `next_action: not_allowed`, including `memory.ingest`. With authorization to write to the local store, use **HTTP `POST /v1/memory/ingest`** and the local bearer token. Free reads also require authentication; a free operation is not anonymous access.

In a second terminal, make the running sidecar's token available as `SHOGUN_API_TOKEN` through your secure environment (the CLI uses the data directory's protected `api.token` when no override is set). This example stores synthetic text in the calling agent's scratch namespace:

```sh
curl --fail-with-body --silent --show-error \
  http://127.0.0.1:7464/v1/memory/ingest \
  -H "Authorization: Bearer ${SHOGUN_API_TOKEN:?Set the local token for the running sidecar}" \
  -H 'Content-Type: application/json' \
  --data '{"source":"first-run","actor":"agent:first-run","namespace":"agent_scratch","thread_key":"first-run","idempotency_key":"first-run-example-v1","events":[{"content":"Synthetic example: the demo checklist is complete."}]}'
```

Inspect the returned `result` counters and `event_ids`; retry the same input with the same `idempotency_key` to replay it without appending again. Successful ingestion records an event; it does not promise immediate extraction of a commitment or open loop. Scratch data belongs to the named actor's namespace. For real work, supply your own actor, source, thread and idempotency key; see [the ingest contract](../../docs/api/core-tools.md).

If startup reports `memory_update_pending` or `memory_newer_than_app`, resolve the store/version prerequisite with its owner. Re-running `install` does not initialize or migrate the store. An HTTP `401` requires the running server's token; a write through the read-only MCP connection will remain denied.

The sidecar uses the server telemetry posture: no telemetry unless `analytics.json` explicitly opts in with `opt_out: false`. The package minor and `SKILL.md` minor must match (`0.1`); use the skill from the corresponding checkout or release.

## Develop

```sh
pnpm --filter shogunagent typecheck
pnpm --filter shogunagent test
pnpm --filter shogunagent build
```

Tests use an in-memory filesystem, fake spawn and fake fetch, with no network.
