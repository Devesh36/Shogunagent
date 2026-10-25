# Benchmarks — the same-harness run

`benchmarks.json` in this directory is produced by one command, from one commit, over ten seeds, by
the benchmark that already lived in the tree (`crates/memory-bench`). Nothing in it comes from a
model: no embedding, no reader, no judge. This page says how to reproduce it and what every number
means, and records the two weaknesses E12-T1 closed (temporal recall@1, abstention false evidence)
before and after.

## Run it

```bash
# Release build; ten seeds (42..51); the daemon's retrieval path. The committed file was made at
# --events 20000 (see "Scale" below); without --events the harness runs at the 100k SLO scale.
cargo run -p memory-bench --release -- harness \
  --seeds 10 --events 20000 --out docs/benchmarks/benchmarks.json

# Same, with one fresh on-disk SQLite per run (write latency then includes the filesystem).
cargo run -p memory-bench --release -- harness --db-dir /tmp/shogun-harness --out /tmp/benchmarks.json

cargo run -p memory-bench -- --help      # the HARNESS section lists every flag
```

Always `--release`: debug latency is roughly an order of magnitude off, and `environment.profile`
in the file says which one produced it. The run is deterministic for a given `(seeds, events, k)`:
the corpora derive from the seeds through a SplitMix64 defined in `crates/memory-bench/src/rng.rs`,
not from any crate whose output may change between versions.

**Scale.** The committed `benchmarks.json` is ten seeds × four suites at **20k events** per
corpus (18 minutes in memory on the Linux container that produced it). The 100k
scale the SLO is stated at was run once per suite (seed 42) for the table below, not ten times:
ingesting 100k events costs about ten minutes of CPU per run on this machine (the dedup lookup on
`event_log.content_hash` is a table scan, a pre-existing property of the write path and not
something this change touches), and forty such runs do not fit a working session. Quality metrics
did not move between 20k and 100k (both tables below); latency does, so quote p95 from the 100k
rows. Per-run reports (one JSON per `(workload, seed)`) come from the plain `memory-bench`
command; the harness keeps only the aggregate.

## What is in `benchmarks.json`

```json
{
  "schema": "1",
  "generated_at": "2026-10-25T09:40:00Z",      // RFC 3339, UTC
  "commit": "…",                               // build-time commit of the binary; null outside a checkout
  "commit_dirty": false,                       // true: uncommitted changes, or the tree moved since the build
  "embedding": null,                           // {model, dim} of the pinned model with --embedder; null = lexical only
  "reader": null,                              // {model} of the answering model; null = no model answered
  "judge": null,                               // {model} of the scoring model; null = metrics are exact, not judged
  "seeds": [42, 43, …],
  "environment": { "os", "arch", "profile", "in_memory", "search_path", "events", "k" },
  "suites": [ { … } ]
}
```

The committed file names the parent commit `e21f3ea` with `commit_dirty: true`: it was measured on
the working tree of this change before the change was committed (the file cannot carry the hash of
the commit that contains it). A re-run from the merged commit gives the same quality numbers; only
latency moves with the machine.

Every suite row has the same keys; a value a suite cannot express is `null`, never `0`.

| key | meaning |
|---|---|
| `name` | the suite (below) |
| `status` | `ok` — ran; `not_run` — skipped, with `reason` |
| `metric` | the headline metric's name |
| `value` | mean of the per-seed values |
| `ci95` | `[lo, hi]`: 95% interval of that mean across seeds, Student's t with `n − 1` degrees of freedom (`crates/memory-bench/src/harness/stats.rs`). Seeds are the unit of replication: the interval says how much the number moves with the data. For one seed it equals `[value, value]`. |
| `n` | seeds that produced a value |
| `tokens_per_answer` | mean estimated tokens of event text a query returned, four characters per token: the context a reader would be handed per answer. A size indicator, not a tokenizer. |
| `write_cost_micro_usd` | model spend on the write path per event, in micro-USD. `0.0` in the default run because ingestion calls no model (lexical and gram indexing only). It is not a compute price. |
| `p95_ms` | mean per-seed p95 of query latency, milliseconds (nearest-rank, `spike_harness::stats::Percentiles`, the one p95 definition in the repository) |
| `direction` | `higher` or `lower` is better |
| `workload`, `queries_per_seed`, `per_seed` | the generating workload, how many questions each seed asked, and the ten raw values behind `value` |

### Suites

