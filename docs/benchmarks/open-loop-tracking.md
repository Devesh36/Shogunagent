# Open-loop tracking benchmark v1

A benchmark for the part of memory that agent-memory benchmarks do not measure: **keeping track of
what is owed**. Given a stream of a person's email, chat, meeting notes and agent conversations,
does a system know, at the end, which commitments and open loops exist, which were resolved,
which due dates moved, and which promise-shaped sentences were never promises at all — and can it
prove each one with a quote?

LoCoMo and LongMemEval ask questions about past conversations; their answer keys have audited
error rates (about 6.4% of LoCoMo's questions corrupt the score) and their default LLM judge
accepts most deliberately wrong but topically adjacent answers. This benchmark has no judge and
no model anywhere in scoring: the data is synthetic and generated with its answer key, and every
metric is exact arithmetic over the system's end-of-stream state. It reports a confidence
interval over seeds for every number, because single-run comparisons on these benchmarks are
mostly statistically indistinguishable.

- Code: `crates/loop-bench/` (generator `src/gen.rs`, scorer `src/score.rs`, adapters
  `src/adapter.rs`, runner `src/harness.rs`, CLI `src/bin/loop-bench.rs`).
- Measured results: [`open-loop-tracking/`](open-loop-tracking/) (results JSON per adapter).
- Version: `1.0`. Plan item: E28 (public v1; E12-T3 was the internal v0).

## Task

A system receives the events of one synthetic principal in time order, one hour at a time, and
may process in the background after each hour. At the end of the stream it is asked for its
state: a list of **commitments** and **open loops**, each with

| field | meaning |
|---|---|
| `kind` | `commitment` or `open_loop` |
| `text` | its description |
| `status` | `open` or `closed` (resolved, cancelled, done) |
| `due_ms` | the due date it holds (unix ms; compared by UTC day), or null |
| `confidence` | the system's own confidence, 0–1 |
| `provenance` | `[{event_id, quote}]`: the dataset events it cites and the exact span of each that proves the row |

That list is all that is scored. How the system gets there (extraction at ingest, a nightly
job, a model, rules) is its business; it must not read the gold file.

## Dataset card

| | |
|---|---|
| Content | Synthetic. Twelve fictional people, five fictional principals, twenty-four artifacts and six projects from fixed pools in `src/gen.rs`. **No personal data, no real conversations, no model-generated text.** |
| Generation | Deterministic: SplitMix64 (`memory_bench::rng`, in-tree) seeded by `(tier, seed)`, fixed templates. The same seed gives the same stream on any machine, forever. |
| Channels | `email`, `chat`, `meeting` (notes), `agent_turn` (a `user:` turn to an assistant). |
| Time | Day 0 is Monday 2026-09-07 UTC; events at working hours (09:00–18:00 UTC). |
| Planted | Commitments (the principal's own, 60%, and other people's to the principal, 40%) with a due date; open loops of four kinds (waiting on someone, a reply owed, a review requested, a decision pending). |
| Later events | Resolutions ("Sent the Q3 budget to Oren."), cancellations ("Never mind about …"), supersessions ("Update: … moves to Thursday."), once or twice per item. |
| Distractors | Hypotheticals ("If the budget comes through, I will send …"), other people's promises to third parties ("FYI, Priya told Kenji she will send …"), retracted offers ("I was going to send …, but that is off now."), promises already kept ("Last month I sent …"). |
| Filler | Per-channel chatter with no promise or loop in it. |
| Licence | Apache-2.0, like the repository. |

**Tiers** (per seed):

| tier | days | commitments | open loops | resolved / cancelled | due-date moves | distractors | filler per item | what makes it harder |
|---|---:|---:|---:|---|---:|---:|---:|---|
| `easy` | 5 | 4 | 4 | ~30% / 0% | 0 | 4 | 3 | one sentence per event, canonical wording, updates on the same channel |
| `medium` | 10 | 8 | 8 | ~35% / ~10% | 3 | 8 | 6 | greetings and sign-offs around the sentence; resolutions and updates arrive on another channel |
| `hard` | 15 | 12 | 12 | ~40% / ~10% | 6 (2 move twice) | 16 | 10 | paraphrased promises and loops ("Leave the board memo with me; it'll be with Oren by Friday."), several items in one meeting note |

The gold file per `(tier, seed)` lists each item with its opening event and exact quote, its
first and latest due date, the events that moved it, its final status and the event that closed
it, and each distractor with its event and span. Every gold quote is an exact span of its event
(tested for every tier on five seeds).

## Metrics

Matching: a prediction and a gold item of the same kind match when the prediction cites the
item's opening (or update) event, or its text has token F1 ≥ 0.5 with the item's description or
quote (stop words removed). Pairs are taken one-to-one, best first, so a second row for the same
item is an unmatched prediction.

| metric | definition | better |
|---|---|---|
| `tracking_f1` | micro F1 over commitments and open loops together (**headline**) | higher |
| `commitment_recall` / `_precision` / `_f1` | matched commitments over gold commitments / over predicted commitments | higher |
| `open_loop_recall` / `_precision` / `_f1` | the same for open loops | higher |
| `resolution_accuracy` | of the items resolved or cancelled in the stream, the share matched **and** reported `closed` (end to end: unmatched counts as not closed) | higher |
| `false_closure_rate` | of the matched items still open, the share reported `closed` | lower |
| `supersession_accuracy` | of the items whose due date moved, the share matched **and** holding the latest due date (same UTC day) | higher |
| `false_commitment_rate` | of the distractors, the share that produced an unmatched prediction (citing the distractor's event, or with text close to it) | lower |
| `provenance_validity` | of the predictions, the share whose every cited quote is non-empty and an exact substring of the cited event | higher |
| latency | wall time of ingest (per event), background processing (per run) and the final read | lower |
| cost | model calls and model spend (micro-USD) reported by the adapter | lower |

A metric whose denominator is empty is `null`, never `0` (for example supersession on `easy`,
precision when nothing was predicted).

**Operating points.** Every run is scored twice: `all` (every row the system reports) and
`actionable` (rows with confidence ≥ 0.5, the band ShogunAgent renders at all). A system that
reports no confidence should report 1.0.

**Aggregation.** Mean and 95% interval over seeds (Student's t with n − 1 degrees of freedom,
`memory_bench::harness::stats`, the same as `benchmarks.json`). An interval is not clipped to
[0, 1]. Five seeds (42–46) per tier by default.

## Running it

```bash
# The built-in adapters: ShogunAgent's engine on its local rules, the "will" baseline, the oracle.
cargo run -p loop-bench --release -- run --adapter shogunagent   --seeds 5 --out shogunagent.json
cargo run -p loop-bench --release -- run --adapter will-baseline --seeds 5 --out baseline.json
cargo run -p loop-bench --release -- run --adapter oracle        --seeds 5     # every metric 1

# Add the hard-tier headline to a benchmarks.json as the `open_loop_tracking` suite row.
cargo run -p loop-bench --release -- run --adapter shogunagent --seeds 10 \
  --merge-into docs/benchmarks/benchmarks.json

cargo run -p loop-bench -- --help
```

`--tiers easy,medium,hard`, `--seeds N` and `--first-seed 42` select the runs. The run is
deterministic in its quality numbers; latency moves with the machine.

### The adapters

- **`shogunagent`** (`ShogunAgentAdapter`): the engine in process, one fresh in-memory principal
  store per stream. Each event goes through `memory.ingest` (the same parser and store path the
  MCP, REST and CLI faces use; `source` = channel, `thread_key` = thread). After each hour the
  hourly Dream Cycle runs on the **on-device local rules**
  (`shogun_core::tenancy::dream_tick::tick_principal_local`: no model, no network) — the lane a
  server principal without a relay credential uses. At the end the state is read through
  `state.export` with `format: "export-v2"`, every band, rows not superseded.
- **`will-baseline`** (`WillBaseline`): every sentence containing the word "will" or "I'll" is an
  open commitment with no due date, quoted exactly, confidence 1. It exists to show the scorer
  discriminates: it never closes anything, never reads a date, finds no open loops, and is fooled
  by any distractor with "will" in it.
- **`oracle`**: the gold as predictions, to check a pipeline end to end.

## Submitting results for another system

There is no hosted leaderboard yet. To have a system's numbers compared with the ones below:

1. Generate the streams and answer keys you will run (the seeds and tiers are the contract):

   ```bash
   for tier in easy medium hard; do for seed in 42 43 44 45 46; do
     cargo run -p loop-bench --release -- generate --tier $tier --seed $seed --out data/$tier-$seed
   done; done
   ```

   `data/<tier>-<seed>/events.jsonl` is the stream (one event per line: `id`, `ts_ms`, `channel`,
   `thread`, `author`, `content`); `gold.json` is the answer key. Feed the events to your system in
   order, hour by hour, without showing it `gold.json`.

2. Write your system's end-of-stream state per run as `preds/<tier>-<seed>.json`:

   ```json
   {
     "adapter": { "name": "my-memory", "version": "0.3.1", "model": "provider/model-id or null", "description": "…" },
     "predictions": [
       { "kind": "commitment", "text": "send the Q3 budget to Oren by Friday", "status": "open",
         "due_ms": 1789689600000, "confidence": 0.9,
         "provenance": [{ "event_id": "e0042", "quote": "I'll send the Q3 budget to Oren by Friday." }] }
     ],
     "latency": { "ingest_ms": 0, "process_ms": 0, "read_ms": 0, "total_ms": 0, "events": 0, "ingest_ms_per_event": 0 },
     "usage": { "model_calls": 0, "input_tokens": 0, "output_tokens": 0, "cost_micro_usd": 0 }
   }
   ```

   `latency` and `usage` are optional but a submission without them is reported without cost.

3. Score them:

   ```bash
   cargo run -p loop-bench --release -- score --dir preds --seeds 5 --out my-memory.json
   ```

4. Open a pull request adding `my-memory.json` under `docs/benchmarks/open-loop-tracking/`, with
   the commit of this repository you scored against, the exact command, and the model and
   configuration your system used. Or implement `SystemAdapter` (`crates/loop-bench/src/adapter.rs`)
   for your system and add it to `by_name`, so anyone can rerun it.

A result is comparable with another only at the same benchmark version, tiers and seeds.

## Measured numbers

Release build, Linux x86_64 container shared with other builds (latency is indicative), five
seeds (42–46) per tier, operating point `all`, 2026-10-25. Mean [95% interval]; one value when the
seeds agreed exactly. Full files: [`results-shogunagent-local-rules.json`](open-loop-tracking/results-shogunagent-local-rules.json),
[`results-will-baseline.json`](open-loop-tracking/results-will-baseline.json).

**ShogunAgent, local rules (no model)**

| tier | tracking F1 | commit. recall | commit. precision | loop recall | loop precision | resolution | false closure | supersession | false commitment | provenance |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| easy | 0.842 [0.77, 0.92] | 1.000 | 0.840 [0.73, 0.95] | 0.600 [0.43, 0.77] | 1.000 | 0.000 | 0.000 | — | 0.200 [0.06, 0.34] | 1.000 |
| medium | 0.859 [0.84, 0.88] | 1.000 | 0.889 | 0.600 [0.53, 0.67] | 1.000 | 0.000 | 0.000 | 0.000 | 0.125 | 1.000 |
| hard | 0.540 [0.33, 0.75] | 0.433 [0.25, 0.62] | 0.894 [0.76, 1.03] | 0.350 [0.11, 0.59] | 1.000 | 0.000 | 0.000 | 0.000 | 0.037 [0.00, 0.08] | 1.000 |

Ingest 0.81–0.90 ms per event; background processing 0.28 s (easy), 0.95 s (medium), 1.85 s
(hard) per run, hourly ticks included; 0 model calls, 0 cost.

**"will" baseline**

| tier | tracking F1 | commit. recall | commit. precision | loop recall | loop precision | resolution | false closure | supersession | false commitment | provenance |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| easy | 0.580 [0.56, 0.60] | 1.000 | 0.693 [0.62, 0.77] | 0.000 | — | 0.000 | 0.000 | — | 0.450 [0.31, 0.59] | 1.000 |
| medium | 0.593 | 1.000 | 0.727 | 0.000 | — | 0.000 | 0.000 | 0.000 | 0.375 | 1.000 |
| hard | 0.493 [0.38, 0.60] | 0.767 [0.57, 0.97] | 0.696 [0.56, 0.83] | 0.000 | — | 0.000 | 0.000 | 0.000 | 0.263 [0.18, 0.35] | 1.000 |

The oracle scores 1 on every metric and 0 on both false rates (tested).

**What the numbers say.**

- The scorer separates the systems where they differ and agrees where they do not. Both find the
  canonical promises (commitment recall 1.0 on easy and medium); only the engine finds open loops
  and rejects most distractors (false commitment 0.04–0.20 against 0.26–0.45). Both quote exactly,
  so provenance validity is 1.0 for both — the metric is there to catch systems that paraphrase
  their evidence.
- **On its local rules the engine resolves nothing and moves no due date** (resolution and
  supersession 0.000 on every tier). That is the documented design, not a scoring artefact: the
  on-device rules only *propose* low-confidence candidates (≤ 0.4); closing loops, folding
  duplicates and re-dating commitments is the loop consolidator's job after the Batch review,
  which needs the model lane. For the same reason the `actionable` operating point (≥ 0.5) is
  empty for this adapter on every tier (tracking F1 0.000; in the results files).
- Paraphrase is where the local rules break: on `hard` commitment recall drops to 0.43, below the
  baseline's 0.77, because "Leave the board memo with me" has no cue the rules know. The engine
  still wins on the headline through open loops and precision.
- The numbers to beat with the Batch lane are therefore resolution, supersession and hard-tier
  recall. Running this adapter through a Batch lane (the relay) is a follow-up: it needs a model
  and so a named reader, a cost line and a network run, none of which this document claims.

## Limits

- Synthetic text from templates: a system tuned to these templates can overfit them. The
  paraphrase tier and the distractor families exist to make that harder, not impossible; a
  future version should add a held-out template set released only for scoring.
- English only. The engine's rules cover Japanese too; the generator does not yet.
- One principal per stream, no cross-principal facts, no calendar events.
- Text-match tolerance (token F1 ≥ 0.5) is a choice; the `counts` in every run let anyone
  recompute with another rule.
