# Codex-to-Codex context verification

## Result observed on 28 October 2026

**Passed: source context saved by one real Codex session was retrieved by a fresh
Codex session through ShogunAgent, including after a server restart and after an
explicit deadline revision.** No Batch relay, Claude login, or scripted model
response was used in this test.

The four sessions made seven successful MCP calls. Their prompts, distinct thread
IDs, actual tool arguments/results, source quotes, answers, and original transcript
SHA-256 hashes are in [the evidence artifact](verification/codex-context-2026-10-28.json).
It contains synthetic test data only, with no bearer tokens or database keys.

| Step | Observed result |
| --- | --- |
| Writer checks an unpredictable project name | Recall returned no evidence, facts, or items |
| Writer saves the initial note | `memory.ingest` inserted event 1 |
| Test server stops and restarts with the same encrypted store/key | A new process served the persisted source |
| Fresh reader receives only the project name and field questions | `memory.recall` and `memory.get_events` returned the correct owner, deadline, PDF format, and random verification phrase from event 1 |
| Reader is asked for an unrecorded budget | Answered unknown |
| A new writer session saves a deadline revision | `memory.ingest` inserted event 2 |
| Another fresh reader receives the same question, without either note | Opened events 1 and 2; selected 2030-06-24 instead of 2030-06-18 and explained the explicit replacement |

The project was `Juniper-e908c924`. The receiving prompts contained neither the
owner, either date, format value, nor random verification phrase. Each session
started in a different empty working directory, used `--ephemeral`, and did not
resume or fork another conversation. Codex memory, plugins, apps, chronicle, and
shell tools were disabled for these invocations. Only the selected Shogun tools
were configured; the actual traces contain no shell or filesystem calls.

Two separate private bearers mapped to `writer` and `reader` within the dedicated
`codex-eval` principal. This tested shared world context between authenticated
agent identities, not separate-user access. The existing demo store was untouched.

## What this proves and what it does not

- This was real model-driven MCP use: Codex chose and executed `memory.ingest`,
  `memory.recall`, and `memory.get_events`. The harness did not perform the writes
  on the model's behalf or give the reader the expected answers.
- Stored source context survives a fresh conversation and an engine restart.
- The receiving model correctly interpreted an explicit revision in the retrieved
  sources. Both source events remain stored; this does not prove typed-state
  conflict resolution or deletion of obsolete facts.
- Recall's `facts` and `items` arrays remained empty. Automatic extraction, the
  Batch relay, `handoff.create` / `handoff.accept`, and cross-product clients were
  not tested by this run. It does not transfer an entire chat automatically.
- This is one synthetic scenario, not a general retrieval-quality benchmark,
  adversarial isolation test, production-readiness assessment, or CI completion.
- These model calls used the existing Codex ChatGPT login. The test did not
  configure a relay credential or send source text through the Batch processing
  path. As with normal Codex use, the model received the test prompts and MCP results.

## Environment and the earlier connection failure

Codex CLI was `0.164.0` on macOS. A fresh invocation with
`--ignore-user-config` and the feature overrides in the runner completed actual
tool calls. Earlier attempts using the ordinary configuration failed in the
code-mode host. This successful isolation does **not** identify the particular
setting responsible or prove the ordinary configuration has been repaired.
No global client configuration or authentication files were changed.

The reusable runner was additionally exercised on the final read against the real
server: both MCP calls and the revised answer passed again. Its refusal to
overwrite an existing transcript was checked, and the three deliverable files
were checked against the private test credential values to ensure none leaked.

The source checkout was merged main `940c8c4`. To avoid another large build on a
disk-constrained Mac, the test reused the release `shogun-api` from the earlier
fresh checkout at `4e8210f`. Its SHA-256 is recorded in the artifact. The engine
implementation matches the relevant merged code; the intervening MCP change adds
the new server routing guide. Consequently this run does not verify that new
initialization wording in a rebuilt daemon. The bridge was the merged script.