| suite | workload | metric | what the number means |
|---|---|---|---|
| `query_p95` | `clean` (100k unique events, 500 questions, one answer each) | `query_p95_ms` | 95th percentile of one search's wall time through `search_federated`, the path every Memory API search runs. NFR-SLO-04 states the budget at 500 ms for 100k events. An in-memory run (`environment.in_memory: true`) measures retrieval without the filesystem; it is a regression signal, not a certification (`docs/shogunai-origin/memory-quality-verification.md` describes what certifies). |
| `temporal_recall_at_1` | `temporal` (12 projects each restating a fact three times, A → B → A; present-tense question "where does {p} currently store its records") | `recall_at_1` | share of questions whose top result carries the fact asserted **last**. The earlier value is a genuine row that matches the words; ranking it first is a wrong answer to a present-tense question. |
| `abstention_false_evidence` | `abstention` (chatter only; 500 questions about decisions no row ever made) | `false_evidence_rate` | share of unanswerable questions that still got rows back — the share of the time a downstream agent would be handed evidence for an answer that does not exist. Lower is better; `0.0` means the search abstained every time. The bench opts in to the relevance floor's empty result (`abstain_below_floor`, below); the floor itself is measured identically on every path. |
| `multi_session_coverage_at_5` | `multi-session` (48 questions whose answer is two facts stated far apart, in different apps) | `coverage_at_5` | share of questions whose **whole** answer (both facts) was inside the top 5. Recall asks whether a part appeared; coverage asks whether the question became answerable. |
| `omnimemeval` | OmniMemEval question set | — | `not_run`: the dataset is not bundled and this build has no reader/judge adapter. `SHOGUN_OMNIMEMEVAL_DIR` is the hook the adapter will read. |
| `mem0-harness` | Mem0 public harness conversations | — | `not_run`, same reason; `SHOGUN_MEM0_HARNESS_DIR`. |

The external entries are deliberately present as rows so a consumer of the file sees that they
were not run, rather than not seeing them.

## E12-T1 — what changed and the numbers

Two known weaknesses of the bench were at the search layer, not the bench
(`docs/implementation-plan.md` §E12). Both fixes are in `crates/shogun-memory/src/search/` and
apply to `search_federated`, the daemon's path.

**Temporal recall@1 (0.000 → 1.000).** The corpus restates a value (PostgreSQL →
SQLite → PostgreSQL). The third statement is byte-identical to the first, so `insert_or_touch`
collapses it onto the first row and advances `last_seen_at` while `ts` stays at the first sighting;
the superseded SQLite row is therefore the *newer* row by `ts`, and BM25 length normalisation
prefers its shorter sentence as well. The old value ranked first in every project. Now:

- the question's recency cue ("latest", "newest", "currently", "now", 最新, 現在, 直近, 最近 …) is
  read by the search itself (`query_asks_for_latest`); the cue words are not searched for as text,
  like relative dates;
- with the cue — or `FederatedSearchOptions::prefer_latest` — the rows that match the question
  equally well (same concept coverage) are ordered by when each was **last seen**
  (`max(ts, last_seen_at)` for events, period end for derived memory), with the newest scoring up to
  ×2 the oldest (`LATEST_BOOST`); a more recent row that matches fewer of the question's words is
  not promoted over an older full match;
- the bench passes the daemon's time bounds (`since_ts = 0`, `to_ts = now`) so `since_ts` / `to_ts`
  filtering is on the measured path (a test pins both bounds, `to_ts` exclusive), and the temporal
  question gained the word "currently". Earlier temporal baselines (`crates/memory-bench/BASELINE.md`)
  used the old wording on the hybrid path and are not comparable.

**Abstention false evidence (100% → 0.000).** Retrieval ORs the question's words
and had no notion of "nothing here is relevant", so a question about the kestrel compliance audit
returned every row that mentioned kestrel. `search_federated` now measures a **relevance floor**
(`RELEVANCE_FLOOR_COVERAGE = 0.5`) on every search and reports the verdict in
`FederatedSearchResult::relevance` (`passed` / `below_floor` / `no_candidates`, with the measured
coverage). What a failing verdict does is the caller's choice, **opt-in per request**
(`FederatedSearchOptions::abstain_below_floor`, default off):

- **opted in — the bench** (`crates/memory-bench`, `--path federated`): `hits` and
  `raw_event_ranks` come back empty, so a question nothing in memory answers hands a reader no
  rows. This is the right contract for a consumer that reads every returned row as evidence, and
  it is what `abstention_false_evidence` measures.
