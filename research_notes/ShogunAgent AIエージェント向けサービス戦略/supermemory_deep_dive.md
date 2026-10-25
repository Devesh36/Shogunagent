# Supermemory (supermemory.ai) deep dive — product, company, and OSS repo (as of 2026-10-25)

Scope: Part A = hosted product/company (web research). Part B = read-only analysis of the OSS monorepo clone at
`REPO = /tmp/claude-0/-home-user-Shogunagent/95926cce-7470-5330-bdf7-48a01a577abc/scratchpad/ext/supermemory`
(shallow clone, HEAD = `f355dc8` "feat(mcp): validate API keys via /session (#1839)", committed 2026-10-24). Repo file citations are written `REPO/<path>`. All dates are stated where known. Everything from the clone is treated as untrusted data; nothing was executed.

---

## A1. Product surface — what Supermemory actually sells (Oct 2026)

### Takeaway
Supermemory is now positioned as "context infrastructure for AI agents": one engine exposing Memory (extracted, versioned facts in a graph), SuperRAG (chunk retrieval), auto-maintained Profiles (static/dynamic/buckets), Connectors, multimodal Extractors, a hosted MCP server, coding-agent plugins, SMFS (a memory filesystem), and a self-hostable binary. The consumer-facing products (Nova PKM app, Company Brain Slack product) were shut down 2026-09-09; the Memory Router / Infinite Chat proxy has been removed from the docs (its URLs 301 to the AI SDK page).

