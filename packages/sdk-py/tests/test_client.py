from __future__ import annotations

import asyncio

import pytest

from conftest import PAYMENT_402, Fake, recall_payload, rpc_result, tool_text
from shogunagent import (
    LOCAL_BASE_URL, ActionDeclined, ActionResult, ApprovalRequired, AuthenticationError, InputRequired, PaymentRequiredError,
    PermissionDeniedError, RateLimitError, ServiceUnavailableError, ShogunAgent, ToolError,
)


def test_defaults_to_local_sidecar_and_sends_mcp_headers(fake: Fake) -> None:
    fake.tool("memory.recall", tool_text(recall_payload()))
    c = ShogunAgent(transport=fake.transport)
    assert c.base_url == LOCAL_BASE_URL
    r = c.recall("rollout", budget_tokens=500, include_scratch=True)
    call = fake.calls[0]
    assert call.url == "http://127.0.0.1:7464/mcp"
    assert call.headers["mcp-method"] == "tools/call"
    assert call.headers["mcp-name"] == "memory.recall"
    assert call.headers["mcp-protocol-version"] == "2026-07-28"
    assert "authorization" not in call.headers
    assert call.body["params"] == {"name": "memory.recall", "arguments": {"query": "rollout", "budget_tokens": 500, "include_scratch": True}}
    assert [i.band for i in r.items] == ["high", "medium", "low"]
    assert r.envelope is not None and r.envelope.usage is not None and r.envelope.usage.cost_micro_usd == 0


def test_hosted_bearer_and_sse(fake: Fake) -> None:
    import json

    sse = f"event: message\ndata: {json.dumps({'jsonrpc': '2.0', 'id': 1, 'result': {'content': [{'type': 'text', 'text': json.dumps(recall_payload())}]}})}\n\n"
    fake.tool("memory.recall", {"headers": {"content-type": "text/event-stream"}, "text": sse})
    c = fake.client(base_url="https://api.shogunagent.dev/")
    assert len(c.recall("x").items) == 3
    assert fake.calls[0].url == "https://api.shogunagent.dev/mcp"
    assert fake.calls[0].headers["authorization"] == "Bearer sa_live_test"


def test_ingest_generates_key_and_sends_quantity(fake: Fake) -> None:
    fake.tool("memory.ingest", tool_text({"result": {"event_ids": [1, 2], "inserted": 2, "deduplicated": 0, "replayed": False, "thread_key": "t"}}))
    r = fake.client().ingest(turns=[{"role": "user", "content": "hi"}, {"role": "assistant", "content": "hello"}], thread_key="t1")
    args = fake.calls[0].body["params"]["arguments"]
    assert args["idempotency_key"].startswith("sdk_")
    assert r.idempotency_key == args["idempotency_key"]
    assert args["actor"] == "agent:test" and args["source"] == "shogunagent-sdk" and args["namespace"] == "world" and args["thread_key"] == "t1"
    assert fake.calls[0].headers["mcp-quantity"] == "2"
    assert r.inserted == 2


def test_ingest_validation_before_network(fake: Fake) -> None:
    fake.tool("memory.ingest", tool_text({"result": {}}))
    c = fake.client()
    c.ingest(events=[{"content": "x"}], idempotency_key="mine", namespace="agent_scratch")
    assert fake.calls[0].body["params"]["arguments"]["idempotency_key"] == "mine"
    with pytest.raises(Exception) as e:
        c.ingest(events=[])
    assert getattr(e.value, "code", None) == "invalid_events"
    with pytest.raises(Exception):
        c.ingest(events=[{"content": "x"}] * 201)
    assert len(fake.calls) == 1


def test_context_pack_and_state_reads(fake: Fake) -> None:
    rows = {"tool": "x", "results": [{"text": "send plan", "confidence": 0.9, "possibly": False, "table": "commitments", "id": 7}, {"text": "maybe", "confidence": 0.6, "possibly": True}]}
    fake.tool("memory.get_context_pack", tool_text({"result": {"pack": "p"}}))
    for t in ["state.commitments.list", "state.open_loops.list", "state.people.list", "state.projects.list", "state.people.get", "state.projects.get"]:
        fake.tool(t, tool_text(rows))
    c = fake.client()
    assert c.context_pack("Heron").result == {"pack": "p"}
    assert c.state.commitments.list(include_low=False, limit=5, offset=10).results[1].possibly is True
    c.state.open_loops.list()
    c.state.people.list()
    c.state.projects.list()
    c.state.people.get(12)
    c.state.projects.get(3)
    names = [x.headers["mcp-name"] for x in fake.calls]
    assert names == ["memory.get_context_pack", "state.commitments.list", "state.open_loops.list", "state.people.list", "state.projects.list", "state.people.get", "state.projects.get"]
    assert fake.calls[1].body["params"]["arguments"] == {"include_low": False, "limit": 5, "offset": 10}
    assert fake.calls[5].body["params"]["arguments"] == {"id": 12}