- **not opted in — the daemon's paths** (`Db::search_request`, which `memory.search` on MCP /
  REST / CLI, the AI keyword phase and the search box call; and `assemble_context`, the Notch's
  suggestion and context assembly): the best candidates are still returned, and the verdict
  travels with them (`SearchResponseV1::relevance`). A failing floor is expressed through the
  answerability signal instead of by emptying the result: a top hit that would otherwise read as
  answerable is reported `answerable: false, reason: low_quality`, and the weaker verdicts
  (`insufficient_signal`, `missing_anchor`, `no_eligible_match`) are left as they were. A generic
  question ("what happened in the meeting") therefore keeps its suggestions with a conservative
  verdict, as the daemon's tests pin, and an agent reading `memory.search` sees both the rows
  and that they do not cover the question.

The rule, per candidate among the `limit` best by score:

- coverage = idf-weighted share of the question's **content** words the row contains. Source cues
  ("meeting", "standup", 会議 …) are never required but count when matched; a word no row in scope
  contains weighs like an average present word (its idf would otherwise be extreme, and it says
  nothing about the archive — "decide" in "what did we decide"); a question in two scripts
  (Orchid のローンチ延期) is covered when either script's words are;
- pass when coverage > 0.5, or coverage = 0.5 with a single word missing (one of two words found
  is a plausible answer; two of four is a question more specific than the row);
- a row a vector lane found passes outright (it shares meaning, not words); a question with no
  content words (dates and cues only) is not judged.

