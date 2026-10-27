# State-quality end-to-end evaluation

Run `scripts/check-state-quality.sh` from the checkout. The test needs the Rust
toolchain and the same native SQLCipher build dependencies as the server. It creates
temporary encrypted files for two principals and removes them afterwards.

The scenario exercises the real engine path: `memory.ingest` storage and replay,
hourly Dream Cycle review, loop consolidation, persisted state and `memory.recall`.
The gateway and SDK HTTP transport are covered separately by `pnpm test:e2e`.

## Deterministic regression coverage

The synthetic conversation starts on Monday, 26 October 2026, in the host timezone:

1. The user promises Toru the Nebula launch deck by Friday and asks Toru to confirm
   the launch venue. Expect one grounded commitment and one waiting-on-them loop;
   a promise between two other people must not become the user's obligation.
2. The deck moves to **next Monday**. Expect the same commitment ID, a deadline of
   2 November, and a recall quote from the new message.
3. The deck is sent and the venue confirmation is received. Expect done/closed
   state, with no outstanding commitment or open-loop item returned by recall.

Each stage checks ingest idempotency, the exact source quote and event, confidence
bands, and isolation from a second principal whose encrypted store uses another key.
Repeating the same hour must make no further model calls. After each stage, every
handle for that principal is released and its encrypted store is reopened. Recall
must be unchanged, and replaying the ingest must still insert nothing.

A snooze scenario uses `state.update` to defer both the commitment and the waiting
loop. Neither may appear as a current recall item or context fact before its wake
time, including after the store is reopened. At the exact wake time both must return
with their source quotes, without another model call. Clearing a snooze must also
restore the items immediately. Source history remains searchable throughout.

An adversarial test runs four model answers through the same parser and
persistence path: invented quotes, event IDs not shown to the model, confidence
below the consolidator threshold, and another principal's source text. None may
become authoritative state. A malformed-provider-output test also proves that a
failed live answer cannot silently fall back to the scripted replay.

The default runner uses **hand-authored model answers**. Passing it verifies the
engine's contracts and catches regressions; it measures no real model's extraction
quality. The normal CI `daemon-server` test run includes these tests automatically.

## Full local stack

Use Node 22 from `.nvmrc`, pnpm from `package.json`, the Rust/SQLCipher build
dependencies, and Python with the `packages/sdk-py` dependencies installed. A local
PostgreSQL instance must be running, with a role allowed to create databases.

```bash
nvm use
pnpm install --frozen-lockfile
cargo build --locked -p shogun-core --features daemon-server --bin shogun-api
SHOGUN_E2E_POSTGRES_URL='postgres://localhost:5432/postgres?sslmode=disable' pnpm test:e2e
```

Add your local database username to the URL if it differs from your OS username.
The runner accepts only loopback database hosts, creates a randomly named test
database and encrypted stores, and removes them when finished. It starts the real
sidecar, gateway and router, then checks both SDKs, tenant isolation, exactly-once
ingest billing, free reads, pending L3 approvals and a sidecar process restart.
It makes no provider calls and sends no messages.

On machines with limited disk space, the Rust commands can use
`CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0` to omit
debug symbols and incremental compilation artifacts.

## Opt-in live runner

After signing into a supported vendor CLI, run one of:

```bash
scripts/check-state-quality.sh --live codex
scripts/check-state-quality.sh --live claude-code
scripts/check-state-quality.sh --live gemini-cli
```

This sends the same production review/consolidation prompts to the selected CLI
through the existing contained, text-only subscription adapter. Authentication
stays in the vendor CLI; its default model and account allowance apply. The runner
does not request a particular model. The
evaluation requests six model steps (two per conversation stage), uses only the
checked-in synthetic conversation, and creates no external sends or payments.

The adapter is confined to this test binary. Production Dream Cycle still uses its
normal Batch/relay routing. This run evaluates model output and state application;
it does not verify the deployed relay, its credentials or its billing integration.

An explicit live run must fail on missing authentication, an unsupported provider,
non-JSON output, or a quality assertion. It never falls back to scripted answers.
The live test is ignored in normal CI; no provider call is made by the default runner,
even if `SHOGUN_STATE_QUALITY_LIVE` is already exported. Changing model versions can
change live results. Record the provider, CLI version, model when known, and command
when reporting a live run.

## Regressions found while adding the scenario

- Recall selected the oldest reviewed quote after a deadline update. It now prefers
  the newest valid, readable source within the reviewed provenance class. A mixed
  world/scratch provenance regression also checks that a world-only reader cannot
  receive a private source quote and that an invalid quote falls back to valid proof.
- Recall still returned completed commitments and closed loops as current state
  items. They are now omitted from those items; dated source evidence remains searchable.
- Snoozed commitments and loops still appeared as current recall items and inline
  context facts. Both paths now honor `deferred_until`, using the store's clock and
  the existing wake-time predicate. The regression failed with both snoozed rows
  returned at high confidence before the fix.
