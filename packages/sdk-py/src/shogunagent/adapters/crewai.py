"""CrewAI adapter: wrap a ``Crew``.

CrewAI builds each agent's system prompt from its role, goal and backstory, so the block goes into
every agent's ``backstory`` (replacing any earlier block) before ``kickoff``; the crew's final
output is ingested afterwards::

    from crewai import Agent, Crew, Task
    from shogunagent.adapters.crewai import with_shogun_agent

    crew = with_shogun_agent(Crew(agents=[writer], tasks=[draft]), thread_key="task-42")
    result = crew.kickoff(inputs={"topic": "the Heron rollout"})

The recall query is ``query=`` when given, otherwise the tasks' descriptions with the kickoff
inputs filled in. Every other attribute is the wrapped crew's. No import of ``crewai``.
"""

from __future__ import annotations

from typing import Any

from ..context import replace_context
from ..middleware import AsyncShogunAgentMemory, ShogunAgentMemory, options_from


def _fill(text: str, inputs: dict[str, Any] | None) -> str:
    if not inputs:
        return text
    try:
        return text.format(**inputs)
    except (KeyError, IndexError, ValueError):
        return text


def _output_text(result: Any) -> str:
    raw = getattr(result, "raw", None)
    if isinstance(raw, str):
        return raw
    return "" if result is None else str(result)


class ShogunCrew:
    """A crew with recall before ``kickoff`` and ingest after it."""

    def __init__(self, crew: Any, *, query: str | None = None, **options: Any) -> None:
        self._crew = crew
        self._query = query
        self._opts = options_from(options)
        self._memory_: ShogunAgentMemory | None = None
        self._amemory_: AsyncShogunAgentMemory | None = None

    @property
    def _memory(self) -> ShogunAgentMemory:
        if self._memory_ is None:
            self._memory_ = ShogunAgentMemory(self._opts, "crewai")
        return self._memory_

    @property
    def _amemory(self) -> AsyncShogunAgentMemory:
        if self._amemory_ is None:
            self._amemory_ = AsyncShogunAgentMemory(self._opts, "crewai")
        return self._amemory_

    def __getattr__(self, name: str) -> Any:
        return getattr(self._crew, name)

    def recall_query(self, inputs: dict[str, Any] | None = None) -> str:
        if self._query:
            return _fill(self._query, inputs)
        parts = [_fill(str(getattr(t, "description", "") or ""), inputs) for t in getattr(self._crew, "tasks", []) or []]
        return "\n".join(p for p in parts if p)

    def _inject(self, block: str) -> None:
        for agent in getattr(self._crew, "agents", []) or []:
            agent.backstory = replace_context(getattr(agent, "backstory", "") or "", block)

    def kickoff(self, inputs: dict[str, Any] | None = None, **kw: Any) -> Any:
        query = self.recall_query(inputs)
        self._inject(self._memory.block(query))
        result = self._crew.kickoff(inputs=inputs, **kw)
        self._memory.remember(user=query, assistant=_output_text(result), turn_id=_turn_id(result))
        return result

    async def kickoff_async(self, inputs: dict[str, Any] | None = None, **kw: Any) -> Any:
        query = self.recall_query(inputs)
        self._inject(await self._amemory.block(query))
        result = await self._crew.kickoff_async(inputs=inputs, **kw)
        await self._amemory.remember(user=query, assistant=_output_text(result), turn_id=_turn_id(result))
        return result


def _turn_id(result: Any) -> str | None:
    for name in ("id", "run_id"):
        v = getattr(result, name, None)
        if isinstance(v, str):
            return v
    return None


def with_shogun_agent(crew: Any, *, query: str | None = None, **options: Any) -> ShogunCrew:
    """Wrap a crew. Options: see ``ShogunAgentOptions``; ``query`` overrides the recall query."""
    return ShogunCrew(crew, query=query, **options)
