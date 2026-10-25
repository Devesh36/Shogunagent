# Competitive Landscape: Memory / Context Layer for AI Agents (excluding supermemory) — as of October 2026

Research date: 2026-10-25. Scope: every meaningful "memory / context layer for AI agents" competitor other than supermemory (covered by a separate researcher), plus memory features built into the OpenAI / Anthropic / Google / AWS / Microsoft platforms. All facts carry inline sources and, where available, dates. Vendor-authored comparison blogs (Vectorize, Cognee, MemoryLake, Mnemoverse, Atlan, etc.) are flagged as such; their numbers are self-interested.

Method note: GitHub API access was blocked in this sandbox, so star counts come from dated third-party pages and vendor pages and are marked with their date. Several official pricing pages were fetched directly on 2026-10-25 (mem0, Zep, Letta, Cognee, Pieces, AWS AgentCore, Anthropic memory tool, Claude Code memory docs, Weaviate Engram GA post) and are the most reliable numbers here.

---

## Key Question 1: Who are the competitors and what do they offer? (comparison table + per-company profiles)

### Takeaway
The "developer memory API" layer is crowded with ~10 funded vendors plus five hyperscaler/platform offerings, converging on the same API shape (add/ingest → async extraction → search/retrieve → optional graph/profile, almost all with an MCP server) and on usage-based pricing. Only three startups have raised more than $10M (Mem0 $24M, Engram $98M, Recall.ai $38M for meeting capture), and several 2024-era players have exited or shut down in 2025–26 (Limitless→Meta, Rewind capture disabled, Graphlit winding down, Khoj Cloud closed, Dot shut down).

### Comparison table (October 2026)

| Vendor / product | What it is | API shape (MCP?) | Storage model | Pricing (quoted) | License | Funding / traction | Target segment | Key weakness |
|---|---|---|---|---|---|---|---|---|
| **Mem0** (Mem0 Inc.; OpenMemory MCP) | Managed + OSS memory layer; extract/consolidate/retrieve facts per user/agent | add / search / get_all / delete, graph memory, "Dream" consolidation; OpenMemory MCP (add, search, list, delete_all) | Vector + optional graph (entity linking) | Hobby free (10k add / 1k retrieval req/mo), Starter $19/mo, Pro $249/mo (500k add / 50k retrieval, graph, Dream), Enterprise custom (on-prem, SSO) | Apache-2.0 (OSS core) | $24M total ($20M Series A led by Basis Set, Oct 2025); API calls 35M (Q1'25)→186M (Q3'25); ~41k stars (Apr 2026); "90k+ developers" claim (Jun 2026) | App developers adding personalization to chatbots/agents | Benchmark credibility disputed by Zep & Letta; platform ≠ OSS scores; flat fact store, weak temporal reasoning |
| **Zep** (Graphiti) | Managed temporal knowledge-graph memory; Graphiti OSS engine | Episodes ingest → graph search / context block; "Context MCP Server" seats; custom entity/edge types | Temporal (bi-temporal) knowledge graph + embeddings | Free 10k credits/mo; Flex $125/mo (50k credits); Flex Plus $375/mo (200k); Enterprise custom (VPC, BYO keys). 1 credit = 350 bytes ingested; retrieval & storage free | Graphiti Apache-2.0; Zep server CE deprecated Apr 2025 | ~$2.3M total raised per CB Insights (YC, Bessemer); Graphiti ~24–31k stars (2026 blogs) | Developers needing temporal/relational memory; enterprises (SOC2, HIPAA) | Small funding; expensive LLM-heavy ingestion; no self-host of Zep server (only Graphiti) |
| **Letta** (ex-MemGPT) | Stateful-agent platform (agents own editable memory blocks); Letta Cloud, Letta Code, Agent File (.af), Context Repositories (git-versioned memory) | Agents API (memory blocks, archival memory, tools); .af import/export; MCP tool support | Agent-managed memory blocks + archival vector store; git-versioned memory repos | Free (3 agents, 5 MiB/memory repo); Pro $20/mo (20 agents, 50 MiB); API plan $20/mo + $0.10/active agent/mo + $0.00015/s tool exec; Teams Pro $20/seat; Enterprise custom | Apache-2.0 | $10M seed led by Felicis (Sep 2024), ~$70M post (secondary estimate) | Developers building long-lived autonomous agents; coding agents | Framework lock-in (memory lives inside Letta agents); small team; pricing shifted toward Letta Code |
| **Cognee** | Graph + vector "memory engine" turning docs/code/conversations into a knowledge graph; Cognee Cloud | cognify pipeline (add → cognify → search); MCP; Claude Code / Codex integrations | Hybrid graph + vector; bi-temporal (enterprise) | Free (1 workspace, 1M tokens); Standard $1.00 per 1M tokens processed + $5/extra workspace; Enterprise BYOC custom | Apache-2.0 | $7.5M seed led by Pebblebed (Feb 19, 2026); ~32.7k stars (pricing page, Oct 2026) | Enterprise knowledge/code memory; EU/BYOC | ETL-heavy, LLM cost at ingest; cloud still young |
| **LangMem** (LangChain) | SDK for long-term memory on LangGraph's BaseStore (semantic / episodic / procedural) | manage_memory / search_memory tools; background extractor | Key-value namespace store with vector search (Postgres/pgvector, MongoDB, in-memory) | Free OSS; storage via LangGraph Platform pricing | MIT (LangChain) | Last release 0.0.30 (Oct 2025); ~1.3–1.7k stars | LangGraph developers | Low maintenance activity; framework-bound; no entity resolution/graph |
| **Memobase** | User-profile-based long-term memory for chat apps (structured profile + event timeline) | insert → buffer/flush → profile/context fetch; MCP via Dify plugin | Structured profile schema + events (Postgres/Redis) | Self-host free; cloud free tier + pay-as-you-go tokens (prices not verified) | Apache-2.0 | Small OSS project; no disclosed funding | Consumer chatbot/companion apps (profile memory) | Narrow (user profile); small team |
| **Honcho** (Plastic Labs) | "Personal identity platform" — dialectic user models; Honcho 3 | peers/sessions, `.chat()` dialectic query, `context()` | Representation store + reasoning layer | Ingestion $2 per 1M tokens; `context()` free; `.chat()` from $0.01/query; $5 new-account credits; startup program $1,000 credits | AGPL-3.0 | $5.35–5.4M pre-seed (Variant, White Star, Betaworks, Mozilla Ventures); CB Insights lists $18.8M total (unverified) | Companion/social AI apps needing user psychology modeling | AGPL; niche positioning; token-metered |
| **Memori** (GibsonAI / Memori Labs) | SQL-native memory engine (SQLite/Postgres/MySQL), "no vector DB" | one-line `memori.enable()` wrapping LLM calls; entities/rules/preferences as rows | Relational (SQL) | Free OSS; DigitalOcean 1-click (infra cost only); GibsonAI platform $0–49/mo | Apache-2.0 | OSS launched Jul 2025; HN launch Sep 2025; no disclosed funding | Enterprises wanting auditable, on-prem SQL memory | No published benchmarks; small community |
| **Hindsight** (Vectorize) | Biomimetic memory (retain / recall / reflect; "mental models") | retain, recall, reflect, mental models, knowledge pages; OpenAI Agents SDK integration | Hybrid (vector + graph-ish entity links) | Cloud pay-as-you-go prepaid credits (per-token retain/recall, per-call reflect, storage >30 days); rates not public in docs; enterprise invoiced | MIT | Vectorize (parent) venture-backed; 91.4% LongMemEval (Dec 2025), 94.6% claim (2026) | Developers; benchmark-driven buyers | Parent company's main business is RAG pipelines; newer product |
| **Personal.ai** | Enterprise "AI Memory Platform" (My AI, Persona Studio, Memory Core; MODEL-4 deployable on-prem) | Proprietary; persona + memory stack; no public API pricing | Proprietary personal language model + memory | Enterprise-only, scoped per operator; no public pricing | Proprietary | Older (2020-era) company; no 2026 round found | Regulated enterprises (carriers, legal) wanting private personas | Opaque; pivoted away from consumer; pricing not public |
| **Limitless** (ex-Rewind; Pendant + API) | Wearable lifelog + desktop recall; developer API (lifelogs, Ask AI, MCP server) | REST + MCP (lifelogs) | Transcripts/time-series on cloud | Acquired by Meta Dec 5, 2025; Pendant sales halted; existing users moved to free unlimited plan; support "at least a year" | Proprietary | Acquired (Reality Labs); Rewind Mac app disabled all screen/audio capture Dec 19, 2025 | Was: prosumer lifeloggers | Effectively exiting the market; API sunset risk after ~Dec 2026 |
| **Screenpipe** | Local-first 24/7 screen+mic capture → searchable context for agents | Local REST/SDK; MCP; "pipes" plugins | Local SQLite + embeddings | Free (1 device); Basic $25/mo; Business $50/seat/mo; Enterprise custom | Changed Jun 2026 from MIT to source-available "Screenpipe Commercial License" (non-commercial free) | No funding found | Developers/prosumers wanting Rewind-like local capture | License change erodes OSS trust; capture ≠ structured state |
| **Recall.ai** | Meeting-bot + desktop recording API (raw transcripts for apps) | REST (bots, recordings, transcripts) | Audio/transcript storage | $0.50/recording-hour (down from $0.70); +$0.15/hr transcription; 7 days free storage then $0.05/hr per 30 days; startups $0.25/hr first 10k hrs | Proprietary | $38M Series B (Sep 2025; Bessemer, HubSpot Ventures, Salesforce Ventures) | Meeting-intelligence vendors | Capture layer only — no memory/inference; not a memory competitor per se but an input source |
| **Pieces for Developers** | On-device Long-Term Memory (LTM-2.x) engine capturing workflow context across apps; MCP server | Pieces MCP server; LTM query by time/topic/person/tool | Local on-device store | Pro $18.99/mo; Enterprise $22.99/mo (or $69.99/quarter); 7-day trial | Proprietary | No 2026 funding news found | Individual developers | Per-seat consumer pricing; closed; unclear scale |
| **Khoj** | Self-hostable "second brain" (notes/docs/web) agent | Chat + search API; Obsidian/Emacs/WhatsApp | Vector over user docs | Khoj Cloud discontinued Apr 15, 2026 (single secondary source); self-host free | AGPL-3.0 | Pivoting to two new products (secondary source) | Self-hosters | Cloud shut down; not an agent-memory API |
| **Graphlit** | "Context layer for AI agents" — ingestion + semantic memory platform | GraphQL/REST + MCP | Hybrid graph/vector | Was Hobby $49, Starter $199, Growth $999/mo; **winding down as of Sep 2026** (signups closed) | Proprietary | Shutting down | Was: developers | Exiting market |
| **Papr** | "Predictive memory" graph+vector memory for devs and a chat app | REST/SDK; MCP | Graph + vector | Free (1k memory ops / 2.5k active memories); Developer $100/mo; Growth $500/mo; Chat $20/mo (third-party listings; official page 404) | Proprietary (SDK OSS?) | No disclosed funding | Developers | Tiny; unclear viability |
| **Basic Memory** (Basic Machines) | Local-first Markdown knowledge graph written by AI via MCP; Obsidian-compatible; optional cloud sync | MCP tools (write_note, search, build_context) | Local Markdown files + SQLite index | OSS free; cloud pricing not found | AGPL-3.0 | ~3.9–4.1k stars (2026) | Individual knowledge workers using Claude/Cursor | AGPL; no inference layer; per-user, not team |
| **Weaviate Engram** | Managed memory & context service in Weaviate Cloud (extract-transform-commit pipelines; per-project/user/property scopes) | REST + Python SDK; async pipelines; templates (personalization, continual learning; multi-agent state "coming") | Hybrid (vector + BM25) over structured memories | Free tier 1,000 pipeline runs/mo; paid from $45/mo (press); GA Jun 3, 2026 | Proprietary (DB is OSS) | Weaviate (well-funded vector DB vendor) | Existing Weaviate customers; enterprise | Late entrant; tied to Weaviate Cloud |
| **Pinecone Nexus** | "Semantic knowledge layer / knowledge engine" preparing task-optimized representations for agents | Managed API | Vector-first | GA Aug 6, 2026 (single non-primary source); no pricing found | Proprietary | Pinecone | Enterprise RAG customers | Knowledge layer, not per-user memory; weak primary sourcing |
| **Engram (startup, not Weaviate)** | "Learned memory layer for AI" — per-customer models that pre-study org data; claims frontier-level answers at up to 100× fewer tokens | Enterprise deployments (Microsoft 365, Notion, Harvey testing) | Learned/parametric memory | Not disclosed | Proprietary | **$98M out of stealth, Jun 26, 2026** (General Catalyst, Modern co-lead; Sequoia, Kleiner Perkins; ~$600M valuation per one outlet) | Legal, healthcare, financial services enterprises | Pre-revenue, research-heavy, 13 people |
| **Mastra Observational Memory** | Text-based observer/reflector compression inside Mastra agent framework | Framework feature, no DB | Text (no vector/graph) | OSS free | Apache-2.0 (Mastra) | 94.87% LongMemEval (gpt-5-mini) — vendor-reported, Feb 2026 | TypeScript agent developers | Framework-bound; context compression more than cross-session store |
| **ByteRover** | Shared, version-controlled memory layer for coding agents/teams | MCP + CLI; "agent2memory" | Versioned memory store | Not found | Proprietary | Pre-seed (Ascend Vietnam Ventures, January Capital); 7 employees; claims 92.2% LoCoMo | Dev teams using Cursor/Claude Code/Copilot | Tiny; unverified claims |
| **Memory Store (YC 2026)** | Shared context layer for a team's agents from meetings, Slack, Claude sessions (grouped by project/person/decision), via MCP | MCP | — | Not listed | — | $500K seed (YC, Jan 2026) | Teams | Very early |
| **Tabnine Context Engine** | Agent-neutral permission-aware memory layer across codebases, docs, policies for Cursor/Copilot/Claude Code | Enterprise product | — | Enterprise | Proprietary | Tabnine | Enterprise dev orgs | Not standalone memory API |
| **Synvo AI** | On-prem/on-device "Enterprise Memory Layer" (NTU spinout, Singapore) | — | — | — | — | $1M seed (Jun 2026, Fuel Ventures Asia) | Enterprises in APAC | Very early |
| **MemoryLake / Mnemoverse / EverMind (EverMemOS)** | Smaller 2026 entrants publishing SEO comparison content | MCP/APIs | Various | Not reliably found | Mixed | No verified funding (MemoryLake claims Hillhouse/Lightspeed backing on its own blog) | Developers | Marketing-heavy, unverified |

