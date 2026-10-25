"""E36b: ``with_shogun_agent(node)`` inside a real LangGraph graph (``StateGraph(MessagesState)``
with an ``InMemorySaver`` checkpointer), the node calling a LangChain chat model that records the
messages it received. No provider, no network."""

from __future__ import annotations

import asyncio
from typing import Any

from conftest import PAYMENT_402, Fake, memory_fake, recall_payload, tool_text
from frameworks_support import block_count, no_network, require  # noqa: F401  (no_network: autouse fixture)
from shogunagent import PaymentRequiredError
from shogunagent.adapters import langgraph as adapter

graph_mod = require("langgraph.graph")
checkpoint = require("langgraph.checkpoint.memory")
chat_models = require("langchain_core.language_models.chat_models")
messages_mod = require("langchain_core.messages")
outputs = require("langchain_core.outputs")


class RecordingModel(chat_models.BaseChatModel):  # type: ignore[misc,name-defined]
    """A chat model that answers a fixed reply and records what reached it."""

    seen: list = []
    reply: str = "I will draft the plan."

    @property
    def _llm_type(self) -> str:
        return "recording"

    def _generate(self, messages: list, stop: Any = None, run_manager: Any = None, **kw: Any) -> Any:
        self.seen.append([(m.type, str(m.content)) for m in messages])
        message = messages_mod.AIMessage(content=self.reply)
        return outputs.ChatResult(generations=[outputs.ChatGeneration(message=message)])


def build(fake: Fake, model: RecordingModel, use_async: bool = False, **opts: Any) -> Any:
    counter = {"n": 0}

    def call_model(state: Any) -> Any:
        counter["n"] += 1
        answer = model.invoke(state["messages"])
        return {"messages": [messages_mod.AIMessage(content=answer.content, id=f"ai-{counter['n']}")]}

    async def acall_model(state: Any) -> Any:
        counter["n"] += 1
        answer = await model.ainvoke(state["messages"])
        return {"messages": [messages_mod.AIMessage(content=answer.content, id=f"ai-{counter['n']}")]}

    client = fake.aclient() if use_async else fake.client()
    node = adapter.with_shogun_agent(acall_model if use_async else call_model, client=client, **opts)
    builder = graph_mod.StateGraph(graph_mod.MessagesState)
    builder.add_node("agent", node)
    builder.add_edge(graph_mod.START, "agent")
    builder.add_edge("agent", graph_mod.END)
    return builder.compile(checkpointer=checkpoint.InMemorySaver())


def test_invoke_recalls_before_and_ingests_after_under_thread_id() -> None:
    fake = memory_fake()
    model = RecordingModel(seen=[])
    app = build(fake, model)
    config = {"configurable": {"thread_id": "thread-7"}}
    out = app.invoke({"messages": [messages_mod.SystemMessage("You are helpful."), messages_mod.HumanMessage("What do I owe Oren?")]}, config)

    saw = model.seen[0]
    assert saw[0][0] == "system" and saw[0][1].startswith("You are helpful.")
    assert block_count(saw[0][1]) == 1
    assert "send the rollout plan by Friday" in saw[0][1]
    assert "SECRET-LOW-ITEM" not in str(saw)
    stored = " ".join(str(m.content) for m in app.get_state(config).values["messages"])
    assert "<shogunagent" not in stored, "the block must not be written into the graph state"
    assert out["messages"][-1].type == "ai"
    assert [c.headers.get("mcp-name") for c in fake.calls] == ["memory.recall", "memory.ingest"]
    args = fake.tool_calls("memory.ingest")[0].body["params"]["arguments"]
    assert args["thread_key"] == "thread-7"
    assert args["turns"] == [{"role": "user", "content": "What do I owe Oren?"}, {"role": "assistant", "content": "I will draft the plan."}]
    assert args["idempotency_key"].startswith("turn:")


def test_async_graph_replaces_the_block_on_the_next_turn() -> None:
    fake = memory_fake()
    model = RecordingModel(seen=[])
    app = build(fake, model, use_async=True)
    config = {"configurable": {"thread_id": "thread-8"}}

    async def go() -> None:
        await app.ainvoke({"messages": [messages_mod.SystemMessage("sys"), messages_mod.HumanMessage("first")]}, config)
        await app.ainvoke({"messages": [messages_mod.HumanMessage("second")]}, config)

    asyncio.run(go())
    assert block_count(str(model.seen[1])) == 1
    assert [c.body["params"]["arguments"]["query"] for c in fake.tool_calls("memory.recall")] == ["first", "second"]
    keys = [c.body["params"]["arguments"]["idempotency_key"] for c in fake.tool_calls("memory.ingest")]
    assert len(keys) == 2 and keys[0] != keys[1]


def test_402_on_ingest_is_surfaced_once_and_the_graph_completes() -> None:
    fake = Fake().tool("memory.recall", tool_text(recall_payload())).tool("memory.ingest", PAYMENT_402)
    errors: list = []
    app = build(fake, RecordingModel(seen=[]), on_error=lambda e, p: errors.append((e, p)))
    out = app.invoke({"messages": [messages_mod.HumanMessage("q")]}, {"configurable": {"thread_id": "t"}})
    assert out["messages"][-1].type == "ai"
    assert len(fake.tool_calls("memory.ingest")) == 1
    assert len(errors) == 1 and isinstance(errors[0][0], PaymentRequiredError) and errors[0][1] == "ingest"
    assert errors[0][0].code == "payment_required"
