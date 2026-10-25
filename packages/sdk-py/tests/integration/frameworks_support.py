"""Real-framework integration tests (E36b): the adapters inside the installed frameworks, with a
fake model and the fake ShogunAgent transport (``httpx.MockTransport``). Nothing may reach the
network: every test runs under ``no_network``, which makes any socket connection fail and records
it, and the test fails if one was attempted.

The frameworks are optional (``pip install -e 'packages/sdk-py[integration]'``). Without them the
tests are skipped, never passed; CI sets ``SHOGUNAGENT_REQUIRE_INTEGRATION=1`` so a missing
framework fails instead of skipping.
"""

from __future__ import annotations

import importlib
import os
import socket
from types import ModuleType
from typing import Any, Iterator

import pytest


def require(module: str) -> ModuleType:
    if os.environ.get("SHOGUNAGENT_REQUIRE_INTEGRATION") == "1":
        return importlib.import_module(module)
    return pytest.importorskip(module)


@pytest.fixture(autouse=True)
def no_network(monkeypatch: pytest.MonkeyPatch) -> Iterator[list[Any]]:
    attempts: list[Any] = []

    def refuse(*args: Any, **kwargs: Any) -> Any:
        attempts.append(args[1] if len(args) > 1 else args)
        raise OSError("network access attempted in an offline test")

    monkeypatch.setattr(socket.socket, "connect", refuse)
    monkeypatch.setattr(socket.socket, "connect_ex", refuse)
    monkeypatch.setattr(socket, "create_connection", lambda *a, **k: refuse(None, a))
    for name in ("OPENAI_API_KEY", "ANTHROPIC_API_KEY", "LANGCHAIN_TRACING_V2", "LANGSMITH_TRACING", "LANGCHAIN_API_KEY", "LANGSMITH_API_KEY"):
        monkeypatch.delenv(name, raising=False)
    monkeypatch.setenv("OPENAI_AGENTS_DISABLE_TRACING", "1")
    yield attempts
    assert attempts == [], f"a framework tried to reach the network: {attempts}"


BLOCK_OPEN = '<shogunagent context="state-of-work" readonly>'


def block_count(text: str) -> int:
    return text.count(BLOCK_OPEN)