Calibration: on the bench's abstention questions the best chatter row covers one of four words
(0.25) or — "hardware budget" against "budget spreadsheet for kestrel" — two of four (0.50, two
words missing); the answer-bearing workloads cover two of three or more (≥ 0.67). On the golden
search eval (`docs/shogunai-origin/search-eval.md`, 132 cases, `cargo test -p shogun-core
--features db --test search_golden_eval`) the result is unchanged to the third decimal — with the
floor folded into answerability as described above, re-measured 2026-10-25: R@5 0.985, R@10
0.989, MRR 0.948, zero-result 0.000, answerable 0.873, false-answerable 0.000. An earlier draft
of the floor (plain coverage ≥ 0.5, cues excluded) blanked two positives there ("Orchid の
ローンチ延期", "standup notes yesterday"); the cue and script rules above are what fixed them. A
first version of the floor emptied the result on every path; that broke the daemon's suggestion
contract (generic and over-long questions must still return candidates) and was replaced by the
opt-in the same day.

One side effect of the recency cue was also corrected: the cue words ("now", "latest",
"currently") leave the search's lexical terms, but the chat's follow-up detection counts what a
question names and must not read "When is the Orchid launch now?" as a two-word follow-up that
borrows the previous turn's topic. It counts the cue words back in
(`shogun_memory::search::is_recency_cue_word`); ranking and the floor are unaffected.

### Before → after

Measured with `crates/memory-bench`, release build, `--path federated`, Linux container with
other builds running (advisory latency, not a certifying machine). "Before" is commit `e21f3ea`
with the unchanged search layer, run on 2026-10-25 before this change was applied; its temporal
row used the old question wording, which on that search layer scores the same as the new one would
(no cue is read anywhere in it). "After" is this change.

| suite | before — seed 42 | after — 20k × 10 seeds, mean [95% CI] | after — 100k, seed 42 |
|---|---:|---:|---:|
| temporal recall@1 (12 questions) | 0.000 (20k; stale value first in 12/12, MRR 0.500) | 1.000 [1.000, 1.000] | 1.000 |
| abstention false-evidence (500 questions) | 1.000 (20k, 100 questions: 100/100) | 0.000 [0.000, 0.000] | 0.000 |
| multi-session coverage@5 (48 questions) | 1.000 (20k) | 1.000 [1.000, 1.000] | 1.000 |
| query p95, clean, in-memory (500 questions) | — (not measured before) | 6.68 ms [6.10, 7.26] | 22.12 ms |
| query p95, clean, on-disk (500 questions) | 12.24 ms (100k; p50 7.53, R@1 0.932, MRR 0.965) | — | 21.87 ms (p50 13.56, R@1 0.932, MRR 0.965) |

Re-measured on 2026-10-25 after the floor became opt-in (release, 20k × seeds 42–44, in memory,
`--path federated`, the bench opted in): temporal recall@1 1.000 [1.000, 1.000], abstention
false-evidence 0.000 [0.000, 0.000], multi-session coverage@5 1.000 [1.000, 1.000], query p95
6.13 ms [3.04, 9.21]. The committed `benchmarks.json` keeps the ten-seed run; the quality values
are identical, and the measured path is the same code (the opt-in only names what the bench was
already getting).

The SLO budget is 500 ms; the on-disk 100k p95 is 4.4% of it. Tokens per answer and the
raw per-seed values are in `benchmarks.json`. The abstention "before" was taken with 100 questions
(the same 72 distinct questions cycle); every one returned rows.

The 100k query p95 did move: 12.24 → 21.87 ms on disk (p50 7.53 → 13.56), with recall@1 and MRR
unchanged. Two things are new on the measured path — the time-bound filter (`since_ts` / `to_ts`,
previously not passed by the bench) and the relevance-floor pass over the `limit` best candidates
(idf per question word, one coverage sum per row) — and both runs are one seed on a shared
container, so the size of the move is indicative rather than exact. It is the cost of the two
fixes, not noise to ignore: at 4.4% of the budget it is acceptable, and `query_p95` in
`benchmarks.json` (6.68 ms at 20k, ten seeds) is the regression signal for anything that moves it
further.

## What is not in this file yet

- **OmniMemEval and the Mem0 harness are not run.** The rows exist with `status: "not_run"` and
  the environment variables that will point at the datasets. Running them needs a reader model to
  answer from retrieved memory and a judge to score it; when those adapters land, `reader` and
  `judge` stop being `null` and the file names the models.
- **No embedding in the committed file.** The default build has no embedder, so every number in
  `benchmarks.json` is lexical (`embedding: null`). A hybrid number is not comparable with it.

## The vector lane (E24)

`--embedder <NAME>` turns the vector lane on for a run or a whole harness: `test` is the
deterministic concept embedder (`shogun_memory::embed::TestEmbedder`, no model, CI-safe); with
`cargo run -p memory-bench --features embed-local`, a fetched model by name
(`nomic-embed-text-v1.5`, read from `SHOGUN_MODELS_DIR`; `shogun models fetch` writes it, and the
ONNX Runtime 1.22 library comes from `ORT_DYLIB_PATH`). The corpus is embedded after ingest,
outside every timed section, and every query is embedded inside the timed search.

**One model for every arm.** MemDelta (arXiv 2606.29914) measured the embedding swap alone moving
LongMemEval accuracy by 6.2 points, so arms embedded with different models are not comparable. The
harness hands one embedder instance to every suite and seed, each per-run report names its model
(`embedding`), the harness refuses to assemble a file whose arms disagree
(`HarnessError::EmbeddingNotPinned`), and `benchmarks.json` records it:

```json
"embedding": { "model": "nomic-embed-text-v1.5@384", "dim": 384 }
```

`model` is the vector-space id: the model's name and the stored width (nomic's 768-wide output is
stored as its 384-wide Matryoshka prefix). The store records the same id on every vector row and
never ranks rows of two models against each other (V102, `shogun_memory::embedding_space`).

One seed at 600 events (release build, in memory, this container), for orientation only:

| arm | query p95 ms | temporal recall@1 | abstention false evidence | multi-session coverage@5 |
|---|---:|---:|---:|---:|
| lexical (`embedding: null`) | 2.1 | 1.000 | 0.000 | 1.000 |
| `test` | 2.6 | 1.000 | 0.168 | 1.000 |
| `nomic-embed-text-v1.5@384` | 21.9 | 1.000 | **1.000** | 1.000 |

The abstention row is the open problem: the semantic relevance floor
(`MAX_SEMANTIC_L2_DISTANCE = 1.0` in `search/federated.rs`, i.e. cosine ≥ 0.5) was set for e5,
and nomic places unrelated text of the same domain inside it, so every unanswerable question
returns a vector neighbour as evidence. The floor has to become per model (a property of the
`ModelSpec`) before a hybrid file is published.
- **Write cost is model spend only.** `write_cost_micro_usd` is `0.0` because ingestion makes no
  model call; it does not price CPU or storage.
- **Not a certification.** `environment.in_memory` and `environment.profile` say what the run was;
  the search SLO is certified on the owner's machine on a release build, not here.
