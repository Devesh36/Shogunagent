"""LangGraph adapter: wrap a graph node that calls the model (sync or async).

Before the node runs it sees ``state["messages"]`` with the block in its first system message (or
a new one prepended; any earlier block replaced). After it returns, the last human message and the
AI reply it produced are ingested under the graph's ``thread_id`` (or ``thread_key``)::

    from langgraph.graph import StateGraph, MessagesState, START
    from shogunagent.adapters.langgraph import with_shogun_agent

    def call_model(state):
        return {"messages": [model.invoke(state["messages"])]}

    graph = StateGraph(MessagesState).add_node("agent", with_shogun_agent(call_model)).add_edge(START, "agent").compile()

Only the messages handed to the node change; the graph state is never written with the block. A
system message is replaced by a plain ``{"role": "system", "content": …}`` dict, which LangChain
chat models accept. No import of LangChain.
"""

from __future__ import annotations

import inspect
from typing import TYPE_CHECKING, Any, Callable, Optional

if TYPE_CHECKING:  # never imported at run time: the adapter does not depend on LangChain
    from langchain_core.runnables import RunnableConfig

from ..context import replace_context, strip_context
from ..middleware import AsyncShogunAgentMemory, ShogunAgentMemory, options_from, role_of, text_of


def _content(m: Any) -> Any:
    return m.get("content") if isinstance(m, dict) else getattr(m, "content", None)


def inject_into_messages(messages: list[Any], block: str) -> list[Any]:
    first = next((i for i, m in enumerate(messages) if role_of(m) == "system"), -1)
    if first < 0:
        return [{"role": "system", "content": block}, *messages] if block else list(messages)
    out = []
    for i, m in enumerate(messages):
        if role_of(m) != "system":
            out.append(m)
            continue
        text = text_of(_content(m))
        new = replace_context(text, block) if i == first else strip_context(text)
        out.append(m if new == text else {"role": "system", "content": new})
    return out


def _last_human(messages: list[Any]) -> str:
    for m in reversed(messages):
        if role_of(m) == "human":
            return text_of(_content(m))
    return ""


def _messages_of(state: Any) -> list[Any]:
    return list(state.get("messages", []) if isinstance(state, dict) else getattr(state, "messages", []) or [])


def _with_messages(state: Any, messages: list[Any]) -> Any:
    if isinstance(state, dict):
        return {**state, "messages": messages}
    clone = __import__("copy").copy(state)
    clone.messages = messages
    return clone


def _reply(update: Any) -> Any:
    produced = update.get("messages") if isinstance(update, dict) else getattr(update, "messages", None)
    if produced is None:
        return None
    items = produced if isinstance(produced, list) else [produced]
    for m in reversed(items):
        if role_of(m) == "ai":
            return m
    return None


def _thread(config: Any) -> str | None:
    if isinstance(config, dict):
        tid = (config.get("configurable") or {}).get("thread_id")
        return tid if isinstance(tid, str) else None
    return None


def with_shogun_agent(node: Callable[..., Any], **options: Any) -> Callable[..., Any]:
    """Wrap a node ``(state, config=None) -> update``.

    The wrapper's ``config`` parameter is annotated ``Optional[RunnableConfig]`` (as a string,
    thanks to ``from __future__ import annotations``): LangGraph injects the run config, and with it
    the ``thread_id`` the turn is ingested under, only into a parameter annotated that way (or not
    at all). Found by the real-framework test (E36b); with ``Any`` every thread landed under
    ``langgraph:default``.
    """
    opts = options_from(options)
    takes_config = len(inspect.signature(node).parameters) >= 2

    def call(state: Any, config: Any) -> Any:
        return node(state, config) if takes_config else node(state)

    def _remember_kw(user: str, update: Any, config: Any) -> dict[str, Any] | None:
        reply = _reply(update)
        if reply is None:
            return None
        rid = reply.get("id") if isinstance(reply, dict) else getattr(reply, "id", None)
        return {"user": user, "assistant": text_of(_content(reply)), "turn_id": rid if isinstance(rid, str) else None,
                "thread_key": opts.thread_key or _thread(config)}

    if inspect.iscoroutinefunction(node):
        amemory = AsyncShogunAgentMemory(opts, "langgraph")

        async def wrapped(state: Any, config: Optional[RunnableConfig] = None) -> Any:  # noqa: UP045
            messages = _messages_of(state)
            user = _last_human(messages)
            block = await amemory.block(user)
            update = await call(_with_messages(state, inject_into_messages(messages, block)), config)
            kw = _remember_kw(user, update, config)
            if kw:
                await amemory.remember(**kw)
            return update

        return wrapped

    memory = ShogunAgentMemory(opts, "langgraph")

    def wrapped_sync(state: Any, config: Optional[RunnableConfig] = None) -> Any:  # noqa: UP045
        messages = _messages_of(state)
        user = _last_human(messages)
        block = memory.block(user)
        update = call(_with_messages(state, inject_into_messages(messages, block)), config)
        kw = _remember_kw(user, update, config)
        if kw:
            memory.remember(**kw)
        return update

    return wrapped_sync
