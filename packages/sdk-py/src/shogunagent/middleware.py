"""The behavior every ``with_shogun_agent()`` adapter shares (same contract as the TS SDK):

- before the model call, ``recall(query = the last user message, budget_tokens)`` and put the
  result in the system prompt as ONE escaped, replaceable state-of-work block (High as fact,
  Medium as ``possibly:``, Low never);
- after the call, ``ingest`` the turn under ``thread_key`` with an idempotency key derived from the
  turn id, so a retried turn replays instead of appending.

A ShogunAgent outage never fails the model call: errors go to ``on_error`` (a 402 is surfaced once
as a warning when no hook is set) and the call proceeds without the block or the ingest.
"""

from __future__ import annotations

import hashlib
import warnings
from dataclasses import dataclass
from typing import Any, Callable, Literal

from .client import AsyncShogunAgent, ShogunAgent
from .context import context_block, replace_context, strip_context
from .errors import PaymentRequiredError

MAX_TURN_CHARS = 16_000


@dataclass
class ShogunAgentOptions:
    """Options of every adapter."""

    client: ShogunAgent | AsyncShogunAgent | None = None
    api_key: str | None = None
    base_url: str | None = None
    budget_tokens: int = 2000
    thread_key: str | None = None
    ingest: bool = True
    recall: bool = True
    namespace: Literal["world", "agent_scratch"] = "world"
    include_scratch: bool | None = None
    actor: str | None = None
    source: str | None = None
    on_error: Callable[[BaseException, str], None] | None = None


def turn_idempotency_key(thread_key: str, turn_id: str) -> str:
    """``turn:<sha256(thread, turn id)[:40]>`` — identical to the TS SDK's key for the same input."""
    return "turn:" + hashlib.sha256(f"{thread_key}\u0000{turn_id}".encode()).hexdigest()[:40]


def text_of(content: Any) -> str:
    """The text of a message's content in any of the shapes the frameworks use."""
    if content is None:
        return ""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        out = []
        for part in content:
            if isinstance(part, str):
                out.append(part)
            elif isinstance(part, dict) and isinstance(part.get("text"), str) and part.get("type") in (None, "text", "input_text", "output_text"):
                out.append(part["text"])
            elif hasattr(part, "text") and isinstance(getattr(part, "text"), str):
                out.append(part.text)
        return "\n".join(x for x in out if x)
    if isinstance(content, dict):
        if isinstance(content.get("text"), str):
            return content["text"]
        if "content" in content:
            return text_of(content["content"])
    if hasattr(content, "content"):
        return text_of(content.content)
    return ""


def role_of(message: Any) -> str:
    """``system`` / ``human`` / ``ai`` / ``tool`` for dicts and LangChain-style objects."""
    if isinstance(message, dict):
        r = message.get("role") or message.get("type") or ""
    else:
        r = getattr(message, "type", None) or getattr(message, "role", None) or ""
    return {"user": "human", "assistant": "ai"}.get(str(r), str(r))


class _Base:
    def __init__(self, options: ShogunAgentOptions, adapter: str) -> None:
        self.options = options
        self.adapter = adapter
        self._warned = False

    def _report(self, error: BaseException, phase: str) -> None:
        try:
            if self.options.on_error is not None:
                self.options.on_error(error, phase)
            elif isinstance(error, PaymentRequiredError) and not self._warned:
                self._warned = True
                warnings.warn(f"shogunagent: {phase} answered 402 {error.code}; next_action={error.next_action} top_up={error.top_up_url}", stacklevel=3)
        except Exception:
            pass

    def _client_kw(self) -> dict[str, Any]:
        kw: dict[str, Any] = {"source": self.options.source or f"shogunagent-sdk:{self.adapter}"}
        if self.options.api_key is not None:
            kw["api_key"] = self.options.api_key
        if self.options.base_url is not None:
            kw["base_url"] = self.options.base_url
        if self.options.actor is not None:
            kw["actor"] = self.options.actor
        return kw

    def _turns(self, user: str | None, assistant: str | None) -> list[dict[str, str]]:
        turns = []
        u = strip_context(user or "").strip()
        a = strip_context(assistant or "").strip()
        if u:
            turns.append({"role": "user", "content": u[:MAX_TURN_CHARS]})
        if a:
            turns.append({"role": "assistant", "content": a[:MAX_TURN_CHARS]})
        return turns

    def _ingest_kw(self, turns: list[dict[str, str]], turn_id: str | None, thread_key: str | None) -> dict[str, Any]:
        thread = thread_key or self.options.thread_key or f"{self.adapter}:default"
        tid = turn_id or "content:" + hashlib.sha256("\u0001".join(f"{t['role']}\u0000{t['content']}" for t in turns).encode()).hexdigest()
        return {
            "turns": turns,
            "thread_key": thread,
            "idempotency_key": turn_idempotency_key(thread, tid),
            "namespace": self.options.namespace,
            "metadata": {"adapter": self.adapter, "turn_id": tid[:200]},
        }


class ShogunAgentMemory(_Base):
    """Synchronous core."""

    def __init__(self, options: ShogunAgentOptions, adapter: str) -> None:
        super().__init__(options, adapter)
        client = options.client
        self.client: ShogunAgent = client if isinstance(client, ShogunAgent) else ShogunAgent(**self._client_kw())

    def block(self, query: str | None) -> str:
        q = (query or "").strip()
        if not self.options.recall or not q:
            return ""
        try:
            return context_block(self.client.recall(q[:2000], budget_tokens=self.options.budget_tokens, include_scratch=self.options.include_scratch))
        except Exception as e:  # never fail the model call
            self._report(e, "recall")
            return ""

    def inject(self, system: str | None, query: str | None) -> str:
        return replace_context(system or "", self.block(query))

    def remember(self, *, user: str | None, assistant: str | None, turn_id: str | None = None, thread_key: str | None = None) -> bool:
        if not self.options.ingest:
            return False
        turns = self._turns(user, assistant)
        if not turns:
            return False
        try:
            self.client.ingest(**self._ingest_kw(turns, turn_id, thread_key))
            return True
        except Exception as e:
            self._report(e, "ingest")
            return False


class AsyncShogunAgentMemory(_Base):
    """Asynchronous core."""

    def __init__(self, options: ShogunAgentOptions, adapter: str) -> None:
        super().__init__(options, adapter)
        client = options.client
        self.client: AsyncShogunAgent = client if isinstance(client, AsyncShogunAgent) else AsyncShogunAgent(**self._client_kw())

    async def block(self, query: str | None) -> str:
        q = (query or "").strip()
        if not self.options.recall or not q:
            return ""
        try:
            return context_block(await self.client.recall(q[:2000], budget_tokens=self.options.budget_tokens, include_scratch=self.options.include_scratch))
        except Exception as e:
            self._report(e, "recall")
            return ""

    async def inject(self, system: str | None, query: str | None) -> str:
        return replace_context(system or "", await self.block(query))

    async def remember(self, *, user: str | None, assistant: str | None, turn_id: str | None = None, thread_key: str | None = None) -> bool:
        if not self.options.ingest:
            return False
        turns = self._turns(user, assistant)
        if not turns:
            return False
        try:
            await self.client.ingest(**self._ingest_kw(turns, turn_id, thread_key))
            return True
        except Exception as e:
            self._report(e, "ingest")
            return False


def options_from(kw: dict[str, Any]) -> ShogunAgentOptions:
    """Build options from adapter keyword arguments (unknown keys are an error)."""
    return ShogunAgentOptions(**kw)
