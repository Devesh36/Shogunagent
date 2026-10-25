# AI-Agent Memory & World-Model Systems: Technical State of the Art (as of October 2026)

Research scope: benchmarks, architectures, 2025–2026 papers, privacy/security, implementation stack, and agent-facing interfaces, to inform ShogunAgent (memory / world-model / context service spun out of ShogunAI). All dates are the source's publication or last-checked date; "vendor claim" means self-reported by the system's maker, "same-harness" means a third party ran several systems under one configuration.

---

## Key Question 1: Benchmarks — which are credible, what do leaders claim?

### Takeaway
There is no official leaderboard for any of the major agent-memory benchmarks; the 90–95% headline numbers on LoCoMo/LongMemEval are vendor self-reports that collapse by 15–40 points when a third party runs the same systems under one harness (OmniMemEval), so the only credible comparison today is a same-harness run, and the newer BEAM (100K–10M tokens) and MemoryAgentBench (ICLR 2026) are the benchmarks that still discriminate between systems. For ShogunAgent, "proving it" means publishing a same-harness run with named reader/judge models, token budgets and latency, not a single accuracy number.

### Cited Findings

**Benchmark definitions and known weaknesses**
- LoCoMo: 1,540 questions in four categories (single-hop, multi-hop, open-domain, temporal), scored with F1 and LLM grading; LongMemEval: 500 questions in six categories incl. knowledge updates and temporal reasoning; BEAM: 1M and 10M token scales across ten categories with "high evaluation cost"; DolphinBench: 600 tasks with real tool calls measuring task completion, cost and latency (new, few systems published) — [Mem0, "State of AI Agent Memory 2026", updated Oct 10 2026](https://mem0.ai/blog/state-of-ai-agent-memory-2026)
- Zep's critique of LoCoMo (May 6 2025): conversations average ~16,000–26,000 tokens ("within modern context windows"); full-context GPT-4o-mini baseline (~73%) beat Mem0's best score (~68%); no knowledge-update questions; category 5 (adversarial) unusable due to missing ground truth, so both Mem0 and Zep excluded it; speaker-attribution errors and ambiguous questions — [Zep blog](https://www.getzep.com/blog/lies-damn-lies-statistics-is-mem0-really-sota-in-agent-memory/)
- Independent commentators: LoCoMo QA set is only 10 conversations, adversarial category excluded, dialogues are synthetic LLM-generated — [DEV Community](https://dev.to/gde03/the-ai-memory-benchmark-everyone-quotes-forbids-saying-i-dont-know-o1n); [memoryplugin field guide](https://www.memoryplugin.com/wiki/evaluating.html)
- Mem0 itself concedes: "smaller benchmarks like LoCoMo and LongMemEval can be materially improved by aggressive retrieval strategies, larger context windows, or frontier models" — [Mem0 docs, memory evaluation](https://docs.mem0.ai/core-concepts/memory-evaluation)
- Mem0 lists sources of score variance: dataset revision, ingestion method, answer model, top-k, reranking, prompt format, judge leniency, recall-only vs end-to-end grading, adversarial memories, token/latency accounting; "recall-only scores are systematically higher than end-to-end scores" — [Mem0 State of Memory 2026](https://mem0.ai/blog/state-of-ai-agent-memory-2026)
- BEAM ("Beyond a Million Tokens", Univ. of Alberta / UMass Amherst, ICLR 2026, arXiv Oct 2025): 100 conversations (100K–10M tokens), 2,000 validated questions over 10 memory abilities (contradiction resolution, event ordering, instruction following, etc.); finding: "even LLMs with 1M-token context windows, with or without retrieval augmentation, struggle as dialogues get longer"; proposed LIGHT framework (long-term episodic memory + short-term working memory + scratchpad) gains 3.5%–12.69% over strongest baselines — [arXiv 2510.27246](https://arxiv.org/abs/2510.27246); [ICLR 2026 poster](https://iclr.cc/virtual/2026/poster/10006595); [HF dataset BEAM-10M](https://huggingface.co/datasets/Mohammadta/BEAM-10M)
- MemoryAgentBench (UCSD, Hu/Wang/McAuley; arXiv 2507.05257, v4 revised Jun 28 2026; ICLR 2026): four competencies — accurate retrieval, test-time learning, long-range understanding, selective forgetting; converts long-context datasets into incremental multi-turn format; conclusion: "current methods fall short of mastering all four competencies" — [arXiv 2507.05257](https://arxiv.org/abs/2507.05257); [ICLR 2026 poster](https://iclr.cc/virtual/2026/poster/10010781)
- MemBench (Renmin Univ. / Huawei Noah's Ark, arXiv Jun 2025): separates factual vs reflective memory; participation vs observation scenarios; measures effectiveness, efficiency, capacity — [arXiv 2506.21605](https://arxiv.org/html/2506.21605v1)
- AgentMemBench (arXiv 2608.00009, Aug 2026): five-family taxonomy of memory strategies, measures efficiency as well as quality; positions itself against MemoryAgentBench — [arXiv 2608.00009](https://arxiv.org/pdf/2608.00009)
- LongMemEval-V2 exists as a newer variant; the LongMemEval repo has no official leaderboard — [benchmarklist.com](https://benchmarklist.com/benchmarks/longmemeval_v2/); [aiagentmemory.org leaderboard article](https://aiagentmemory.org/articles/agent-memory-leaderboard/)

**The Mem0 vs Zep controversy (2025)**
- Mem0's paper (arXiv 2504.19413, ECAI 2025, Table 2) reported LLM-judge J=66.88 for Mem0 vs 65.99 for Zep on LoCoMo; Mem0 Graph 68.44 — [rywalker summary](https://rywalker.com/research/mem0); [Zep rebuttal](https://www.getzep.com/blog/lies-damn-lies-statistics-is-mem0-really-sota-in-agent-memory/)
- Zep rebuttal (May 6 2025): corrected Zep = 75.14% ± 0.17; full-context GPT-4o-mini = 72.90%; methodology errors alleged: (1) Mem0 gave the user role to both speakers; (2) appended timestamps to messages instead of using Zep's `created_at`, breaking temporal reasoning; (3) ran searches sequentially, inflating latency. p95 search latency: Mem0 Base 0.200s, Zep corrected 0.632s, Mem0 Graph 0.657s. Note: the page body says ~10% lead, page metadata says 24% — internal inconsistency — [Zep blog](https://www.getzep.com/blog/lies-damn-lies-statistics-is-mem0-really-sota-in-agent-memory/)
- Letta and Zep have both publicly disputed Mem0's benchmark claims — [memoryplugin field guide](https://www.memoryplugin.com/wiki/evaluating.html)

**Vendor-claimed scores (2026)**
- Mem0 (April 2026 algorithm): LoCoMo 92.5, LongMemEval 94.4, BEAM 1M 64.1, BEAM 10M 48.6; ~6,956 tokens/query on LoCoMo vs ~26,000 for full-context in the 2025 paper; temporal +29.6 pts and multi-hop +23.1 pts vs its prior algorithm; judge model not named on the page — [Mem0 State of Memory 2026](https://mem0.ai/blog/state-of-ai-agent-memory-2026); [Mem0 memory-benchmarks repo](https://github.com/mem0ai/memory-benchmarks)
- Zep research page: LoCoMo 94.7% at 155 ms retrieval latency and 5,760-token context; LongMemEval 90.2% (451/500), median 4,408 tokens/question — [Zep research](https://getzep.com/research); aggregator lists the reader/judge as GPT-5.4 — [aiagentmemory.org](https://aiagentmemory.org/articles/agent-memory-leaderboard/). Mem0's competitor page lists Zep at 80.32% @ 189ms (up to 83%) LoCoMo and 71.2 LongMemEval (GPT-4o) — older/different configs — [Mem0 blog](https://mem0.ai/blog/zep-vs-mem0-which-ai-memory-layer-should-you-choose)
- Hindsight (Vectorize; arXiv 2512.12818, Dec 14 2025): with a 20B open backbone, overall accuracy 39% → 83.6% vs full-context same backbone, beating full-context GPT-4o; larger backbone: LongMemEval 91.4%, LoCoMo 89.61% vs 75.78% for strongest prior open system — [arXiv 2512.12818](https://arxiv.org/abs/2512.12818). Aggregator lists Hindsight vendor claims at LongMemEval-S 94.6, LoCoMo 92, BEAM 1M 73.9, BEAM 10M 64.1; README says Virginia Tech and Washington Post collaborators reproduced results (a reproduction, not an independent audit) — [aiagentmemory.org](https://aiagentmemory.org/articles/agent-memory-leaderboard/)
- Mastra Observational Memory (updated Feb 17 2026): LongMemEval-S 94.87% (gpt-5-mini actor), 93.27% (gemini-3-pro-preview), 89.20% (gemini-3-flash-preview), 84.23% (gpt-4o); observer/reflector = gemini-2.5-flash; judge gpt-4o. Their own comparison table: Hindsight 91.40% (gemini-3-pro), Supermemory 85.20% (gemini-3-pro)/84.60% (gpt-5)/81.60% (gpt-4o), Oracle (gpt-4o) 82.40%, Mastra RAG topK 20 80.05%, Zep 71.20%, full-context gpt-4o 60.20%. Author notes multi-session category ceiling ~87.2% shared with Hindsight, and single-session-preference has only 30 questions (one flipped question = 3.3 pts) — [Mastra research](https://mastra.ai/research/observational-memory)
- Supermemory: LongMemEval-S Recall@20 with aggregation, 97% overall with gpt-4o (SSU 97, SSA 100, SSP 95, KU 100, TR 95, MS 96); LLM-judge runs 84.6% (gpt-5) and 85.2% (gemini-3-pro); baselines: Zep 71.2%, full-context 60.2% (gpt-4o) — [Supermemory eval report](https://supermemory.ai/research/longmembench). Separate experimental "ASMR" agentic approach claims ~99% on LongMemEval-S but is "not our main production Supermemory engine (yet)" — [Supermemory blog](https://supermemory.ai/blog/we-broke-the-frontier-in-agent-memory-introducing-99-sota-memory-system/)
- Honcho (Plastic Labs): LongMemEval-S 90.4, LoCoMo 89.9, BEAM 1M 61.8, BEAM 10M 40.9 (Claude Haiku 4.5 reader; gemini-2.5-flash-lite extraction) — [aiagentmemory.org](https://aiagentmemory.org/articles/agent-memory-leaderboard/)
- MemOS (MemTensor): LongMemEval-S 89.2, LoCoMo 88.8, BEAM 10M 56.8 (GPT-4.1-mini answers, GPT-4o-mini judge); later vendor figures LoCoMo 92.34 / LongMemEval 93.40 — [aiagentmemory.org](https://aiagentmemory.org/articles/agent-memory-leaderboard/); [woshipm (Chinese, promotional)](https://www.woshipm.com/ai/6434817.html)
- Letta: LoCoMo 74.0 (gpt-4o-mini, filesystem storage); LongMemEval not published — [Mem0 State of Memory 2026](https://mem0.ai/blog/state-of-ai-agent-memory-2026)
- Other arXiv self-reports: Agent Zero Memory 95.60% LongMemEval — [arXiv 2608.29606](https://arxiv.org/pdf/2608.29606); EverMemOS — [arXiv 2601.02163](https://arxiv.org/pdf/2601.02163); MemPalace's 96.6% is retrieval recall (R@5) not answer accuracy — [vectorize.io](https://vectorize.io/articles/mempalace-benchmarks)

**Same-harness results (OmniMemEval, run by MemTensor/MemOS; GPT-4.1-mini answers, GPT-4o-mini judge; checked Oct 12 2026)**
- LoCoMo / LongMemEval-S / BEAM 100K / BEAM 10M / LoCoMo context tokens: MemOS 88.83 / 89.20 / 66.87 / 56.75 / 5,400; Cognee 83.48 / 51.80 / 59.30 / 56.02 / 32,532; EverOS 82.75 / 80.40 / 58.64 / 47.73 / 8,559; Hindsight 81.99 / 72.20 / 70.22 / 59.75 / 24,683; Mem0 (cloud) 77.68 / 56.00 / 70.41 / 43.33 / 17,395; Letta 77.12 / 77.67 / 69.22 / 52.30 / 14,188; Supermemory 73.53 / 66.07 / 65.98 / 52.49 / 15,238; Viking 69.33 / 61.07 / 70.76 / 58.14 / 5,964; Zep/Graphiti 63.83 / 79.80 / 68.70 / 56.11 / 1,862 — [aiagentmemory.org leaderboard](https://aiagentmemory.org/articles/agent-memory-leaderboard/)
- Comparability note from the same page: Mem0 cloud 56.0 under OmniMemEval vs 94.4 vendor-reported on LongMemEval; Zep 63.8 vs 94.7 vendor-reported on LoCoMo; Hindsight 82.0 vs 92; "only MemOS matches its own README figures"; both public leaderboards have an owner (Vectorize runs agentmemorybenchmark.ai, MemTensor runs OmniMemEval) and OmniMemEval states competitor configs are not claimed optimal — [aiagentmemory.org](https://aiagentmemory.org/articles/agent-memory-leaderboard/)
- EverMind's unified framework evaluates EverOS, Mem0, MemOS, Zep, MemU through their online APIs under same datasets/metrics/answer model and reports EverOS best (vendor-run) — [EverMind blog](https://evermind.ai/blogs/a-unified-evaluation-framework-for-ai-memory-systems)
- MemReader paper (arXiv Apr 2026) LongMemEval table: Mem0 66.40%, MemOS 77.80%, EverOS 83.00% (different setup, not comparable) — [arXiv 2604.07877](https://arxiv.org/pdf/2604.07877)

**Long-context baselines**
- Full-context gpt-4o on LongMemEval-S = 60.2% (cited by both Mastra and Supermemory) — [Mastra](https://mastra.ai/research/observational-memory); [Supermemory](https://supermemory.ai/research/longmembench)
- Chroma "Context Rot" (Jul 14 2025, 18 models incl. Claude Opus 4, Gemini 2.5 Pro, GPT-4.1, o3, Qwen3): on LongMemEval, 306 prompts averaging ~113k tokens full vs ~300 tokens focused — all models scored significantly higher on focused prompts; Claude showed the largest gap, largely via abstentions; performance degrades non-uniformly with input length even on simple tasks; a single distractor lowers performance, four lower it further — [Chroma research](https://www.trychroma.com/research/context-rot)
- NVIDIA RULER: "only half of them can maintain satisfactory performance at the length of 32K" despite near-perfect needle-in-haystack — cited via [glasp summary](https://glasp.ai/articles/context-rot-rag-long-context-hybrid) (secondary)

### Inferences
- A credible ShogunAgent proof is a published same-harness run (ideally submitting to OmniMemEval and running the open Mem0 memory-benchmarks harness) reporting accuracy, context tokens per answer, and p95 latency together, with reader and judge models named. Any single accuracy number without those will be dismissed given the Mem0/Zep history.
- Token-efficiency is now a first-class axis: MemOS reaches 88.8 LoCoMo with 5.4K tokens while Hindsight needs 24.7K for 82.0; Zep uses 1.9K tokens. ShogunAI's derived-state tier naturally produces compact context, which is a defensible benchmark story.
- LoCoMo and LongMemEval-S are near saturation for retrieval (95–97% recall) and the differentiating categories are multi-session reasoning (~87% ceiling) and knowledge updates/temporal reasoning. BEAM-10M (best ~56–65) and MemoryAgentBench's test-time learning / selective forgetting are where a world-model/state-table approach could show clear gains.
- Because ShogunAI's data is screen text, meetings, email and calendar (not chat transcripts), none of these benchmarks directly measures its workload; a ShogunAgent-specific benchmark (e.g., "open-loop / commitment tracking from heterogeneous event streams") would need to be built, ideally alongside DolphinBench-style task-outcome metrics.

### Gaps
- No independent (non-vendor-owned) leaderboard exists; both OmniMemEval and agentmemorybenchmark.ai are run by a competitor.
- The judge model for Mem0's 94.4/92.5 claims is not named in the source; GPT-5.4 as Zep's reader/judge appears only on an aggregator.
- No per-system numbers found for GPT-5 / Claude / Gemini raw 1M–10M context baselines on BEAM beyond the paper's qualitative statement; LongMemEval-M scores for top systems were not found.
- I found no reliable sources on STATE-Bench beyond its mention in Mem0's table.

---

## Key Question 2: Architectures — what wins and at what cost?

### Takeaway
No single architecture wins across benchmarks: temporal-graph (Zep) wins on token efficiency and latency, hierarchical/OS-style (MemOS, Mastra OM) leads same-harness accuracy with small contexts, "reasoning-substrate" designs (Hindsight) lead vendor LongMemEval claims but spend 4–5× more tokens, and vector-only Mem0 has shifted toward hybrid (semantic + BM25 + entity) retrieval with an entity table replacing an external graph store. The common convergence is toward exactly ShogunAI's shape: raw events → structured derived state (entities, relations, validity intervals, provenance) → compact summaries, with background consolidation and a stable, cacheable context prefix.

### Cited Findings

**Vector-only / hybrid retrieval (Mem0)**
- Mem0 April 2026: single-pass ADD-only extraction; retrieval runs three scoring passes (semantic, BM25 keyword, entity) in parallel and fuses them; entity linking replaces external graph stores via a parallel `{collection}_entities` collection; the `relations` field was removed, so relationships cannot be traversed directly; five memory types (working, episodic, semantic, procedural, hierarchical) with procedural "still early-stage"; scoping by `user_id`, `agent_id`, `run_id`/`session_id`, `app_id`/`org_id`; async writes default in v1.0.0 — [Mem0 State of Memory 2026](https://mem0.ai/blog/state-of-ai-agent-memory-2026)

**Temporal knowledge graph (Zep / Graphiti)**
- Graphiti: non-lossy updates with timeline of facts and validity periods; bi-temporal model tracking when events occurred and when ingested; contradicted facts get an `invalid_at` timestamp rather than deletion; three layers: episodic, semantic (entities/facts), community subgraphs; vendor paper (Jan 2025) claims 94.8% vs 93.4% over MemGPT on Deep Memory Retrieval and up to 18.5% LongMemEval accuracy improvement with 90% latency reduction vs baseline — [arXiv 2501.13956](https://arxiv.org/html/2501.13956v1); [rywalker summary](https://rywalker.com/research/zep)
- Graphiti supports Neo4j and FalkorDB; had a Kuzu in-process driver; releases through June 2026 (v0.29.2) per third-party blog — [graphiti-core PyPI](https://pypi.org/project/graphiti-core/0.17.4/); [aktagon signals](https://signals.aktagon.com/articles/2026/07/zep-a-temporal-knowledge-graph-architecture-for-agent-memory/)
- Same-harness: Zep/Graphiti uses 1,862 context tokens on LoCoMo (lowest of 14) but scores 63.83 LoCoMo / 79.80 LongMemEval — [aiagentmemory.org](https://aiagentmemory.org/articles/agent-memory-leaderboard/)

**Hierarchical / OS-style (MemGPT→Letta, MemoryOS, MemOS, Mastra OM)**
- Letta sleep-time compute (Apr 21 2025, "MemGPT 2.0"): two agents per sleep-enabled agent — primary (talks to user, cannot edit core memory) and sleep-time agent (holds core-memory editing tools, rewrites in-context memory asynchronously); "reasoning during sleep time transforms 'raw context' into 'learned context'"; argues MemGPT's bundling of memory management into one agent made it slower and messier; sleep agent can use a stronger/slower model (e.g., gpt-4o-mini primary, gpt-4.1/Sonnet 3.7 sleep) — [Letta blog](https://www.letta.com/blog/sleep-time-compute)
- Letta Agent File (.af, April 2025): open format to export/import a stateful agent (memory blocks, tools, state) between self-hosted and cloud; persisted in Postgres or SQLite — [Letta blog](https://letta.com/blog/agent-file)
- MemoryOS (arXiv 2506.06326): claims superiority over A-MEM on LoCoMo (self-run), criticizes A-MEM's multi-step link generation for latency and error accumulation — [arXiv 2506.06326](https://arxiv.org/pdf/2506.06326)
- Mastra Observational Memory (Feb 2026): Observer + Reflector background agents; three tiers (raw messages → dated prioritized observations at a token threshold → reflections at a second threshold); each observation carries up to three dates (observation, referenced, relative); triggers are token-based; the context prefix is stable and append-only so it can be prompt-cached; compression 3–6× on text (6× on LongMemEval), anecdotally 5–40× on tool-call-heavy workloads; avg context ~30k tokens over a 57M-token benchmark — [Mastra research](https://mastra.ai/research/observational-memory)
- EverMemOS "self-organizing memory operating system for structured long-horizon reasoning" (Jan 2026) — [arXiv 2601.02163](https://arxiv.org/pdf/2601.02163)

**Agentic memory (A-MEM)**
- A-MEM (NeurIPS 2025): Zettelkasten-style structured notes with dynamic indexing and linking; on LoCoMo with GPT-4o-mini, multi-hop F1 45.85 vs 9.09 for baseline in its table; ~2,520 tokens vs 16,910 for full context — [arXiv 2502.12110](https://arxiv.org/pdf/2502.12110); [NeurIPS 2025](https://papers.nips.cc/paper_files/paper/2025/hash/19909c36f51abc4856b4560aff3d36d6-Abstract-Conference.html)

**Structured "world model" / state approaches (closest to ShogunAI)**
- Hindsight: "four logical networks that distinguish world facts, agent experiences, synthesized entity summaries, and evolving beliefs"; operations retain / recall / reflect; temporal, entity-aware retain layer; reflection layer updates beliefs traceably; motivated by the claim that vector/graph top-k retrieval "blurs evidence and inference, handles long horizons poorly, and limits explanation" — [arXiv 2512.12818](https://arxiv.org/abs/2512.12818). Memanto paper notes Hindsight scores highest but at "complexity score of 4 out of 4" — [arXiv 2604.22085](https://arxiv.org/pdf/2604.22085)
- Supermemory engine: atomic memories generated from chunks with the chunk kept as evidence; relational versioning with three edge types — "updates" (state change/correction), "extends" (detail), "derives" (inference from multiple memories); temporal metadata `documentDate` and `eventDate`; hybrid search injects source chunks — [Supermemory eval report](https://supermemory.ai/research/longmembench); memory decay with tiered hot/cold storage (Cloudflare KV for hot) — [Supermemory engineering blog](https://blog.supermemory.ai/memory-engine)
- Honcho (Plastic Labs): "dialectic reasoning" runs after conversations to maintain a running model of user preferences, style, goals, patterns; supplies a user representation, a "peer card" of key facts and a session summary truncated to a `contextTokens` budget — [Hermes Agent Honcho docs](https://hermes-agent.nousresearch.com/docs/user-guide/features/honcho)
- BEAM's LIGHT: long-term episodic memory + short-term working memory + scratchpad of key facts — [arXiv 2510.27246](https://arxiv.org/abs/2510.27246)
- Governed Shared Memory / MemClaw (Jun 2026): "Long-context retrieval alone is insufficient for production multi-agent memory"; mechanisms: scoped retrieval, temporal supersession, provenance tracking, policy-governed propagation — [arXiv 2606.24535](https://arxiv.org/abs/2606.24535)

**"Just use long context + caching"**
- Context rot evidence (above): full-context gpt-4o 60.2% on LongMemEval-S vs 80–95% for memory systems — [Mastra](https://mastra.ai/research/observational-memory); [Chroma](https://www.trychroma.com/research/context-rot)
- BEAM: 1M-context models with or without RAG degrade as dialogues lengthen — [arXiv 2510.27246](https://arxiv.org/abs/2510.27246)
- Secondary 2026 claims that newest Opus-class models mitigate rot (8-needle at 1M tokens from high-20s% to high-70s%) — unverified — [braindetox](https://braindetox.kr/en/posts/tokenmaxxing_dead_2026.html)
- Anthropic prompt caching economics (official, Oct 2026): 5-min cache writes 1.25× base input, 1-hour writes 2×, reads 0.1× (0.025× on Fable 5.1/Mythos 5.1; 0.05× on Opus 5.5/Sonnet 5.5); min cacheable 512 tokens on current models; cache prefix order tools → system → messages; up to 4 breakpoints, 20-block lookback; e.g., Sonnet 5.5 $2/MTok base, $0.10 cache hit; Haiku 5.5 $0.10 base / $0.01 hit (≤100K prompts) — [Anthropic prompt caching docs](https://platform.claude.com/docs/en/build-with-claude/prompt-caching)
- Gemini: implicit caching on by default for 2.5+ models; explicit caching not supported in the Interactions API or on Gemini 4 Argon+; min tokens 4,096 (Gemini 4 Argon, 3.x) / 2,048 (2.5); for Gemini 4 agent loops the previous request is cached only if the new request adds ≥2,050 tokens — [Gemini caching docs](https://ai.google.dev/gemini-api/docs/caching)
- Anthropic context editing + memory tool launch: memory tool + context editing improved performance 39% over baseline; context editing alone 29% — [Anthropic announcement](https://www.anthropic.com/news/context-management)
- Mastra: stable prefixes let "many providers cut token costs 4–10× versus uncached prompts" — [Mastra](https://mastra.ai/research/observational-memory)

**Latency / cost figures**
- Zep: 155 ms retrieval (vendor, 2026) / 189 ms (Mem0's citation); Zep corrected p95 0.632s vs Mem0 Base 0.200s (2025) — [Zep research](https://getzep.com/research); [Zep blog](https://www.getzep.com/blog/lies-damn-lies-statistics-is-mem0-really-sota-in-agent-memory/)
- Tokens per answer same-harness: 1.9K (Zep) to 32.5K (Cognee) — [aiagentmemory.org](https://aiagentmemory.org/articles/agent-memory-leaderboard/)

### Inferences
- The field has converged on a 3-tier, bi-temporal, provenance-carrying design (raw evidence → typed facts/entities with `valid_at`/`invalid_at` or `updates`/`extends`/`derives` edges → synthesized summaries/beliefs) plus a background consolidator — which is structurally ShogunAI's raw events → derived state → summaries + Dream Cycle. ShogunAgent's differentiation is therefore not the tiering but (a) the richness of the derived state schema (people/projects/commitments/open loops), (b) multi-source ingestion beyond chat, and (c) local-first execution.
- Cacheable, stable context prefixes (Mastra OM pattern) combined with 0.025–0.1× cache-read pricing make a "profile + current state block" injected every turn nearly free; this argues for ShogunAgent exposing a deterministic, append-only "context block" API rather than per-turn ad-hoc retrieval.
- Pure graph stores are losing favor (Mem0 removed traversable relations; Kuzu died); an entity/state table with typed relation columns in SQLite/Postgres captures most of the value with far less operational cost.

### Gaps
- No same-harness latency or $/query comparison across architectures was found; only tokens per answer.
- No published head-to-head between an explicit "entity/state table" world model (like ShogunAI's) and graph/vector systems on any standard benchmark.
- OpenAI Responses API state (`previous_response_id`) behavior and pricing were not covered by any retrieved source.

---

## Key Question 3: Research papers 2025–2026 — consolidation, forgetting, multi-agent, portability, provenance, user models

### Takeaway
2026 research has moved from "how to retrieve" to "how memory should change over time": sleep/dream-style consolidation is now a mainstream design (Letta sleep-time agents, SleepGate, "Language Models Need Sleep", Human-Inspired Memory Architecture), provenance and governed sharing are formalized for multi-agent fleets (MemClaw/ArgusFleet), and surveys name heterogeneous (non-uniform) forgetting and "authority-conditioned mutability" as the open problems — all of which map directly onto ShogunAI's Dream Cycle and world-model provenance.

### Cited Findings

**Consolidation / "sleep" / "dream" cycles**
- Letta sleep-time compute (Apr 2025): background agent rewrites in-context memory during idle time; "Pareto improvement" shifting compute from latency-critical interactions to idle periods; configurable frequency — [Letta blog](https://www.letta.com/blog/sleep-time-compute)
- "Learning to Forget: Sleep-Inspired Memory Consolidation" / SleepGate (Mar 2026, Ying Xie): periodic sleep cycles over the KV cache evict stale entries and merge the rest, targeting proactive interference — [arXiv 2603.14517](https://arxiv.org/html/2603.14517v1)
- "Language Models Need Sleep" (May 2026, Lee, McLeish, Goldstein, Fanti): when context fills, extra passes write consolidated context into fast weights of state-space layers; limitations: training instability and overhead; agent-level consolidation listed as future application — [alphaxiv 2605.26099](https://www.alphaxiv.org/abs/2605.26099v1)
- "Human-Inspired Memory Architecture for LLM Agents" (May 2026): six mechanisms incl. a sleep-phase consolidation step; synthetic calibration method sets pipeline thresholds without benchmark exposure — [emergentmind 2605.08538](https://www.emergentmind.com/papers/2605.08538)
- Mastra OM's Reflector merges related observations, notes patterns and drops superseded content, triggered by token thresholds — [Mastra](https://mastra.ai/research/observational-memory)
- Hindsight's "reflect" operation updates beliefs traceably — [arXiv 2512.12818](https://arxiv.org/abs/2512.12818)

**Forgetting / decay**
- "Always-On Agents: A Survey of Persistent Memory, State, and Governance" (Jun 2026): most work decays uniformly whereas real state is heterogeneous; names "authority-conditioned mutability" as open — [arXiv 2606.30306](https://arxiv.org/pdf/2606.30306)
- "Memory for Autonomous LLM Agents" survey (Mar 2026): open challenges — continual consolidation, causally grounded retrieval, trustworthy re-reflection, learned forgetting, multimodal embodied memory — [arXiv 2603.07670](https://arxiv.org/pdf/2603.07670v1)
- MemoryAgentBench treats selective forgetting as one of four core competencies that current methods fail to master — [arXiv 2507.05257](https://arxiv.org/abs/2507.05257)
- Supermemory describes memory decay with hot/cold tiers — [Supermemory blog](https://blog.supermemory.ai/memory-engine)

**Multi-agent memory and governance**
- Governed Shared Memory for Multi-Agent LLM Systems (Jun 23 2026, Margalit et al.): four failure modes — unauthorized leakage, stale propagation, contradiction persistence, provenance collapse; MemClaw multi-tenant service + ArgusFleet harness; 100% of depth-four derivation chains reconstructed with correct writer identity at sub-second per hop; zero cross-fleet leakage; production bugs found: sub-tenant scope bypass on direct GET-by-id, and a synchronous near-duplicate gate rejecting contradictory writes before the async contradiction detector ran — [arXiv 2606.24535](https://arxiv.org/abs/2606.24535)
- "The Song, Not the Pin" (NeurIPS 2026): shared long-term memory is incomplete as retrievable content; action requires explicit provenance, time-bounded validity and revocable coordination authority — [NeurIPS 2026](https://neurips.cc/virtual/2026/169580)
- Claude Code subagents can maintain their own separate auto-memory directories; the main conversation's memory is not loaded into subagents except forks — [Claude Code docs](https://code.claude.com/docs/en/memory)

**Provenance**
- Agent Zero Memory: "provenance-aware long-term memory" (Aug 2026) — [arXiv 2608.29606](https://arxiv.org/pdf/2608.29606); Eywa: "provenance-grounded long-term memory" (May 2026), cautions that external benchmark rows "are not controlled reproductions" — [arXiv 2605.30771](https://arxiv.org/pdf/2605.30771)
- Supermemory keeps the source chunk attached to each atomic memory as evidence — [Supermemory](https://supermemory.ai/research/longmembench)

**Portability / agent-file formats**
- Letta .af (April 2025): open import/export of full agent state via SDK — [Letta blog](https://letta.com/blog/agent-file)
- Anthropic launched Claude memory import/export around Mar 1 2026 (third-party reports): copy-paste flow — run a prepared prompt in the other assistant, paste into Claude's import tool, review; labeled experimental in March 2026 — [TechCabal May 27 2026](https://techcabal.com/2026/05/27/4-easy-steps-to-move-your-chatgpt-memory-to-claude/); [Medium guide](https://medium.com/@unicodeveloper/how-to-import-memory-into-claude-from-chatgpt-gemini-or-grok-step-by-step-guide-c78ac8f7e4a4)
- Claude Code auto memory is plain markdown (`MEMORY.md` index + topic files) that users can edit, move (`autoMemoryDirectory`) or delete — [Claude Code docs](https://code.claude.com/docs/en/memory)

**Personal user-model inference**
- Honcho dialectic reasoning: multi-pass reasoning about the user appended as a second context layer, with cadence/depth settings; outputs user representation + peer card + session summary within a token budget — [Hermes Honcho plugin README](https://download.hermesedi.duckdns.org/.hermes/hermes-agent/plugins/memory/honcho/README.md)
- Mastra OM "Observer" tracks current task and suggested response with priority tags — [Mastra](https://mastra.ai/research/observational-memory)
- Mem0 stores agent-generated facts alongside user-stated facts — [Mem0 State of Memory 2026](https://mem0.ai/blog/state-of-ai-agent-memory-2026)

**Other notable 2026 systems**
- MemReader: "from passive to active extraction" (Apr 2026) — [arXiv 2604.07877](https://arxiv.org/pdf/2604.07877); Memanto: typed semantic memory with information-theoretic retrieval (Apr 2026) — [arXiv 2604.22085](https://arxiv.org/pdf/2604.22085); ZenBrain 7-layer neuroscience-inspired architecture (Apr 2026) — [arXiv 2604.23878](https://arxiv.org/pdf/2604.23878); Agentic Context Management (Jul 2026) — [arXiv 2607.21503](https://arxiv.org/pdf/2607.21503)

### Inferences
- ShogunAI's "Dream Cycle" has strong academic and commercial precedent (Letta sleep-time, Mastra Reflector, SleepGate). The differentiating research framing available to ShogunAgent is *heterogeneous decay by state type* (commitments vs. preferences vs. ephemeral screen context) and *authority-conditioned mutability* (who/what may overwrite a fact), both named as open problems in the Jun 2026 survey.
- MemClaw's production bugs (scope bypass on GET-by-id; sync dedup gate beating async contradiction detection) are concrete test cases ShogunAgent should include in its own harness.
- A `.af`-like export plus a "memory import prompt" convention are now expected; ShogunAgent should ship an open export schema (entities, facts with validity intervals, provenance pointers) from day one.

### Gaps
- No peer-reviewed quantification of how much "dream/consolidation" improves downstream accuracy vs. cost beyond Letta's qualitative "Pareto" claim and Mastra's compression ratios.
- No standard agent-memory portability format beyond Letta's .af; no evidence of cross-vendor adoption.
- Honcho's theory-of-mind claims have no benchmark I could find beyond its LongMemEval/LoCoMo/BEAM vendor numbers.

---

## Key Question 4: Privacy and security

### Takeaway
Memory poisoning is a demonstrated, peer-reviewed attack class (MINJA, NeurIPS 2025/ICLR 2026) that works through ordinary queries, and 2026 brought the first defenses (A-MemGuard, OWASP Agent Memory Guard, ICML 2026 empirical study); on the regulatory side, no DPA has issued agent-memory-specific guidance, GDPR Art. 17 erasure against embeddings/consolidated memories is widely described as unsolved, and EU AI Act high-risk obligations take effect August 2026 — which collectively favor ShogunAgent's local-first, provenance-linked, cascade-deletable design.

### Cited Findings

**Attacks**
- MINJA, "Memory Injection Attacks on LLM Agents via Query-Only Interaction" (Dong et al.; NeurIPS 2025 poster, also listed ICLR 2026): attacker injects malicious records only via queries and output observations; "bridging steps" link victim queries to malicious reasoning; "indication prompt" plus "progressive shortening"; effective across diverse agents; "any user can influence agent memory"; recommended defenses: memory isolation, strong user authentication, secure memory management, prompt filtering — [arXiv 2503.03704](https://arxiv.org/abs/2503.03704); [NeurIPS 2025](https://neurips.cc/virtual/2025/poster/118152)
- A-MemGuard paper cites ASB figure: even advanced LLM-based detectors miss 66% of poisoned memory entries because they look harmless in isolation — [arXiv 2510.02373](https://arxiv.org/html/2510.02373v1)
- Memory poisoning attack and defense on EHR agents (Jan 2026) — [arXiv 2601.05504](https://arxiv.org/pdf/2601.05504v1)

**Defenses**
- A-MemGuard (Oct 2025): consensus-based validation (flags memories whose reasoning paths diverge from related memories) + dual-memory design storing detected failures as "lessons"; claims attack success rate cut by over 95% with minimal utility cost — [arXiv 2510.02373](https://arxiv.org/html/2510.02373v1)
- "An Empirical Study of Memory Poisoning Defenses for LLM Agents" — ICML 2026 — [ICML 2026 poster](https://icml.cc/virtual/2026/poster/61006)
- OWASP Agent Memory Guard (reported released Jun 1 2026): open-source runtime layer between agent and memory store screening every read/write; reported 92.5% recall at 59µs latency (secondary source, unverified) — [feedbagel](https://feedbagel.com/post/owasp-agent-memory-guard-stop-ai-agents-from-being-weaponized-through-their-own-); [Kiteworks](https://kiteworks.substack.com/p/ai-agent-memory-poisoning-owasp-defense)
- MemClaw governance: scoped retrieval, provenance, policy-governed propagation; found and fixed a sub-tenant scope bypass in production — [arXiv 2606.24535](https://arxiv.org/abs/2606.24535)

**Vendor security guidance**
- Anthropic memory tool: client-side so "you control where and how the data is stored"; handler must reject paths outside `/memories` (path traversal e.g. `/memories/../../secrets.env`, URL-encoded `%2e%2e%2f`); "Claude usually refuses to write sensitive information to memory files. For stronger guarantees, add validation that strips sensitive data"; cap file sizes; expire unaccessed files; ZDR applies per API data-retention docs — [Anthropic memory tool docs](https://platform.claude.com/docs/en/agents-and-tools/tool-use/memory-tool)
- Claude Code auto memory is "machine-local"; not shared across machines or cloud environments; off by default in self-hosted environments; `permissions.blockReadsOutsideWorkingDirectories` blocks loading memory from repo-supplied directories (supply-chain protection) — [Claude Code docs](https://code.claude.com/docs/en/memory)
- Mem0: OpenMemory MCP stores memory locally with a dashboard; FastEmbed local embeddings reduce data egress; memory scoping `USER_ID` from authenticated identity; Mem0 lists "privacy and consent architecture" as unsolved and application-layer today — [Mem0 State of Memory 2026](https://mem0.ai/blog/state-of-ai-agent-memory-2026)
- Supermemory MCP: separate server instance per user selected via URL path parameter; isolation by unique URLs — [Supermemory MCP tech docs](https://docs.supermemory.ai/supermemory-mcp/technology)

**Regulation**
- GDPR Art. 17: engineering guides read it as requiring finding all stored data about a person, deleting underlying records (not just de-indexing), and proving deletion; "locating all relevant embeddings is not tractable once an agent has learned facts across many sessions" — [DEV Community](https://dev.to/plur9/is-ai-agent-memory-gdpr-compliant-a-developers-guide-412)
- "No commercially available vector database provides a provable deletion mechanism for data embedded in a vector store"; recommends per-user namespaces and embedding-ID → source-record mapping for cascading deletes — [tianpan.co Apr 10 2026](https://tianpan.co/blog/2026-04-10-gdpr-ai-agents-compliance-architecture)
- CNIL guidance on agent-based AI (via TrustArc): persistent memory enables hyper-personalization but increases risks incl. outdated/inaccurate information — [TrustArc](https://trustarc.com/products/privacy-data-governance/nymity-research/france-cnil-agent-based-ai-guidance/)
- March 2025: 32 European DPAs launched a coordinated investigation into whether organizations can prove deletion — [DEV Community](https://dev.to/manav8498/your-ai-agent-remembers-everything-can-it-prove-it-forgot-3l79)
- "No regulator has issued guidance specific to agent memory as distinct from training data"; erasure against consolidated memory "has no production-ready answer" — [TinyFish](https://current.tinyfish.ai/issue/25/foundations/article/13154)
- EU AI Act high-risk obligations / logging enforceable Aug 2  2026 (secondary sources; one claims 10-year log retention, which conflicts with other readings — verify against Regulation 2024/1689) — [sota.io](https://sota.io/blog/aws-bedrock-agentcore-eu-gdpr-ai-act-multi-agent-state-memory-tool-calls-2026); [brenner-axiom](https://brenner-axiom.codeberg.page/research/2026-02-22-legal-framework-agentic-ai-eu/)

### Inferences
- A derived-state design where every fact carries a provenance pointer to its raw event enables the cascade deletion regulators want (delete raw event → invalidate derived facts → regenerate summaries), which embeddings-only systems cannot offer; this is a concrete compliance advantage ShogunAgent can market.
- Because screen-text ingestion will capture third parties' personal data (colleagues' names, emails), ShogunAgent needs per-source consent/retention policies and redaction *before* consolidation, not just at retrieval.
- Memory-write validation (consensus checks à la A-MemGuard, source-authority tagging) should be a core API feature, not an afterthought, since agents consuming ShogunAgent memory will be exposed to MINJA-style injection via any ingested document.

### Gaps
- No primary OWASP page for Agent Memory Guard was retrieved; figures are secondhand.
- No source found on how Zep, Letta or Supermemory implement encryption-at-rest with user-held keys; no evidence of any commercial memory vendor offering client-side encryption.
- No primary legal text verified for AI Act dates/retention; treat as secondary.

---

## Key Question 5: Implementation stack

### Takeaway
The pragmatic 2026 stack for a local-first Rust memory service is SQLite (+FTS5) with sqlite-vec or LanceDB for vectors, an entity/relation table instead of a graph database (Kuzu was archived Oct 2025 after Apple's acquisition; FalkorDB/Neo4j are server-based), local embeddings via Qwen3-Embedding or nomic with hosted fallbacks (OpenAI text-embedding-3-large ~$0.13/MTok, Voyage-4 ~$0.12/MTok with a 200M-token free tier), and for a hosted tier either pgvector on existing Postgres or object-storage-backed Turbopuffer (~$0.30/M vectors/month storage, $64/month minimum) or Cloudflare Durable Objects (supermemory's choice) for per-user isolation.

### Cited Findings

**Embedded / local vector + FTS**
- sqlite-vec: exact flat index (no index build), single SQLite file holds conversations, preferences and chunks; author's benchmark claims it outperforms Pinecone/Weaviate/Chroma on tested workloads (unverified, author-run) — [DEV HyperNexus](https://dev.to/hypernexus/sqlite-vector-search-the-dependency-free-ai-memory-stack-that-outperforms-pinecone-5d27)
- LanceDB: OpenClaw ships an official memory plugin storing long-term memory in LanceDB with auto recall/capture around each turn; built-in dimensions only for text-embedding-3-small (1536) / -large (3072), others need explicit dimension config — [OpenClaw memory-lancedb](https://docs.openclaw.ai/plugins/memory-lancedb.md); Agno pairs LanceDB with SQLite — [Agno docs](https://docs.agno.com/examples/concepts/rag/rag-with-lance-db-and-sqlite); vxdb exposes embedded LanceDB over MCP with in-process fastembed — [vxdb](https://github.com/Xuanwo/vxdb)
- Rust crates in the space: datacules-agentdb — [docs.rs](https://docs.rs/crate/datacules-agentdb/0.5.3/source/README.md)

**Graph stores**
- Kuzu GitHub archived Oct 10 2025 (final v0.11.3); EU DMA filing showed Apple agreed Oct 9 2025 to acquire Kùzu Inc.; community forks only — [The Register](https://www.theregister.com/2025/10/14/kuzudb_abandoned/); [PuppyGraph](https://www.puppygraph.com/blog/what-is-kuzudb); [PyPI kuzu](https://pypi.org/project/kuzu/)
- FalkorDB is a Redis module using GraphBLAS (server-based, not in-process); markets itself as Kuzu migration target — [FalkorDB](https://www.falkordb.com/blog/kuzudb-to-falkordb-migration/)
- GitLab's Kuzu-replacement analysis treats Neo4j as the "boring solution" base case — [GitLab issue](https://gitlab.com/gitlab-org/rust/knowledge-graph/-/work_items/254)
- Mem0 removed traversable relations in favor of an entity collection — [Mem0 State of Memory 2026](https://mem0.ai/blog/state-of-ai-agent-memory-2026)

**Embedding models**
- OpenAI text-embedding-3-large ≈ $0.13/MTok, MTEB 64.6, Matryoshka truncation — [Rework](https://resources.rework.com/tools/ai-data-infra/best-embedding-models-2026); [PremAI](https://www.premai.io/blog/best-embedding-models-for-rag-2026-ranked-by-mteb-score-cost-and-self-hosting/)
- voyage-4-large ≈ $0.12/MTok, 200M free tokens; not on public MTEB as of Jan 2026 (vendor claim) — [Rework](https://resources.rework.com/tools/ai-data-infra/best-embedding-models-2026)
- Qwen3-Embedding-8B ≈ 70.6 multilingual MTEB, ≈ 80.7 code; "best open-source/self-host pick" — [PremAI](https://www.premai.io/blog/best-embedding-models-for-rag-2026-ranked-by-mteb-score-cost-and-self-hosting/)
- nomic-embed: open-weight, laptop-class — [techsy](https://techsy.io/en/blog/best-embedding-models-for-rag)
- Mem0 supports FastEmbed for local embeddings; 20 vector-store backends — [Mem0 State of Memory 2026](https://mem0.ai/blog/state-of-ai-agent-memory-2026)

**Hosted vector DBs and cost**
- Turbopuffer: S3-backed, pay-per-use, ~$0.30/M vectors/month stored; plan minimums reported at $64/month (Launch) and $256/month (Scale); "dramatically cheaper for large corpora at the cost of some latency" — [agentdeals](https://agentdeals.dev/vector-database-pricing); [costbench](https://costbench.com/compare/lancedb-vs-turbopuffer/); [alexcloudstar](https://www.alexcloudstar.com/blog/vector-database-comparison-2026)
- pgvector: free on existing Postgres; at ~2M 1024-dim vectors one author's RDS bill rose ~15% — [DEV alexcloudstar](https://dev.to/alexcloudstar/vector-database-comparison-2026-pgvector-pinecone-turbopuffer-and-qdrant-55ak)
- Qdrant: Apache 2.0 self-host; managed 2-node 8GB/50GB ≈ $90–150/month; cloud free 1GB tier — [noqta](https://noqta.tn/en/blog/vector-database-comparison-2026-pgvector-pinecone-qdrant-weaviate); [agentdeals](https://agentdeals.dev/vector-database-pricing)
- LanceDB Cloud: usage-based after $100 credits / contact-sales — [agentdeals](https://agentdeals.dev/vector-database-pricing)

**Serverless / Cloudflare (supermemory)**
- Supermemory MCP: React Router app calling api.supermemory.ai/v3; separate MCP server instance per user via URL path; SSE transport; hosted on Cloudflare using Durable Objects for persistent connections; "shipped in about 5 hours" — [Supermemory MCP tech](https://docs.supermemory.ai/supermemory-mcp/technology)
- Supermemory SMFS mounts memory at `/memory` inside a Cloudflare Container (container-enabled Durable Object) so agents use file commands — [Supermemory SMFS docs](https://supermemory.ai/docs/smfs/providers/cloudflare.md)
- Supermemory uses Cloudflare KV for hot tier — [Supermemory engineering](https://blog.supermemory.ai/memory-engine)
- Durable Objects: each has its own durable, transactional, strongly consistent storage; globally addressable ID; evicted after inactivity so in-memory state is lost — [Cloudflare docs](https://developers.cloudflare.com/durable-objects/concepts/what-are-durable-objects/index.md)

**Multi-tenant isolation**
- MemClaw: tenant isolation held but sub-tenant scope was bypassed on direct GET-by-id (fixed) — [arXiv 2606.24535](https://arxiv.org/abs/2606.24535)
- Mem0 scoping dimensions: `user_id`, `agent_id`, `run_id`, `app_id`/`org_id` — [Mem0](https://mem0.ai/blog/state-of-ai-agent-memory-2026)
- GDPR guides recommend per-user namespaces with embedding-ID → source mapping — [tianpan.co](https://tianpan.co/blog/2026-04-10-gdpr-ai-agents-compliance-architecture)

### Inferences
- For 1M memories (≈1M vectors at 1024 dims), infra cost is dominated by minimums, not storage: Turbopuffer ≈ $0.30 storage + $64 plan minimum; pgvector on an existing Postgres is near-zero incremental; a single-user local SQLite file is free. LLM extraction/consolidation tokens (not vector storage) will be the real cost driver — Mem0's ~7K tokens/retrieval and Hindsight's ~25K show that.
- Given Kuzu's death and Mem0's retreat from graph traversal, ShogunAgent should model relations as typed rows (subject, predicate, object, valid_from, valid_to, provenance) in SQLite/Postgres and offer graph-like queries via SQL recursive CTEs, rather than depend on a graph engine.
- A Durable-Object-per-user design matches ShogunAgent's "one user = one world model" shape and gives natural isolation, but sync from a local-first Rust core to an edge object is the hard part; supermemory's SMFS (file mount) shows a filesystem-shaped sync surface is viable.

### Gaps
- No authoritative, current price for Turbopuffer, LanceDB Cloud or Qwen3-Embedding hosting was retrieved; all figures are from secondary comparison blogs that conflict.
- No benchmark of sqlite-vec vs LanceDB in Rust at 1M+ vectors from a neutral party.
- No evidence found of a maintained embedded graph DB successor to Kuzu for Rust.

---

## Key Question 6: Interfaces — what agents actually call

### Takeaway
Three interface shapes dominate in October 2026: (1) Anthropic's file-shaped memory tool (`memory_20250818`: view/create/str_replace/insert/delete/rename under `/memories`, client-side storage, auto-injected "always view memory first" protocol) and Claude Code's `MEMORY.md` index with a 200-line/25KB load budget; (2) MCP memory servers converging on `add_memory` / `search_memory(ies)` / `update_memory` / `remove_memory` with type, tags and namespace parameters; and (3) "context block" injection with explicit token budgets (Honcho `contextTokens`, Mastra's cacheable observation log). ShogunAgent should support all three: a filesystem-shaped view for Claude/SMFS-style agents, MCP add/search tools, and a budgeted `get_context` endpoint.

### Cited Findings

**Anthropic memory tool (API)**
- Tool spec `{"type": "memory_20250818", "name": "memory"}`; no input schema defined by developer; available on all Claude 4+ models; no beta header required for the tool itself (helpers live in SDK beta namespace) — [Anthropic memory tool docs](https://platform.claude.com/docs/en/agents-and-tools/tool-use/memory-tool)
- Commands: `view` (dir listing up to 2 levels, file with 6-char right-aligned line numbers, optional `view_range`, truncates >16,000 chars), `create` (file_text), `str_replace` (old_str/new_str; errors on duplicate), `insert` (insert_line/insert_text), `delete`, `rename` (old_path/new_path); cannot delete/rename `/memories` root; `/memories` is a prefix the handler maps to real storage (per-user dir or DB keys) — [same](https://platform.claude.com/docs/en/agents-and-tools/tool-use/memory-tool)
- API auto-injects system prompt: "ALWAYS VIEW YOUR MEMORY DIRECTORY BEFORE DOING ANYTHING ELSE... ASSUME INTERRUPTION: Your context window might be reset at any moment" — [same](https://platform.claude.com/docs/en/agents-and-tools/tool-use/memory-tool)
- Pairs with context editing (`clear_tool_uses_20250919`, client-side) and server-side compaction; recommended "multisession pattern": initializer session writes progress log + feature checklist; each session reads then updates — [same](https://platform.claude.com/docs/en/agents-and-tools/tool-use/memory-tool); [Anthropic context management](https://www.anthropic.com/news/context-management)
- Supermemory ships an integration layered on the Claude memory tool — [Supermemory docs](https://supermemory.ai/docs/integrations/claude-memory.md)

**Claude Code auto memory (Oct 2026 docs)**
- Two systems: CLAUDE.md/AGENTS.md (user-written instructions) and auto memory (Claude-written learnings); both loaded every session; auto memory loads only the first 200 lines or 25KB of `MEMORY.md`; topic files loaded on demand; four note kinds recorded as `type` frontmatter; stored at `~/.claude/projects/<project>/memory/`, shared across worktrees of the same repo; machine-local; configurable via `autoMemoryEnabled`, `autoMemoryDirectory`, `CLAUDE_CODE_DISABLE_AUTO_MEMORY=1`; Claude Code warns/errors when the index exceeds limits; subagents can have their own memory via a `memory` field — [Claude Code memory docs](https://code.claude.com/docs/en/memory)

**MCP memory servers**
- Common tool names: `add_memory`, `search_memory`, `update_memory`, `remove_memory` (plus short-term session scratchpad tools) — [Glama Memory MCP](https://glama.ai/mcp/servers/oic05j7hm8); Mem0 official MCP: `add_memory` and `search_memories` with filters — [Mem0 MCP](https://glama.ai/mcp/servers/@mem0ai/mem0-mcp)
- Typical parameters: content, type (preference/context/decision/learning), tags, namespace; search over content and tags with type/tag/namespace filters — [Glama Memory MCP](https://glama.ai/mcp/servers/v8iw2e2qmj)
- 113+ MCP clients incl. Claude Code (secondary, Jun 2026) — [aimemory.pro](https://aimemory.pro/blog/mcp-memory-server-guide)
- Mem0 OpenMemory MCP: local storage with review dashboard — [Mem0](https://mem0.ai/blog/state-of-ai-agent-memory-2026)

**Context-budget / profile injection**
- Honcho injects user representation + peer card + session summary "truncated to fit contextTokens budget" — [Hermes Honcho docs](https://hermes-agent.nousresearch.com/docs/user-guide/features/honcho)
- Mastra OM: stable append-only observation log prefix enables prompt caching; token-threshold triggers — [Mastra](https://mastra.ai/research/observational-memory)
- Supermemory SMFS: memory as a mounted filesystem (`/memory`) inside a container — [Supermemory SMFS](https://supermemory.ai/docs/smfs/providers/cloudflare.md)
- Anthropic: keep `view` output capped and let Claude page with `view_range` — [memory tool docs](https://platform.claude.com/docs/en/agents-and-tools/tool-use/memory-tool)

**Consumer memory portability**
- Claude import memory (reported Mar 1 2026): paste a prompted export from ChatGPT/Gemini/Grok; memories viewable under Settings > Capabilities — [Medium guide](https://medium.com/@unicodeveloper/how-to-import-memory-into-claude-from-chatgpt-gemini-or-grok-step-by-step-guide-c78ac8f7e4a4); [TechCabal](https://techcabal.com/2026/05/27/4-easy-steps-to-move-your-chatgpt-memory-to-claude/)

### Inferences
- The de-facto "lowest common denominator" across Anthropic, Letta (MemFS git-backed markdown), Supermemory SMFS and Claude Code is a *filesystem of markdown/text files with an index file*. ShogunAgent can expose its derived state as a virtual filesystem (`/memories/people/<name>.md`, `/memories/projects/<id>.md`, `/memories/open_loops.md`) and immediately work with the Anthropic memory tool without custom integration, while MCP `search_memory`/`get_context` serve retrieval-centric agents.
- An index file capped at ~200 lines/25KB (Claude Code's limit) is a useful design constraint: ShogunAgent's "profile/summary" tier should fit that budget, with detail in on-demand topic files.
- Since the Anthropic API injects an "assume interruption, write progress to memory" protocol, agents will write *task state* into memory constantly; ShogunAgent should separate agent-scratch namespaces from user world-model facts (authority-conditioned mutability) to avoid agent notes polluting the user model.

### Gaps
- No official OpenAI documentation retrieved on ChatGPT memory internals or Responses API state (`previous_response_id`) semantics/pricing.
- No formal MCP specification or community standard for memory tools exists in retrieved sources; naming (search_memory vs search_memories) varies per server.
- No source on streaming context injection APIs (server-pushed memory updates mid-turn).
