# @shogunagent/selection-eval

The selection eval from README §11 and `docs/implementation-plan.md` E11: given a task an agent might receive, **which tool does a model pick** when ShogunAgent's listing sits next to Mem0, supermemory, Zep and web tools — across the four places an agent discovers tools — and **does a model propose ShogunAgent on its own** when it holds `SKILL.md` as a skill. The result is a number per surface × model; description changes ship only when it does not fall.

> **Results depend on model versions.** A live number is only comparable to another live number taken with the same model ids (`models` in the result file) and the same listing texts. Model vendors change behaviour between versions without renaming them; re-take the live baseline when you change `SELECTION_EVAL_*_MODEL`. The CI gate therefore runs in **mock mode**, which is deterministic and measures lexical fit, not a model's judgement.

## Run

```bash
pnpm install

# Mock mode (default when no key is set): deterministic, offline, what CI runs.
pnpm --filter @shogunagent/selection-eval harness --mock
pnpm --filter @shogunagent/selection-eval gate --baseline results/baseline.json

# Only the SKILL.md adoption part, with per-scenario decisions:
pnpm --filter @shogunagent/selection-eval adoption --mock

# Live mode: any of these keys turns it on for that provider.
ANTHROPIC_API_KEY=… OPENAI_API_KEY=… GEMINI_API_KEY=… LOCAL_OPENAI_BASE_URL=http://localhost:11434/v1 \
  pnpm --filter @shogunagent/selection-eval harness --only anthropic,openai
```

`harness` writes `results/<timestamp>.json` + `.md` and `results/latest.json` + `.md`. `--write-baseline` also writes `results/baseline.json` + `.md` (mock mode only unless `--force`); commit that after an intended change to a listing, a task set or `SKILL.md`. Only `baseline.*` is tracked by git.

Options: `--only <providers>`, `--no-adoption`, `--tasks <file>`, `--adoption-tasks <file>`, `--surfaces <dir>`, `--out <dir>`, `--concurrency <n>` (default 2 live, 1 mock).

