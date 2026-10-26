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
| `open_loop_tracking` | `crates/loop-bench` hard tier (synthetic email / chat / meeting / agent streams with planted commitments, open loops, resolutions, date changes and distractors) | `tracking_f1` | micro F1 of commitments and open loops at the end of the stream, operating point `all`. Not produced by the memory-bench harness: `loop-bench run --adapter shogunagent --seeds 10 --merge-into docs/benchmarks/benchmarks.json` adds or replaces this row. The other metrics (resolution, supersession, distractors, provenance) and the definition are in [`open-loop-tracking.md`](open-loop-tracking.md). Not in the committed file yet. |

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
  it is what `abstention_false_evidence` measures. Since E51 (2026-10-26) the opt-in also applies
  the floor **per row** when the verdict passes: only rows that clear the floor on their own are
  returned (a cluster of one session's rows stays when any member clears it). Before, the verdict
  passed when any of the `limit` best did, and then every candidate came back, so a row holding
  only the project name rode along with an unrelated row that passed (see "What moved (E51)"
  below).
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
  nothing about the archive — "decide" in "what did we decide");
- CJK content counts in the gram index's units (`terms::floor_units`): a two-character word is one
  unit, a longer compound or katakana word its bigrams (請求書 = 請求 + 求書, リリース = リリ + リー +
  ース), each weighing what its word weighs, and a unit matches when the row holds that gram
  (招聘会议 against 「招聘 … 的会议」 matches 招聘 and 会议). Before this, a particle-less CJK clause
  was one word, and the project name typed beside it made half the question;
- a question in two scripts is covered by its CJK part alone (the archive holds each item in one
  language), never by its Latin part alone: beside CJK content the Latin words are usually a
  project or product name, so a row must match at least one CJK unit. 「Orchid のローンチ延期」
  against an English row is a cross-lingual question, the vector lane's job, as in every other
  language;
- pass when coverage > 0.5, or coverage = 0.5 with a single unit missing (one of two words found
  is a plausible answer; two of four is a question more specific than the row). The tie is read on
  the whole question only: the CJK part of a mixed question, judged on its own, must cover more
  than half (E51). Without the name the question is about, half of its CJK part (休暇 of
  「Robin の会議は休暇で中止?」) is the same topic for some other project;
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
ローンチ延期", "standup notes yesterday"); the cue rule above fixed the second, and a rule that let
the Latin words of a mixed question stand alone fixed the first. That rule was also what made every
Japanese and Chinese unanswerable question in the multilingual suite answer (below), and it was
replaced by the CJK-unit and mixed-script rules above on 2026-10-25; the golden eval is unchanged
to the third decimal by the replacement (R@5 0.985, MRR 0.948, answerable 0.873, false-answerable
0.000 lexical; R@5 0.981, MRR 0.948, false-answerable 0.000 with the test embedder), because
"Orchid のローンチ延期" was already `missing_anchor` in the daemon's answerability. A
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

## Multilingual retrieval (E43)