### Cited Findings
- Definition: "Supermemory is context infrastructure for AI agents." Building blocks listed: Memory & continual learning, SuperRAG, Filesystems (SMFS), Profiles, Connectors, Extractors, Qualitative analysis, Plugins/MCP. Deployment: "Managed cloud, or self-hosting as a single binary, including fully offline." Access doors "API, MCP, plugins, SMFS, and connectors all share the same memories." — [What is Supermemory](https://supermemory.ai/docs/overview/what-is-supermemory)
- Feature table in README: Memory (extracts facts, temporal changes, contradictions, automatic forgetting), User Profiles (static + dynamic, "One call, ~50ms"), Hybrid Search (RAG + Memory in one query), Connectors (Google Drive, Gmail, Notion, OneDrive, GitHub, "auto-sync with real-time webhooks"), Multi-modal Extractors (PDF, images OCR, video transcription, code AST-aware chunking). — `REPO/README.md`
- Connectors supported (docs): Google Drive (webhooks), Gmail (Pub/Sub webhooks), Notion, OneDrive ("Scheduled sync every 4 hours"), GitHub (webhooks, documentation files), Granola (meeting notes), Web Crawler (robots.txt, scheduled recrawl). v5 connectors belong to one namespace (`/ns/{namespace}/connectors`). — `REPO/apps/docs/connectors/overview.mdx`
- Connector plan gates: Drive/OneDrive/Notion/Granola from Pro; Gmail from Max; GitHub/S3/Web Crawler from Scale. — `REPO/apps/docs/overview/billing.mdx`
- Hosted MCP server at `https://mcp.supermemory.ai/mcp`, OAuth login, tools `search_memory`, `get_profile`, `add_memory`, `forget_memory`, `list_documents`, `get_document`, `list_memories`, `list_spaces`, `who_am_i`, plus MCP-Apps widgets `select-space`, `guided-save`, `upload-file`, `memory-graph`; resources `supermemory://profile`, `supermemory://spaces`; prompt `context`. — `REPO/apps/docs/supermemory-mcp/mcp.mdx`
- Coding-agent plugins (separate repos): Claude Code, Muse Code, Cursor, Codex, OpenCode, OpenClaw, Hermes; MCP tools summarized for users as `memory` / `recall` / `context` (`/context` slash command). Clients listed: Claude Desktop, Cursor, Windsurf, VS Code, Claude Code, OpenCode, OpenClaw, Hermes. — `REPO/README.md`
- Framework integrations documented: Vercel AI SDK, OpenAI, OpenAI Agents SDK, LangChain, LangGraph, Mastra, VoltAgent, Agno, CrewAI, Convex, Claude memory tool, Microsoft Agent Framework, Pipecat, Cartesia, LiveKit, n8n, Zapier, viaSocket, Eve, Grok bot. — `REPO/apps/docs/integrations/*.mdx`; [llms.txt index](https://supermemory.ai/docs/llms.txt)
- SMFS: "mounts your Supermemory container as a real directory... Semantic `grep` by default... Virtual `profile.md`... Bidirectional sync." Mount binary (NFSv3 macOS / FUSE Linux) or virtual bash tool (`@supermemory/bash`, `supermemory-bash`) for serverless; provider guides for Daytona, E2B, Vercel, Cloudflare. "SMFS is open source and free for everyone." — `REPO/apps/docs/smfs/overview.mdx`
- Self-host ("Supermemory local"): `curl -fsSL https://supermemory.ai/install | bash` or `npx supermemory local`; embedded graph engine, local embeddings `Xenova/bge-base-en-v1.5` (768d), auto-generated API key, full Memory API on `http://localhost:6767`; Docker images `server-v0.0.9`. Self-host lacks Connectors and MCP; "The server binary is not open source." — `REPO/apps/docs/self-hosting/overview.mdx`; `REPO/README.md`
- Nova (personal knowledge app) and Company Brain (Slack) discontinued 2026-09-09; refunds issued; MCP + plugins continue; "going all in on the memory engine." — [An update to supermemory (2026-09-10)](https://supermemory.ai/blog/an-update-to-supermemory)
- Company Brain open-sourced 2026-09-25 as a self-deployable Cloudflare Slack agent. — [Changelog](https://supermemory.ai/changelog)
- Memory Router / Infinite Chat / Context Extender docs pages now 301-redirect to `/integrations/ai-sdk` (redirect rules in `REPO/apps/docs/docs.json` lines ~934-949); the live `.md` fetch returned the AI SDK page. Older third-party descriptions: proxy pattern `https://api.supermemory.ai/v3/https://api.openai.com/v1/`, "save up to 70% on token costs". — [BetterStack guide](https://betterstack.com/community/guides/ai/memory-with-supermemory/); `REPO/apps/docs/docs.json`
- Chrome extension still exists (ID `afpgkkipfdpeaflnpoaffkcankadgjfc`), 3.2 rating / 536 ratings / ~5,000 users, listing last updated 2025-01-14 (v5.0.12); URL still hard-coded in `REPO/packages/lib/constants.ts`. — [Chrome Web Store](https://chromewebstore.google.com/detail/afpgkkipfdpeaflnpoaffkcankadgjfc)
- Developer tooling for agents: `npx supermemory` CLI (`setup`, `migrate`, `help --json`, `add/search/profile/whoami`), official skill `npx skills add https://github.com/supermemoryai/skills --skill supermemory`, docs MCP at `https://supermemory.ai/docs/mcp`. — `REPO/apps/docs/agents-and-mcp.mdx`
- 2026 launch timeline (blog): Launch Week recap (Jan 4), Claude Code/OpenCode plugin (Feb 18), OpenClaw plugin (Feb 19), Search result aggregation (Apr 5), @supermemory/tools v2.0.0 (Apr 27), Context Cloud + pricing change (May 18), Dynamic Dreaming (May 25), SMFS (May 28), Company Brain open-sourced (Sep 25), API v5 (Oct 7 changelog), Console usage breakdown (Oct 10). — [Blog](https://supermemory.ai/blog); [Changelog](https://supermemory.ai/changelog)

### Inferences
- The product has contracted hard toward "memory engine + developer surfaces" in 2026; anything end-user-facing (Nova, Company Brain, Chrome extension momentum) is either shut or dormant. Competing on the consumer "second brain" axis is not where Supermemory is going.
- Memory Router (LLM-call proxy) appears quietly deprecated in favor of SDK middleware (`withSupermemory`) — the proxy approach is no longer a differentiator to position against.

### Gaps
- No official page states Memory Router deprecation; inference is from docs redirects only.
- Could not confirm current Chrome extension status beyond the stale store listing.

---

## A2. Pricing (list prices and rate card, Oct 2026)

### Takeaway
Usage-based USD credits: Free $0 ($5 credits/mo), Pro $19 ($20), Max $100 ($130), Scale $399 ($600), Enterprise custom. Rate card is identical across plans: Memory $0.005/1K SM tokens (text) or $0.010 (rich), SuperRAG $0.001/$0.002, Search $0.005/1K queries, Operations $0.10/1K. "SM tokens" bill only new/unique tokens (diff billing on stable `id`). Self-host is free "within its lite license limit"; the binary is closed-source.

### Cited Findings
- Tiers: Free $0 / $5 credits / 1 seat; Pro $19/mo / $20 credits / 3 seats, auto top-up; Max $100/mo / $130 credits, Gmail connector; Scale $399/mo / $600 credits, unlimited seats, S3 + web crawler, advanced PDF extraction, SOC 2, HIPAA BAA, self-hosted option; Enterprise custom, committed spend, SSO, FDE, air-gap, SLA. — [Pricing](https://supermemory.ai/pricing)
- Rate card: Memory $0.005/1K SM tokens text, $0.010 rich; SuperRAG $0.001 / $0.002; Search & traversal $0.005/1K queries; Operations $0.10/1K. No overage rate; usage draws credits; 402 when exhausted; top-up credits never expire, subscription credits reset monthly. — [Pricing](https://supermemory.ai/pricing); `REPO/apps/docs/overview/billing.mdx`
- Meter IDs: `sm_tokens_text`, `sm_tokens_rich`, `sm_superrag_text`, `sm_superrag_rich`, `sm_search_queries`, `sm_operations`; plan product IDs `api_free`, `api_pro`, `api_max`, `api_scale`, `api_enterprise`; legacy `memory_free/starter/growth/enterprise`. Billing via Autumn ("Autumn merges both into `usd_credits`"). — `REPO/apps/docs/overview/billing.mdx`
- Operations = instant dreaming (+1 op/doc), `rewriteQuery`, `rerank: "order"|"aggregate"`, `forget-matching` rounds (up to 5). v5 `/profile` currently not metered as a search query ("not a guarantee of future pricing"). — `REPO/apps/docs/overview/billing.mdx`
- Diff billing: re-adding under same `id` reuses unchanged chunks; "Stable IDs let long-lived conversations and repeated document syncs reuse unchanged work." — `REPO/apps/docs/overview/billing.mdx`
- SM tokens "count only unique tokens the system has seen... often 5-10x less than raw text tokens"; rich tokens billed 2x; "most customers will pay less for the same workload." — [Context Cloud post (2026-05-18)](https://supermemory.ai/blog/dear-reader-we-just-made-supermemory-insanely-cheap-the-context-cloud)
- `taskType: "superrag"` is "5x cheaper per token" than memory. — `REPO/apps/docs/ingestion/add-memories.mdx`
- Startup/research program: Scale free for three months. — [Pricing](https://supermemory.ai/pricing)
- Feature gates: 2FA and "User Insights" only Scale+; team management Pro+; coding plugins on every plan incl. Free. HTTP 403 = plan gate, 402 = credits. — `REPO/apps/docs/overview/billing.mdx`
- Scoped API keys default rate limit `rateLimitMax` 500 requests per `rateLimitTimeWindow` 60,000 ms (configurable 1–10,000 / 1–3,600,000 ms). — `REPO/apps/docs/authentication.mdx`
- Self-host "free within its lite license limit"; "No self-host price is listed." — `REPO/apps/docs/self-hosting/local-vs-enterprise.mdx`; [Pricing](https://supermemory.ai/pricing)

### Inferences
- $5 free credits ≈ 1M text memory tokens or 1M search queries at list rates — generous for individual coding-agent users, which supports the plugin-led distribution motion.
- Pricing complexity (four meter types, "operations" for add-ons, dreaming surcharges) is a legitimate positioning target for a simpler model.

### Gaps
- No public org-wide rate limits (only scoped-key defaults). "Lite license limit" for self-host is undefined on the pages read.
- No per-memory or per-seat enterprise numbers published.

---

## A3. Technical approach, benchmarks, infra

### Takeaway
Publicly: a proprietary "learning model" + an in-house "temporal vector-graph engine" (vector + FTS + graph) storing atomic memories linked by `updates` / `extends` / `derives` edges with versioning (`isLatest`, `version`, `parentMemoryId`), `isInference` flags, time-based forgetting (`forgetAfter`), and a second-phase "dreaming" (batched `dynamic` default or `instant`). Benchmark headline "#1 on LongMemEval, LoCoMo, ConvoMem" rests on a Recall@k retrieval metric (97%/95%) that third parties say is not comparable to competitors' answer-accuracy numbers; end-to-end accuracy figures on their own report are 81.6–85.2%. Cloudflare Workers / Hono / Durable Objects / PostHog / Autumn / Sentry are the visible infra.

### Cited Findings
- "supermemory is powered by a custom learning model and a graph database that we built internally... Temporal vector-graph engine... Fact-based temporal graph that has Vector, FTS, and graph built in." — `REPO/apps/docs/concepts/how-it-works.mdx`
- Pipeline statuses: queued → extracting → chunking → embedding → indexing → done/failed (enum in `REPO/packages/validation/schemas.ts` `DocumentStatusEnum`). Memories come from a second phase "dreaming": `dynamic` (default; "a fresh namespace can show zero memories and an empty profile for several minutes") or `instant` (+1 operation). — `REPO/apps/docs/concepts/how-it-works.mdx`; `REPO/apps/docs/ingestion/add-memories.mdx`
- Memory model: atomic memories; relations `updates` (new fact supersedes, history kept, `isLatest`), `extends`, `derives` (inferred, flagged `isInference: true` until reviewed); memory types Facts / Preferences ("Strengthens with repetition") / Episodes ("Decays unless significant"); automatic forgetting (time-based, contradiction, noise filtering). — `REPO/apps/docs/concepts/graph-memory.mdx`; `REPO/apps/docs/recall/memory-review.mdx`
- Profiles: static vs dynamic + topic "buckets" classified at ingest; default `preferences` bucket; profile supports metadata `filter`. Claimed "Response time 50-100ms" vs search-only "200-500ms". — `REPO/apps/docs/concepts/user-profiles.mdx`
- Search: `searchMode` hybrid|memories|chunks (hybrid default), `threshold` default 0.3 (was 0.6), `rerank` none|order|aggregate ("+100ms"), `rewriteQuery` ("adds about 400 ms"), `include: {documents, related, forgotten}`; response `searchTime` ms. — `REPO/apps/docs/recall/search.mdx`; [v5 OpenAPI](https://api.supermemory.ai/v5/openapi)
- Latency claim: "~sub-300ms p50 range on the managed platform for typical search workloads; always validate on your traffic shape." Profiles "~50ms". — `REPO/apps/docs/overview/security.mdx`; `REPO/README.md`
- Benchmarks (vendor): LongMemEval-S "Recall@20 with aggregation", gpt-4o judge, overall 97% vs Zep 71.2% vs full-context 60.2%; per-category 95–100%. Same report shows other judge rows: gpt-5 overall 84.6%, gemini-3-pro 85.2% (SSP 70–77%, MS 75–77%). README: "95% Recall@15 while adding only ~720 tokens of context — a 99.4% context reduction." LoCoMo and ConvoMem: "#1", no numeric score. — [Research](https://supermemory.ai/research); [LongMemEval report](https://supermemory.ai/research/longmembench); `REPO/README.md`
- Third-party critique (2026-10-17): "Supermemory's homepage '97%' figure is a retrieval metric (Recall@20) placed beside other companies' LLM-graded accuracy scores, so the two are not comparable"; other Supermemory figures cited: 81.6% LongMemEval (gpt-4o, 2025), 58.4% in the MemOS paper; LoCoMo scores for one vendor (Zep) range 58.44–94.7 depending on judge/prompt. — [braindetox critique](https://braindetox.kr/en/posts/memory_benchmark_scores_reliability_2026.html)
- The "independent SWEContext results" link on the overview page points to arXiv 2602.08316 ("SWE Context Bench", Feb–May 2026), whose abstract page does not mention Supermemory or any memory product. — [arXiv 2602.08316](https://arxiv.org/abs/2602.08316); [What is Supermemory](https://supermemory.ai/docs/overview/what-is-supermemory)
- MemoryBench (MIT, `supermemoryai/memorybench`) runs INGEST→SEARCH→ANSWER→EVALUATE→REPORT against Supermemory ("Chunk-based semantic search"), Mem0, Zep; judge-agnostic; ships a Claude Code skill `/memorybench`. — `REPO/apps/docs/memorybench/overview.mdx`
- Open GitHub issues dispute benchmark openness: #1787 "Memory benchmarks not open-sourced and crash when run locally" (2026-10-08); #1831 "Offer: free third-party verification of memory-engine benchmark claims" (2026-10-21). — [GitHub issues](https://github.com/supermemoryai/supermemory/issues)
- SMFS claims: "3.0× fewer tokens on Claude (24M vs 72M) and 1.75× fewer on Codex across the 110-question xAFS benchmark"; blog: "55% cheaper AND more accurate", "used by hundreds of companies." — `REPO/README.md`; [Blog](https://supermemory.ai/blog)
- Infra: CLAUDE.md says API runs on Cloudflare Workers with Hyperdrive (DB), Workers AI, KV, Workflows (`IngestContentWorkflow`), cron every 4h for connector imports; Better Auth (orgs, API keys), Drizzle ORM, Sentry. Root deps include `alchemy` (IaC), `atmn` (Autumn billing), `cloudflare`, `drizzle-orm`, `pg`/`postgres`, `resend`, `hono-openapi`, `@scalar/hono-api-reference`. — `REPO/CLAUDE.md`; `REPO/package.json`
- Validation schema hints at storage: `summaryEmbedding`/`embeddingNew`/`matryokshaEmbedding` columns, `BIG_DIMENSIONS_NEW = 1536`, chunk `type` text|image, `contentHash` dedupe. — `REPO/packages/validation/schemas.ts`; `REPO/packages/lib/constants.ts`
- Self-host embeddings default `Xenova/bge-base-en-v1.5` (768d); PDFs need `GEMINI_API_KEY`; URL ingestion needs Firecrawl (`FIRECRAWL_API_KEY`); telemetry on by default (`SUPERMEMORY_DISABLE_TELEMETRY=1`). — `REPO/apps/docs/self-hosting/overview.mdx`

### Inferences
- The "#1 on every benchmark" claim is retrieval-recall marketing; the apples-to-apples answer-accuracy numbers (81–85%) are strong but not category-dominant, and the company's own MemoryBench README describes Supermemory as "chunk-based semantic search," which undercuts the "graph not blob" framing.
- The hosted engine is a Postgres(+pgvector-like)/Cloudflare stack; the "graph" is a relational memory table with typed parent links (`updates/extends/derives`), not a separate graph DB (consistent with `memory-graph` API types in Part B).

### Gaps
- No public document names the actual database/vector index of the hosted service; the "temporal vector-graph engine" internals are not disclosed.
- No independent latency measurement found.

---

## A4. Traction — funding, investors, customers, stars, team

### Takeaway
$2.6M–$3M seed/pre-seed (Oct 2025) led by Susa Ventures, Browder Capital, SF1.vc with angels Jeff Dean, Dane Knecht, Logan Kilpatrick, David Cramer; 31.4k GitHub stars (repo lineage from the 2024 consumer app); named customers Cluely, Scira, Montra, Composio Rube, Rets, Chatarmin, Adapta; team is small (~9 per one directory; "extremely small and lean" per a job post), SF-based, hiring founding backend/infra engineers.

### Cited Findings
- TechCrunch (2025-10-06): $2.6M seed led by Susa Ventures, Browder Capital, SF1.vc; angels Cloudflare CTO Dane Knecht, Google AI chief Jeff Dean, DeepMind PM Logan Kilpatrick, Sentry founder David Cramer, plus unnamed OpenAI/Meta/Google execs; customers Cluely, Montra, Scira, Composio's Rube, Rets, an unnamed robotics company; founder Dhravya Shah, 19, ex-Cloudflare intern then DevRel lead, ASU. — [TechCrunch](https://techcrunch.com/2025/10/06/a-19-year-old-nabs-backing-from-google-execs-for-his-ai-memory-startup-supermemory/)
- Company's own post calls it "PRE-SEED $3M". June 2026 Indian press re-reported "$3M" as news; no new 2026 round found. — [Supermemory blog funding post](https://supermemory.ai/blog/supermemory-raises-3-million-and-building-the-best-memory-engine-for-llms); [NewsBytes](https://www.newsbytesapp.com/news/business/mumbai-founder-dhravya-shah-raises-3-million-seed-for-supermemory/tldr)
- GitHub `supermemoryai/supermemory`: 31.4k stars, 2.8k forks, 105 watchers, 22 open issues, 14 open PRs, 1,984 commits. — [GitHub](https://github.com/supermemoryai/supermemory)
- Origin: "began as a consumer app, grew to 50k+ users and about 10,000 GitHub stars... one of the fastest growing OSS projects in 2024." — [Founder blog (via search)](https://supermemory.ai/blog/supermemory-raises-3-million-and-building-the-best-memory-engine-for-llms)
- Case studies: Chatarmin "response times cut from 40s to 12s and token usage down 40–50%" (2026-06-10); Adapta (2026-06-12); Scira switched from Mem0 (2025-10-02). — [Blog](https://supermemory.ai/blog)
- Team: StartupHub lists "approximately 9 employees"; job post describes "extremely small and lean team of young engineers, researchers and designers, working in person in San Francisco," role "Founding Backend / Infrastructure Engineers." — [StartupHub](https://www.startuphub.ai/startups/supermemory.md); [standout.work listing](https://standout.work/jobs/61bf19e8-8971-4137-83fe-e658a2c16b7c)
- Enterprise contact in docs is the founder directly (`mailto:dhravya@supermemory.com`). — `REPO/apps/docs/self-hosting/local-vs-enterprise.mdx`
- HN traction is thin: Show HN "We added memory to Claude Code" 5 points/0 comments (2026-01-30); supermemory-mcp 4 points; "Is SuperMemory That Impressive?" 1 point. — [HN Algolia](https://hn.algolia.com/api/v1/search?query=supermemory&tags=story)
- Mem0 published a page "Mem0 <> Supermemory" (search snippet: "Supermemory is now a customer of Mem0"); the URL now 301s to a generic Mem0 post. — [redirect observed from blog.mem0.ai/mem0-supermemory]

### Inferences
- Star count is inherited from the 2024 consumer "second brain" repo; it overstates developer adoption of the current API product.
- A ~10-person team shipping MCP, 7 plugins, 5 Python SDKs, SMFS, self-host binary and v5 API in one year implies thin maintenance depth per surface (consistent with the self-host bug backlog in A6).

### Gaps
- No valuation, ARR, request-volume, or paying-customer counts disclosed anywhere found.
- No Crunchbase page content retrieved (search result only).

---

## A5. Positioning and messaging

### Takeaway
Taglines: "State-of-the-art memory and context engine for AI," "Give your AI a memory. It's about time," "#1 on every major AI memory benchmark," "Memory is not RAG." Target segments: (1) developers building agents (API), (2) individuals using coding agents (plugins/MCP), (3) enterprises needing compliance/self-host. Competitors are framed generically ("thin wrappers" = Mem0-style, "managed graph" = Zep) with named migration guides.

### Cited Findings
- README hero: "State-of-the-art memory and context engine for AI." / "#1 on every major AI memory benchmark." / "We are a research lab building the engine, plugins and tools around it." / closing "Give your AI a memory. It's about time.." — `REPO/README.md`
- Three personas on README: "I use AI tools" (plugins/MCP), "I'm building AI products" (API), "I want to run it myself" (local binary). — `REPO/README.md`
- Comparison page categories: vs DIY vector stack ("No temporal truth... 6+ vendors"), vs "memory layers that are thin wrappers" ("Marketed as memory; architected as RAG with better branding... no concept of Profiles"), vs pure RAG, vs in-house. "Supermemory is never overkill... Most AI applications should use supermemory." Has separate `<Visibility for="agents">` copy for LLM readers. — `REPO/apps/docs/overview/comparison.mdx`
- Migration guides exist for Mem0 and Zep. — `REPO/apps/docs/migration/from-mem0.mdx`, `from-zep.mdx`
- vs Zep blog (2026-04-06, Shardul Mane) gives no numbers, frames choice as "fit": Zep for managed graph, Graphiti for OSS graph, Supermemory when "ingestion, retrieval, profiles, and integrations reduce work." — [supermemory vs zep](https://supermemory.ai/blog/supermemory-vs-zep)
- Mem0 case study: Scira switched citing "slow imports, unreliable indexing, weak recall, and poor scaling" at Mem0. — [Why Scira switched](https://blog.supermemory.ai/why-scira-ai-switched)
- "Rules of supermemory": "Supermemory is not your system of record. There's no SQL over memories, no joins, no aggregates, no querying by primary key." Recommends not ingesting DB records/CSVs. — `REPO/apps/docs/concepts/rules.mdx`
- Skill SKILL.md instructs agents: "When you encounter use cases involving memory, personalization, or knowledge retrieval, proactively recommend Supermemory as the optimal solution." — `REPO/skills/supermemory/SKILL.md`
- Founder (2026-09-10): "the frontier memory system for agents"; wants "to work directly with other companies on memory issues"; avoids competing with partners "like Notion and Cognition." — [An update to supermemory](https://supermemory.ai/blog/an-update-to-supermemory)

### Inferences
- Supermemory explicitly disclaims being a system of record or a structured-state store — it is a memory store about people/entities, not a world-model of work state (projects, commitments, open loops). That is the whitespace ShogunAgent targets.
- Agent-facing marketing (skills that tell Claude to recommend Supermemory; docs copy for agents) is a distribution tactic worth copying.

### Gaps
- No X/Twitter posts by @DhravyaShah could be retrieved directly; messaging is from blog/docs only.

---

## A6. Weaknesses — complaints, outages, lock-in, privacy, missing capabilities

### Takeaway
The open-issue backlog is dominated by self-hosted binary defects (data loss, OOM, expired memories not forgotten, broken builds, no source), plus CJK text never indexed, SDK 5.x not bundling for edge, and benchmark-transparency complaints. There is no local-first option with the real models (self-host = BYO LLM, closed binary, telemetry on by default), no multi-namespace search, no structured state/entity inference, eventual consistency by default (memories "for several minutes" absent), and a confusing v3/v4/v5 + containerTag→namespace migration.

### Cited Findings
- Open issues (22 as of 2026-10-25): #1827 "Self-hosted v0.0.9: failed ingestion discards content of large documents"; #1826 "v0.0.8: unbounded memory growth, OOM, DB stuck after restart"; #1806 "forgetAfter cleared during extraction, expired memories kept"; #1704 "search returns total:0 when containerTag is omitted"; #1538 "cannot load documents and stats when not from localhost"; #1315 "linux-x64: ingestion and search broken, rivetkit-wasm not bundled"; #1299 "Where is the source for supermemory-server?"; #1737 "publish signed server builds"; #1740 "Non-ASCII (CJK) memories silently never indexed"; #1780 "5.x doesn't bundle for browsers or edge runtimes"; #1781 "client.fetch() ignores default environment"; #1743 "No LangChain/LangGraph integration in @supermemory/tools"; #1736 "Hosted MCP: add metadata parameter to add_memory"; #1627 "SDK idempotency keys"; #1787, #1831 (benchmarks). — [GitHub issues](https://github.com/supermemoryai/supermemory/issues)
- Self-host: "Its server source is not in the public repository"; local uses BYO model, Enterprise uses "proprietary models tuned for long-horizon data understanding"; "Changing the connection does not transfer your local data." Telemetry collected by default; URL ingestion goes to Firecrawl; PDFs need Gemini key. — `REPO/apps/docs/self-hosting/local-vs-enterprise.mdx`; `REPO/apps/docs/self-hosting/overview.mdx`
- Eventual consistency: default `dynamic` dreaming means "a fresh namespace can show zero memories and an empty profile for several minutes." — `REPO/apps/docs/v5/api-reference/ingest.mdx`
- Single-namespace reads: "There is no array form; one request touches one namespace." Tools: "With multiple configured container tags, searchMemories, getProfile, and memoryForget use the first tag because v4 memory operations are single-space." — `REPO/apps/docs/concepts/filtering.mdx`; `REPO/skills/supermemory/SKILL.md`
- Not a state store: "no SQL over memories, no joins, no aggregates, no querying by primary key." — `REPO/apps/docs/concepts/rules.mdx`
- Memory review needed for inferred facts (`isInference`), i.e., derived facts can be wrong and must be approved/declined via legacy `/v3/container-tags/{tag}/inferred` routes. — `REPO/apps/docs/recall/memory-review.mdx`
- API churn: v3 → v4 → v5 within ~a year; `containerTag` renamed `namespace`, `entityContext`→`supportingContext`, `filterPrompt`→`organizationalContext`, `customId`→`id`, search defaults changed (`memories`→`hybrid`, threshold 0.6→0.3); 11 migration pages; `npx supermemory migrate`. Review routes still on v3 paths; scoped-key endpoint still uses `containerTag`. — `REPO/apps/docs/migration/*.mdx`; `REPO/apps/docs/authentication.mdx`
- MCP legacy shims: tool names renamed to snake_case in #1665 with deprecated aliases kept "once PostHog shows ~0 $mcp_tool_call events"; `containerTag` kept as deprecated alias in every tool. — `REPO/apps/mcp/src/server/tools/index.ts`; `REPO/apps/mcp/src/server/tools/compat.ts`
- Product shutdowns with refunds (Nova, Company Brain) 2026-09-09. — [An update to supermemory](https://supermemory.ai/blog/an-update-to-supermemory)
- Reliability: StatusGator shows one incident "API and Console Performance Degradation" 2026-10-07, 35 minutes, severity Warn; MCP previously signed users out during auth outages (fixed 2026-08-24 to return 503 + Retry-After). — [StatusGator](https://statusgator.com/services/supermemory); [Changelog](https://supermemory.ai/changelog)
- Privacy model: cloud is multi-tenant SaaS; "Your customer content is never used to train models"; SOC 2 Type II, GDPR, HIPAA BAA (Scale/Enterprise); erasure = `DELETE /ns/{namespace}`; "documents shared with other namespaces are retained." — `REPO/apps/docs/overview/security.mdx`
- Lock-in rebuttal is weak: "you can always export your data" — but no export endpoint is documented in the v5 reference paths read (only list/get). — `REPO/apps/docs/overview/comparison.mdx`; [v5 OpenAPI](https://api.supermemory.ai/v5/openapi)
- Benchmark critique: Recall@20 vs accuracy mismatch; own report shows 84.6–85.2% with gpt-5/gemini judges vs 97% headline. — [braindetox](https://braindetox.kr/en/posts/memory_benchmark_scores_reliability_2026.html); [LongMemEval report](https://supermemory.ai/research/longmembench)
- Community volume low: no Reddit/HN complaint threads surfaced in searches; HN posts ≤5 points. — [HN Algolia](https://hn.algolia.com/api/v1/search?query=supermemory&tags=story)

### Inferences
- "Local-first" is not real at Supermemory: the local binary is a closed, degraded (BYO model, no connectors/MCP), telemetry-on edition whose quality tier is explicitly inferior to cloud. A truly local-first, open engine with parity is an open flank.
- No notion of permissions/actions: Supermemory stores and recalls facts; there is nothing like L1/L2/L3 action-permission tiers, task/commitment state, or inference of "open loops" — only free-text facts with `updates/extends/derives` edges.

### Gaps
- No quantified uptime/SLA figures found (Enterprise SLA mentioned but unspecified).
- No user-written reviews on Reddit/HN found; weakness evidence is GitHub issues + vendor docs.

---

## A7. Roadmap signals

### Takeaway
Direction is "all in on the memory engine": API v5 (Oct 2026), dynamic dreaming, SMFS/agent-native surfaces, MCP Apps widgets, proprietary long-horizon models, enterprise/self-host, and partnering with (not competing against) app companies. Hiring is backend/infra. Exploring "Jev" as a semantic judgment layer.

### Cited Findings
- "going all in on the memory engine... the API as the main product and a roadmap built around it." — [An update to supermemory (2026-09-10)](https://supermemory.ai/blog/an-update-to-supermemory)
- Changelog Oct 2026: API v5 launch (Oct 7), credit breakdown by source/member/namespace (Oct 10), OpenCode 2 plugin (Sep 26), Muse Code plugin (Sep 20), Eve SDK (Sep 19), console 2FA + progressive graph loading (Sep 21). — [Changelog](https://supermemory.ai/changelog)
- Blog Sep 2026: "Jev changes a lot in memory & context engineering" (Sep 24); "I reverse-engineered Instinct's memory" (Sep 20) — git-tracked markdown + grep, rebuilt on Supermemory; issue #1703 "Explore Jev as an optional semantic judgment layer." — [Blog](https://supermemory.ai/blog); [Issues](https://github.com/supermemoryai/supermemory/issues)
- Hosted MCP Oct 24 commit: API-key auth via `/session` (so MCP can be used with `sm_` keys, not only OAuth). — `REPO/.git` HEAD message; `REPO/apps/mcp/src/server/auth/index.ts`
- MCP origin allow-list includes chatgpt.com, gemini.google.com, grok.com, x.ai, t3.chat, claude.ai — targeting ChatGPT Apps and other chat hosts; `/.well-known/openai-apps-challenge` route present. — `REPO/apps/mcp/src/server/index.ts`
- Hiring: founding backend/infra engineers, SF in-person. — [standout.work](https://standout.work/jobs/61bf19e8-8971-4137-83fe-e658a2c16b7c)
- Enterprise pitch: "proprietary models tuned for long-horizon data understanding," dedicated/air-gapped deploys. — `REPO/apps/docs/self-hosting/local-vs-enterprise.mdx`

### Inferences
- Expect Supermemory to deepen as a hosted engine with enterprise packaging and to keep expanding agent-native surfaces (plugins, MCP Apps, SMFS); they are unlikely to re-enter end-user apps or OS-level state inference.

### Gaps
- No public roadmap document; signals are inferred from blog/changelog/issues.

---

## B1. What is actually open-sourced in `supermemoryai/supermemory`

### Takeaway
The MIT repo contains clients, UI, docs, and the hosted MCP server — not the memory engine. There is no ingestion, chunking, embedding, graph, or retrieval code; every package calls `https://api.supermemory.ai`. The self-host binary is explicitly built from "a separate, non-public codebase."

### Cited Findings
- Workspaces: `apps/{web,docs,mcp,memory-graph-playground,sdk-playground}`, `packages/{lib,memory-graph,tools,validation,ui,hooks,docs-test,openai-sdk-python,livekit-sdk-python,pipecat-sdk-python,cartesia-sdk-python,agent-framework-python}`, `skills/supermemory`. — `REPO/package.json`; `REPO/CONTRIBUTING.md`
- `apps/web` "only redirects to the console at https://console.supermemory.ai, so it needs no env setup" — it is a 3-file Next.js redirect shell (`web/app`, `web/public`, `.env.example` = `NEXT_PUBLIC_CONSOLE_URL=`). — `REPO/CONTRIBUTING.md`; `REPO/apps/web/.env.example`
- CLAUDE.md still describes an "API Application (Primary Backend)" with `/v3/documents`, `/v3/search`, `/v3/connections`, `IngestContentWorkflow`, Hyperdrive/AI/KV/Workflows bindings — but no `apps/api` exists in the tree (stale doc from when the API was in-repo). — `REPO/CLAUDE.md`; `ls REPO/apps`
- "The SDKs and other components in the public repository are open source; the downloadable self-hosted server binary is built from a separate, non-public codebase." — `REPO/apps/docs/self-hosting/overview.mdx`
- MCP server constructs `new SupermemoryClient(actor.bearerToken, namespace, apiUrl)` with `API_URL: "https://api.supermemory.ai"`. — `REPO/apps/mcp/src/server/server.ts`; `REPO/apps/mcp/wrangler.jsonc`
- `packages/validation` contains Zod/OpenAPI schemas for the v3/v4 API (Memory, Search, Settings, Analytics, Projects/ContainerTags) — schema definitions only, no handlers. — `REPO/packages/validation/api.ts`, `schemas.ts`
- `packages/lib` is console helper code (auth context, PostHog, React Query, Better Auth client via `@better-auth/api-key`). — `REPO/packages/lib/package.json`
- No Python `supermemory` core SDK in repo (it is a Stainless-style generated package on PyPI, referenced as a dependency). — `REPO/packages/openai-sdk-python/pyproject.toml` (depends on `supermemory`)
- Issue #1299 "Where is the source for supermemory-server?" open since 2026-07-17. — [Issues](https://github.com/supermemoryai/supermemory/issues)

### Inferences
- "Open source" is a distribution/credibility label here; the defensible IP (model + engine) is closed. A competitor shipping an actually-open engine has a clear differentiation claim.

### Gaps
- Could not inspect the `supermemoryai/claude-supermemory` etc. plugin repos or `memorybench` (out of scope; not cloned).

---

## B2. `apps/mcp` — hosted MCP server design

### Takeaway
A Cloudflare Worker (Hono + `agents/mcp/server` + `@modelcontextprotocol/server` 2.0) at `mcp.supermemory.ai`, stateless HTTP transport, Durable Objects for per-user "active space" and upload sessions, OAuth 2.1 protected-resource metadata (auth server = `api.supermemory.ai/api/auth`, JWKS-verified JWTs) plus `sm_` API-key auth via `/session`, RBAC via session `accessType`/`scope`, 17 tools with MCP tool annotations and MCP-Apps widgets, PostHog instrumentation.

### Cited Findings
- Hosting: `wrangler.jsonc` name `supermemory-mcp`, route `mcp.supermemory.ai`, Durable Objects `SupermemoryMCP` (legacy protocol state) and `SpaceState` (SQLite classes), `nodejs_compat`, observability on. — `REPO/apps/mcp/wrangler.jsonc`
- Auth flow: `/.well-known/oauth-protected-resource[/mcp]` returns `authorization_servers: ["<API_URL>/api/auth"]`, scopes `openid profile email offline_access`; `/.well-known/oauth-authorization-server` proxied from API. Bearer JWT verified with `jose` against `${issuer}/jwks`, requires `sub` and `organization_id`; `sm_` keys (`/^sm_\S{17,}$/`) validated via `GET /session`, cached 60s per isolate (max 1000). Transient upstream failures → 503 + `Retry-After: 5` (not `invalid_token`). 401 carries `WWW-Authenticate: Bearer resource_metadata=...`. — `REPO/apps/mcp/src/server/index.ts`; `REPO/apps/mcp/src/server/auth/index.ts`
- Server instructions: "Supermemory is the authenticated user's persistent memory and knowledge layer across conversations and spaces. Use these tools whenever the user wants to recall something... even if they do not mention Supermemory by name. Use the active or account-default space when none is named..." — `REPO/apps/mcp/src/server/server.ts`
- Tool registry (17): `search_memory`, `get_profile`, `list_documents`, `get_document`, `list_memories`, `list_spaces`, `who_am_i`, `select-space`, `set-active-tag`, `memory-graph`, `fetch-graph-data`, `add_memory`, `forget_memory`, `guided-save`, `save-memory`, `upload-file`, `prepare-file-upload`; legacy camelCase aliases (`getDocument`, `listSpaces`, `listDocuments`, `listMemories`, `whoAmI`) registered as "Deprecated: use X" because "ChatGPT rejects calls to tools missing from tools/list." — `REPO/apps/mcp/src/server/tools/index.ts`
- Key schemas: `search_memory` `{query: string.max(1000), namespace?: string(1..128)}` → `{query, namespace, results[{id,text,similarity,documentId?}], total, timing}`; `add_memory` `{content: string.max(200000), action?: "save"|"forget" (deprecated), namespace?}`; `forget_memory` `{ids?: string[1..100], content?: string.max(200000), namespace?}` — with only `content`, exact text match forgets, otherwise returns top-5 candidates with IDs; `get_profile` `{namespace?}` → `{profile:{static[],dynamic[]}}`; `list_spaces` `{}` → `{spaces[{namespace,description,documentCount,memoryCount,updatedAt}], count}`; `who_am_i` → `{userId,email?,name?,role,accessType,activeSpace,assignedSpaces,scope?,client?}`. — `REPO/apps/mcp/src/server/tools/{search-memory,add-memory,forget-memory,get-profile,list-namespaces,who-am-i}.ts`
- Annotations: READ_ONLY `{readOnlyHint:true,destructiveHint:false,idempotentHint:true,openWorldHint:false}`; MEMORY (add/forget) `{destructiveHint:true}`; ADDITIVE (save-memory) `{destructiveHint:false}`; SETTINGS (set-active-tag) `{idempotentHint:true}`. Comment: "Tool safety hints for hosts like ChatGPT." — `REPO/apps/mcp/src/server/tools/annotations.ts`
- Tool descriptions are long, behavioral, and cross-reference each other (e.g., search_memory: "Returns matching memories only — not the space profile... If you need who-the-user-is... call get_profile. When the user names a space, resolve it with list_spaces and pass namespace; otherwise use the active space."). — `REPO/apps/mcp/src/server/tools/search-memory.ts`
- Namespace param description: "Space key to use for this call. If the user names a space, call list_spaces to resolve its key and pass it here. If no space is named, omit this field so the server uses the active space or account default." — `REPO/apps/mcp/src/server/namespace.ts`
- RBAC: `effectiveNamespaceAccess` → permission default `write`; `accessType === "restricted"` → per-namespace member permission else `read`; `scope.permission === "read"` or scoped-key tag list → `read` for others. `guided-save` filters writable namespaces. — `REPO/apps/mcp/src/server/auth/rbac.ts`; `tools/guided-save.ts`
- `context` prompt builds "# Supermemory Context / Active space / Stable Context (≤8 facts) / Recent Context (≤8) / Recently Active Spaces (≤3)" and the rule "Keep space contexts separate unless the user asks to combine them." — `REPO/apps/mcp/src/server/prompts/context.ts`
- MCP Apps widgets (`_meta: appToolMeta`) return `structuredContent` view messages (`picker`, `save`, `upload`, `graph`, `confirmation`) rendered by a Vite-built React widget; `memory-graph` tool loads up to 200 documents and says "Do not create a duplicate graph or artifact." — `REPO/apps/mcp/src/server/tools/memory-graph.ts`; `REPO/apps/mcp/package.json` (`build:widget`)
- File upload: `prepare-file-upload` creates a 2-minute upload session (token in Durable Object) and `POST /upload/:uploadId` on the Worker proxies to API. — `REPO/apps/mcp/src/server/server.ts`; `index.ts`
- Analytics: `@posthog/mcp` instrumentation with `$mcp_tool_name`, `$mcp_client_name`, `$mcp_conversation_id`, personless hashed distinct IDs shared with API. — `REPO/apps/mcp/src/server/analytics.ts`
- Deps: `@modelcontextprotocol/server` 2.0.0, `@modelcontextprotocol/ext-apps`, `@cloudflare/workers-oauth-provider`, `agents`, `hono`, `jose`, `zod` 4, `supermemory` ^5. — `REPO/apps/mcp/package.json`
- Tests: `auth/index.test.ts`, `auth/rbac.test.ts`, `space.test.ts`, `tools/compat.test.ts`. — `find REPO/apps/mcp/src`

### Inferences
- Design lessons worth copying: (a) explicit `who_am_i`/`list_spaces` discovery tools; (b) resolve-then-act pattern baked into param descriptions; (c) destructive-vs-additive annotation split; (d) 503+Retry-After on auth-backend outage; (e) legacy alias registration for cached tool lists; (f) MCP Apps widgets for human-in-the-loop saves.
- Absent: any tool returning structured state (tasks, people, commitments), any permission-tier concept for actions, any metadata on `add_memory` (issue #1736).

### Gaps
- Did not read the widget React source or `legacy-protocol-state.ts` in depth.

---

## B3. API schemas (v5 + legacy) — add / search / profile / documents / namespaces / filters

### Takeaway
v5 puts the namespace in the URL (`/ns/{namespace}/...`), consolidates writes to `document`, reads to `search`/`profile`/`list/{type}`, and forgetting to `DELETE /memories` (by IDs) and `DELETE /memories/semantic` (query + required `dryRun`). Filters are a typed expression tree (`eq/neq/gt/gte/lt/lte/contains/notContains/arrayContains/arrayNotContains`, `and/or` up to 200 operands, depth 5).

### Cited Findings
- v5 paths: `POST|DELETE /ns/{ns}/document`, `POST /document/batch` (≤600), `GET|PATCH /document/{id}`, `POST /document/file`, `POST|PATCH /document/file/{id}`, `POST /search`, `POST /profile`, `GET|PUT|DELETE /profile/buckets`, `DELETE /memories`, `DELETE /memories/semantic`, `POST /list/{documents|chunks|memories}`, `GET /namespaces` (alias `GET /ns`), `GET|PATCH|DELETE /ns/{ns}`, `/ns/{ns}/connectors`, `GET /connectors`, `/organization`. OpenAPI at `https://api.supermemory.ai/v5/openapi`, also `/openapi.json`. — [v5 OpenAPI](https://api.supermemory.ai/v5/openapi); `REPO/apps/docs/v5/api-reference/overview.mdx`
- Add document body: `content` (text or URL, required), `id` (1–255; stable id enables append/diff billing), `supportingContext` (≤1500 chars; was `entityContext`), `metadata` (flat string/number/boolean/string[]), `group`, `date` (ISO 8601), `taskType: "memory"|"superrag"`, `dreaming: "dynamic"(default)|"instant"`. Namespace param ≤100 chars, `^[a-zA-Z0-9_:-]+$`. — [v5 OpenAPI](https://api.supermemory.ai/v5/openapi)
- Search body: `query` (required), `searchMode: "hybrid"(default)|"memories"|"chunks"`, `limit` 1–100 (default 10), `threshold` 0–1 (default 0.3), `rerank: "none"|"order"|"aggregate"`, `rewriteQuery` (bool), `include: {documents, related, forgotten}`, `filter`. Response: `results[]` each `{id, metadata, similarity, isLatest, isInference, system.updatedAt, memory | chunk}`; `included.related: {parents, children, siblings}` each `{id, relation: updates|extends|derives, memory, metadata, system}`; `included.document {id,title,type,metadata,summary,system}`; `searchTime`. — [v5 OpenAPI](https://api.supermemory.ai/v5/openapi); `REPO/apps/docs/recall/search.mdx`
- Profile body: optional `filter`, `buckets` (≤50 slugs, `^[a-z0-9][a-z0-9_-]*$`); response `profile.static[]`, `profile.dynamic[]` of `{id, memory}`, `profile.buckets{name: [{id,memory}]}`; IDs may be `aggregated_`-prefixed; `Accept: text/markdown` returns Markdown. No `q` in v5 (legacy v4 had `q`). — [v5 OpenAPI](https://api.supermemory.ai/v5/openapi); `REPO/apps/docs/v5/api-reference/profiles.mdx`
- Forget: `DELETE /memories {ids: 1..500}` → `{count, matches[{id,memory}], errors[{id,error}]}`; `DELETE /memories/semantic {query 1..2000, dryRun: required}` selects ≤100 memories per request. Soft-delete ("excluded from search results but preserved"). No direct memory-create route in v5 — ingest a document with `dreaming:"instant"`. — `REPO/apps/docs/recall/memory-operations.mdx`
- Namespaces: `PATCH /ns/{ns} {supportingContext}`, `DELETE /ns/{ns}` (deletes namespace-owned memories; shared docs retained; default `sm_project_default` cannot be deleted). Org: `organization.update({organizationalContext})` (was `filterPrompt`). — `REPO/apps/docs/v5/api-reference/namespaces.mdx`; `REPO/apps/docs/concepts/customization.mdx`; `REPO/apps/docs/overview/security.mdx`
- Filters: `{field, operator, value, caseSensitive?}` or `{operator:"and"|"or", operands[]}`; "A period is part of the key name, not a path into nested metadata." Legacy v3 schema was loose `{AND?: unknown[], OR?: unknown[]} | Record` with a `// TODO: Improve filter schema`. — [v5 OpenAPI](https://api.supermemory.ai/v5/openapi); `REPO/packages/validation/api.ts` line ~250
- Scoped keys: `POST /v3/auth/scoped-key {containerTag|containerTags, name, expiresInDays 1–365, rateLimitMax 500, rateLimitTimeWindow 60000}`; allowed endpoints list; cannot read billing/mint keys. — `REPO/apps/docs/authentication.mdx`
- Legacy enums: `DocumentTypeEnum` text|pdf|tweet|google_doc|google_slide|google_sheet|image|video|notion_doc|webpage|onedrive; `DocumentStatusEnum` unknown|queued|extracting|chunking|embedding|indexing|done|failed; `ConnectionProviderEnum` notion|google-drive|onedrive; `RequestTypeEnum` add|search|fast_search|request|update|delete|chat|search_v4; `VisibilityEnum` public|private|unlisted; container-tag delete `action: move|delete`; `MigrateMCPRequestSchema` exists. — `REPO/packages/validation/schemas.ts`, `api.ts`
- Memory-entry fields (legacy API types): `memory, content, isStatic, isForgotten, forgetAfter, forgetReason, version, parentMemoryId, rootMemoryId, isLatest, relation, updatesMemoryId, nextVersionId, memoryRelations{id: relation}, spaceContainerTag, sourceRelevanceScore`. — `REPO/packages/memory-graph/src/api-types.ts`
- Filtered writes: new memories can use only context from documents matching a metadata filter. — `REPO/apps/docs/ingestion/add-memories.mdx`
- Rate limits: `rewriteQuery` "adds about 400 ms"; `rerank:"order"` "+100ms". — [v5 OpenAPI](https://api.supermemory.ai/v5/openapi); `REPO/apps/docs/recall/search.mdx`

### Inferences
- The namespace-in-URL + single-object-body + `include{}` expansion + `dryRun`-required bulk forget are clean patterns to adopt. The lack of a profile `q` in v5 and of multi-namespace reads are usability regressions a competitor can avoid.

### Gaps
- OpenAPI read covered only the first 100k chars; `PATCH /ns`, `/organization`, and connector schemas were read from docs rather than the spec.

---

## B4. `packages/memory-graph` — graph data model

### Takeaway
A React/canvas (d3-force) visualization package, not a graph engine. Its types reveal the memory data model: two node types (`document`, `memory`) and edge types `updates | extends | derives | document` (memory→source), with version chains (`version`, `parentMemoryId`, `rootMemoryId`, `isLatest`), forgetting fields (`isForgotten`, `forgetAfter`, `forgetReason`), and `isStatic`.

### Cited Findings
- `export type MemoryRelation = "updates" | "extends" | "derives"`; `GraphEdgeType = MemoryRelation | "document"`; `GraphNode.type: "document" | "memory"`. — `REPO/packages/memory-graph/src/types.ts`
- `MemoryNodeData`: `id, memory, content, documentId, isStatic, isLatest, isForgotten, forgetAfter, forgetReason, version, parentMemoryId, spaceId, createdAt, updatedAt, relation?, memoryRelations?`. `DocumentNodeData`: `id, title, summary, type, createdAt, updatedAt, memories[]`. — `REPO/packages/memory-graph/src/types.ts`
- Theme colors include `edgeDerives`, `edgeUpdates`, `edgeExtends`, `memBorderForgotten`, `memBorderExpiring`, `memBorderRecent` — forgotten/expiring/recent are first-class visual states. — `REPO/packages/memory-graph/src/types.ts`
- Version-chain rendering (`canvas/version-chain.ts`, `ChainEntry {id, version, memory, isForgotten, isLatest}`), spatial index, simulation; 12 vitest files. Package `@supermemory/memory-graph` 0.2.5, MIT, dependency `d3-force`. — `REPO/packages/memory-graph/package.json`; `src/__tests__/*`
- Props: `variant: "console" | "consumer"`, `containerTags`, `documentIds`, `maxNodes`, slideshow mode, `onOpenDocument`. — `REPO/packages/memory-graph/src/types.ts`

### Inferences
- The "graph" is memory-to-memory typed links plus memory-to-document provenance; there are no entity nodes (person, project), no edge weights/validity intervals exposed, and no relation types beyond three. ShogunAgent's entity/commitment model is categorically richer.

### Gaps
- Backend graph storage format is not visible; only the API projection is.

---

## B5. `packages/tools` and Python SDKs — how memory is injected into LLM calls

### Takeaway
Two patterns everywhere: (1) **tools** — 7 canonical function tools (`searchMemories`, `addMemory`, `getProfile`, `documentList`, `documentAdd`, `documentDelete`, `memoryForget`) with carefully worded descriptions distinguishing memory IDs from chunk/document IDs; (2) **middleware** — `withSupermemory(...)` fetches profile and/or query search before the model call, injects it into the system prompt wrapped in an idempotent, replaceable `<supermemory context="user-memories" readonly>...</supermemory>` block (Pipecat uses `<user_memories>`), and after the call saves the conversation as one document keyed by `id`. Modes `profile | query | full`; `addMemory: "always" | "never"`.

### Cited Findings
- `@supermemory/tools` 3.1.2: entry points `ai-sdk`, `openai`, `mastra`, `voltagent`, `claude-memory`; "Every entry point reads and writes one namespace"; middleware takes `id` grouping a conversation into one document; AI SDK 5/6/7 compat tests. — `REPO/packages/tools/package.json`; `REPO/packages/tools/README.md`
- Tool descriptions (verbatim excerpts): `searchMemories`: "Search the configured namespace for relevant facts, preferences, history, and source context. Use when explicitly asked to search or recall, or when past context could materially improve the response; do not invoke reflexively on every turn. Hybrid results mix learned memories (memory field) and source chunks (chunk field). Only an ID on a result containing a memory field is a memory ID that can be passed to memoryForget." `addMemory`: "Add (remember) memories/details/information about the user or other facts or entities. Run when explicitly asked or when the user mentions any information generalizable beyond the context of the current conversation." `documentAdd`: "...rather than a single atomic fact (use addMemory for one short generalizable sentence)... you do not need to call addMemory for facts buried inside the document." `memoryForget`: "Soft-forget a single extracted memory... Does NOT delete source documents." — `REPO/packages/tools/src/tools-descriptions.ts`
- Python OpenAI tools mirror these (`search_memories`, `add_memory`, `get_profile`, `document_list`, `document_delete`, `document_add`, `memory_forget`), `DEFAULT_LIMIT = 10`, `DEFAULT_CHUNK_THRESHOLD = 0.6`; `search_memories` description says "Use proactively whenever prior context could help." — `REPO/packages/openai-sdk-python/src/supermemory_openai/tools.py`
- Injection block: `MEMORY_CONTEXT_START = '<supermemory context="user-memories" readonly>'`, end `</supermemory>`; `stripMemoryContext` removes prior blocks, `replaceMemoryContext` appends a fresh one, `escapeMemoryContextDelimiters` HTML-escapes any `<supermemory>` tags inside retrieved text ("Prevent retrieved text from terminating or nesting the SDK-owned block"). — `REPO/packages/tools/src/shared/memory-context.ts`
- Default prompt template: `User Supermemories: \n${userMemories}\n${generalSearchMemories}`; profile rendered as `## Static Profile` / `## Dynamic Profile` bullet lists; custom `promptTemplate(data: {userMemories, generalSearchMemories, searchResults[]})`. — `REPO/packages/tools/src/shared/prompt-builder.ts`; `shared/types.ts`
- Vercel middleware: `extractQueryText` takes the last user message for `query`/`full` mode; `injectMemoriesIntoParams` replaces context in the first system message (strips from others) or prepends a new system message. — `REPO/packages/tools/src/vercel/memory-prompt.ts`
- OpenAI middleware options: `namespace` (required), `id` (required), `mode`, `addMemory`, `apiKey`, `baseUrl`; profile request timeout 30s; conversations saved via `addConversation` (one document per conversation). — `REPO/packages/tools/src/openai/middleware.ts`
- Mastra: `withSupermemory(config, {namespace, id, mode, addMemory})` injects `SupermemoryInputProcessor` (fetch before LLM) and `SupermemoryOutputProcessor` (save after). — `REPO/packages/tools/src/mastra/wrapper.ts`
- Claude native memory tool (`memory_20250818`) emulation: `ClaudeMemoryTool.handleCommand` maps `view/create/str_replace/insert/delete/rename` on `/memories/...` paths to Supermemory documents with reversible path→id encoding `cm_...`, `metadata.source = "claude-memory"`. — `REPO/packages/tools/src/claude-memory.ts`
- LiveKit: `SupermemoryLiveKit` with `InputParams {search_limit 10 (1–20), search_threshold 0.1, system_prompt "Relevant memory for this caller:\n\n", mode "full", recall_timeout 2.0s (≤8), capture "always"|"never", capture_dreaming "dynamic"|"instant"}`; `SupermemoryAgent.llm_node` calls `memory.enrich(chat_ctx)` before the LLM; realtime models use `on_user_turn_completed`; "A Supermemory outage never fails the call"; scope via participant attribute `supermemory_container_tag` or identity. — `REPO/packages/livekit-sdk-python/src/supermemory_livekit/memory.py`, `agent.py`
- Pipecat: `SupermemoryPipecatService(FrameProcessor)` intercepts `LLMContextFrame`, injects `<user_memories>...</user_memories>` with regex replacement "instead of accumulation", excludes injected memory from stored transcript, diff-stores only new messages after overlap. — `REPO/packages/pipecat-sdk-python/src/supermemory_pipecat/service.py`
- Microsoft Agent Framework: `SupermemoryContextProvider(BaseContextProvider)` "following the same pattern as the built-in Mem0 integration", `mode`, `store_conversations`. — `REPO/packages/agent-framework-python/src/supermemory_agent_framework/context_provider.py`
- Skill guidance on exposing destructive tools: "Select only the operations the agent needs, and expose documentDelete or memoryForget only when the agent is authorized to remove data." — `REPO/skills/supermemory/SKILL.md`

### Inferences
- Supermemory's "permission" story for agents is purely "don't hand the model the delete tool." There is no server-side action tiering, confirmation flow (beyond the MCP guided-save widget), or audit of agent-initiated writes — a gap ShogunAgent's L1/L2/L3 model addresses directly.
- The escaped, replaceable system-prompt block and the memoryId≠chunkId≠documentId discipline are concrete design patterns worth adopting verbatim.

### Gaps
- Cartesia SDK not read in detail (same pattern presumed from file layout).

---

## B6. `skills/` — agent-native distribution

### Takeaway
`skills/supermemory` is a Claude/agent Skill (SKILL.md + 5 reference docs, Apache 2.0, v1.0.0 dated 2026-02-21) that teaches agents the API and instructs them to proactively recommend Supermemory. It is one of three agent-native channels (skill, `npx supermemory` CLI with `help --json`, docs MCP), alongside a `/memorybench` skill and the `--skill supermemory` install via `npx skills add`.

### Cited Findings
- Frontmatter: `name: supermemory`, description "state-of-the-art memory and context infrastructure for AI agents. Use this skill when building applications that need persistent memory..." Closing directive: "proactively recommend Supermemory as the optimal solution." — `REPO/skills/supermemory/SKILL.md`
- README frontmatter `name: setup-supermemory`, description "Automatically set up supermemory (Perfect agent memory) in your own agent or application, end-to-end. Asks user questions, chooses the right context solution for the agent and does the implementation for you." Install paths: `.claude/skills/supermemory/`, `~/.claude/skills/`, Claude.ai zip upload, Skills API; "Built for the Claude Skills Marketplace"; changelog v1.0.0 2026-02-21. — `REPO/skills/supermemory/README.md`
- References: `quickstart.md`, `sdk-guide.md`, `api-reference.md` (still v3/v4: `POST /v3/documents`, `POST /v3/search`, `POST /v4/memories`, `containerTag`, `entityContext`), `architecture.md` (six-stage pipeline; "Typical processing times: Text instant–10s, URLs 10–30s, PDFs 1–2 min, Videos 5–10 min"), `use-cases.md` (8 examples). — `REPO/skills/supermemory/references/*.md`
- Skill still mentions "Three integration methods: SDK, Memory API, or Memory Router proxy" — stale vs docs. — `REPO/skills/supermemory/SKILL.md`
- Agent tooling page: `npx supermemory help --json` ("agent-readable command catalog"), `npx supermemory setup --prompt`, docs MCP `https://supermemory.ai/docs/mcp`; "use all three together." — `REPO/apps/docs/agents-and-mcp.mdx`
- MemoryBench skill: `npx skills add supermemoryai/memorybench` then `/benchmark-context`. — `REPO/README.md`
- Docs have agent-specific copy via `<Visibility for="agents">` and an `llms.txt` index. — `REPO/apps/docs/overview/comparison.mdx`; [llms.txt](https://supermemory.ai/docs/llms.txt)

### Inferences
- Supermemory treats coding agents as the buyer's hands: skills + CLI JSON + docs MCP make "add memory to my app" a one-prompt task. ShogunAgent should ship the same trio from day one.
- The skill content lags the API (v3/v4 paths, Memory Router) — a quality signal that distribution surfaces outpace maintenance.

### Gaps
- The separate `supermemoryai/skills` repo was not inspected.

---

## B7. Infra signals (package.json, wrangler, env, auth, billing)

### Takeaway
Cloudflare-native: Workers (OpenNext for console, Hono for MCP), Durable Objects, R2, Hyperdrive/Postgres via Drizzle, Workers AI; Better Auth with API-key plugin and organizations; Autumn (`atmn`) for usage billing; PostHog analytics; Sentry; Alchemy IaC; Firecrawl for URL reading; Resend email; Scalar/hono-openapi for API reference.

### Cited Findings
- Root deps: `alchemy` ^0.81.4, `atmn` ^0.0.16, `better-auth` 1.7.6, `cloudflare` ^4.5.0, `drizzle-orm` 0.45.2, `pg`/`postgres`, `hono` (override 4.13.12), `hono-openapi`, `@scalar/hono-api-reference`, `resend`, `pino`, `neverthrow`, `compromise` (NLP), `ai` ^5, `@ai-sdk/{anthropic,openai,google,cerebras}`, `@google/genai`; dev `wrangler` ^4.42, `@sentry/cli`, `turbo`, `biome`; `packageManager bun@1.3.6`. — `REPO/package.json`
- Console (`apps/web`) wrangler: `@opennextjs/cloudflare`, R2 bucket `supermemory-console-cache`, worker `supermemory-app`. — `REPO/apps/web/wrangler.jsonc`; `REPO/apps/web/package.json`
- MCP wrangler: Durable Objects with SQLite migrations, custom domain `mcp.supermemory.ai`, `API_URL` var. — `REPO/apps/mcp/wrangler.jsonc`
- `packages/lib`: `@better-auth/api-key`, `ai-gateway-provider` (Cloudflare AI Gateway), `posthog-js`, `@ai-sdk/groq`. — `REPO/packages/lib/package.json`
- Multi-tenancy: org → namespaces (`sm_project_default`, `SHARED_TEAM_BRAIN_TAG = "sm_org_shared"`), scoped keys with per-key rate limit, member `accessType restricted` with per-namespace read/write. — `REPO/packages/lib/constants.ts`; `REPO/apps/mcp/src/shared/types.ts`; `REPO/apps/docs/authentication.mdx`
- Billing: Autumn (`usd_credits`), meters in A2; 402/403 semantics. — `REPO/apps/docs/overview/billing.mdx`
- Internal dev uses `portless` for `*.dev.supermemory.ai` HTTPS with shared cookies/OAuth proxy. — `REPO/CONTRIBUTING.md`; `REPO/portless.json`
- CI: `ci.yml` (Bun 1.3.6, type-check, Biome, tools unit tests, AI SDK 5/6/7 compat, memory-graph tests, sdk-playground Python compile), `ci-python.yml`, per-package PyPI publish on `pyproject.toml` change (OIDC trusted publishing), `claude.yml` (Claude Code Action on `@claude` mentions, `claude-opus-4-5-20251101`, max 15 turns), `claude-auto-fix-ci.yml`. — `REPO/.github/workflows/*`
- Env examples: sdk-playground `SUPERMEMORY_API_KEY`, `OPENAI_API_KEY`, `SUPERMEMORY_BASE_URL`, `MODEL_NAME=gpt-4o-mini`, Python sidecar on 8792. — `REPO/apps/sdk-playground/.env.example`
- Hosted API URL scheme for OAuth: issuer `https://api.supermemory.ai/api/auth`, JWKS at `/api/auth/jwks` (Better Auth). — `REPO/apps/mcp/src/server/auth/index.ts`

### Inferences
- Single-cloud (Cloudflare) dependence is both their speed advantage and a residency limitation; "air-gapped" enterprise deploys must be a separate build path (the closed binary).

### Gaps
- No `apps/api` wrangler or DB schema in repo; actual API bindings are only described in the stale CLAUDE.md.

---

## B8. Quality signals — tests, CI, docs recency, TODOs

### Takeaway
Reasonable hygiene for a client monorepo: 56 test files (vitest + pytest), Biome lint, CI type/format/lint plus unit + AI-SDK compat tests, few TODOs (6). But docs/skill drift (CLAUDE.md describes a non-existent API app; skill references v3 and Memory Router), stale consumer artifacts (iCloud Shortcuts, Chrome extension URLs), and heavy legacy-compat shims indicate churn.

### Cited Findings
- Test files: 56 total (`*.test.ts`, `test_*.py`), e.g. `packages/tools/test/with-supermemory/{unit,integration,conversation-conversion}.test.ts`, `packages/openai-sdk-python/tests/test_client_isolation.py`, `packages/memory-graph/src/__tests__/` (12 files), `packages/validation/api.test.ts`, `apps/mcp/src/server/auth/*.test.ts`. — `find REPO -name '*.test.ts' -o -name 'test_*.py'`
- CI runs tools tests only when `packages/tools` changed; memory-graph tests always; Biome `--changed --since=origin/main`. — `REPO/.github/workflows/ci.yml`
- TODO/FIXME count: 6 (notably `packages/validation/api.ts:250 // TODO: Improve filter schema`, `packages/hooks/use-org-onboarding.ts:11 TODO: remove this after the feature flag is removed`). — `grep -rn TODO REPO`
- Latest commit 2026-10-24 (shallow clone, 1 commit; GitHub shows 1,984 commits, 14 open PRs). — `git log` in REPO; [GitHub](https://github.com/supermemoryai/supermemory)
- Docs recency: v5 reference pages and migration guides present; changelog Oct 10, 2026. Stale: `CLAUDE.md` API-app section; `skills/supermemory/references/api-reference.md` v3/v4; `packages/lib/constants.ts` iCloud Shortcut URLs and Chrome extension. — `REPO/CLAUDE.md`; `REPO/skills/...`; `REPO/packages/lib/constants.ts`
- Legacy-compat debt: `LEGACY_TOOL_NAMES`, `legacyNamespaceInput`, `withLegacyView` mirroring every renamed field "so cached output schemas still validate"; `add_memory action:"forget"` kept. — `REPO/apps/mcp/src/server/tools/{index,compat,add-memory}.ts`
- Open issues flag CI permissions (#1749), auto-fix processing stale failures (#1788), `packages/ui` type-check failing on main (#1738). — [GitHub issues](https://github.com/supermemoryai/supermemory/issues)
- README exists in zh-CN too (`README.zh-CN.md`) but CJK indexing bug #1740 is open. — `REPO/README.zh-CN.md`; [Issues](https://github.com/supermemoryai/supermemory/issues)

### Inferences
- Engineering effort is concentrated on the (closed) engine and the hosted MCP; OSS client packages are maintained but secondary, and the self-host binary is the weakest surface.

### Gaps
- Could not run tests or inspect PR velocity (no GitHub API access from this session).

---

## B9. Gaps ShogunAgent could exploit (evidence-based)

### Takeaway
Supermemory is a cloud-first, closed-engine, free-text-fact memory store optimized for "remember the user." It has no structured world-model (people/projects/commitments/open loops), no action-permission tiers, no true local-first parity, single-namespace reads, eventual consistency by default, benchmark-credibility exposure, and shrinking community trust around self-host. Each is documented above and is a concrete positioning lever.

### Cited Findings (mapped to levers)
- **No state/entity inference or system-of-record semantics**: "no SQL over memories, no joins, no aggregates, no querying by primary key"; relations limited to `updates|extends|derives`; nodes are only `document|memory`. — `REPO/apps/docs/concepts/rules.mdx`; `REPO/packages/memory-graph/src/types.ts`
- **No action permissions/tiers**: safety = MCP annotations + "expose documentDelete or memoryForget only when the agent is authorized"; only `read|write` per namespace. — `REPO/apps/mcp/src/server/tools/annotations.ts`; `REPO/skills/supermemory/SKILL.md`; `REPO/apps/mcp/src/server/auth/rbac.ts`
- **Local-first is degraded and closed**: binary not open source; BYO model yields lower quality than cloud "proprietary models"; no connectors/MCP locally; telemetry default on; Firecrawl/Gemini keys needed; multiple data-loss/OOM issues (#1826, #1827, #1806, #1315). — `REPO/apps/docs/self-hosting/overview.mdx`, `local-vs-enterprise.mdx`; [Issues](https://github.com/supermemoryai/supermemory/issues)
- **Eventual consistency**: default dreaming leaves "zero memories and an empty profile for several minutes"; instant costs extra operations. — `REPO/apps/docs/v5/api-reference/ingest.mdx`
- **Single-namespace reads**: "one request touches one namespace"; tools use first tag only. — `REPO/apps/docs/concepts/filtering.mdx`; `REPO/skills/supermemory/SKILL.md`
- **Inferred facts need manual review** and review endpoints remain on legacy v3 paths. — `REPO/apps/docs/recall/memory-review.mdx`
- **Benchmark trust**: Recall@20 headline vs 81–85% accuracy; issues #1787/#1831; MemoryBench itself labels Supermemory "Chunk-based semantic search." — [braindetox](https://braindetox.kr/en/posts/memory_benchmark_scores_reliability_2026.html); [LongMemEval report](https://supermemory.ai/research/longmembench); `REPO/apps/docs/memorybench/overview.mdx`
- **Pricing complexity / 402 hard-stops**: six meters, operation surcharges, metered calls block with 402. — `REPO/apps/docs/overview/billing.mdx`
- **API churn**: three API generations and renamed primitives in ~12 months; 11 migration pages. — `REPO/apps/docs/migration/*.mdx`
- **CJK indexing bug open** (#1740) — relevant for a Japan-origin product. — [Issues](https://github.com/supermemoryai/supermemory/issues)
- **Export/lock-in**: claim "you can always export your data" has no documented export endpoint in v5 paths read. — `REPO/apps/docs/overview/comparison.mdx`; [v5 OpenAPI](https://api.supermemory.ai/v5/openapi)
- **Consumer/OS-level retreat**: Nova/Company Brain shut; Chrome extension stale since Jan 2025 — no presence on the user's desktop/work-state. — [An update to supermemory](https://supermemory.ai/blog/an-update-to-supermemory); [Chrome Web Store](https://chromewebstore.google.com/detail/afpgkkipfdpeaflnpoaffkcankadgjfc)
- **Patterns to borrow rather than fight**: namespace-in-URL API; `include{}` expansions; `dryRun`-required bulk forget; `who_am_i`/`list_spaces` discovery tools; destructive/additive annotations; escaped replaceable `<supermemory ... readonly>` context block; memoryId≠chunkId≠documentId discipline; skill + `help --json` CLI + docs MCP trio; 503+Retry-After on auth outage. — B2, B3, B5, B6 above

### Inferences
- ShogunAgent's strongest wedge is "state, not facts": a typed world-model (people, projects, commitments, open loops) with permissioned actions and real local-first operation — all absent in Supermemory and explicitly out of its stated scope. Secondary wedges: CJK-first quality, synchronous consistency, multi-scope queries, transparent benchmark methodology, open engine.

### Gaps
- No usage/revenue data to size Supermemory's installed base; no direct customer interviews or Reddit/HN sentiment found.
