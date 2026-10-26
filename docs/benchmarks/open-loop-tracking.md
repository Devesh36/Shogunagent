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
| `heldout` | 15 | 12 | 12 | ~40% / ~10% | 6 (2 move twice) | 16 | 10 | the hard tier's shape with its own phrasing families (below), and ~30% of updates and closings posted on a thread of their own |

**The held-out tier** (`--tiers heldout`, plan E49; not in the default run, so v1.0 results are
unchanged). Every planted sentence — openings, resolutions, cancellations, due-date moves and
distractors — comes from template families written for it **without looking at the engine's
rules**: other verbs ("Let me take care of …; Oren will have it no later than Friday", "Pinged
Priya again about …, nothing back", "Signed off on …"), the passive voice ("… will be shared
with Oren by me on Friday", "… was handed to Oren today", "… has been rescheduled, new date
…"), non-native English ("OK I make … for Oren until Friday, no problem", "I already give … to
Oren, it is finish"), and Japanese sentences around English names and dates
("the Q3 budgetはFridayまでにOren Valeさんへお送りします。",
"the board memoの締切はSeptember 21に変更になりました"). About 30% of the updates and closings arrive on a new thread
(another email, another chat channel) instead of the item's. The gold, the canonical text and
the scorer are the same as for the other tiers. These families must not be used to write or
tune rules; a change that does should say so and report the held-out numbers from before it.

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
# The built-in adapters: ShogunAgent's engine on its local rules, the same engine with an offline
# scripted reviewer on its Batch lane (not a model), the "will" baseline, the oracle.
cargo run -p loop-bench --release -- run --adapter shogunagent   --seeds 5 --out shogunagent.json
cargo run -p loop-bench --release -- run --adapter shogunagent-scripted-review --seeds 5 --out scripted.json
cargo run -p loop-bench --release -- run --adapter will-baseline --seeds 5 --out baseline.json
cargo run -p loop-bench --release -- run --adapter oracle        --seeds 5     # every metric 1

# The held-out tier (not in the default run).
cargo run -p loop-bench --release -- run --adapter shogunagent --tiers heldout --seeds 5 --out heldout.json

# Add the hard-tier headline to a benchmarks.json as the `open_loop_tracking` suite row.
cargo run -p loop-bench --release -- run --adapter shogunagent --seeds 10 \
  --merge-into docs/benchmarks/benchmarks.json

cargo run -p loop-bench -- --help
```

`--tiers easy,medium,hard` (or `heldout`), `--seeds N` and `--first-seed 42` select the runs. The run is
deterministic in its quality numbers; latency moves with the machine.

### The adapters

- **`shogunagent`** (`ShogunAgentAdapter`): the engine in process, one fresh in-memory principal
  store per stream. Each event goes through `memory.ingest` (the same parser and store path the
  MCP, REST and CLI faces use; `source` = channel, `thread_key` = thread). After each hour the
  hourly Dream Cycle runs on the **on-device local rules**
  (`shogun_core::tenancy::dream_tick::tick_principal_local`: no model, no network) — the lane a
  server principal without a relay credential uses. Its StateUpdate step reads the outcomes later
  messages of a thread report for that thread's open rows (`shogun_memory::loop_updates`, E49).
  At the end the state is read through `state.export` with `format: "export-v2"`, every band,
  rows not superseded.
- **`shogunagent-scripted-review`** (`ScriptedReviewAdapter`, `src/scripted.rs`, plan E49): the
  same engine, ingest and read as `shogunagent`, but after each hour it runs the server's
  production Batch tick (`shogun_core::tenancy::dream_tick::tick_principal_batch`, the path
  `shogun-api --dream-tick` takes for a principal with a relay credential): the hourly review
  through the **Batch lane**, then the loop consolidator when its gate says so (unread signals or
  outcomes, at most hourly, eight a local day — counted from the traceability rows the lane
  writes before each call, as the relay does), with the lane answered in process by an
  **offline scripted reviewer — not a model**. Its window-review answer is the on-device rules run
  over each numbered line, anchored on that line as the answer contract asks; its consolidator
  answer re-lists every new signal with its exact quote at confidence 0.9 and applies every
  outcome it is shown ("Outcomes noticed in later messages") through that outcome's own quote.
  It reads only the input of each call, never the gold. Every check the engine runs on a real
  answer (exact span of a shown event, confidence ≥ 0.84, at most six items, quoted resolutions)
  runs unchanged. It measures the route to the actionable band, not model quality; results files
  name it, and it reports 0 model calls.
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
seeds (42–46) per tier, 2026-10-25. Mean [95% interval]; one value when the seeds agreed exactly.
"Before" is the engine at plan E28 (commit `cac2e3f`); "after" is the engine with plan E49
(outcomes read off later messages, paraphrase rules, the consolidator changes below), same seeds,
same generator, same scorer. The "after" files were re-run after the consolidator was wired into
the server's Batch tick (the scripted arm now goes through `tick_principal_batch` and its gate):
every quality number on the three v1.0 tiers came out identical to the first E49 run; only
latency moved (the latency lines below are from that re-run).

Full files in [`open-loop-tracking/`](open-loop-tracking/):
[`results-shogunagent-local-rules.json`](open-loop-tracking/results-shogunagent-local-rules.json) (after; since 2026-10-26 the E51 run, see [Held-out tier after E51](#held-out-tier-after-e51)),
[`results-shogunagent-local-rules-before-e49.json`](open-loop-tracking/results-shogunagent-local-rules-before-e49.json) (before),
[`results-shogunagent-scripted-review.json`](open-loop-tracking/results-shogunagent-scripted-review.json),
[`results-will-baseline.json`](open-loop-tracking/results-will-baseline.json);
held-out tier: [`results-heldout-shogunagent-local-rules.json`](open-loop-tracking/results-heldout-shogunagent-local-rules.json),
[`results-heldout-shogunagent-scripted-review.json`](open-loop-tracking/results-heldout-shogunagent-scripted-review.json),
[`results-heldout-will-baseline.json`](open-loop-tracking/results-heldout-will-baseline.json) (before any tuning);
after E51: [`results-heldout-shogunagent-local-rules-e51.json`](open-loop-tracking/results-heldout-shogunagent-local-rules-e51.json),
[`results-heldout-shogunagent-scripted-review-e51.json`](open-loop-tracking/results-heldout-shogunagent-scripted-review-e51.json).

### ShogunAgent, local rules (no model)

**Before (E28), operating point `all`**

| tier | tracking F1 | commit. recall | commit. precision | loop recall | loop precision | resolution | false closure | supersession | false commitment | provenance |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| easy | 0.842 [0.77, 0.92] | 1.000 | 0.840 [0.73, 0.95] | 0.600 [0.43, 0.77] | 1.000 | 0.000 | 0.000 | — | 0.200 [0.06, 0.34] | 1.000 |
| medium | 0.859 [0.84, 0.88] | 1.000 | 0.889 | 0.600 [0.53, 0.67] | 1.000 | 0.000 | 0.000 | 0.000 | 0.125 | 1.000 |
| hard | 0.540 [0.33, 0.75] | 0.433 [0.25, 0.62] | 0.894 [0.76, 1.03] | 0.350 [0.11, 0.59] | 1.000 | 0.000 | 0.000 | 0.000 | 0.037 [-0.00, 0.08] | 1.000 |

**After (E49), operating point `all`**

| tier | tracking F1 | commit. recall | commit. precision | loop recall | loop precision | resolution | false closure | supersession | false commitment | provenance |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| easy | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 0.000 | — | 0.000 | 1.000 |
| medium | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 0.000 | 1.000 | 0.000 | 1.000 |
| hard | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 0.940 [0.87, 1.02] | 0.000 | 0.900 [0.79, 1.01] | 0.000 | 1.000 |

Change, `all` (metrics that moved):

| tier | metric | before | after | delta |
|---|---|---:|---:|---:|
| easy | tracking_f1 | 0.842 | 1.000 | +0.158 |
| easy | commitment_precision | 0.840 | 1.000 | +0.160 |
| easy | open_loop_recall | 0.600 | 1.000 | +0.400 |
| easy | resolution_accuracy | 0.000 | 1.000 | +1.000 |
| easy | false_commitment_rate | 0.200 | 0.000 | -0.200 |
| medium | tracking_f1 | 0.859 | 1.000 | +0.141 |
| medium | commitment_precision | 0.889 | 1.000 | +0.111 |
| medium | open_loop_recall | 0.600 | 1.000 | +0.400 |
| medium | resolution_accuracy | 0.000 | 1.000 | +1.000 |
| medium | supersession_accuracy | 0.000 | 1.000 | +1.000 |
| medium | false_commitment_rate | 0.125 | 0.000 | -0.125 |
| hard | tracking_f1 | 0.540 | 1.000 | +0.460 |
| hard | commitment_recall | 0.433 | 1.000 | +0.567 |
| hard | commitment_precision | 0.894 | 1.000 | +0.106 |
| hard | open_loop_recall | 0.350 | 1.000 | +0.650 |
| hard | resolution_accuracy | 0.000 | 0.940 | +0.940 |
| hard | supersession_accuracy | 0.000 | 0.900 | +0.900 |
| hard | false_commitment_rate | 0.037 | 0.000 | -0.037 |

**Operating point `actionable` (≥ 0.5), before and after: empty on every tier** (no row; tracking
F1 0.000). That is the rule, not a gap: local rules alone never lift a candidate to 0.5 (owner
decision 2026-10-02). The path to that band is the next arm.

After (re-run): easy: ingest 1.19 ms/event, processing 0.38 s/run; medium: ingest 1.03 ms/event, processing 1.30 s/run; hard: ingest 1.03 ms/event, processing 2.91 s/run (hourly ticks included; the container was busier than for the first run; before: easy: ingest 0.90 ms/event, processing 0.28 s/run; medium: ingest 0.81 ms/event, processing 0.95 s/run; hard: ingest 0.81 ms/event, processing 1.85 s/run); 0 model calls, 0 cost.

### ShogunAgent, scripted review (offline script, not a model)

The same engine with its Batch lane answered by the **offline scripted reviewer**
(`--adapter shogunagent-scripted-review`, see [The adapters](#the-adapters)). The reviewer's
answers are scripted from the same on-device rules, so this arm can find nothing the local rules
do not; what it shows is the route a candidate takes to the actionable band and that every step on
it is gated by an exact quote. **These are not model numbers.** It did not exist before E49, so
there is no before row.

**Operating point `actionable` (≥ 0.5)**

| tier | tracking F1 | commit. recall | commit. precision | loop recall | loop precision | resolution | false closure | supersession | false commitment | provenance |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| easy | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 0.000 | — | 0.000 | 1.000 |
| medium | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 0.000 | 1.000 | 0.000 | 1.000 |
| hard | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 0.940 [0.87, 1.02] | 0.000 | 0.900 [0.79, 1.01] | 0.000 | 1.000 |

**Operating point `all`**

| tier | tracking F1 | commit. recall | commit. precision | loop recall | loop precision | resolution | false closure | supersession | false commitment | provenance |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| easy | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 0.000 | — | 0.000 | 1.000 |
| medium | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 0.000 | 1.000 | 0.000 | 1.000 |
| hard | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 0.940 [0.87, 1.02] | 0.000 | 0.900 [0.79, 1.01] | 0.000 | 1.000 |

What the consolidator did in the first E49 run, before it went through the production tick and its traced gate, summed over the five seeds (counts from
`ScriptedReviewAdapter::applied`): easy 44 calls, 40 re-lists, 14 resolutions; medium 112 calls, 95 re-lists, 27 resolutions; hard 178 calls, 156 re-lists, 59 resolutions; 0 items opened as new rows and 0 refused, on every tier. No item was ever *opened* as a new row: every
re-list landed in place on the local candidate that cites the same span, so the store holds one
row per item. Every actionable row carries the consolidator's confidence (0.9), and every close
of a reviewed row went through a `resolved` entry quoting the outcome's own words.

Re-run through the wired tick: easy: ingest 1.14 ms/event, processing 0.28 s/run; medium: ingest 1.08 ms/event, processing 1.06 s/run; hard: ingest 1.22 ms/event, processing 2.25 s/run; 0 model calls, 0 cost (the script is not billed and is not a model call).

### "will" baseline (unchanged)

| tier | tracking F1 | commit. recall | commit. precision | loop recall | loop precision | resolution | false closure | supersession | false commitment | provenance |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| easy | 0.580 [0.56, 0.60] | 1.000 | 0.693 [0.62, 0.77] | 0.000 | — | 0.000 | 0.000 | — | 0.450 [0.31, 0.59] | 1.000 |
| medium | 0.593 | 1.000 | 0.727 | 0.000 | — | 0.000 | 0.000 | 0.000 | 0.375 | 1.000 |
| hard | 0.493 [0.38, 0.60] | 0.767 [0.57, 0.97] | 0.696 [0.56, 0.83] | 0.000 | — | 0.000 | 0.000 | 0.000 | 0.263 [0.18, 0.35] | 1.000 |

The oracle scores 1 on every metric and 0 on both false rates (tested).

### Held-out tier (`heldout`): before any tuning

The rules above were never shown these families. Run on 2026-10-25 with the engine as of the E49
change described here, five seeds (42–46), **before any rule was changed for them** (none has
been since: the outcome rules for text without a thread and the consolidator wiring do not
touch threaded text, and a re-run after them gave the same numbers). The number to compare is
the hard tier's 1.000 above: the gap is how much of that score was the templates.

| arm | point | tracking F1 | commit. recall | commit. precision | loop recall | loop precision | resolution | false closure | supersession | false commitment | provenance |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| local rules | all | 0.489 [0.42, 0.56] | 0.383 [0.33, 0.44] | 1.000 | 0.267 [0.13, 0.40] | 1.000 | 0.000 | 0.000 | 0.000 | 0.000 | 1.000 |
| local rules | actionable | 0.000 | 0.000 | — | 0.000 | — | 0.000 | — | 0.000 | 0.000 | — |
| scripted review | all | 0.489 [0.42, 0.56] | 0.383 [0.33, 0.44] | 1.000 | 0.267 [0.13, 0.40] | 1.000 | 0.000 | 0.000 | 0.000 | 0.000 | 1.000 |
| scripted review | actionable | 0.489 [0.42, 0.56] | 0.383 [0.33, 0.44] | 1.000 | 0.267 [0.13, 0.40] | 1.000 | 0.000 | 0.000 | 0.000 | 0.000 | 1.000 |
| "will" baseline | all = actionable | 0.206 [0.11, 0.30] | 0.233 [0.12, 0.35] | 1.000 | 0.000 | — | 0.000 | 0.000 | 0.000 | 0.000 | 1.000 |

Local rules: ingest 0.72 ms/event, processing 1.59 s/run; scripted review: ingest 0.75 ms/event,
processing 1.47 s/run; 0 model calls, 0 cost.

- **Tracking F1 falls from 1.000 (hard) to 0.489.** About two in five held-out commitments and one
  in four open loops are found; the rest are worded in ways no rule reads (other verbs, the
  passive, non-native English, Japanese with English names in it).
- **Resolution and supersession are 0.000.** None of the held-out ways of saying "done", "called
  off" or "moved" ("went out to …", "has … in hand", "was handed to …", "has been shelved",
  "is now expected …", "has been rescheduled, new date …", Japanese "送付済み", "変更になりました")
  is a cue of the outcome rules, and about 30% of them arrive on another thread, where the rules
  do not look by design.
- **What held: precision.** Commitment and loop precision stay 1.000, the false-commitment rate
  stays 0.000 against the new distractor families, there is no false closure, and every quote is
  exact. The rules miss rather than invent.
- The scripted-review arm matches the local rules exactly, as it must: its reviewer is scripted
  from the same rules. It shows the route to the actionable band holds for what the rules find;
  a real model's reviewer is the arm that could close the recall gap, and it is still unmeasured.

**What the numbers say.**

- **Resolution and supersession are no longer zero.** Before, the engine resolved nothing and
  moved no due date on any tier. Now a later message in the same thread that says the item was
  done, called off or moved is recorded as a typed update with its quote and event
  (`loop_updates`, V104) and, for a local candidate, closes or re-dates it with that quote linked
  as provenance — never raising its confidence. Hard-tier resolution reaches 0.940 and
  supersession 0.900, with no false closure on any tier.
- **Every remaining miss is one rule refusing to guess.** The hard tier folds several meeting
  items into one note; their later updates arrive in the thread of the item's *original* meeting,
  not of the note that carries the item, so "same thread" never holds and nothing is closed
  (seed 46: three of three misses). Matching an outcome across threads by wording alone is what
  the rule deliberately does not do.
- **Paraphrased promises are found without new false commitments.** Hard-tier commitment recall
  goes from 0.433 to 1.000 (the "will" baseline is at 0.767) and the false-commitment rate from
  0.037 to 0.000: the new rules read acceptances ("Leave the memo with me", "Count on me for …",
  "… is coming your way") and the reported forms of loops ("No word yet from …", "asked you to
  review …", "Decision pending on …"), and they veto hypotheticals ("If …, I will …", "Maybe …,
  no promises") and retracted offers ("I was going to …, but that is off now").
- **Read these as an upper bound for template-shaped text.** The new phrases were written while
  looking at this generator's template families, so a perfect score on them says the rules cover
  those families, not that they generalise. Two checks against overfitting: each rule has unit
  tests with forms the generator never produces (`extract::paraphrase_tests`,
  `loop_updates::tests`), and the independent extraction eval (`cargo run -p extract-eval`, six
  golden days written for E-series extraction work) is byte-identical before and after — the same
  21 commitments and 18 open loops, the same 4 and 1 extras, precision unchanged. The held-out
  tier is the real test, and it says the rules do not generalise much past these families:
  tracking F1 0.489, resolution and supersession 0.000, precision intact (previous section; E51 took it to 0.665, see [Held-out tier after E51](#held-out-tier-after-e51)).
- **How a candidate reaches the actionable band** is the scripted-review arm: the hourly window
  review turns the local candidate into a typed signal with the exact span of its line; the loop
  consolidator re-lists it in place at its answer's confidence, which code accepts only at ≥ 0.84
  and only on an exact quote of an event the call was shown; outcomes of a row the consolidator
  owns are shown to it ("Outcomes noticed in later messages") and change the row only through a
  `resolved` entry or a re-list that quotes them. With that path, every tier's `actionable` point
  matches its `all` point.
- What a real model adds or loses on this path is still unmeasured: it needs a named model, a cost
  line and a network run, none of which this document claims.

### Held-out tier after E51

Plan E51 (2026-10-26) adds rules for the three families the held-out tier exposed: the passive
voice, Japanese sentences around English names and dates, and outcomes posted on a different
thread than the item. The pre-tuning table above is kept as it was.

**How the rules were written.** Not from the held-out templates: `crates/loop-bench/src/gen.rs`
was not opened while writing them (only the `Event` struct's fields were seen, to know what the
adapter ingests). The rules come from general English and Japanese phrasing, and each has its own
unit tests with sentences written for them, positives and distractors
(`extract::paraphrase_tests`: `a_passive_promise_by_me_is_my_commitment`,
`passive_notices_other_agents_and_conditions_are_not_promises`,
`japanese_promises_around_english_names_are_commitments`,
`japanese_hedges_closings_and_reports_are_not_commitments`, …; `loop_updates::tests`:
`passive_outcomes_are_read`, `japanese_outcomes_are_read_with_their_exact_span`,
`an_outcome_on_another_thread_closes_the_one_row_it_clearly_names`,
`an_outcome_on_another_thread_that_is_ambiguous_or_thin_changes_nothing`, …; `due::tests`:
`english_dates_inside_japanese_sentences`). Two caveats, stated so the numbers can be weighed:
this document's own dataset card and pre-tuning notes quote about fifteen held-out fragments
("… was handed to Oren today", "has been rescheduled, new date …", "送付済み",
"変更になりました", …) and they were read before the rules were written, so the held-out tier is
less blind than a set nobody has seen; and the loop-bench was run only after every unit test
passed, once, then once more after one change made because of a **hard**-tier (not held-out)
false closure (below). The held-out numbers did not move between those two runs.

What the rules are:

- **Passive voice.** A promise with the speaker as the agent ("… will be sent to Oren by me on
  Friday", "is being handled by me", "from my side"), not when conditional; someone's promise to
  the user ("will be sent to you", "you will be sent …") only with a deadline the date parser
  reads, so a notice ("a confirmation will be sent to you") is not one. Loops: "is still awaited
  from", "no reply has been received from", "your review is requested", "your approval is needed",
  "has not been decided", "no decision has been made". Outcomes: an auxiliary and a participle as
  whole words ("was handed to", "has been signed", "got approved"), passive cancellations ("has
  been called off", "was shelved"; "dropped off" is a delivery and refused), and moves whose new
  date comes anywhere after the cue ("has been rescheduled; the new date is Thursday", "was
  extended to September 21"). The existing future / negation / question vetoes apply.
- **Japanese around English names and dates.** The date parser reads an English date followed
  by a deadline particle ("Fridayまでに", "September 21に"), as "by Friday" reads in English.
  Content words split where Latin script meets Japanese, so "budgetはFridayまでにOren" is budget /
  Oren, not one token. More promise forms ("お送りいたします", "提出予定です" unless another person
  is the subject, "任せてください" only with a deadline), reported promises to the user
  ("送ってくださるとのこと"), Japanese hedges that veto every Japanese promise ("〜たら", "もし",
  "かもしれ", "つもりでしたが"), loops ("まだ届いていません", "返事がありません", "チェックをお願い",
  "まだ決まっていません"), and outcomes ("送付済み", "届きました", "中止", "必要ありません", and
  re-dates "Mondayに変更", "9月14日に延期" with the date nearest the cue, after any "から"), under
  their own vetoes (negative, まだ, 予定, a question, a condition, long ago).
- **Outcomes on another thread** (`loop_updates::pick_cross_thread`). A threaded outcome that
  names no row of its own thread may name a row of another thread when it shares at least three
  content words with it and at least half of its own; among them a name (capitalised in the row
  or the outcome: the counterparty or the project) and a word that is not a name (the artifact);
  and no other open row is within two shared words of it. A hedged sentence or a retracted offer
  ("Scratch my offer on …") names nothing across threads. Screen text never takes this path
  (no thread; an app-scoped source is refused even with one), and meeting text never leaves a
  lead for a consolidator-owned row. What it may then do is what an outcome on the row's own
  thread may do (owner decisions 2026-10-02 / 2026-10-14): close or re-date a **local
  candidate**, with the quote as provenance and its confidence untouched (still < 0.5); a row
  the **consolidator** or the **user** owns is left as it is, with the update recorded unread for
  the next consolidator call. No migration (`loop_updates`, V104, already holds these rows).

Five seeds (42–46), release build, same container conditions as above, 2026-10-26.

| arm | point | tracking F1 | commit. recall | commit. precision | loop recall | loop precision | resolution | false closure | supersession | false commitment | provenance |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| local rules | all | 0.665 [0.59, 0.74] | 0.517 [0.43, 0.60] | 1.000 | 0.483 [0.37, 0.60] | 1.000 | 0.176 [0.11, 0.24] | 0.000 | 0.300 [-0.07, 0.67] | 0.000 | 1.000 |
| local rules | actionable | 0.000 | 0.000 | — | 0.000 | — | 0.000 | — | 0.000 | 0.000 | — |
| scripted review | all | 0.665 [0.59, 0.74] | 0.517 [0.43, 0.60] | 1.000 | 0.483 [0.37, 0.60] | 1.000 | 0.176 [0.11, 0.24] | 0.000 | 0.300 [-0.07, 0.67] | 0.000 | 1.000 |
| scripted review | actionable | 0.665 [0.59, 0.74] | 0.517 [0.43, 0.60] | 1.000 | 0.483 [0.37, 0.60] | 1.000 | 0.176 [0.11, 0.24] | 0.000 | 0.300 [-0.07, 0.67] | 0.000 | 1.000 |

Held-out, before (pre-tuning table) → after E51, local rules, `all` (the scripted arm moved the
same way at both points):

| metric | before | after | delta |
|---|---:|---:|---:|
| tracking_f1 | 0.489 | 0.665 | +0.176 |
| commitment_recall | 0.383 | 0.517 | +0.134 |
| open_loop_recall | 0.267 | 0.483 | +0.216 |
| resolution_accuracy | 0.000 | 0.176 | +0.176 |
| supersession_accuracy | 0.000 | 0.300 | +0.300 |
| precision (both kinds), false closure, false commitment, provenance | 1.000 / 0.000 / 0.000 / 1.000 | unchanged | 0 |

The v1.0 tiers, same five seeds, before (E49, re-run on 2026-10-26 from the parent commit: the
same numbers as the "After (E49)" tables above) → after E51. Every other metric of every tier
and both arms is identical, `actionable` included:

| tier | metric | before | after |
|---|---|---:|---:|
| hard | resolution_accuracy | 0.940 [0.87, 1.02] | 0.986 [0.95, 1.03] |
| hard | supersession_accuracy | 0.900 [0.79, 1.01] | 1.000 |

Both are the cross-thread rule: the hard tier's meeting items whose updates arrive in the thread
of the item's original meeting (see "Every remaining miss" above) now close and move. The first
after-run also had one **false closure** on hard (seed 44, false-closure rate 0.020 [-0.04, 0.08]):
a retracted offer posted on another thread ("Scratch my offer on the training schedule for …,
someone else has it.", a distractor) closed the open commitment about the same schedule and
person. The rule was changed so that a hedged sentence or a retracted offer names nothing across
threads (`extract::hedged_or_retracted`, with its own test), and the false closure is back to
0.000; this was a change made after looking at a hard-tier result, which the hard tier's 1.000s
already were.

Latency (local rules, after): easy ingest 0.83 ms/event, processing 0.27 s/run; medium 0.84 ms,
1.03 s; hard 0.83 ms, 2.26 s; held-out 0.74 ms, 1.75 s (before: hard 0.95 ms, 2.64 s; held-out
0.79 ms, 1.76 s — the container was shared, read these as unchanged). 0 model calls, 0 cost.

What these say:

- **About a third of the held-out gap closed, with precision intact.** Tracking F1 0.489 →
  0.665; commitment and loop precision stay 1.000, no distractor became a row, nothing was
  closed that should not be, and every quote is exact. The rules still miss rather than invent.
- **Resolution and supersession are no longer zero on held-out text, but low** (0.176, 0.300 with
  a wide interval). An outcome can only close a row that was found, so the recall left on the
  table (half the commitments and loops) caps them, and the families E51 did not target (other
  verbs, non-native English) are still unread.
- **The v1.0 tiers did not lose anything**, and the hard tier's two remaining misses of the E49
  run (cross-thread meeting updates) are now read.
- The extraction eval (`cargo run -p extract-eval -- report`) is byte-identical before and after
  E51: no Japanese or passive rule fires on its six golden days.

## Limits

- Synthetic text from templates: a system tuned to these templates can overfit them. The
  paraphrase tier and the distractor families exist to make that harder, not impossible. The
  held-out tier is in the repository, so it is held out only as long as nobody writes rules from
  it; a version released only for scoring would be stronger.
- English only on the v1.0 tiers; the held-out tier adds Japanese sentences around English names
  and dates, not fully Japanese streams (dates stay English, as the generator writes them).
- One principal per stream, no cross-principal facts, no calendar events.
- Text-match tolerance (token F1 ≥ 0.5) is a choice; the `counts` in every run let anyone
  recompute with another rule.