`memory-bench multilingual` (`crates/memory-bench/src/multilingual.rs`) is one fixed corpus of 18
facts, three per language in English, Japanese, Spanish, German, French and Chinese, plus 30 rows
of everyday chatter, five per language. Every fact states two topics (cost + contract,
launch + bug …), no topic pair repeats, and each names an invented project. 70 questions ask about
it on the federated path (the daemon's), k = 10, abstaining below the relevance floor:

| kind | n | what it asks |
|---|---:|---|
| `lexical` | 18 | the fact's own words, in the fact's language |
| `paraphrase` | 7 | same language, synonyms instead of the fact's words (the project name may stay) |
| `cross_lingual` | 18 | asked in one language about a fact written in another; three per language |
| `cjk_short` | 12 | two- and three-character Japanese and Chinese words (定例, 請求, 发票, 服务器) |
| `cjk_sentence` | 3 | a CJK question with no particles or spaces (发票费用是多少, 請求書支払期限) |
| `abstention` | 12 | two per language: a topic pair nothing states, about a project the corpus names |

It reports **R@5** and **MRR** over the answerable questions and **false-answerable** (the share
of unanswerable questions that still returned a row) per query language and per kind. The corpus
has no seed, so a run is exact rather than a sample. CI runs it lexical and with the test
embedder (`crates/memory-bench/tests/multilingual_eval.rs`, which asserts the numbers below); a
real model runs the same cases with `--embedder <model>`.

```bash
cargo run -p memory-bench --release -- multilingual                     # lexical
cargo run -p memory-bench --release -- multilingual --embedder test     # deterministic concept embedder
cargo run -p memory-bench --release --features embed-local -- multilingual \
  --embedder multilingual-e5-small --out /tmp/e5-multilingual.json      # the real model (see below)
```

### Numbers (2026-10-25, this container)

Three measurements: **E43** as shipped, **after** the CJK floor fix (CJK content counted in gram
units, a mixed-script question not carried by its Latin words alone; see the rule above), and
**E51** (2026-10-26: an abstaining search returns only rows that clear the floor on their own, and
the CJK part of a mixed question needs more than half). The tables are E51; the two "what moved"
tables below give the earlier values.

By query language, E51:

| language | answerable | R@5 lexical | MRR lexical | R@5 `test` | MRR `test` | false-answerable (both) |
|---|---:|---:|---:|---:|---:|---:|
| en | 8 | 0.375 | 0.375 | 1.000 | 1.000 | 0.000 |
| ja | 15 | 0.733 | 0.733 | 1.000 | 1.000 | 0.000 |
| es | 7 | 0.429 | 0.429 | 1.000 | 1.000 | 0.000 |
| de | 7 | 0.571 | 0.571 | 0.857 | 0.857 | 0.000 |
| fr | 7 | 0.429 | 0.429 | 1.000 | 1.000 | 0.000 |
| zh | 14 | 0.571 | 0.571 | 0.929 | 0.929 | 0.000 |
| overall | 58 | 0.552 | 0.552 | 0.966 | 0.966 | 0.000 |

By kind, E51:

| kind | R@5 lexical | MRR lexical | R@5 `test` | MRR `test` |
|---|---:|---:|---:|---:|
| lexical | 1.000 | 1.000 | 1.000 | 1.000 |
| paraphrase | 0.286 | 0.286 | 1.000 | 1.000 |
| cross_lingual | 0.000 | 0.000 | 0.944 | 0.944 |
| cjk_short | 1.000 | 1.000 | 1.000 | 1.000 |
| cjk_sentence | 0.000 | 0.000 | 0.667 | 0.667 |
| abstention (false-answerable) | 0.000 | | 0.000 | |

#### What moved (E51), before → after

Every cell not listed is identical, including every false-answerable (0.000 in every language and
both columns) and the abstention kind. The lexical cells fall and the `test` cells rise; the
trade-off was taken on purpose (product rule: low-confidence state is never rendered as fact).

| cell | before | E51 | why |
|---|---:|---:|---|
| ja R@5 / MRR lexical | 0.867 / 0.789 | 0.733 / 0.733 | "Robin の会議は休暇で中止?" and "Tern の出張の締切はいつ?" are no longer "found": their answer row holds only the project name, and it was returned because another row (Lark's 休暇, Falcon's 出張) passed the floor on half of the question's CJK part. Neither row is evidence for the question; the result is now empty |
| cross_lingual R@5 / MRR lexical | 0.111 / 0.046 | 0.000 / 0.000 | the same two questions. Lexically, cross-lingual questions share only the name with their fact, which the floor refuses in every language; they are the vector lane's job |
| overall R@5 / MRR lexical | 0.586 / 0.566 | 0.552 / 0.552 | the two above |
| ja MRR `test` | 0.967 | 1.000 | answers the vector lane found are no longer ranked behind lexical rows that did not clear the floor themselves |
| es MRR `test` | 0.821 | 1.000 | same |
| de MRR `test` | 0.667 | 0.857 | same |
| fr R@5 / MRR `test` | 0.857 / 0.594 | 1.000 / 1.000 | same; "Y a-t-il une erreur dans la facture de Swift ?" was 8th, now 1st |
| overall R@5 / MRR `test` | 0.948 / 0.863 | 0.966 / 0.966 | the four above |
| paraphrase MRR `test` | 0.929 | 1.000 | same |
| cross_lingual R@5 / MRR `test` | 0.889 / 0.643 | 0.944 / 0.944 | same (Swift); the German Petrel question is still past the test embedder's cutoff |

Unchanged by E51 (re-measured 2026-10-26): the golden search eval (R@5 0.985, MRR 0.948,
false-answerable 0.000 keyword-only; R@5 0.981, MRR 0.948, false-answerable 0.000 with the test
embedder — the daemon's path does not opt in, and the verdict is the same rule over the same
`limit` best, except the CJK-part tie, which no golden case reaches), and the harness quality
suites at 20k events, seeds 42–44, release, in memory: temporal recall@1 1.000, abstention
false-evidence 0.000, multi-session coverage@5 1.000 (query p95 8.39 ms [7.14, 9.64] on a
container running other builds; 6.13 ms on the previous three-seed run, latency is advisory here).

#### What moved (CJK floor fix), E43 → after

| cell | E43 | after | why |
|---|---:|---:|---|
| ja false-answerable (lexical and `test`) | 1.000 | **0.000** | Heron の会議の費用 and Wren のリリースの期限 abstain |
| zh false-answerable (lexical and `test`) | 1.000 | **0.000** | Merlin 团队的休假预算 and Plover 的合同有错误吗 abstain |
| abstention false-answerable (lexical and `test`) | 0.333 | **0.000** | |
| ja R@5 / MRR lexical | 0.933 / 0.856 | 0.867 / 0.789 | "Kestrel の契約更新の費用は?" (English fact) was found through the name alone |
| zh R@5 / MRR lexical | 0.857 / 0.857 | 0.571 / 0.571 | the three zh cross-lingual questions and "Kite 团队的候选人什么时候开会" (name + 团队, 2 of 12 units) |
| overall R@5 / MRR lexical | 0.672 / 0.652 | 0.586 / 0.566 | the five above |
| cross_lingual R@5 / MRR lexical | 0.333 / 0.269 | 0.111 / 0.046 | four name-only finds; the two left (Robin, Tern) are returned because another row passes the floor on the question's CJK part alone (休暇 in the Lark row, 出張 in the Falcon row: one of two CJK words), and the name-matched answer comes back with it (closed by E51, above) |
| paraphrase R@5 / MRR lexical | 0.429 / 0.429 | 0.286 / 0.286 | the Kite question above |

The lexical recall given up is not separable from the false answers: "Kestrel の契約更新の費用は?"
(answerable) and "Heron の会議の費用はいくら?" (not) share exactly the same evidence with their
rows — the project name and nothing else — and the English, Spanish, German and French
cross-lingual questions were already refused for it. With the vector lane on, every one of them is
found again: the `test` columns are unchanged except that false-answerable is now 0.000 in every
language.

`embedding` for the `test` columns is `shogun-test/concept-v2@384`: its concept axes carry the
words of all six languages, so these columns prove the cross-lingual path end to end (store,
embed job, active space, cutoff, fusion, relevance floor); they are not a model's quality. The
real-model column is the owner's run below.

What the numbers say:

- **Same-language lexical retrieval works in every language** (lexical kind 1.000), and so do
  two- and three-character CJK words.
- **Cross-lingual questions are a vector-lane job.** Lexically only the project name is shared,
  and the relevance floor rightly refuses a row that covers one word of four. Until the
  2026-10-25 floor fix the Japanese and Chinese ones were found anyway (0.333), because their
  particles and Han runs left the name as half of the question's concepts; the 0.111 that was left
  was two Japanese questions returned beside another row that passed on their CJK part, and E51
  took it to 0.000. With the vector lane on, 17 of 18 are found; the miss is a German question
  whose many hashed function words push it just past the test embedder's cutoff (the French one
  that was ranked 8th behind non-clearing lexical rows is 1st since E51).
