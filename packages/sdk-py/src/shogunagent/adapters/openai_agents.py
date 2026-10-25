"""OpenAI Agents SDK (Python, ``openai-agents``) adapter.

Wrap ``Runner.run`` (or ``Runner.run_sync``) once. Each run recalls before the first model call
and puts the block in the agent's instructions through the run's ``call_model_input_filter``
(composed with yours); when the run returns, the input and ``final_output`` are ingested with an
idempotency key derived from ``last_response_id``::

    from agents import Agent, Runner
    from shogunagent.adapters.openai_agents import with_shogun_agent

    run = with_shogun_agent(Runner.run, thread_key="task-42")
    result = await run(Agent(name="helper", instructions="…"), "What do I owe Oren?")

No import of ``agents`` at module load; ``RunConfig`` is imported only when you pass none.
"""

from __future__ import annotations

import dataclasses
import inspect
import json
from typing import Any, Callable

from ..context import replace_context
from ..middleware import AsyncShogunAgentMemory, ShogunAgentMemory, options_from, text_of


def last_user_input(value: Any) -> str:
    """The last user message of a run input (a string or a list of input items)."""
    if isinstance(value, str):
        return value
    if isinstance(value, list):
        for item in reversed(value):
            role = item.get("role") if isinstance(item, dict) else getattr(item, "role", None)
            if role == "user":
                return text_of(item.get("content") if isinstance(item, dict) else getattr(item, "content", None))
    return ""


def _output_text(output: Any) -> str:
    if output is None:
        return ""
    if isinstance(output, str):
        return output
    try:
        return json.dumps(output, default=str)
    except (TypeError, ValueError):
        return str(output)


def _replace(obj: Any, **changes: Any) -> Any:
    if dataclasses.is_dataclass(obj) and not isinstance(obj, type):
        return dataclasses.replace(obj, **changes)
    if isinstance(obj, dict):
        return {**obj, **changes}
    clone = __import__("copy").copy(obj)
    for k, v in changes.items():
        setattr(clone, k, v)
    return clone


def injecting_filter(block: str, previous: Callable[..., Any] | None = None) -> Callable[..., Any]:
    """A ``call_model_input_filter`` that writes ``block`` into the instructions after ``previous``."""

    async def _filter(data: Any) -> Any:
        model_data = getattr(data, "model_data", None) if not isinstance(data, dict) else data.get("model_data")
        if previous is not None:
            out = previous(data)
            model_data = await out if inspect.isawaitable(out) else out
        instructions = model_data.get("instructions") if isinstance(model_data, dict) else getattr(model_data, "instructions", None)
        return _replace(model_data, instructions=replace_context(instructions or "", block))

    return _filter


def _run_config_with(run_config: Any, block: str) -> Any:
    if run_config is None:
        from agents import RunConfig  # type: ignore[import-not-found]

        return RunConfig(call_model_input_filter=injecting_filter(block))
    previous = getattr(run_config, "call_model_input_filter", None) if not isinstance(run_config, dict) else run_config.get("call_model_input_filter")
    return _replace(run_config, call_model_input_filter=injecting_filter(block, previous))


def with_shogun_agent(run: Callable[..., Any], **options: Any) -> Callable[..., Any]:
    """Wrap ``Runner.run`` (async) or ``Runner.run_sync``. Options: see ``ShogunAgentOptions``."""
    opts = options_from(options)
    if inspect.iscoroutinefunction(run):
        amemory = AsyncShogunAgentMemory(opts, "openai-agents")

        async def wrapped(agent: Any, input: Any, *args: Any, run_config: Any = None, **kw: Any) -> Any:
            user = last_user_input(input)
            block = await amemory.block(user)
            result = await run(agent, input, *args, run_config=_run_config_with(run_config, block), **kw)
            await amemory.remember(user=user, assistant=_output_text(getattr(result, "final_output", None)), turn_id=getattr(result, "last_response_id", None))
            return result

        return wrapped

    memory = ShogunAgentMemory(opts, "openai-agents")

    def wrapped_sync(agent: Any, input: Any, *args: Any, run_config: Any = None, **kw: Any) -> Any:
        user = last_user_input(input)
        block = memory.block(user)
        result = run(agent, input, *args, run_config=_run_config_with(run_config, block), **kw)
        memory.remember(user=user, assistant=_output_text(getattr(result, "final_output", None)), turn_id=getattr(result, "last_response_id", None))
        return result

    return wrapped_sync
