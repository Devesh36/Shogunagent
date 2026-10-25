"""A fake transport that plays the gateway / sidecar (no network): MCP ``tools/call`` by tool
name, REST by path. Works for both the sync and the async client (``httpx.MockTransport``)."""

from __future__ import annotations

import base64
import json
from typing import Any, Callable

import httpx
import pytest

from shogunagent import AsyncShogunAgent, ShogunAgent

Reply = dict[str, Any]


def tool_text(payload: Any, is_error: bool = False) -> Reply:
    return {"body": {"jsonrpc": "2.0", "id": 1, "result": {"content": [{"type": "text", "text": json.dumps(payload)}], "isError": is_error}}}


def rpc_result(result: Any) -> Reply:
    return {"body": {"jsonrpc": "2.0", "id": 1, "result": result}}


def recall_payload(**overrides: Any) -> dict[str, Any]:
    result = {
        "items": [
            {"kind": "commitment", "id": 7, "text": "send the rollout plan by Friday", "confidence": 0.9, "band": "high", "quote": "I'll send the rollout plan by Friday.", "event_id": 481},
            {"kind": "open_loop", "id": 3, "text": "waiting on Oren's budget numbers", "confidence": 0.6, "band": "medium", "quote": "Oren: budget numbers next week", "event_id": 482},
            {"kind": "person", "id": 12, "text": "SECRET-LOW-ITEM", "confidence": 0.3, "band": "low", "quote": None, "event_id": 483},
        ],
        "facts": ["Oren Vale leads the Heron rollout."],
        "possibly": ["possibly: Heron ships in November."],
        "evidence": [{"event_id": 481, "ts_ms": 1792000000000, "source": "agent", "title": "standup", "excerpt": "I'll send the rollout plan by Friday."}],
        "truncated": False, "budget_tokens": 2000, "used_tokens": 120, "include_scratch": False,
    }
    result.update(overrides)
    return {"tool": "memory.recall", "result": result, "usage": {"unit": "read", "quantity": 1, "cost_micro_usd": 0}, "latency_ms": 4, "next_action": None}


PAYMENT_402: Reply = {
    "status": 402,
    "headers": {"payment-required": base64.urlsafe_b64encode(json.dumps({"x402Version": 2, "error": "payment_required", "accepts": [{"rail": "card", "url": "https://api.test/v1/wallet/topup"}]}).encode()).decode().rstrip("=")},
    "body": {
        "error": {"code": "payment_required", "message": "This call costs 6000 micro-USD and the wallet has 1000 spendable.", "next_action": "top_up"},
        "payment": {"amount_micro_usd": 6000, "unit": "ingest", "accepts": [{"rail": "card", "url": "https://api.test/v1/wallet/topup", "method": "POST"}], "retry_after": 0, "balance_micro_usd": 1000},
        "usage": {"unit": "ingest", "quantity": 0, "cost_micro_usd": 0}, "latency_ms": 3,
    },
}


class Call:
    def __init__(self, request: httpx.Request) -> None:
        self.url = str(request.url)
        self.method = request.method
        self.headers = {k.lower(): v for k, v in request.headers.items()}
        raw = request.content
        self.body = json.loads(raw) if raw else None


class Fake:
    def __init__(self) -> None:
        self.calls: list[Call] = []
        self.tools: dict[str, Callable[[Call], Reply]] = {}
        self.routes: dict[str, Callable[[Call], Reply]] = {}

    def tool(self, name: str, reply: Reply | Callable[[Call], Reply]) -> Fake:
        self.tools[name] = reply if callable(reply) else (lambda _c, r=reply: r)
        return self

    def route(self, path: str, reply: Reply | Callable[[Call], Reply]) -> Fake:
        self.routes[path] = reply if callable(reply) else (lambda _c, r=reply: r)
        return self

    def tool_calls(self, name: str) -> list[Call]:
        return [c for c in self.calls if c.headers.get("mcp-name") == name]

    def handler(self, request: httpx.Request) -> httpx.Response:
        call = Call(request)
        self.calls.append(call)
        if request.url.path == "/mcp":
            name = call.body["params"]["name"]
            h = self.tools.get(name)
            reply = h(call) if h else {"body": {"jsonrpc": "2.0", "id": 1, "error": {"code": -32602, "message": f"unknown_tool: {name}"}}}
        else:
            h = self.routes.get(request.url.path)
            reply = h(call) if h else {"status": 404, "body": {"error": {"code": "not_found", "message": "no fake", "next_action": None}}}
        headers = {"content-type": "application/json", **reply.get("headers", {})}
        content = reply["text"] if "text" in reply else json.dumps(reply.get("body"))
        return httpx.Response(reply.get("status", 200), headers=headers, content=content.encode())

    @property
    def transport(self) -> httpx.MockTransport:
        return httpx.MockTransport(self.handler)

    def client(self, **kw: Any) -> ShogunAgent:
        return ShogunAgent(transport=self.transport, **{"api_key": "sa_live_test", "actor": "agent:test", **kw})

    def aclient(self, **kw: Any) -> AsyncShogunAgent:
        return AsyncShogunAgent(transport=self.transport, **{"api_key": "sa_live_test", "actor": "agent:test", **kw})


def memory_fake(recall: Any = None) -> Fake:
    """Answers recall, and ingest with replay by idempotency key."""
    seen: set[str] = set()

    def ingest(c: Call) -> Reply:
        key = c.body["params"]["arguments"]["idempotency_key"]
        replayed = key in seen
        seen.add(key)
        return tool_text({"result": {"event_ids": [1], "inserted": 0 if replayed else 1, "deduplicated": 0, "replayed": replayed, "thread_key": "t"}, "usage": {"unit": "ingest", "quantity": 1, "cost_micro_usd": 500}, "latency_ms": 1})

    def accept(c: Call) -> Reply:
        args = c.body["params"]["arguments"]
        key = f"handoff:{args['handoff']['handoff_id']}:{args['actor']}"
        replayed = key in seen
        seen.add(key)
        result = {"handoff_id": args["handoff"]["handoff_id"], "accepted": 3, "inserted": 0 if replayed else 3, "replayed": replayed,
                  "event_ids": [1, 2, 3], "thread_key": f"handoff:{args['handoff']['handoff_id']}", "namespace": "agent_scratch", "idempotency_key": key}
        return tool_text({"tool": "handoff.accept", "result": result, "usage": {"unit": "events", "quantity": 0 if replayed else 3}, "latency_ms": 1})

    return Fake().tool("memory.recall", tool_text(recall or recall_payload())).tool("memory.ingest", ingest).tool("handoff.accept", accept)


@pytest.fixture
def fake() -> Fake:
    return Fake()