- **The vector lane adds no false evidence** (`the_vector_lane_adds_answers_and_no_false_evidence`
  pins this): false-answerable is identical with it on and off.
- **Japanese and Chinese abstention (fixed 2026-10-25; was false-answerable 1.000 in both, with or
  without an embedder).** The relevance floor passed a row that covers half of a question's
  concepts while missing only one, and let the Latin words of a mixed-script question carry it on
  their own. A CJK question loses its particles (の, は) and Chinese keeps a whole Han run as one
  concept, so "Heron の会議の費用はいくら?" was two content concepts (Heron, 費用; 会議 is a source
  cue) and "Plover 的合同有错误吗" two (Plover and one long Han compound): the project name alone
  reached 50% (and 100% of the Latin part), and the rows the name matches were returned. "Wren の
  リリースの期限は?" returned the Wren row because it shares 期限 (two of three concepts). The floor
  now counts CJK content in gram units (Plover 的合同有错误吗 is Plover + six units; Wren のリリースの
  期限 is Wren + リリ + リー + ース + 期限, of which the row holds two of five) and requires a mixed
  question's row to match some of its CJK content (`search/federated.rs::relevance_floor`,
  `search/terms.rs::floor_units`). The golden eval is unchanged (see "Relevance floor" above).

### The FTS5 trigram path on CJK (measured)