def test_tool_errors(fake: Fake) -> None:
    fake.tool("state.people.get", tool_text({"error": "not_found"}, is_error=True))
    fake.tool("memory.recall", {"body": {"jsonrpc": "2.0", "id": 1, "error": {"code": -32602, "message": "missing_query: pass query"}}})
    c = fake.client()
    with pytest.raises(ToolError) as e:
        c.state.people.get(99)
    assert e.value.code == "not_found"
    with pytest.raises(ToolError) as e2:
        c.recall("")
    assert e2.value.code == "missing_query"


def test_actions_l1_l2_l3(fake: Fake) -> None:
    def execute(c):  # noqa: ANN001
        kind = c.body["params"]["arguments"]["kind"]
        if kind == "local_search":
            return tool_text({"result": {"hits": [1]}})
        if kind == "edit_note":
            if c.body["params"].get("inputResponses"):
                ok = c.body["params"]["inputResponses"][0]["action"] == "accept"
                return tool_text({"result": {"ok": True}} if ok else {"ran": False, "declined": True})
            return rpc_result({"resultType": "input_required", "inputRequests": [{"id": "l2_confirm", "mode": "form", "message": "Confirm"}]})
        return rpc_result({"resultType": "approval_required", "approval": {"approval_id": 42, "level": "L3", "url": "https://approve.test/approvals/42", "message": "Hand the URL"}})

    polls = {"n": 0}

    def status(_c):  # noqa: ANN001
        polls["n"] += 1
        return tool_text({"approval_id": 42, "status": "pending" if polls["n"] < 3 else "sent"})

    fake.tool("actions.execute", execute).tool("actions.status", status)
    c = fake.client()
    out = c.actions.execute({"kind": "local_search", "query": "plan"})
    assert isinstance(out, ActionResult) and out.result == {"hits": [1]}
    l2 = c.actions.execute({"kind": "edit_note", "text": "x"})
    assert isinstance(l2, InputRequired) and l2.input_requests[0].id == "l2_confirm"
    assert isinstance(l2.respond(True), ActionResult)
    assert fake.calls[-1].body["params"]["inputResponses"] == [{"id": "l2_confirm", "action": "accept", "content": {"confirm": True}}]
    assert isinstance(c.actions.execute({"kind": "edit_note"}).respond(False), ActionDeclined)  # type: ignore[union-attr]
    l3 = c.actions.execute({"kind": "send_email", "to": "a@b.c"})
    assert isinstance(l3, ApprovalRequired)
    assert l3.approval.url == "https://approve.test/approvals/42"
    assert l3.status().status == "pending"
    assert l3.wait(interval=0.001).status == "sent"


def test_unreshaped_pending_envelope_is_an_approval(fake: Fake) -> None:
    fake.tool("actions.execute", tool_text({"pending": True, "approval_id": 9, "level": "L3"})).tool("actions.status", tool_text({"approval_id": 9, "status": "rejected"}))
    out = fake.client().actions.execute({"kind": "post_message"})
    assert isinstance(out, ApprovalRequired) and out.approval.approval_id == 9
    assert fake.client().actions.wait_for_approval(9, interval=0.001).status == "rejected"