### Platform vendors (free/bundled memory) — summary row

| Platform | What ships | Price | Notes |
|---|---|---|---|
| **OpenAI** | ChatGPT memory ("Dreaming" rebuild, Jun 4 2026); Agents SDK Sessions (client-side) + `conversation_id` / `previous_response_id` (server-side, Responses API); Responses API itself is stateless re: long-term memory | Bundled with model tokens | No cross-session semantic memory API for developers; Assistants API retired H1 2026 |
| **Anthropic** | Memory tool (`memory_20250818`, client-side file ops, all Claude 4+ models, no beta header); context editing; server-side compaction; Claude Code auto memory (MEMORY.md, machine-local); Managed Agents memory (public beta Apr 23, 2026) + "Dreaming" consolidation (research preview, May 6, 2026); consumer Claude memory (Aug 2026 update) | Memory tool is free beyond tokens; storage is yours | Memory tool is a *protocol*, not a store — leaves room for storage vendors |
| **Google** | Vertex AI Agent Engine Memory Bank (LLM-generated memories, scopes, revisions, ADK quickstart); Gemini app Past Chats (free users Feb 2026), Personal Intelligence, Gemini Live memory (Jun 2026) | Memory Bank: $0.25/1k memories stored/mo + $0.50/1k retrieved (Jan 2026 structure); new structure from Sep 1, 2026: $0.30/GiB-month storage + vCPU-hour-based reads/writes (1 vCPU-h ≈ $0.085 per 3M reads) | Pricing changed twice in 2026 |
| **AWS** | Bedrock AgentCore Memory: short-term (events) + long-term (built-in or self-managed extraction strategies) | STM: $1.00/GB ingested, $0.20/GB retrieved, $0.10/GB-month (from Oct 6, 2026; was $0.25/1k events); LTM: $0.75/1k records/mo built-in or $0.25/1k self-managed; $0.50/1k retrievals; worked example $18.97/mo | Most granular public pricing in category |
| **Microsoft** | Foundry Agent Service Memory (user / session / procedural memory; Memory Store API; TTL) — public preview as of Aug 2026; M365 Copilot memory & "communication memory" (on by default, Purview-auditable) | Preview; Copilot bundled at $30/user/mo | Preview limits: 100 scopes/store, 10k memories/scope, 1k search rpm (third-party) |

### Cited Findings — per-company profiles