Lexical search has two indexes (`crates/shogun-memory/src/search/terms.rs`): the V1 trigram index
matches any substring of three or more characters, and the V81 gram index matches two-character
units as whole tokens. What the suite and the golden eval show for CJK:

| query shape | example | result | why |
|---|---|---|---|
| Japanese 2-character word | 定例, 請求, 期限, 採用, 休暇, 出張 | found, rank 1 (6/6) | V81 gram index (kanji bigrams, okurigana folded) |
| Japanese 3-character word | 請求書 | found, rank 1 | trigram index |
| Chinese 2-character word | 发票, 招聘, 故障, 出差 | found, rank 1 (4/4) | V81 gram index (Han bigrams) |
| Chinese 3-character word | 服务器 | found, rank 1 | trigram index |
| Japanese phrase with particles | Wren の請求書の支払い期限 | found, rank 1 | particles split it into concepts |
| Japanese compound with no particles | 請求書支払期限 | **not found** | one compound; matches only when every bigram is in a row, and 書支 is not (の stood between them) |
| Chinese sentence | 发票费用是多少, 服务器出现故障了吗 | **not found** | Chinese has no particles: the whole Han run is one compound term, and no row contains all of its bigrams (是多, 多少) |
| mixed script, Han run contiguous in the row | Merlin 发票费用 | found, rank 1 | split at the script boundary; the row contains 发票费用 |
| mixed script, Han run not contiguous in the row | Kite 招聘会议, Plover 服务器故障 | found, rank 1 | the Han run is one compound the row does not contain (聘会, 器故 are missing) and is retrieved through the name; the floor counts the compound in gram units and the row holds most of them (招聘, 会议; 服务, 务器, 故障). Until 2026-10-25 the floor accepted it on the name alone. Without the name it is the `cjk_sentence` case |

The golden search eval (`crates/shogun-core/tests/search_golden_eval.rs`, Japanese-heavy:
two-character words, surnames, particles, okurigana variants) holds at R@5 0.985, MRR 0.948,
false-answerable 0.000 lexical, unchanged by E43. Chinese segmentation (splitting a Han run into
words) is not implemented; the gap is the `cjk_sentence` row, and a vector lane with a
multilingual model is the shipped answer to it (2 of 3 found with the test embedder).

### Calibrating `multilingual-e5-small` on a machine with the model

The owner's run, once the model is pinned and fetched (`docs/embedding-model-setup.md`):

```bash
export SHOGUN_MODELS_DIR=…            # where `shogun models fetch` wrote multilingual-e5-small/
export ORT_DYLIB_PATH=…/libonnxruntime.so   # ONNX Runtime 1.22.x
cargo build --release -p memory-bench --features embed-local
B=target/release/memory-bench
# 1. The suite at the spec's cutoff (0.63), then a sweep around it.
$B multilingual --embedder multilingual-e5-small --out /tmp/e5-ml.json
for f in 0.55 0.60 0.63 0.66 0.70 0.75 0.80; do
  $B multilingual --embedder multilingual-e5-small --vector-cutoff "$f" --out "/tmp/e5-ml-$f.json"
  $B harness --embedder multilingual-e5-small --vector-cutoff "$f" \
    --seeds 3 --events 20000 --out "/tmp/e5-harness-$f.json"
done
```

Keep the largest cutoff at which, together:

1. `abstention_false_evidence` (harness) is 0.000 on every seed;
2. no language's false-answerable in the multilingual suite is higher than the lexical baseline
   above (0.000 in every language since the 2026-10-25 floor fix);
3. `temporal_recall_at_1` and `multi_session_coverage_at_5` hold.

Set `MULTILINGUAL_E5_MAX_L2_DISTANCE` in `crates/shogun-memory/src/embed.rs` to it, record the
real-model columns (per-language R@5, MRR, false-answerable, and the cross-lingual row) in the
tables above with the model's space id, and only then consider making it the default for
non-English principals. Every arm of a comparison uses this one model: never put an e5 number
next to a nomic or `test` number as if they measured the same system.

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

One seed at 600 events (release build, in memory, this container), for orientation only, before
the per-model cutoff below:

| arm | query p95 ms | temporal recall@1 | abstention false evidence | multi-session coverage@5 |
|---|---:|---:|---:|---:|
| lexical (`embedding: null`) | 2.1 | 1.000 | 0.000 | 1.000 |
| `test` | 2.6 | 1.000 | 0.168 | 1.000 |
| `nomic-embed-text-v1.5@384` | 21.9 | 1.000 | **1.000** | 1.000 |

### The per-model relevance cutoff

