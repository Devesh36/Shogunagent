# shogunagent (CLI)

`npx shogunagent` — start the local ShogunAgent sidecar and connect the AI apps on this machine to it (plan E10-T10). ShogunAgent is the state layer for AI agents: people, projects, commitments and open loops, with a quote and a confidence on every fact. Hosted, no install: `https://api.shogunagent.dev/mcp`.

```
npx shogunagent install            # download the release binaries for this platform (sha256-verified)
npx shogunagent --connect          # start the sidecar and write a read-only MCP entry into the agent apps found here
npx shogunagent help --json        # machine-readable help: commands, endpoints, skill version
```

What it does, and does not do:

- **Binaries** come only from a release of `Torutesu/Shogunagent` (assets `shogunagent-<tag>-<target>.tar.gz` + `.sha256`, `.github/workflows/release.yml`), downloaded only on `install` / `--install`, verified against the published checksum before anything is extracted, and installed under the data directory. Or from `SHOGUNAGENT_BIN_DIR` (absolute). Never from the current directory or PATH.
- **The sidecar** is `shogun-api` on `127.0.0.1:7464` with the store `memory.db`, the client token `api.token` and the store key `db.key` under the OS data directory (`~/.local/share/shogunagent`, `~/Library/Application Support/shogunagent`, `%APPDATA%\shogunagent`; or `SHOGUNAGENT_DATA_DIR`). Token and key are generated once, kept at mode 0600, and never written anywhere else. `SHOGUN_API_TOKEN` / `SHOGUN_DB_KEY` in the environment take precedence.
- **Connecting** writes `"shogunagent": { "command": "npx", "args": ["-y", "shogunagent@0.1", "mcp"] }` into Claude Code (`~/.claude.json`), Cursor (`~/.cursor/mcp.json`), Codex CLI / ChatGPT desktop (`~/.codex/config.toml`) and Claude Desktop — only the apps that are present, only our entry, every other byte kept, the result re-read and compared before it is written, a backup next to the file, and only after an interactive yes or `--yes` / `--connect`. Idempotent. The rules are a port of ShogunAI's `client_install.rs`.
- **`shogunagent mcp`** is what those entries run: `shogun-mcp` on the local store behind a read-only filter. A `tools/call` outside the read set is answered with `next_action: not_allowed` and never reaches the server; `tools/list` is filtered. Writes go through the hosted endpoint with an agent key.
- **Telemetry**: none. The sidecar runs with the server posture (nothing sent unless `analytics.json` in the data directory says `opt_out: false`).

Version lock: the package minor equals the `SKILL.md` minor (`0.1`). An agent that holds a different skill version should re-read `https://shogunagent.dev/SKILL.md`.

Develop: `pnpm --filter shogunagent typecheck && pnpm --filter shogunagent test` (tests use an in-memory filesystem, a fake spawn and a fake fetch; no network). `pnpm --filter shogunagent build` compiles `src/` to `dist/` for `bin/shogunagent.mjs`.
