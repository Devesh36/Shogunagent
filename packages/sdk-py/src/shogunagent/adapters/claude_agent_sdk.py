"""Claude Agent SDK (Python, ``claude-agent-sdk``) adapter.

Wrap ``query`` once. A string prompt is recalled first and the block goes into
``options.system_prompt`` (a string is replaced in place; the ``claude_code`` preset gets it in
``append``); every message is passed through; when the stream ends the prompt and the final
``ResultMessage.result`` are ingested with an idempotency key derived from the result's id::

    from claude_agent_sdk import query, ClaudeAgentOptions
    from shogunagent.adapters.claude_agent_sdk import with_shogun_agent

    q = with_shogun_agent(query, thread_key="task-42")
    async for message in q(prompt="What is still open with Oren?", options=ClaudeAgentOptions()):
        print(message)

No import of ``claude_agent_sdk``: options may be its dataclass, a dict, or ``None``.
"""

from __future__ import annotations

import dataclasses
from typing import Any, AsyncIterator, Callable

from ..context import replace_context
from ..middleware import AsyncShogunAgentMemory, options_from, text_of


def inject_system_prompt(system: Any, block: str) -> Any:
    """Put the block in a system prompt of either shape (string or preset dict)."""
    if system is None:
        return block or None
    if isinstance(system, str):
        return replace_context(system, block)
    if isinstance(system, dict):
        append = replace_context(system.get("append") or "", block)
        return {**system, **({"append": append} if append else {})}
    return system


def _with_system_prompt(options: Any, block: str) -> Any:
    if options is None:
        return {"system_prompt": block} if block else None
    if isinstance(options, dict):
        sp = inject_system_prompt(options.get("system_prompt"), block)
        return {**options, **({"system_prompt": sp} if sp is not None else {})}
    sp = inject_system_prompt(getattr(options, "system_prompt", None), block)
    if sp is None:
        return options
    if dataclasses.is_dataclass(options):
        return dataclasses.replace(options, system_prompt=sp)
    clone = __import__("copy").copy(options)
    clone.system_prompt = sp
    return clone


def _field(obj: Any, name: str) -> Any:
    return obj.get(name) if isinstance(obj, dict) else getattr(obj, name, None)


def with_shogun_agent(query: Callable[..., AsyncIterator[Any]], *, recall_query: str | None = None, **options: Any) -> Callable[..., AsyncIterator[Any]]:
    """Wrap ``query(prompt=…, options=…)``. ``recall_query`` names what to recall for a streamed prompt."""
    memory = AsyncShogunAgentMemory(options_from(options), "claude-agent-sdk")

    async def wrapped(*, prompt: Any, options: Any = None, **kw: Any) -> AsyncIterator[Any]:
        user = prompt if isinstance(prompt, str) else recall_query
        block = await memory.block(user)
        opts = _with_system_prompt(options, block)
        result_text = ""
        assistant_text = ""
        turn_id = None
        session_id = None
        call_kw = {"prompt": prompt, **kw}
        if opts is not None:
            call_kw["options"] = opts
        async for message in query(**call_kw):
            kind = type(message).__name__
            mtype = _field(message, "type")
            if kind == "AssistantMessage" or mtype == "assistant":
                content = _field(message, "content")
                inner = _field(message, "message")
                if content is None and inner is not None:
                    content = _field(inner, "content")
                text = text_of(content)
                if text:
                    assistant_text = text
            if kind == "ResultMessage" or mtype == "result":
                if isinstance(_field(message, "result"), str):
                    result_text = _field(message, "result")
                session_id = _field(message, "session_id")
                turn_id = _field(message, "uuid")
                if not isinstance(turn_id, str) and isinstance(session_id, str):
                    turn_id = f"{session_id}:{_field(message, 'num_turns')}"
            yield message
        await memory.remember(
            user=prompt if isinstance(prompt, str) else None,
            assistant=result_text or assistant_text,
            turn_id=turn_id if isinstance(turn_id, str) else None,
            thread_key=memory.options.thread_key or (f"claude:{session_id}" if isinstance(session_id, str) else None),
        )

    return wrapped
