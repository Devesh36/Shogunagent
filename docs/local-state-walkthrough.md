# From Maya's source notes to a useful agent handoff

The website animation is an illustration. The basic quickstart proves source
ingestion and recall; it does not configure a model. This guide separates a
repeatable engine replay from the credentialed automatic-processing path.

## 1. Run the engine replay locally

From the repository root, with the same Rust prerequisites as the quickstart:

```sh
cargo run --profile fast --locked -p shogun-core \
  --features daemon-server --example maya_handoff
```

This creates an isolated in-memory database. It neither opens your demo store nor
contacts a provider. Three synthetic notes describe Maya Chen, the Kestrel launch,
a promise to send launch copy, and a pending review, spread across two simulated
hourly sessions. **The model responses are
scripted fixtures.** The remainder is real engine code:

1. `agent_ingest` stores the sources and checks an idempotent replay.
2. Before review, local heuristic candidates remain below the confidence threshold;
   recall must not present them as facts.
3. The production hourly review parses anchored fixture answers, validates source
   spans, stores people/projects, and runs the loop consolidator.
   The two sessions satisfy the engine's existing entity-visibility rule; it
   deliberately hides people/projects mentioned in only one hourly session.
4. Recall must contain a person, project, commitment, and open loop, each with an
   event ID and supporting quote. The engine computes the confidence bands; the
   fixture does not assign entity confidence. Visible repeated entities may be
   reported as High by recall even though initial extraction starts at Medium.
5. `knowledge_neighbors` must connect Maya and Kestrel through their shared source.
   This is evidence co-occurrence, not a typed “owns” or “approved” relationship.
6. The real handoff service creates a package with facts and entities and accepts it
   into the writer's scratch namespace. It does not send or publish anything.

The command prints the evidence and handoff JSON and exits unsuccessfully if an
assertion fails. This verifies pipeline behavior; it is **not a model-accuracy
benchmark, an HTTP/client test, or proof that automatic extraction is configured**.

Implementation: [`maya_handoff.rs`](../crates/shogun-core/examples/maya_handoff.rs).
The existing `state_quality_e2e` tests additionally cover changed deadlines,
completed work, snoozing, rejected fabricated quotes, and principal isolation:

```sh
cargo test --release --locked -p shogun-core --features daemon-server \
  --test state_quality_e2e -- --nocapture
```

## 2. Configure actual processing on the demo server

**Prerequisite:** an operator-issued Shogun Batch relay credential. This is not the
local `.shogun-demo/api.token`, a Claude/Codex login, or an Anthropic API key.
The production tick's model lane uses that relay; without a sealed credential it
runs only local rules. Those rules remain low-confidence and do not extract the
rich state shown in the animation. There is no public hosted-signup shortcut here.

This live path sends eligible source content to the configured relay and may use
its quota. Use a synthetic, dedicated evaluation principal. No live relay run was
performed in this audit because a test credential was unavailable.

After the quickstart has provisioned `demo`, store its relay credential without
putting the value in a shell command, history, or plaintext configuration:

```sh
python3 - <<'PY'
import getpass, os, pathlib, subprocess
root = pathlib.Path('.shogun-demo').resolve()
env = dict(os.environ)
env['SHOGUN_DB_KEY_DEMO'] = (root / 'db.key').read_text().strip()
credential = getpass.getpass('Shogun Batch relay credential: ')
if not credential.strip():
    raise SystemExit('No credential supplied; nothing changed.')
subprocess.run([
    './target/release/shogun-api', '--store-relay-credential', 'demo',
    '--multi', str(root / 'principals'), '--key-provider', 'env',
], input=credential + '\n', text=True, env=env, check=True)
PY
```

Use your actual release-binary path if you set `CARGO_TARGET_DIR`. The credential
is sealed using the principal's store key. Keep that key and the store together.

## 3. Add new source notes, then run the tick

Use authenticated `memory.ingest` through the connected agent or REST as in the
quickstart. Supply these notes as three events, `source: maya-live-walkthrough`,
`actor: agent:researcher`, `thread_key: kestrel-launch`, and a fresh
`idempotency_key` for this run:

```text
Maya Chen is the product lead at Kestrel Labs. Kestrel is our launch project. We work together on the Kestrel launch copy over Slack.
I'll send Maya Chen the Kestrel launch copy by Friday.
The Kestrel launch copy is waiting for Maya Chen's review.
```

Leave `ts_ms` unset to use the real ingest time. The hourly review excludes the
current incomplete hour: wait until the next hour boundary before running it.
People/projects also need mentions in two distinct hourly source sessions before
they appear in normal lists and recall. In a later hour, ingest a fresh follow-up
such as “Maya Chen and I discussed the Kestrel launch copy again on Slack; her
review is still pending.” Then wait for that hour to finish and run the tick again.
Do not fabricate timestamps or bypass the visibility rule to force a live result.
Use fresh notes after a previous local-only tick; the review cursor may already
have consumed older events. Attaching a credential does not rewind that cursor.

```sh
(
  set -eu
  export SHOGUN_DB_KEY_DEMO="$(cat .shogun-demo/db.key)"
  ./target/release/shogun-api --dream-tick \
    --multi "$PWD/.shogun-demo/principals" --key-provider env
)
```

The tick is a one-shot command, not a background scheduler. Look for
`batch_lane=1` and a completed review, not merely exit status zero. `local_lane=1`
means the model lane was not selected. A pending/failed review needs investigation
or a later retry; never seed fixture results into the live store to make it pass.

## 4. Inspect state, evidence, and the receiving agent

Through the authenticated MCP connection:

1. Call `memory.recall` with `query: Kestrel Maya`, `budget_tokens: 4000`. Inspect
   `items`, confidence bands, event IDs and quotes. Check `state.people.list`,
   `state.projects.list`, `state.commitments.list`, and `state.open_loops.list`.
2. Read the original source with `memory.get_events`. Use `wiki.neighbors` with
   the returned `person-<id>` key to inspect evidence links.
3. Call `handoff.create` with a task to draft the Kestrel launch copy, the same
   query, and the receiving agent's actual ID in `to_agent`. Inspect both `facts`
   and `entities`; empty arrays mean this is not yet the rich workflow.
4. Connect the receiving client using its own bearer on the same principal.
   Call `handoff.accept` with the returned `handoff`, its own `actor`, and an
   idempotency key, then recall with `include_scratch: true`.

For a second local agent, provision another random private token file using the
quickstart's exclusive-create pattern. Restart the server with both mappings:
`SHOGUN_PRINCIPAL_TOKENS="demo:<first-token>:researcher,demo:<second-token>:writer"`.
Load the values from their private files, never put actual tokens in docs or Git.
Generate each client's bridge config with its corresponding `--token-file`.
The authenticated agent identity controls writes; changing the `actor` body alone
does not impersonate another agent. Separate principals require explicit ACLs.

Success means matching source-backed state and a verifiable receiving-agent
scratch record. Model output can vary, and the engine may reject unsupported
entities or quotes. Keep a failed run visible; do not call it verified merely
because the browser animation or offline replay works.

The production implementation is
[`dream_tick.rs`](../crates/shogun-core/src/tenancy/dream_tick.rs); handoff contracts
are in [the API guide](api/handoff.md). Client setup is in
[local-agent-setup.md](local-agent-setup.md).