The abstention rows above came from one semantic relevance floor for every model
(`MAX_SEMANTIC_L2_DISTANCE = 1.0`, cosine ≥ 0.5, tuned for e5): nomic places unrelated text of the
same domain inside it, so every unanswerable question returned a vector neighbour as evidence.
The cutoff is now a property of the model: `ModelSpec::max_l2_distance`, looked up for the store's
active space by `shogun_memory::embed::max_l2_distance_for` on every search
(`FederatedSearchOptions::max_vector_distance` overrides it). A neighbour beyond it is not
evidence, and below it the lane's quality falls linearly to 0 at the cutoff.

| space | cutoff (L2) | cosine | how it was set |
|---|---:|---:|---|
| `multilingual-e5-small@384` | **0.63** | ≥ 0.80 | E43: conservative, below e5's compressed score band (was 1.00 before E43); not measured here, see "Multilingual retrieval (E43)" |
| `nomic-embed-text-v1.5@384` | **0.70** | ≥ 0.755 | conservative, favouring abstention; not measured here (the model cannot be fetched in CI) |
| `shogun-test/concept-v2@384` (`--embedder test`) | **0.58** | ≥ 0.83 | calibrated below; CI's regression guard |
| anything else (`unspecified`, the hashing mock) | 1.00 | ≥ 0.50 | default |

Calibration of the test embedder (`--path federated --embedder test --vector-cutoff F`, release,
in memory, 500 abstention questions, 12 temporal, 48 multi-session). It was measured on `concept-v1`;
E43's `concept-v2` only adds the Spanish, German, French and Chinese words of each concept axis,
none of which occurs in these English corpora, so the vectors and the numbers below are unchanged
(`bench_smoke::the_vector_lane_abstains_at_its_models_cutoff` still holds):

| cutoff | abstention false evidence | temporal recall@1 | multi-session coverage@5 |
|---:|---|---:|---:|
| 1.00 – 0.66 | 0.168 (seeds 42–44, 600 events; 600 and 5,000 for 0.66–0.69) | 1.000 | 1.000 |
| 0.64 | 0.014 – 0.042 (seed 42, 600 / 5,000 events) | — | — |
| 0.60 – 0.62 | 0.000 – 0.014 (seeds 42–46, 600 – 20,000 events: non-zero on 43 and 44) | 1.000 | 1.000 |
| **0.58** | **0.000** (seeds 42–46 at 2,000 and 20,000 events) | 1.000 | 1.000 |
| 0.55 | 0.000 (the same ten runs) | — | — |

0.58 is the largest value that was 0.000 everywhere it was run. The tests pin it from both sides:
`bench_smoke::the_vector_lane_abstains_at_its_models_cutoff` (0 false evidence at the model's
cutoff, more than 0 at 1.0, temporal and multi-session unchanged) and
`federated_tests::the_vector_cutoff_is_the_active_models`; the paraphrase test still finds its
answer through the vector lane. The golden search eval holds with the lane off (R@5 0.985,
MRR 0.948, false-answerable 0.000, unchanged: the lexical path never reads the cutoff) and with it
on (R@5 0.981, R@10 0.985, MRR 0.948, false-answerable 0.000).

**Recalibrating nomic.** On a machine with the model (`shogun models fetch`, ONNX Runtime 1.22 in
`ORT_DYLIB_PATH`, `SHOGUN_MODELS_DIR` set), build with `--features embed-local` and sweep:

```bash
for f in 0.80 0.75 0.70 0.65; do
  memory-bench harness --embedder nomic-embed-text-v1.5 --vector-cutoff "$f" \
    --seeds 3 --events 20000 --out "/tmp/nomic-$f.json"
done
```

Keep the largest cutoff whose `abstention_false_evidence` is 0.000 on every seed while
`temporal_recall_at_1` and `multi_session_coverage_at_5` hold, and set
`NOMIC_MAX_L2_DISTANCE` in `crates/shogun-memory/src/embed.rs` to it (the report records
`vector_cutoff` whenever the flag is set). Until then nomic's 0.70 errs toward abstaining: a
paraphrase it drops is still reachable by the lexical lanes; a wrong neighbour returned as
evidence is the failure this suite exists to catch. A hybrid `benchmarks.json` is published only
after that run.
- **Write cost is model spend only.** `write_cost_micro_usd` is `0.0` because ingestion makes no
  model call; it does not price CPU or storage.
- **Not a certification.** `environment.in_memory` and `environment.profile` say what the run was;
  the search SLO is certified on the owner's machine on a release build, not here.