Raw local transcripts, invocation manifests, and the private synthetic store were
kept under `/private/tmp/shogun-codex-goal-20261028/`. Temporary paths are local
audit artifacts, not repository dependencies. The dedicated server was stopped
after verification; its store and keys were preserved.

## Repeat the test

Prerequisites: a signed-in Codex CLI with the flags used below, Python 3, and a
built `shogun-api` binary. See [local agent setup](local-agent-setup.md) and the
[official MCP configuration documentation](https://learn.chatgpt.com/docs/extend/mcp?surface=cli).
These commands run actual model sessions using your Codex access. Use synthetic
content and a dedicated store. The runner records evidence; inspect the quotes
and receipts rather than treating CLI exit status alone as acceptance.

From the repository root, in terminal 1:

```sh
export SHOGUN_CODEX_TEST_DIR="$(mktemp -d "${TMPDIR:-/tmp}/shogun-codex-test.XXXXXX")"
export SHOGUN_API_BIN="$PWD/target/release/shogun-api"
# Set SHOGUN_API_BIN to your actual built binary if you use another target directory.
python3 - "$SHOGUN_CODEX_TEST_DIR" <<'PY'
import os, pathlib, secrets, sys
root = pathlib.Path(sys.argv[1])
for name in ['db.key', 'writer.token', 'reader.token']:
    fd = os.open(root / name, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, 'w') as out:
        out.write(secrets.token_hex(32) + '\n')
print('Test directory:', root)
PY
(
  export SHOGUN_DB_KEY_CODEX_EVAL="$(cat "$SHOGUN_CODEX_TEST_DIR/db.key")"
  export SHOGUN_PRINCIPAL_TOKENS="codex-eval:$(cat "$SHOGUN_CODEX_TEST_DIR/writer.token"):writer,codex-eval:$(cat "$SHOGUN_CODEX_TEST_DIR/reader.token"):reader"
  export SHOGUN_API_PORT=7465
  "$SHOGUN_API_BIN" --multi "$SHOGUN_CODEX_TEST_DIR/principals" --key-provider env
)
```

In terminal 2, set `SHOGUN_CODEX_TEST_DIR` to the **same directory printed above**,
then run from the repository root:

```sh
python3 scripts/codex-context-session.py writer-initial \
  --output-dir "$SHOGUN_CODEX_TEST_DIR/evidence" \
  --token-file "$SHOGUN_CODEX_TEST_DIR/writer.token"

# Optional persistence check: stop the test server in terminal 1 with Ctrl+C,
# then rerun its parenthesized start command with the SAME directory and key.

python3 scripts/codex-context-session.py reader-initial \
  --output-dir "$SHOGUN_CODEX_TEST_DIR/evidence" \
  --token-file "$SHOGUN_CODEX_TEST_DIR/reader.token"

python3 scripts/codex-context-session.py writer-update \
  --output-dir "$SHOGUN_CODEX_TEST_DIR/evidence" \
  --token-file "$SHOGUN_CODEX_TEST_DIR/writer.token"

python3 scripts/codex-context-session.py reader-updated \
  --output-dir "$SHOGUN_CODEX_TEST_DIR/evidence" \
  --token-file "$SHOGUN_CODEX_TEST_DIR/reader.token"
```

The runner defaults to `http://127.0.0.1:7465/mcp`; pass `--url` if you choose a
different port. It generates a random project/verification phrase once per output
directory and rejects overwriting a stage transcript. It uses the supplied token
file path, never copies its contents into the configuration, and refuses a
successful result without the required actual MCP calls.

Inspect `*.jsonl` for completed calls and tool errors, `*.command.json` for the
exact prompts/configuration, and `scenario.json` for expected values. Confirm
that the receiving prompts exclude those values, that their source events match
the writer's receipts, and that the final answer follows the revised date without
inventing a budget. End the dedicated server with Ctrl+C after testing; retain
its keys if you want to reopen the store. Keep credentials out of Git.