def test_router_and_quote(fake: Fake) -> None:
    fake.route("/v1/router/discover", {"body": {"result": {"candidates": [{"tool_id": "a"}]}, "usage": {"unit": "call", "quantity": 1, "cost_micro_usd": 0}, "latency_ms": 1, "hints": []}})
    fake.route("/v1/router/inspect", {"body": {"result": {"tool_id": "a"}, "hints": [{"tool_id": "b", "reason": "cheaper"}]}})
    fake.route("/v1/router/run", lambda c: {"body": {"resultType": "input_required", "inputRequests": [], "echo": c.body["idempotency_key"]}})
    fake.route("/v1/quote", lambda c: {"body": {"quote": {"unit": "ingest", "quantity": c.body["quantity"], "cost_micro_usd": 125000}, "method": c.body["method"], "name": c.body["name"], "balance_micro_usd": 1, "sufficient": False, "usage": {"unit": "ingest", "quantity": 0, "cost_micro_usd": 0}, "next_action": "top_up"}})
    c = fake.client(router_base_url="https://router.test")
    assert c.router.discover("read a page", limit=3).result["candidates"][0]["tool_id"] == "a"
    assert c.router.inspect("a").hints[0]["tool_id"] == "b"
    run = c.router.run("a", {"q": 1})
    assert run["resultType"] == "input_required" and run["idempotency_key"].startswith("run_") and run["echo"] == run["idempotency_key"]
    q = c.quote(name="memory.ingest", quantity=250)
    assert q.quote is not None and q.quote.cost_micro_usd == 125000 and q.next_action == "top_up"
    assert [x.url for x in fake.calls[:3]] == ["https://router.test/v1/router/discover", "https://router.test/v1/router/inspect", "https://router.test/v1/router/run"]
    assert fake.calls[3].url.endswith("/v1/quote")


def test_402_exposes_payment(fake: Fake) -> None:
    fake.tool("memory.ingest", PAYMENT_402)
    with pytest.raises(PaymentRequiredError) as e:
        fake.client().ingest(events=[{"content": "x"}])
    err = e.value
    assert err.status == 402 and err.code == "payment_required" and err.next_action == "top_up"
    assert err.payment is not None and err.payment.amount_micro_usd == 6000
    assert err.shortfall_micro_usd == 5000
    assert err.top_up_url == "https://api.test/v1/wallet/topup"
    assert err.payment_required_header is not None and err.payment_required_header["x402Version"] == 2


def test_402_cap_exceeded(fake: Fake) -> None:
    fake.tool("memory.ingest", {"status": 402, "headers": {"retry-after": "43200"}, "body": {"error": {"code": "cap_exceeded", "message": "cap", "next_action": "retry_after"}, "payment": {"amount_micro_usd": 500, "unit": "ingest", "accepts": [], "retry_after": 43200, "cap": {"scope": "key"}}}})
    with pytest.raises(PaymentRequiredError) as e:
        fake.client().ingest(events=[{"content": "x"}])
    assert e.value.code == "cap_exceeded" and e.value.retry_after == 43200 and e.value.top_up_url is None


@pytest.mark.parametrize("status,cls,code,next_action", [
    (401, AuthenticationError, "invalid_key", "authenticate"),
    (403, PermissionDeniedError, "insufficient_scope", "not_allowed"),
    (429, RateLimitError, "rate_limited", "retry_after"),
    (503, ServiceUnavailableError, "upstream_unavailable", "retry_after"),
])
def test_typed_http_errors(fake: Fake, status: int, cls: type, code: str, next_action: str) -> None:
    fake.tool("memory.recall", {"status": status, "headers": {"retry-after": "7"}, "body": {"error": {"code": code, "message": "m", "next_action": next_action}}})
    with pytest.raises(cls) as e:
        fake.client().recall("x")
    assert e.value.code == code and e.value.next_action == next_action and e.value.retry_after == 7


def test_local_flat_error_body(fake: Fake) -> None:
    fake.tool("memory.recall", {"status": 401, "body": {"error": "unauthorized"}})
    with pytest.raises(AuthenticationError) as e:
        fake.client().recall("x")
    assert e.value.code == "unauthorized" and e.value.next_action == "authenticate"


def test_async_client_same_surface(fake: Fake) -> None:
    fake.tool("memory.recall", tool_text(recall_payload())).tool("memory.ingest", PAYMENT_402)
    fake.tool("actions.execute", rpc_result({"resultType": "approval_required", "approval": {"approval_id": 5, "level": "L3", "url": None, "message": "m"}}))
    fake.tool("actions.status", tool_text({"approval_id": 5, "status": "sent"}))

    async def go() -> None:
        c = fake.aclient()
        r = await c.recall("x", budget_tokens=300)
        assert r.items[0].text == "send the rollout plan by Friday"
        with pytest.raises(PaymentRequiredError):
            await c.ingest(events=[{"content": "x"}])
        out = await c.actions.execute({"kind": "send_email"})
        assert isinstance(out, ApprovalRequired)
        assert (await out.wait(interval=0.001)).status == "sent"
        await c.aclose()

    asyncio.run(go())