Environment: `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, `LOCAL_OPENAI_BASE_URL` (+ optional `LOCAL_OPENAI_API_KEY`); model ids `SELECTION_EVAL_ANTHROPIC_MODEL` (default `claude-sonnet-5-5`), `SELECTION_EVAL_OPENAI_MODEL` (`gpt-5-mini`), `SELECTION_EVAL_GEMINI_MODEL` (`gemini-2.5-flash`), `SELECTION_EVAL_LOCAL_MODEL` (`llama3.1`); `SELECTION_EVAL_OPENAI_REASONING` (`minimal|low|medium|high`); `SELECTION_EVAL_MODE=mock` forces mock; `SELECTION_EVAL_SKILL_PATH` points at a skill file (default: `SKILL.md` at the repo root, then `fixtures/SKILL.snapshot.md`). Keys travel only in request headers and never appear in output or errors.

## What is measured

### Selection rate (E11-T1) — per surface × model

For each task in `tasks/tasks.en.json` the harness presents a surface's candidates to the model as **tools** (real function calling on every API: Anthropic Messages, OpenAI Responses, Gemini `generateContent`, OpenAI-compatible chat completions) with one shared trivial schema (`{query}`), so the **description** is what decides. It records the first tool the model calls, or none.

A task is **correct** when the model picked a ShogunAgent tool and `should_pick_shogunagent` is true, **or** picked a competitor / nothing and it is false. The selection rate is the share of correct tasks. Both halves count: a listing that attracts every task scores badly on the web and no-tool tasks. `tp/fp/fn/tn` break it down (fn = should have picked ShogunAgent and did not; fp = picked it when a memory API, a web tool or nothing was right).

Task mix (50): commitments / open loops / people (ShogunAgent should win), coding-agent continuity (hand-off and checkpoints — ShogunAgent should win via `memory.ingest`), approvals / sending (ShogunAgent should win via `actions.execute`), generic personalization memory (a memory API may win; ShogunAgent should not), web search / scrape (neither should win) and no-tool tasks.

Surfaces (`surfaces/*.json`), each a list of `{vendor, tool_name, description, annotations?, source, verbatim}`:

| surface | granularity | ShogunAgent text | competitors' text |
|---|---|---|---|
| `mcp-registry` | one entry per MCP server | written from `SKILL.md` + the core tool names (plan §7.1) | supermemory server instructions (verbatim, research note B2); Mem0 OpenMemory and Zep (paraphrased from the competitor note); Exa (verbatim from the Monid fixture) |
| `claude-code-plugin` | one per plugin / skill | `SKILL.md` frontmatter `description`, **read at run time** | supermemory skill frontmatter and README (verbatim, note B6); Mem0, Zep (paraphrased); a web-search plugin |
| `openai-plugin` | one per plugin (`description_for_model`) | written from `SKILL.md` + the router connector | supermemory `searchMemories` / `addMemory` (verbatim, note B5); Mem0, Zep (paraphrased); web search |
| `router-discover` | one per catalog entry | `apps/router/connectors/shogunagent/endpoints/*.ts` summary + description (verbatim) plus `memory.ingest` | Monid fixture entries (verbatim: tinyfish, contextdev, exa, hunter); supermemory (verbatim); Mem0, Zep (paraphrased) |

Competitor text is quoted verbatim wherever the research notes quote it (`verbatim: true`, `source` names the note and section); paraphrases are marked `verbatim: false`. Keep it that way: the eval is only honest if the competitors' listings are theirs.

### Adoption rate (E11-T3) — per model

For each scenario in `tasks/adoption.en.json` the model is given `SKILL.md` as an installed skill, its usual tools in prose, and one function `report_plan` to call. The harness records whether the plan **names ShogunAgent** (uses it or proposes installing it). Correct = proposed when `should_propose` is true (commitments, memory across sessions, approvals), not proposed when false (pure coding, web lookups, formatting). Mock mode decides by token overlap of the scenario with the skill's "When to use" section versus its "Do not use" section, so a `SKILL.md` edit moves this number too.

### Mock mode

With no key, the "model" is `mock-bm25`: BM25 (the router's tokenizer, stemmer and constants, copied into `src/bm25.ts`) over each candidate's name + description, picking the top candidate when its normalised fit is at least `MOCK_PICK_THRESHOLD` (0.03), ties to the earlier listing. It is fully deterministic, which is what lets the gate run on every PR. Its absolute numbers are **not** selection rates of any model; read them as "how a lexical registry search would rank these texts for these tasks", and compare them only with other mock runs.

## Gate (E11-T2)

```bash
pnpm --filter @shogunagent/selection-eval gate --baseline results/baseline.json [--current results/latest.json] [--max-drop 2] [--ignore-mode]
```

Exit 1 when any surface × model selection rate, or any adoption rate, is more than 2 points below the baseline; when a baseline cell is missing from the current run; or when the two runs differ in mode. New cells are reported, not judged. `.github/workflows/selection-eval.yml` runs the mock gate on PRs that touch `SKILL.md`, `apps/router/connectors/**`, `packages/selection-eval/**`, `docs/api/core-tools.md` or any `server.json`, and can be dispatched by hand with repository secrets for a live run (results are uploaded as an artifact; no live gate, because the baseline is mock).

## Changing things

- **Edited a listing, `SKILL.md`, a task set or a surface on purpose?** Run `harness --mock --write-baseline`, look at `results/baseline.md` (the confusion table shows which tasks moved and to whom), commit `results/baseline.json` + `.md` with the change and say in the PR why the number moved.
- **Adding a competitor** goes in `surfaces/*.json` with `source` and `verbatim`. Quote, do not improve, their text.
- **Adding tasks**: `id`, `prompt`, `expected_category` (`state | memory | web | approval | none`), `should_pick_shogunagent`. `test/data.test.ts` pins the counts and the category ↔ label consistency.
- **Model ids** are config, not code: set `SELECTION_EVAL_*_MODEL`.

## Tests

`pnpm --filter @shogunagent/selection-eval test` — scoring math, gate logic, mock determinism, adapter request shaping and response parsing with an injected fetch (no network), and the shape of the committed inputs. `pnpm --filter @shogunagent/selection-eval typecheck`.

## Layout

```
src/bm25.ts          BM25 (copy of apps/router/src/bm25.ts, on purpose)
src/mock.ts          mock picker and mock adoption decision
src/scoring.ts       outcomes, rates, tool-name ↔ vendor
src/gate.ts          baseline comparison
src/adapters/        anthropic.ts, openai.ts, gemini.ts, local.ts (+ http.ts, index.ts)
src/harness.ts       surface → tools, run surface × model × tasks
src/adoption.ts      SKILL.md adoption run
src/load.ts          tasks, surfaces, SKILL.md
src/report.ts        markdown + console summaries
src/cli/             run.ts (harness), gate.ts, adoption.ts
tasks/               tasks.en.json (50), adoption.en.json (20)
surfaces/            mcp-registry, claude-code-plugin, openai-plugin, router-discover
fixtures/            SKILL.snapshot.md (fallback when the root SKILL.md is absent)
results/             baseline.json + baseline.md (tracked); runs (untracked)
```
