# Connect an agent to your local ShogunAgent server

Complete the [repository quickstart](../README.md#quickstart-save-and-recall-a-source) first.
Keep `shogun-api` running. These instructions use its multi-principal demo store,
not a ShogunAI desktop database. Python 3 is the only bridge dependency.

## 1. Check the connection

From the repository root:

```sh
python3 scripts/mcp-local-bridge.py \
  --token-file "$PWD/.shogun-demo/api.token" --check
```

Expect three PASS lines: `initialize`, `tools/list`, and `tools/call` (identity).
This checks the actual authenticated endpoint without making a model call. Use
`--url http://127.0.0.1:YOUR_PORT/mcp` if you changed `SHOGUN_API_PORT`.

The bridge translates stdio JSON-RPC into HTTP requests with a matching
`Mcp-Method` on every message and `Mcp-Name` on every `tools/call`. It reads the
token file on each request; the token is never copied into generated configuration
or passed in command-line arguments. It accepts only the loopback HTTP endpoint,
does not use HTTP proxies, and never follows redirects. On POSIX, the token file
must be private (`chmod 600 .shogun-demo/api.token`).

The connection has the permissions of the supplied bearer. The quickstart's
evaluation token permits writes; the bridge is not a read-only permission filter.
ShogunAgent still enforces the token's scope and approval rules. Keep normal client
tool approvals enabled and start by testing the read tools below.

## 2. Generate the configuration for your client

The generator prints absolute Python, script, and token-file paths; it does not
modify any application settings. Merge the `shogunagent` entry into existing
configuration rather than replacing other servers. Placeholder examples are in
[`examples/local-agent/`](examples/local-agent/); prefer the generator for actual absolute paths. Regenerate after moving the
repository or Python installation. Keep machine-specific configs out of Git.

### Claude Code

```sh
python3 scripts/mcp-local-bridge.py \
  --token-file "$PWD/.shogun-demo/api.token" --print-config claude-code
```

Merge the output into `.mcp.json` in the project where you use Claude Code. Open
that project in Claude Code and approve this MCP server when prompted. Use
`/mcp` to check it. `claude mcp list` reports connection health after approval.
For an isolated session, save the generated JSON to a private file and run
`claude --strict-mcp-config --mcp-config /absolute/path/to/mcp.json`.

### Cursor

```sh
python3 scripts/mcp-local-bridge.py \
  --token-file "$PWD/.shogun-demo/api.token" --print-config cursor
```

Merge into your project's `.cursor/mcp.json` or personal `~/.cursor/mcp.json`.
Enable `shogunagent` in Cursor's MCP settings. With Cursor CLI, from that project:

```sh
agent mcp enable shogunagent
agent mcp list-tools shogunagent
```

### Codex

```sh
python3 scripts/mcp-local-bridge.py \
  --token-file "$PWD/.shogun-demo/api.token" --print-config codex
```

Merge the printed `[mcp_servers.shogunagent]` table into `~/.codex/config.toml`.
Restart Codex, inspect `/mcp`, then run the read-only verification prompt below.
`codex mcp get shogunagent` checks registration; it does not prove a tool call.

### Claude Desktop

```sh
python3 scripts/mcp-local-bridge.py \
  --token-file "$PWD/.shogun-demo/api.token" --print-config claude-desktop
```

In Claude Desktop, open Settings → Developer → Edit Config. Merge the generated
`mcpServers.shogunagent` entry into `claude_desktop_config.json`, then fully quit
and reopen the app. On macOS the file is normally
`~/Library/Application Support/Claude/claude_desktop_config.json`.
The local server must run on the same computer as Claude Desktop. This configuration
has not been exercised in Claude Desktop during this audit; the app was unavailable.

Client references: [Claude Code MCP](https://code.claude.com/docs/en/mcp),
[Cursor MCP](https://cursor.com/docs/mcp),
[MCP host setup](https://py.sdk.modelcontextprotocol.io/get-started/real-host/).
The installed Codex CLI's `codex mcp --help` describes its local registration options.

## 3. Verify actual tool use

After signing into your chosen client, send:

> Use only ShogunAgent: call profile.whoami, then memory.recall with query
> "Kestrel" and budget_tokens 1000. Show the returned source evidence. Do not
> write data, execute actions, or use other tools.

If you ingested the quickstart sample, the source evidence should include
`The Kestrel demo codeword is violet.` Check the client's tool trace, not just a
prose assertion. A connected status or tool listing alone does not prove this step.

## Direct HTTP clients

Do not set a fixed `Mcp-Method: tools/call` for an entire client: initialization
and discovery use different methods, and `Mcp-Name` changes per tool. A client that
cannot generate these headers per message should use the stdio bridge.

| JSON-RPC body | Required method header | Required name header |
| --- | --- | --- |
| `initialize` | `Mcp-Method: initialize` | None |
| `notifications/initialized` | `Mcp-Method: notifications/initialized` | None |
| `tools/list` | `Mcp-Method: tools/list` | None |
| `tools/call`, name `profile.whoami` | `Mcp-Method: tools/call` | `Mcp-Name: profile.whoami` |
| `tools/call`, name `memory.recall` | `Mcp-Method: tools/call` | `Mcp-Name: memory.recall` |

Every request also uses `Authorization: Bearer <token>` and
`Content-Type: application/json`. POST one JSON-RPC message to `/mcp`; GET and
DELETE return 405. The bridge preserves the engine's negotiated initialization
version, tool results, errors, and notification semantics. It does not weaken the
HTTP header checks or change hosted billing.

## Troubleshooting

- `missing_header: Mcp-Method` / `Mcp-Name`: use the bridge, or generate headers
  from each message as shown above.
- 401: use the token file that configured the running server. A client's model
  login is separate from the ShogunAgent token.
- Connection refused: start the server, check its port, then rerun `--check`.
- Private-file error: check the path and permissions, not the token value.
- Connected but the model will not call tools: sign into the client and approve
  the MCP server/read tools. Never disable all tool permissions as a setup fix.
- Empty facts with matching source evidence: the basic quickstart has no reviewed
  extraction. Follow [the state walkthrough](local-state-walkthrough.md).
- The legacy `packages/cli` stdio adapter opens a single-principal database and is
  read-only. It is a different setup; do not point it at the multi-principal root.

See [the verification record](../apps/web/VERIFICATION.md) for tested client versions
and the distinction between protocol, client connection, and model-driven checks.