**Mem0 (Mem0 Inc.)**
- Raised ~$24M total: $3.9M seed (Kindred Ventures) and $20M Series A led by Basis Set Ventures with Peak XV, GitHub Fund and Y Combinator; reported API calls grew from 35M in Q1 2025 to 186M in Q3 2025; OSS passed 41k GitHub stars (Oct 28, 2025) — [TechCrunch](https://techcrunch.com/2025/10/28/mem0-raises-24m-from-yc-peak-xv-and-basis-set-to-build-the-memory-layer-for-ai-apps/); [FinSMEs](https://www.finsmes.com/2025/10/mem0-raises-24m-in-series-a-funding.html)
- Valuation undisclosed; Caplight estimates $150M (unverified) — [Caplight](https://www.caplight.com/company/mem0)
- Pricing (fetched Oct 25, 2026): Hobby free (10,000 add requests + 1,000 retrieval requests/mo, 1 project), Starter $19/mo (50k add / 5k retrieval), Pro $249/mo (500k add / 50k retrieval, unlimited projects, graph memory/entity linking, "Dream" memory consolidation), Enterprise custom (on-prem, SSO, audit logs, SLA); usage-based pricing available — [mem0.ai/pricing](https://mem0.ai/pricing)
- OpenMemory MCP (announced ~May 13, 2025): local, Docker-based MCP memory server exposing add / search / list / delete_all tools to Cursor, Claude Desktop, Windsurf, Cline; memory stays on the user's machine; hosted no-Docker variant also offered — [Mem0 blog](https://mem0.ai/blog/introducing-openmemory-mcp); [Mem0 docs](https://docs.mem0.ai/openmemory)
- Research paper (arXiv 2504.19413, Apr 28, 2025) claims 26% relative improvement in LLM-as-judge over OpenAI's memory on LoCoMo, 91% lower p95 latency and >90% token savings vs full-context; includes graph variant — [arXiv](https://arxiv.org/abs/2504.19413)
- Claims "90,000+ developers" (Jun 2026) and positions itself as "exclusive memory provider for AWS Agent SDK" — company-reported, not confirmed by an AWS primary source — [rywalker.com research](https://rywalker.com/research/mem0); AWS China blog shows Mem0 paired with Bedrock — [AWS China blog](https://aws.amazon.com/cn/blogs/china/complementary-strengths-mem0-memory-framework-and-amazon-cloud-technologys-enterprise-level-ai-practices)
- Weakness: platform scores (92.5 LoCoMo / 94.4 LongMemEval) come from the managed platform, not the OSS SDK; an independent Vectorize run measured Mem0 OSS at 49.0 on LongMemEval — [Atlan (vendor-adjacent)](https://atlan.com/know/zep-vs-mem0/); [iotdigitaltwinplm comparison](https://iotdigitaltwinplm.com/mem0-vs-letta-vs-zep-agent-memory-frameworks-comparison-2026/)

**Zep / Graphiti**
- Pricing (fetched Oct 25, 2026): Free 10,000 credits/mo (2 projects, 1 Context MCP Server seat); Flex $125/mo for 50,000 credits (overage $25/10k, 600 rpm, 5 projects, 5 MCP seats); Flex Plus $375/mo for 200,000 credits (1,000 rpm, observations, custom extraction, webhooks); Enterprise custom (SOC 2 Type II, HIPAA BAA, VPC or BYO-keys deployment). Credits: 1 credit per 350 bytes of Episode ingested; retrieval, storage, threads, users and graph storage are free — [getzep.com/pricing](https://www.getzep.com/pricing)
- Graphiti is the Apache-2.0 temporal knowledge-graph engine; Zep Community Edition server was deprecated April 2025, so self-hosting means running Graphiti directly — [renezander guide](https://renezander.com/guides/zep-vs-graphiti/); [Cognee vendor page](https://www.cognee.ai/zep-alternatives)
- Graphiti GitHub stars reported at ~24–31k across 2026 sources — [vectorize.io (vendor)](https://vectorize.io/articles/mem0-vs-zep); "240+ customer deployments" claim appears on one vendor-adjacent page only — [ai-deck.app](https://ai-deck.app/blog/graphiti/)
- Paper (arXiv 2501.13956, Jan 2025): 94.8% vs MemGPT's 93.4% on DMR; "up to 18.5%" LongMemEval improvement (84.5% vs 66.0% for MemGPT in the table) with 90% latency reduction — [arXiv HTML](https://arxiv.org/html/2501.13956v1); [Zep blog](https://blog.getzep.com/zep-a-temporal-knowledge-graph-architecture-for-agent-memory)
- Funding: CB Insights lists Zep AI total raised ~$2.3M (last $500K in Jan 2024; investors YC, Bessemer) — [CB Insights compare](https://www.cbinsights.com/compare/letta-vs-zep-2). No 2025–26 round found.

**Letta (ex-MemGPT)**
- $10M seed led by Felicis (Sep 2024) with Sunflower Capital, Essence VC, angels incl. Jeff Dean, Clem Delangue — [The SaaS News](https://www.thesaasnews.com/news/letta-secures-10-million-in-seed-round/); [CB Insights](https://www.cbinsights.com/company/letta/financials); ~$70M post-money cited by a secondary review — [PromptQuorum](https://www.promptquorum.com/power-local-llm/letta-review)
- Pricing (fetched Oct 25, 2026; letta.com/pricing redirects to Letta Code pricing): Free ($0; 3 stateful agents; 5 MiB per memory repository; BYOK); Pro $20/mo (20 agents, 50 MiB/repo, Letta Auto quota, PAYG overage); API Plan $20/mo + $0.10 per active agent/month + $0.00015/sec tool execution; Teams Pro $20/seat/mo; Enterprise custom (RBAC, SAML/OIDC). Remote MCP tools and client-side tools incur no credit cost — [docs.letta.com pricing](https://docs.letta.com/letta-code/pricing)
- Agent File (.af): open standard for serializing stateful agents (system prompt, editable memory, tools, LLM config), portable between self-hosted and Letta Cloud — [Letta docs](https://docs.letta.com/guides/agents/agent-file); [Letta blog](https://letta.com/blog/agent-file)
- Context Repositories (Feb 12, 2026): memory for Letta Code rebuilt on git — every memory change auto-committed; agents keep a local clone of their memory repo — [Letta blog](https://www.letta.com/blog/context-repositories); Letta Code product — [Letta blog](https://www.letta.com/blog/letta-code)
- License Apache-2.0; self-host free — [vectorize.io (vendor)](https://vectorize.io/articles/mem0-vs-letta)
- Letta's own LoCoMo evaluation: plain-file memory searched with grep/open scored 74.0% on GPT-4o-mini, above Mem0's reported 68.5% — [beri.net summary](https://www.beri.net/article/zep-vs-mem0-vs-postgres-agent-memory-store-buy-last-2026) (primary: letta.com/blog/benchmarking-ai-agent-memory/)

**Cognee**
- $7.5M seed (Feb 19, 2026) led by Pebblebed with 42CAP, Vermilion Ventures, angels from DeepMind/n8n/Snowplow; Berlin-based — [Cognee blog](https://www.cognee.ai/blog/cognee-news/cognee-raises-seven-million-five-hundred-thousand-dollars-seed); [Startup.eu](https://www.startup.eu/investments/cognee-7-5m-seed-02-2026)
- Pricing (fetched Oct 25, 2026): Free $0 (1 workspace, 1M tokens, unlimited members/API calls, Claude Code/Codex/MCP integrations); Standard $1.00 per 1M tokens processed, $5 per additional workspace, Slack/Notion/Linear/Drive connectors, code indexing; Enterprise BYOC with bi-temporal memory, conflict resolution, provenance, 6/12/24-month engagement packages; "Cloud runs on gpt-oss-120b"; GitHub ~32.7k stars shown on page — [cognee.ai/pricing](https://www.cognee.ai/pricing)
- License Apache-2.0; 17.7k stars as of Jun 2026 per third-party profile (conflicts with 32.7k on vendor page Oct 2026 — growth or counting difference) — [rywalker.com](https://rywalker.com/research/cognee)

**LangMem (LangChain)**
- Three memory types (semantic, episodic, procedural) on LangGraph BaseStore; background extraction; works with any store implementing the LangGraph store interface (Postgres/pgvector, MongoDB, in-memory) — [Atlan](https://atlan.com/know/long-term-memory-langchain-agents/); [Cognee vendor page](https://www.cognee.ai/langchain-memory)
- Latest PyPI release 0.0.30 (Oct 2025); subsequent activity is maintenance; not deprecated; survived LangChain 1.0 transition — [Cognee vendor page](https://www.cognee.ai/langchain-memory); [vectorize.io (vendor)](https://vectorize.io/articles/langchain-memory-alternatives)
- Stars ~1.3–1.7k (sources vary) — [rywalker.com](https://rywalker.com/research/langmem)

**Memobase**
- Apache-2.0 OSS "user profile-based long-term memory for AI chatbot applications"; built-in per-user buffer to batch-process chats — [GitHub](https://github.com/memodb-io/memobase)
- Cloud: free tier + pay-as-you-go token pricing (third-party listing; "~5x cheaper" claim unverified) — [dreaming.press](https://dreaming.press/stack/memobase); Dify marketplace plugin exists — [Dify](https://marketplace.dify.ai/plugin/acane0320/memobase)

**Honcho (Plastic Labs)**
- Pre-seed $5.35M (press) / $5.4M (company) led by Variant, White Star Capital, Betaworks with Mozilla Ventures, Seed Club, Greycroft, Differential — [Dealroom](https://app.dealroom.co/news/feed/plastic-labs-raises-5-35m-launches-honcho); [Plastic Labs blog](https://plasticlabs.ai/blog/posts/Launching-Honcho;-The-Personal-Identity-Platform-for-AI); CB Insights shows $18.8M total (unverified) — [CB Insights](https://www.cbinsights.com/company/plastic-labs)
- Honcho 3 pricing: ingestion $2 per 1M tokens; `context()` free; `.chat()` queries priced per query from $0.01; $5 free credits; startup program $1,000 credits + 12 months subsidized — [Honcho 3 announcement](https://plasticlabs.ai/blog/posts/Honcho-3); [honcho.dev](https://honcho.dev/); [startup program](https://plasticlabs.typeform.com/startups)
- License AGPL-3.0 (site metadata) — [honcho.dev](https://honcho.dev/)

**Memori (GibsonAI / Memori Labs)**
- OSS SQL-native memory engine (SQLite/Postgres/MySQL), stores entities/rules/preferences as rows; integrates OpenAI/Anthropic/LiteLLM/LangChain; claims 80–90% cost savings vs vector DBs (vendor claim) — [Memori Labs blog](https://www.memorilabs.ai/blog/introducing-memori-the-open-source-memory-engine-for-ai-agents); HN launch "We went back to SQL" — [Hacker News](https://news.ycombinator.com/item?id=45329322)
- OSS release listed Jul 24, 2025; Apache-2.0; DigitalOcean one-click app storing memories in your Droplet's Postgres — [jimmysong landscape](https://landscape.jimmysong.io/projects/memori/); [DigitalOcean](https://docs.digitalocean.com/products/marketplace/catalog/memori/)

**Hindsight (Vectorize)**
- Launched Dec 2025 with 91.4% LongMemEval (validated with Washington Post and Virginia Tech researchers); OSS MIT; paper "Hindsight is 20/20" (arXiv 2512.12818) — [PR Newswire](https://www.prnewswire.com/news-releases/vectorize-breaks-90-on-longmemeval-with-open-source-ai-agent-memory-system-302643146.html); [Vectorize blog](https://vectorize.io/blog/introducing-hindsight-agent-memory-that-works-like-human-memory); [arXiv](https://arxiv.org/pdf/2512.12818); site now claims 94.6% LongMemEval-S vs next-best 74.0% — [hindsight.vectorize.io](https://hindsight.vectorize.io/)
- Hindsight Cloud: pay-as-you-go prepaid credits (per-token retain/recall, per-call reflect and mental-model/knowledge-page refresh, storage beyond 30 days) or enterprise invoicing; HTTP 402 when credits exhausted; actual rates not listed on docs page fetched — [Billing docs](https://docs.hindsight.vectorize.io/billing); [Hindsight Cloud](https://hindsight.vectorize.io/cloud-enterprise/hindsight-cloud)

**Personal.ai**
- Positions as "The AI Memory Platform that remembers, connects, and evolves" with products My AI, Persona Studio, Memory Core; MODEL-4 deployable on customer network under a single agreement per operator; no public pricing (contact sales) — [personal.ai/pricing](https://www.personal.ai/pricing)

**Rewind / Limitless (Pendant + API)**
- Meta acquired Limitless (announced Dec 5, 2025); team into Reality Labs; Pendant sales halted; existing customers moved to free unlimited plan with "at least one year" support and data export — [TechInformed](https://techinformed.com/meta-acquires-limitless-pendant-users-moved-to-free-unlimited-plan/); [mlq.ai](https://mlq.ai/news/meta-acquires-ai-wearables-startup-limitless-ending-sales-of-pendant-device/)
- Developer API (lifelogs, Ask AI history, MCP server) has no published commitment beyond ~Dec 2026 — [api-evangelist/limitless](https://github.com/api-evangelist/limitless)
- Rewind Mac app disabled all screen and audio capture on Dec 19, 2025 — [screenpi.pe blog (competitor)](https://screenpi.pe/blog/rewind-ai-alternative-2026); [luci.memories.ai (competitor)](https://luci.memories.ai/blog/rewind-ai-shut-down-on-device-replacement)
- Meta reportedly developing its own AI pendant (May 30, 2026) — [TechCrunch](https://techcrunch.com/2026/05/30/meta-is-reportedly-developing-an-ai-pendant/)

**Screenpipe**
- Pricing (2026): Free (1 device); Basic $25/mo or $250/yr; Business $50/seat/mo or $500/seat/yr; Enterprise custom — [aicoolies pricing](https://aicoolies.com/pricing/screenpipe); [ai.dosa.dev](https://ai.dosa.dev/tools/screenpipe)
- License changed Jun 2026 from MIT to source-available "Screenpipe Commercial License" (personal/non-commercial/edu/research free; prior versions remain MIT) — [theaiagentindex](https://theaiagentindex.com/agents/screenpipe)

**Recall.ai**
- 2026 pricing: $0.50 per recording hour (from $0.70); transcription +$0.15/hr; no platform fee; 7 days free storage then $0.05/recording-hour per 30 days; first 5 hours free; startup rate $0.25/hr for first 10,000 hours — [Recall.ai blog](https://www.recall.ai/blog/new-recall-ai-pricing-for-2026); [Startup program](https://www.recall.ai/startups)
- $38M Series B (Sep 2025; Bessemer, HubSpot Ventures, Salesforce Ventures, Ridge, YC, RTP Global) — [meetingstack.io](https://meetingstack.io/tools/recall-ai/) (secondary)

**Pieces for Developers**
- Pricing (fetched Oct 25, 2026): Pro $18.99/mo (yearly saves 25%); Enterprise $22.99/mo or $69.99/quarter; 7-day trial; LTM "saved on-device"; Pro includes "Unlimited MCP" via Pieces MCP Server carrying history into Claude, Cursor, Codex, Antigravity — [pieces.app/pricing](https://pieces.app/pricing); LTM-2.7 and MCP docs — [docs.pieces.app](https://docs.pieces.app/products/mcp)

**Khoj**
- AGPL-3.0; self-hostable; GitHub — [khoj repo](https://github.com/ai-khoj/khoj); [everydev listing](https://www.everydev.ai/tools/khoj)
- Khoj Cloud (paid hosted) reportedly discontinued Apr 15, 2026; self-hosting now the only supported path; company shifting to two new products — single secondary source — [PromptQuorum (DE)](https://www.promptquorum.com/de/power-local-llm/khoj-ai-second-brain-review)

**Graphlit**
- Winding down as of Sep 2026; new signups closed; existing customers transitioning; data scheduled for deletion — [hydradb review](https://hydradb.com/blog/graphlit-reviews); [aitoolsatlas](https://aitoolsatlas.ai/tools/graphlit); prior pricing Hobby $49 / Starter $199 / Growth $999 per month — [hydradb](https://hydradb.com/blog/graphlit-reviews); official site still markets "Context layer for AI agents" — [graphlit.com](https://www.graphlit.com/)

**Papr**
- Third-party listings: Starter (Chat) $20/mo; Starter (Developers) $100/mo; Growth (Developers) $500/mo; free tier 1,000 memory ops / 2,500 active memories — [SaaSworthy](https://www.saasworthy.com/product/papr-ai/pricing); [ai-plaza](https://ai-plaza.io/tool/paprai); Crunchbase shows no round amounts — [Crunchbase](https://www.crunchbase.com/organization/papr-01fd); official pricing URL returned 404 on Oct 25, 2026

**Basic Memory (Basic Machines)**
- AGPL-3.0 local-first Markdown knowledge graph via MCP (Claude, Cursor, Codex), Obsidian-compatible; hosted cloud for sync; ~4,090 stars (GitHub org page) / 3,866 (MCP Toplist) — [GitHub](https://github.com/basicmachines-co/basic-memory); [Basic Machines blog](https://basicmachines.co/blog/what-is-basic-memory/); [MCP Toplist](https://mcptoplist.com/server/io.github.basicmachines-co%2Fbasic-memory)

**Weaviate Engram**
- GA Jun 3, 2026 in Weaviate Cloud: async extract-transform-commit pipelines, dedup/preference reconciliation, per-project/per-user/per-property scopes, hybrid semantic+keyword retrieval; templates for personalization and continual learning; multi-agent state templates "coming in following weeks"; free tier — [Weaviate blog](https://weaviate.io/blog/engram-generally-available); press: free tier 1,000 pipeline runs/mo, paid from $45/mo — [Open Source For You](https://www.opensourceforu.com/2026/06/weaviate-launches-engram-to-provide-production-grade-memory/); [BigDATAwire](https://www.hpcwire.com/bigdatawire/this-just-in/weaviate-makes-engram-generally-available-giving-ai-agents-production-grade-memory-2/)

**Pinecone Nexus**
- Described as "semantic knowledge layer between enterprise data and agents"; GA Aug 6, 2026 with 47.4% on Sierra τ-Knowledge and 77–80% cost-per-task reduction — single non-primary (Japanese blog) source; no pricing found — [techjacksolutions](https://techjacksolutions.com/ai-brief/google-and-pinecone-both-bet-on-the-same-missing-layer-this/); [note.com](https://note.com/_kihonushi/n/n7967d341e94d?hl=en)

**Engram (startup — distinct from Weaviate Engram)**
- Out of stealth Jun 26, 2026 with $98M (General Catalyst, Kleiner Perkins, Sequoia, Factory, Modern, Amplify, Neo; angels Assaf Rappaport, Andrej Karpathy, Pieter Abbeel); founders Dan Biderman (CEO), Sabri Eyuboglu (CTO), Jessy Lin, Jack Morris, Scott Linderman, Chris Ré; "learned memory layer" pre-studying org data; claims frontier parity at up to 100× fewer tokens; early partners Microsoft (M365), Notion, Harvey; targets legal/healthcare/financial — [Pulse 2.0](https://pulse2.com/engram-launches-with-98-million-to-build-ai-memory-layer-for-enterprises/); $600M valuation and 13-person team — [VKTR](https://www.vktr.com/ai-news/engram-launches-with-98m-to-give-enterprise-ai-memory/)

**Mastra Observational Memory**
- 94.87% LongMemEval with gpt-5-mini (93.27% gemini-3-pro-preview; 84.23% GPT-4o, beating oracle and Supermemory's prior best); text-based observer/reflector, no vector/graph DB, prompt-cache friendly — [Mastra blog](https://mastra.ai/blog/observational-memory); [The Decoder](https://the-decoder.com/mastras-open-source-ai-memory-uses-traffic-light-emojis-for-more-efficient-compression/)

**Coding-agent and team-memory entrants (2026)**
- ByteRover: shared memory layer for coding agents with memory version control; pre-seed (Ascend Vietnam Ventures, January Capital); 7 employees; claims 92.2% LoCoMo — [everydev](https://www.everydev.ai/developers/byterover); [docs](https://docs.byterover.dev)
- Memory Store (YC, Jan 2026, $500K): shared context layer for a team's agents from meetings (Fathom, Granola), Slack, Claude sessions, grouped by project/person/decision, exposed over MCP; founders Diwank Singh, Ishita Jindal — [neuronfeed](https://neuronfeed.com/startups/memory-store)
- funes (Hugging Face): builds memory from local Claude Code/Codex session logs and publishes to a user-owned HF dataset — [HF blog](https://huggingface.co/blog/funes)
- Tabnine Context Engine: agent-neutral, permission-aware memory across codebases/docs/policies for Cursor, Copilot, Claude Code; Futurum (Jun 18, 2026) asks whether shared memory becomes a standard RFP requirement by 2027 and warns platform vendors may bundle context+orchestration — [Futurum Group](https://futurumgroup.com/insights/will-shared-memory-become-the-missing-link-for-enterprise-scale-multi-agent-ai/)
- Synvo AI (Singapore, NTU spinout): $1M seed (Jun 2026) for on-prem/on-device Enterprise Memory Layer — [building.it.com](https://building.it.com/articles/synvo-ai-seed-funding)
- MemoryLake (claims ex-Alibaba Cloud founders; Hillhouse/Lightspeed backing — self-reported), Mnemoverse, EverMind/EverMemOS (hackathon "backed by OpenAI & AWS", $80k prizes) publish rank-themselves-first comparisons — [MemoryLake](https://www.memorylake.ai/en/blogs/10-best-ai-memory-tools-for-coding-agents-in-2026); [Mnemoverse landscape](https://mnemoverse.com/docs/research/ai-memory-landscape-2026); [EverMind](https://evermind.ai/activities)

**Exits / shutdowns in adjacent "personal context" space**
- Dot (New Computer) shut down Oct 5, 2025 — [TechCrunch](https://techcrunch.com/2025/09/05/personalized-ai-companion-app-dot-is-shutting-down)

### Inferences
- The modal startup offering is now: Apache-2.0 OSS core + managed cloud + MCP server + usage-metered pricing. Differentiation has collapsed to storage model (flat facts vs temporal graph vs SQL vs text compression) and benchmark claims.
- Capture-layer products (Rewind/Limitless, Screenpipe, Recall.ai, Pieces) are consolidating or exiting; the survivors (Recall.ai, Screenpipe) sell raw capture, not inferred state. Nobody in the table ships "state of work" inference (people, projects, commitments, open loops) as the primary product.
- Enterprise money in 2026 moved to "learned memory" (Engram $98M) and hyperscaler-native memory rather than to another vector-store wrapper.

### Gaps
- Exact current GitHub star counts could not be pulled (GitHub API blocked in sandbox); figures are dated third-party numbers.
- No official pricing found for Papr (404), Memobase cloud, Hindsight per-token rates, Basic Memory cloud, Pinecone Nexus, Engram (startup), Personal.ai.
- Zep's funding beyond CB Insights' $2.3M figure is unverified; no 2025–26 round found.
- Graphlit shutdown date and Khoj Cloud shutdown rest on secondary review sites only.

---

## Key Question 2: Which segments are crowded vs. open?

### Takeaway
"Per-user conversational memory API for app developers" is saturated (Mem0, Zep, Letta, Cognee, Memobase, Honcho, Hindsight, Weaviate Engram, plus AWS/Google/Microsoft managed memory). Open or thinly populated: (a) inferred "state of work" (people/projects/commitments/open loops) as a service — no vendor found; (b) cross-agent shared/team memory and agent-to-agent handoff — only seed-stage entrants (Memory Store $500K, ByteRover pre-seed, Tabnine as a feature); (c) local-first/private enterprise memory — mostly OSS self-host (Memori, Basic Memory, Cognee BYOC) with weak commercial offerings; (d) memory for coding agents — active but fragmented, and partly absorbed by Claude Code auto memory / Letta Context Repositories.

### Cited Findings
- Dedicated developer-memory vendors listed as "main players" in Apr 2026: Mem0, Letta, Cognee, Supermemory, Zep — [Mnemoverse landscape (vendor)](https://mnemoverse.com/docs/research/ai-memory-landscape-2026)
- Weaviate Engram GA (Jun 2026) explicitly targets "multi-agent systems that need shared, scoped state" but its multi-agent templates were still "coming in the following weeks" at GA — [Weaviate blog](https://weaviate.io/blog/engram-generally-available)
- Futurum (Jun 18, 2026): enterprises "won't standardize on one agent, model, or IDE," so agent-neutral shared memory (Tabnine) is positioned against Copilot/Cursor/Claude Code proprietary context; open question whether Microsoft/Google/Anthropic open their ecosystems to third-party context layers — [Futurum Group](https://futurumgroup.com/insights/will-shared-memory-become-the-missing-link-for-enterprise-scale-multi-agent-ai/)
- Memory Store (YC, $500K, Jan 2026) is the only product found whose core is "shared context layer for a team's AI agents" from meetings/Slack/Claude sessions grouped by project, person and decision — [neuronfeed](https://neuronfeed.com/startups/memory-store)
- Coding-agent memory: Claude Code auto memory is machine-local ("Files are not shared across machines or cloud environments"), per-repository, MEMORY.md index loads first 200 lines / 25KB; subagents can keep their own auto memory — [Claude Code docs](https://code.claude.com/docs/en/memory); Letta Context Repositories add git-versioned memory for Letta Code (Feb 12, 2026) — [Letta blog](https://www.letta.com/blog/context-repositories); ByteRover, funes, Beacon, MemoryLake chase cross-harness portability — [HF funes](https://huggingface.co/blog/funes); [MemoryLake (vendor)](https://www.memorylake.ai/en/blogs/10-best-ai-memory-tools-for-coding-agents-in-2026)
- Local-first / private: Mem0 OpenMemory MCP keeps memory on the user's machine — [Mem0 blog](https://mem0.ai/blog/introducing-openmemory-mcp); Basic Memory is local Markdown (AGPL) — [GitHub](https://github.com/basicmachines-co/basic-memory); Memori stores in your own SQL DB — [Memori Labs](https://www.memorilabs.ai/blog/introducing-memori-the-open-source-memory-engine-for-ai-agents); Cognee Enterprise is BYOC — [Cognee pricing](https://www.cognee.ai/pricing); Zep Enterprise offers VPC deployment — [Zep pricing](https://www.getzep.com/pricing); Synvo AI targets on-prem/on-device enterprise memory ($1M seed) — [building.it.com](https://building.it.com/articles/synvo-ai-seed-funding); Pieces LTM is on-device — [pieces.app](https://pieces.app/pricing)
- "State of work" inference: a targeted search for products inferring open loops/commitments from email and calendar returned only generic agent patterns ("state of the world snapshots", work-log files) and platform assistants (Gemini agent coordinating calendar/email, Copilot drafting invites) — no dedicated product — [Google Cloud blog](https://cloud.google.com/blog/products/ai-machine-learning/welcome-to-gemini-at-work-2026)
- Microsoft's "communication memory" summarizes a user's accessible emails, chats and meetings into memories with source links (GA from early Sep 2025, on by default for Copilot-licensed users) — the closest platform analogue to state-of-work inference — [M365 admin blog](https://m365admin.handsontek.net/?p=15422)
- Capture-layer consolidation: Limitless acquired by Meta, Pendant sales halted (Dec 2025) — [TechInformed](https://techinformed.com/meta-acquires-limitless-pendant-users-moved-to-free-unlimited-plan/); Rewind capture disabled Dec 19, 2025 — [screenpi.pe](https://screenpi.pe/blog/rewind-ai-alternative-2026); Graphlit winding down Sep 2026 — [hydradb](https://hydradb.com/blog/graphlit-reviews); Khoj Cloud closed Apr 2026 — [PromptQuorum](https://www.promptquorum.com/de/power-local-llm/khoj-ai-second-brain-review); Dot shut down Oct 2025 — [TechCrunch](https://techcrunch.com/2025/09/05/personalized-ai-companion-app-dot-is-shutting-down)

### Inferences
- The white space most relevant to ShogunAgent is the intersection the market has not filled: *inferred work-state* (not raw facts or transcripts) + *cross-agent/team sharing over MCP* + *local-first on macOS*. Microsoft's communication memory is the nearest platform threat but is tenant-bound to M365.
- Coding-agent memory is being commoditized from two directions (Claude Code auto memory, Letta Context Repos) and fragmented from below (ByteRover, funes); it is a hard segment to monetize standalone.
- Capture-layer exits mean fewer independent sources of "what the user did" — an opportunity for a local-first inference layer, but also a signal that consumers did not pay for raw recall.

### Gaps
- No market-sizing data for "shared agent memory" or "state of work" segments; Futurum's survey percentages were not in the article text.
- Could not verify whether any stealth startups target work-state inference specifically.

---

## Key Question 3: What do platform vendors (OpenAI / Anthropic / Google / AWS / Microsoft) now provide that commoditizes third-party memory?

### Takeaway
By October 2026 every major platform ships a memory primitive: AWS and Google sell managed extraction+storage at cents per thousand memories; Anthropic ships a free client-side memory *tool* plus managed-agent memory with "Dreaming" consolidation; Microsoft Foundry has user/session/procedural memory in preview; OpenAI gives developers only conversation-state primitives (sessions, `previous_response_id`) but a strong consumer ChatGPT memory. The commoditized part is "store and retrieve facts per user"; what remains uncommoditized is cross-platform portability, inference of work state, and shared memory across heterogeneous agents.

### Cited Findings

**OpenAI**
- Agents SDK offers four state mechanisms: result history, sessions (client-managed), `conversation_id` and `previous_response_id` (OpenAI-managed, Responses API only); session persistence cannot be combined with server-managed conversation in one run — [OpenAI Agents SDK docs](https://openai.github.io/openai-agents-python/running_agents/); sessions are "the SDK's persistent memory layer" storing items, with a Conversations-API-backed session and `responses.compact` compaction wrapper (TS) — [Agents JS sessions](https://openai.github.io/openai-agents-js/guides/sessions/)
- Responses API "does not provide automatic memory management"; continuation via `previous_response_id` or manual context — [OpenAI Cookbook](https://cookbook.openai.com/examples/agents_sdk/session_memory); Conversations have no TTL while `previous_response_id` chains carry 30-day TTL (secondary) — [Medium comparison](https://shibuiyusuke.medium.com/a-deep-comparison-of-memory-implementations-across-gemini-openai-and-anthropic-the-state-of-5b5fc9c1fa6c)
- Vendors (Mem0, Hindsight) state Responses/Realtime APIs are stateless re: long-term identity, leaving cross-session memory to the app — [Mem0 blog](https://mem0.ai/blog/how-to-add-memory-to-openai-responses-api-agents); [Hindsight blog](https://hindsight.vectorize.io/blog/2026/04/17/openai-agents-persistent-memory)
- ChatGPT: Jan 15, 2026 improved recall from past chats (Plus/Pro); May 5, 2026 draws on past chats, saved memories, files and connected Gmail — [ChatGPT release notes](https://help.openai.com/en/articles/6825453-chatgpt-release-notes); Jun 4, 2026 "Dreaming" rebuilt memory that updates as time passes (e.g., planned trip → completed) with a reviewable memory summary page, rolled to US Plus/Pro first — [OpenAI](https://openai.com/index/chatgpt-memory-dreaming/)
- No official OpenAI developer "memory API" found; a site claiming one is competitor marketing — [search summary](https://aimemory.pro/blog/chatgpt-memory-api)

**Anthropic**
- Memory tool (`memory_20250818`): client-side file operations (view/create/str_replace/insert/delete/rename) under `/memories`; available on all Claude 4+ models; no beta header required; developer controls storage; API auto-injects a "memory protocol" system prompt; pairs with context editing (client) and compaction (server-side) — [Claude Platform docs](https://platform.claude.com/docs/en/agents-and-tools/tool-use/memory-tool); introduced with context editing Sep 2025 — [claude.com blog](https://claude.com/blog/context-management)
- Claude Code auto memory: four note types (user, feedback, project, reference), on by default locally, stored at `~/.claude/projects/<project>/memory/` with MEMORY.md index (first 200 lines/25KB loaded), machine-local, not synced to other machines/cloud; configurable `autoMemoryDirectory`; subagents can have their own — [Claude Code docs](https://code.claude.com/docs/en/memory)
- Managed Agents: memory public beta Apr 23, 2026; "Dreaming" (research preview, May 6, 2026) runs between sessions across up to 100 prior sessions to merge duplicates, resolve contradictions, prune stale entries, producing a separate revised memory; Harvey reported ~6x task-completion improvement internally (no external benchmark) — [The New Stack](https://thenewstack.io/anthropic-managed-agents-dreaming-outcomes/); [TechQuotient](https://techquotient.substack.com/p/dreams-anthropics-new-memory-feature)
- Consumer Claude memory updated Aug 25, 2026: view/edit remembered topics, sensitive topics excluded by default, memory spans chat and Cowork — [SiliconANGLE](https://siliconangle.com/2026/08/25/anthropic-updates-claudes-memory-to-enhance-customization-and-protect-sensitive-topics/)

**Google**
- Vertex AI Agent Engine Memory Bank: LLM-generated memories from session events, memory profiles and revisions, Memory Bank API + ADK quickstart; supports VPC-SC, CMEK, data residency, HIPAA — [Memory Bank overview](https://docs.cloud.google.com/vertex-ai/generative-ai/docs/agent-engine/memory-bank/overview)
- Pricing: Jan 2026 structure $0.25 per 1,000 memories stored/month + $0.50 per 1,000 memories retrieved (LLM costs separate); new structure effective Sep 1, 2026: storage $0.30/GiB-month, reads billed as 1 Agent Compute vCPU-hour ($0.085) per 3M reads, writes 1 vCPU-h per 1M writes — [Vertex AI pricing](https://cloud.google.com/vertex-ai/pricing); [dreaming.press](https://dreaming.press/stack/vertex-ai-memory-bank); preview was free — [Google dev forum](https://discuss.google.dev/t/lets-talk-about-vertex-ai-memory-bank-heres-a-quick-faq-on-the-top-5-questions-weve-seen/244685)
- Gemini app: Past Chats rolled to free users (Feb 26, 2026, ex-Europe) with "Personal context" settings; Personal Intelligence taps Gmail/Photos — [9to5Google](https://9to5google.com/2026/02/26/gemini-past-chats-free/); Gemini Live gained memory (Jun 18, 2026) — [9to5Google](https://9to5google.com/2026/06/18/gemini-live-memory)

**AWS**
- Bedrock AgentCore Memory pricing (official, fetched Oct 25, 2026): short-term memory from Oct 6, 2026: $1.00/GB ingested, $0.20/GB retrieved, $0.10/GB-month (events billed min 12 KB, max 64 KB; previously $0.25 per 1,000 events); long-term: $0.75 per 1,000 records/month built-in strategies, $0.25 per 1,000 for override/self-managed (plus your model costs), $0.50 per 1,000 retrievals; worked "personalized coding assistant" example = $18.97/month — [AWS AgentCore pricing](https://aws.amazon.com/bedrock/agentcore/pricing/)

**Microsoft**
- Foundry Agent Service Memory: public preview; three types — procedural (new at Build 2026), user, session; TTL controls added Jun 3, 2026; used via managed memory search tool or Memory Store APIs; handles "remember this / forget that" — [Microsoft Learn](https://learn.microsoft.com/en-us/azure/foundry/agents/how-to/memory-usage); [Foundry blog Build 2026](https://devblogs.microsoft.com/foundry/memory-build2026/); [Foundry June update](https://devblogs.microsoft.com/foundry/whats-new-in-microsoft-foundry-june-2026/); preview limits 100 scopes/store, 10k memories/scope, 1k search rpm (third-party) — [baeseokjae guide](https://baeseokjae.github.io/posts/microsoft-foundry-agent-service-build-2026-guide/)
- M365 Copilot memory (MC1158329): personalizes from chat history, settings page to manage memories, on by default, admin tenant/user controls, Purview eDiscovery auditable; GA repeatedly slipped — latest snapshot (Sep 1, 2026) says GA mid-Sep to late-Oct 2026 — [Message Center mirror](https://mc.merill.net/message/MC1158329/compare/2026-09-01T16-52-13-017Z/2025-10-23T17-45-04-953Z); [Neowin](https://www.neowin.net/news/microsoft-365-copilot-is-finally-getting-memories/); communication memory (emails/chats/meetings summaries) GA from Sep 2025 — [M365 admin](https://m365admin.handsontek.net/?p=15422)

### Inferences
- Hyperscaler memory is priced at roughly $0.25–0.75 per 1,000 stored memories per month and $0.50 per 1,000 retrievals (AWS, Google Jan-2026 structure) — a price floor that third parties can't undercut on storage alone; third parties must win on quality (extraction, temporal reasoning), portability across platforms, or inference.
- Anthropic's memory tool is deliberately storage-agnostic (client-side), which keeps a lane open for a memory *provider* behind it; Claude Code auto memory being machine-local and per-repo leaves team/cross-device sync unaddressed by Anthropic.
- OpenAI's developer surface is the least "memory-complete," which is why Mem0/Hindsight market directly at Responses API users.

### Gaps
- Google's Sep 2026 Memory Bank write pricing and Microsoft Foundry memory GA pricing not confirmed from primary pages (pricing page too large to fetch fully; Foundry still preview).
- OpenAI Assistants API retirement (H1 2026) comes from a secondary vendor source.

---

## Key Question 4: Benchmarks — published LoCoMo / LongMemEval / MemBench results and controversies

### Takeaway
Benchmark numbers in this category are vendor-reported, run on different harnesses, and have been publicly disputed: Mem0's CTO accused Zep of inflating LoCoMo (84% → corrected 58.44%), Zep countered that Mem0 misconfigured Zep (re-run 75.14% vs Mem0 ~68%), Letta showed plain-file grep beats both (74.0%), and an audit by Penfield Labs found 99 score-corrupting errors in LoCoMo's 1,540 questions. LongMemEval is now the preferred benchmark, where Mastra (94.87%), Hindsight (91.4%→94.6%) and Mem0 platform (94.4) all claim SOTA; independent runs show far lower numbers for OSS Mem0 (49.0).

### Cited Findings
- Mem0 CTO filed issue on Zep's papers repo (closed May 19, 2025) arguing Zep's 84% LoCoMo excluded adversarial questions from the denominator; corrected to 58.44% ± 0.20 — primary: github.com/getzep/zep-papers/issues/5 — summarized in [beri.net](https://www.beri.net/article/zep-vs-mem0-vs-postgres-agent-memory-store-buy-last-2026)
- Zep's rebuttal: Mem0 misconfigured Zep (single-user graph with both speakers as user, timestamps in message text instead of `created_at`, sequential searches); Zep re-run 75.14% ± 0.17 vs Mem0 ~68%; p95 latency 0.632s; full-context baseline ~73% — primary: blog.getzep.com/lies-damn-lies-statistics-is-mem0-really-sota-in-agent-memory/ — summarized in [beri.net](https://www.beri.net/article/zep-vs-mem0-vs-postgres-agent-memory-store-buy-last-2026); [The AI Engineer Substack](https://theaiengineer.substack.com/p/cognee-vs-zep-vs-mem0-vs-letta)
- Letta's evaluation: LoCoMo transcripts in plain files with grep/open tools scored 74.0% (GPT-4o-mini) vs Mem0 graph 68.5% — primary letta.com/blog/benchmarking-ai-agent-memory/ — via [beri.net](https://www.beri.net/article/zep-vs-mem0-vs-postgres-agent-memory-store-buy-last-2026)
- Penfield Labs (a memory vendor) audit: 99 score-corrupting errors in 1,540 LoCoMo questions; LLM judge accepted 62.81% of deliberately wrong answers — [beri.net](https://www.beri.net/article/zep-vs-mem0-vs-postgres-agent-memory-store-buy-last-2026) (no primary link found)
- LoCoMo flaws (speaker misattribution, ambiguous questions; short conversations favor full-context; lacks knowledge-update questions) — [Developers Digest](https://www.developersdigest.tech/blog/best-ai-agent-memory-providers-2026); [iotdigitaltwinplm](https://iotdigitaltwinplm.com/mem0-vs-letta-vs-zep-agent-memory-frameworks-comparison-2026/)
- "The Benchmark Theatre" essay (May 20, 2026) argues almost nothing published about agent-memory scores is reliable — [bloo-mind essays](https://essays.bloo-mind.ai/posts/2026-05-20-mem-eval/)
- Mem0 platform claims 92.5 LoCoMo / 94.4 LongMemEval at 200-memory retrieval budget; independent Vectorize measurement of Mem0 OSS: 49.0 LongMemEval — [Atlan](https://atlan.com/know/zep-vs-mem0/); [iotdigitaltwinplm](https://iotdigitaltwinplm.com/mem0-vs-letta-vs-zep-agent-memory-frameworks-comparison-2026/)
- Zep paper: DMR 94.8% vs MemGPT 93.4%; LongMemEval 84.5% vs 66.0% (MemGPT) — [arXiv 2501.13956](https://arxiv.org/html/2501.13956v1); later third-party run 71.2% (gpt-4o) vs 60.2% full-context — [aktagon](https://signals.aktagon.com/articles/2026/07/zep-a-temporal-knowledge-graph-architecture-for-agent-memory/)
- Hindsight: 91.4% LongMemEval (Dec 2025, Gemini-3 Pro backbone; validated with WaPo/Virginia Tech researchers) → site claims 94.6% LongMemEval-S (2026) — [PR Newswire](https://www.prnewswire.com/news-releases/vectorize-breaks-90-on-longmemeval-with-open-source-ai-agent-memory-system-302643146.html); [hindsight.vectorize.io](https://hindsight.vectorize.io/)
- Mastra Observational Memory: 94.87% (gpt-5-mini), 93.27% (gemini-3-pro-preview), 84.23% (GPT-4o, beating oracle 82.2–82.4% and Supermemory's prior best); LongMemEval = 500 questions over ~57M tokens from ~50 conversations — [Mastra blog](https://mastra.ai/blog/observational-memory); [agentmarketcap](https://agentmarketcap.ai/blog/2026/04/13/mastra-observational-memory-observer-reflector-pattern-agent-costs-longmemeval-2026)
- ByteRover claims 92.2% LoCoMo retrieval accuracy (self-reported) — [everydev](https://www.everydev.ai/developers/byterover)
- Mem0 paper: 26% relative LLM-as-judge improvement over OpenAI memory; 91% lower p95 latency; >90% token savings vs full context — [arXiv 2504.19413](https://arxiv.org/abs/2504.19413)
- Benchmarks: MemBench (Renmin Univ. + Huawei Noah's Ark; arXiv 2506.21605; Findings of ACL 2025) evaluates effectiveness/efficiency/capacity across factual and reflective memory, participation and observation scenarios; compares 7 mechanisms incl. MemGPT, MemoryBank on Qwen2.5-7B — [arXiv](https://arxiv.org/abs/2506.21605); EvoMemBench (arXiv 2605.18421) compares 15 memory methods vs long-context — [alphaXiv](https://www.alphaxiv.org/abs/2605.18421); AgentMemBench (arXiv 2608.00009) with MemGPT/Letta and HippoRAG baselines — [arXiv](https://arxiv.org/pdf/2608.00009); RECON (arXiv 2607.16716) for compositional reasoning over long contexts — [arXiv](https://arxiv.org/pdf/2607.16716)
- Anthropic's Dreaming customer datapoint (Harvey ~6x task completion) has no external benchmark — [The New Stack](https://thenewstack.io/anthropic-managed-agents-dreaming-outcomes/)

### Inferences
- A new entrant should not compete on LoCoMo; the benchmark is discredited. LongMemEval-S (and the newer MemBench/EvoMemBench/AgentMemBench) are where credibility lies, and simple text/file approaches (Letta grep, Mastra OM) match or beat vector/graph stores — undermining the storage-architecture pitch.
- No benchmark measures "state of work" inference (commitments, open loops, project status); a vendor entering that segment would need to publish its own evaluation, which is also an opportunity to define the category.

### Gaps
- Could not locate Penfield Labs' primary audit document or date.
- No published MemBench scores for the commercial vendors (Mem0, Zep, Letta cloud) were found.

---

## Key Question 5: Pricing norms — per-memory, per-API-call, per-token, per-seat?

### Takeaway
Developer memory APIs price on usage, not seats: per-request (Mem0 add/retrieval), per-byte-credit on ingest (Zep: 1 credit/350 bytes, retrieval free), per-token processed (Cognee $1/1M, Honcho $2/1M ingest), per-memory-record stored and per-retrieval (AWS $0.75/1k/mo + $0.50/1k; Google $0.25/1k + $0.50/1k), or per-GB (AWS short-term, Google Sep-2026). Per-seat pricing survives only in end-user products (Pieces $18.99–22.99/user, Screenpipe $50/seat, Letta Teams $20/seat, Copilot $30/user). Free tiers are universal; self-serve paid entry is $19–$125/month.

### Cited Findings
- Mem0: tiers by add/retrieval requests — Hobby free (10k/1k), Starter $19, Pro $249 (500k/50k), Enterprise custom; usage-based option — [mem0.ai/pricing](https://mem0.ai/pricing)
- Zep: credits = ingestion bytes (1 credit per 350 bytes); retrieval/storage/users free; Flex $125 (50k credits), Flex Plus $375 (200k), overage $25/10k; annual −17% — [getzep.com/pricing](https://www.getzep.com/pricing)
- Letta: Free / Pro $20/mo / API plan $20/mo + $0.10 per active agent/month + $0.00015 per second tool execution / Teams Pro $20/seat — [Letta pricing](https://docs.letta.com/letta-code/pricing)
- Cognee: $1.00 per 1M tokens processed; $5 per extra workspace; Enterprise BYOC — [Cognee pricing](https://www.cognee.ai/pricing)
- Honcho: $2 per 1M tokens ingested; `.chat()` from $0.01/query; `context()` free — [Honcho 3](https://plasticlabs.ai/blog/posts/Honcho-3)
- Hindsight Cloud: prepaid credits, per-token retain/recall, per-call reflect, storage >30 days — [Hindsight billing](https://docs.hindsight.vectorize.io/billing)
- Weaviate Engram: free 1,000 pipeline runs/mo; paid from $45/mo — [Open Source For You](https://www.opensourceforu.com/2026/06/weaviate-launches-engram-to-provide-production-grade-memory/)
- AWS AgentCore Memory: $0.75 (built-in) / $0.25 (self-managed) per 1,000 LTM records/month; $0.50 per 1,000 retrievals; STM $1.00/GB in, $0.20/GB out, $0.10/GB-month (Oct 6, 2026) — [AWS pricing](https://aws.amazon.com/bedrock/agentcore/pricing/)
- Google Memory Bank: $0.25/1k memories stored/mo + $0.50/1k retrieved (Jan 2026) → $0.30/GiB-month + vCPU-hour-metered reads/writes (from Sep 1, 2026) — [Vertex AI pricing](https://cloud.google.com/vertex-ai/pricing); [dreaming.press](https://dreaming.press/stack/vertex-ai-memory-bank)
- Per-seat end-user products: Pieces Pro $18.99/mo, Enterprise $22.99/mo — [pieces.app](https://pieces.app/pricing); Screenpipe Basic $25/mo, Business $50/seat/mo — [aicoolies](https://aicoolies.com/pricing/screenpipe); M365 Copilot $30/user/mo — [ibl.ai](https://ibl.ai/blog/enterprise-ai-per-seat-pricing-trap-2026.md)
- Capture APIs: Recall.ai $0.50 per recording hour (+$0.15 transcription) — [Recall.ai](https://www.recall.ai/blog/new-recall-ai-pricing-for-2026)
- Graphlit (pre-shutdown): $49 / $199 / $999 per month with usage credits — [hydradb](https://hydradb.com/blog/graphlit-reviews); Papr $100 / $500 per month developer tiers (third-party) — [SaaSworthy](https://www.saasworthy.com/product/papr-ai/pricing)
- Hidden cost pattern: Graphiti/Zep/Cognee ingestion triggers multiple LLM calls per episode (extraction, dedup, summaries) — [renezander](https://renezander.com/guides/zep-vs-graphiti/)

### Inferences
- Norm for a developer-facing memory service in 2026: free tier → ~$20–$125 self-serve → usage metering on *writes/ingest* (bytes, tokens, or requests) with free or cheap reads; enterprise adds VPC/BYOC, SSO, SOC2/HIPAA.
- A "state of work" service aimed at individuals/teams could plausibly price per seat (like Pieces/Screenpipe/Copilot), while an agent-facing API would be expected to meter on ingest or active entities (Letta's $0.10 per active agent is a precedent for "per-entity" pricing).

### Gaps
- Annual-plan prices for Zep and Letta not shown on pages; Papr/Memobase cloud rates unverified; Google's write-pricing details incomplete.

---

## Key Question 6: Funding totals for the category in 2025–2026

### Takeaway
Disclosed venture funding into dedicated agent-memory startups (excluding supermemory) totals roughly $150M+ for 2025–26 when the June 2026 Engram $98M round is included; excluding Engram, the pure "memory API" startups raised only ~$40–45M (Mem0 $24M, Cognee $7.5M, Plastic Labs ~$5.4M, Letta's $10M was Sep 2024, Memory Store $0.5M, Synvo $1M). One tracker counted 14 agent-memory deals / $124M with a $5.88M median (early 2026), and memory was ~1% of agentic-AI capital in Q2 2026 — a small, seed-heavy category with one outlier round.

### Cited Findings
- Mem0: $24M total incl. $20M Series A (Oct 2025) — [TechCrunch](https://techcrunch.com/2025/10/28/mem0-raises-24m-from-yc-peak-xv-and-basis-set-to-build-the-memory-layer-for-ai-apps/)
- Engram (startup): $98M (Jun 26, 2026), ~$600M valuation — [Pulse 2.0](https://pulse2.com/engram-launches-with-98-million-to-build-ai-memory-layer-for-enterprises/); [VKTR](https://www.vktr.com/ai-news/engram-launches-with-98m-to-give-enterprise-ai-memory/)
- Cognee: $7.5M seed (Feb 2026) — [Cognee blog](https://www.cognee.ai/blog/cognee-news/cognee-raises-seven-million-five-hundred-thousand-dollars-seed)
- Plastic Labs (Honcho): $5.35M pre-seed — [Dealroom](https://app.dealroom.co/news/feed/plastic-labs-raises-5-35m-launches-honcho)
- Letta: $10M seed (Sep 2024) — [The SaaS News](https://www.thesaasnews.com/news/letta-secures-10-million-in-seed-round/)
- Memory Store: $500K (YC, Jan 2026) — [neuronfeed](https://neuronfeed.com/startups/memory-store); Synvo AI: $1M seed (Jun 2026) — [building.it.com](https://building.it.com/articles/synvo-ai-seed-funding); ByteRover: pre-seed, amount undisclosed — [everydev](https://www.everydev.ai/developers/byterover)
- Recall.ai (capture, adjacent): $38M Series B (Sep 2025) — [meetingstack](https://meetingstack.io/tools/recall-ai/)
- Supermemory (for context only; covered elsewhere): ~$2.6–3M seed (Oct 2025), no 2026 Series A found — [Dealroom](https://nz.dealroom.co/news/feed/supermemory-secures-2-6m-for-ai-memory)
- Category tracker: "Agent Memory Systems" 14 deals, $124M, $5.88M median (12 seeds, 2 Series A), ~8 months old at time of search — found via agentic-AI funding trackers — [agentmarketcap sector map](https://agentmarketcap.ai/blog/2026/04/08/agentic-ai-funding-velocity-2026-sector-map-vertical-distribution); [newmarketpitch deals list](https://newmarketpitch.com/blogs/news/agentic-ai-list-deals) (exact page attribution uncertain)
- Memory ≈1% of Q2 2026 agentic-AI capital (secondary tracker) — [newmarketpitch analysis](https://newmarketpitch.com/blogs/news/agentic-ai-funding-analysis)
- Agentic AI overall: $6.42B in 2025; "fewer but bigger bets" in 2026 — [agentmarketcap](https://agentmarketcap.ai/blog/2026/04/08/agentic-ai-funding-velocity-2026-sector-map-vertical-distribution); Q3 2026 agent rounds 59 deals / $6.32B — [gravity.fast](https://gravity.fast/blog/ai-agent-funding-tracker-q3-2026/)
- M&A: Meta acquired Limitless (Dec 2025, undisclosed) — [TechInformed](https://techinformed.com/meta-acquires-limitless-pendant-users-moved-to-free-unlimited-plan/)

### Inferences
- Investors have funded one "memory API" company to Series A (Mem0) and one research-heavy "learned memory" company at scale (Engram); the rest are seed. The category is not where large 2026 checks went, which cuts both ways for a new entrant: low competition for capital, but investor skepticism that memory APIs are a standalone business versus a platform feature.

### Gaps
- No single authoritative 2025–26 market map for agent memory was found (a16z/Sequoia/Menlo maps did not surface in results); totals above are hand-assembled from disclosed rounds and may miss undisclosed or non-English deals.
- Zep's and Papr's funding remain unverified; Mnemoverse/MemoryLake/EverMind funding not found.
