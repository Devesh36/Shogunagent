# shogunagent (Python)

The Python client of ShogunAgent, the state layer for AI agents: people, projects, commitments and open loops, each with a confidence band and the quote that proves it. Python ≥ 3.10, `httpx` is the only dependency, typed with dataclasses. Sync (`ShogunAgent`) and async (`AsyncShogunAgent`) clients with the same surface.

## Quick start

```python
from shogunagent import ShogunAgent, HOSTED_BASE_URL, PaymentRequiredError

sa = ShogunAgent(api_key="sa_live_…", base_url=HOSTED_BASE_URL)  # or ShogunAgent(): local sidecar on 127.0.0.1:7464
recall = sa.recall("what do I owe Oren?", budget_tokens=1500)    # free
for item in recall.items:
    print(item.band, item.text, item.quote)                      # "medium" = say "possibly"
try:
    sa.ingest(turns=[{"role": "user", "content": "Plan is due Friday"}], thread_key="task-42")  # idempotency key generated
except PaymentRequiredError as e:
    print("top up at", e.top_up_url, e.payment)
```

## Middleware: `with_shogun_agent()`

Before each model call it recalls (query = the last user message) and puts one escaped, replaceable `<shogunagent context="state-of-work" readonly>…</shogunagent>` block in the system prompt: High items as facts, Medium items marked `possibly:`, Low items never. An earlier block is replaced, never duplicated, and tags inside recalled text are escaped. After the call it ingests the turn under `thread_key` with an idempotency key derived from the turn id (the same key the TypeScript SDK derives). A ShogunAgent outage never fails the model call: errors go to `on_error` (a 402 is surfaced once as a `UserWarning` when no hook is set).

| Module | Framework | Usage |
|---|---|---|
| `shogunagent.adapters.openai_agents` | OpenAI Agents SDK | `run = with_shogun_agent(Runner.run); await run(agent, "…")` (also `Runner.run_sync`; composes with your `call_model_input_filter`) |
| `shogunagent.adapters.claude_agent_sdk` | Claude Agent SDK | `q = with_shogun_agent(query); async for m in q(prompt="…", options=ClaudeAgentOptions())` |
| `shogunagent.adapters.langgraph` | LangGraph | `graph.add_node("agent", with_shogun_agent(call_model))` (sync or async node) |
| `shogunagent.adapters.crewai` | CrewAI | `crew = with_shogun_agent(Crew(...)); crew.kickoff(inputs=…)` (the block goes into each agent's backstory) |

Options: `budget_tokens` (default 2000), `thread_key`, `ingest=False`, `recall=False`, `namespace="world" | "agent_scratch"`, `include_scratch`, `actor`, `source`, `on_error`, and `client` / `api_key` / `base_url`.

## Client surface

`recall()`, `ingest()` (key generated when absent), `context_pack()`, `state.commitments.list()`, `state.open_loops.list()`, `state.people.list() / get(id)`, `state.projects.list() / get(id)`, `actions.execute()` (returns `ActionResult`, `InputRequired` — L2, show `input_requests` then `respond(True)` — `ApprovalRequired` — L3, hand `approval.url` to the human then `wait()` — or `ActionDeclined`), `actions.status()`, `actions.wait_for_approval()`, `router.discover() / inspect() / run()`, `quote()`, `handoff()` (`POST /v1/handoff`: a typed state-of-work object plus its A2A data part) and `accept_handoff()` (`handoff.accept`: the core validates a received handoff and stores it in **your** `agent_scratch` namespace, never the user's world).

Errors: `AuthenticationError` (401), `PaymentRequiredError` (402: `payment`, `payment_required_header`, `top_up_url`, `shortfall_micro_usd`), `PermissionDeniedError` (403), `RateLimitError` (429, `retry_after`), `ServiceUnavailableError` (5xx), `ToolError`, `TransportError`, `HandoffError`; each with `code` and `next_action`.

## Develop

```bash
cd packages/sdk-py
python3 -m pip install -e '.[test]'
python3 -m pytest -q     # httpx.MockTransport, no network
```
