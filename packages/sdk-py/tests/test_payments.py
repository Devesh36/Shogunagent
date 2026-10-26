from __future__ import annotations

import asyncio
import json
from typing import Any

import pytest

from conftest import Call, Fake, Reply, rpc_result, tool_text
from shogunagent import PaymentOutcome, ShogunAgentError, ToolError

URL = "https://api.shogunagent.dev/approve/tok"

MANDATE = {
    "mandate_id": "pmd_0123456789ab", "agent": "agent:test", "payee": {"kind": "class", "value": "cloud"}, "currency": "USD",
    "max_per_payment_minor": 5000, "total_cap_minor": 20000, "spent_minor": 0, "remaining_minor": 20000, "payments": 0,
    "expires_at_ms": 2_000_000_000_000, "purpose": "Build minutes", "single_use": False, "status": "active", "revision": 1,
    "granted": {"approval_id": 7, "by": "human:owner", "at_ms": 1}, "revoked": None,
}


def approval_required(payload: dict[str, Any], approval_id: int = 7) -> Reply:
    return rpc_result({
        "resultType": "approval_required",
        "approval": {"approval_id": approval_id, "level": "L3", "url": URL, "poll": {"tool": "actions.status", "arguments": {"approval_id": approval_id}}, "message": "Hand the URL to the human."},
        "content": [{"type": "text", "text": json.dumps({"pending": True, "approval_id": approval_id, "url": URL, "next_action": "approve_at_url", **payload})}],
        "isError": False,
    })


def spend_row(status: str, **extra: Any) -> dict[str, Any]:
    row = {"spend_ref": "psp_0123456789ab", "actor": "agent:test", "idempotency_key": "inv-1", "mandate_id": None, "authorization": "approval",
           "approval_id": 9, "decided_by": "human:owner", "payee": "cloud:hetzner", "currency": "USD", "amount_minor": 1250, "status": status,
           "reason_class": None, "rail": "stub", "rail_ref": None, "created_at_ms": 1, "settled_at_ms": None}
    row.update(extra)
    return row


def test_request_mandate_answers_the_url_and_never_approves(fake: Fake) -> None:
    fake.tool("payments.mandates.request", approval_required({"tool": "payments.mandates.request", "mandate_id": "pmd_0123456789ab"}))
    fake.tool("actions.status", tool_text({"approval_id": 7, "status": "pending"}))
    c = fake.client()
    out = c.payments.request_mandate(payee_class="cloud", currency="USD", max_per_payment_minor=5000, total_cap_minor=20000, expires_in_days=30, purpose="Build minutes")
    assert isinstance(out, PaymentOutcome)
    assert out.type == "approval_required"
    assert out.approval is not None and out.approval.url == URL
    assert out.mandate_id == "pmd_0123456789ab"
    assert out.idempotency_key.startswith("mandate_")
    assert fake.tool_calls("payments.mandates.request")[0].body["params"]["arguments"] == {
        "currency": "USD", "max_per_payment_minor": 5000, "total_cap_minor": 20000, "purpose": "Build minutes",
        "idempotency_key": out.idempotency_key, "payee_class": "cloud", "expires_in_days": 30,
    }
    assert out.status().status == "pending"
    assert [call.headers["mcp-name"] for call in fake.calls] == ["payments.mandates.request", "actions.status"]
    assert not any("/approve" in call.url or "/decide" in call.url for call in fake.calls)


def test_request_mandate_narrowed_replayed_and_local_pending(fake: Fake) -> None:
    def answer(call: Call) -> Reply:
        a = call.body["params"]["arguments"]
        if "mandate_id" in a:
            return tool_text({"tool": "payments.mandates.request", "narrowed": True, "mandate": {**MANDATE, "total_cap_minor": 6000}})
        if a["idempotency_key"] == "again":
            return tool_text({"tool": "payments.mandates.request", "replayed": True, "mandate": MANDATE})
        return tool_text({"pending": True, "approval_id": 3, "level": "L3", "origin": "mcp", "mandate_id": "pmd_x"})

    fake.tool("payments.mandates.request", answer)
    c = fake.client()
    narrowed = c.payments.request_mandate(mandate_id=MANDATE["mandate_id"], payee_class="cloud", currency="USD", max_per_payment_minor=5000, total_cap_minor=6000, purpose="Build minutes", idempotency_key="n")
    assert (narrowed.type, narrowed.mandate_id, narrowed.idempotency_key) == ("narrowed", MANDATE["mandate_id"], "n")
    assert narrowed.mandate is not None and narrowed.mandate["total_cap_minor"] == 6000
    assert c.payments.request_mandate(payee="cloud:hetzner", currency="USD", max_per_payment_minor=1, total_cap_minor=1, purpose="x", idempotency_key="again").type == "replayed"
    local = c.payments.request_mandate(payee="cloud:hetzner", currency="USD", max_per_payment_minor=1, total_cap_minor=1, purpose="x", idempotency_key="k")
    assert local.type == "approval_required" and local.approval is not None
    assert (local.approval.approval_id, local.approval.url, local.mandate_id) == (3, None, "pmd_x")


def test_request_mandate_refuses_an_ambiguous_payee_before_any_call(fake: Fake) -> None:
    c = fake.client()
    with pytest.raises(ShogunAgentError):
        c.payments.request_mandate(payee="a", payee_class="b", currency="USD", max_per_payment_minor=1, total_cap_minor=1, purpose="x")
    with pytest.raises(ShogunAgentError):
        c.payments.request_mandate(currency="USD", max_per_payment_minor=1, total_cap_minor=1, purpose="x")
    assert fake.calls == []


def test_list_and_revoke(fake: Fake) -> None:
    fake.tool("payments.mandates.list", lambda call: tool_text(
        {"tool": "payments.mandates.list", "mandate": MANDATE, "spends": [spend_row("settled")]} if "mandate_id" in call.body["params"]["arguments"]
        else {"tool": "payments.mandates.list", "mandates": [MANDATE]}))
    fake.tool("payments.mandates.revoke", tool_text({"tool": "payments.mandates.revoke", "revoked": True, "already_revoked": False, "mandate": {**MANDATE, "status": "revoked"}}))
    c = fake.client()
    assert c.payments.list_mandates()["mandates"][0]["remaining_minor"] == 20000
    one = c.payments.list_mandates(mandate_id=MANDATE["mandate_id"], limit=5)
    assert one["mandate"]["mandate_id"] == MANDATE["mandate_id"] and one["spends"][0]["status"] == "settled"
    assert fake.tool_calls("payments.mandates.list")[1].body["params"]["arguments"] == {"mandate_id": MANDATE["mandate_id"], "limit": 5}
    assert c.payments.revoke_mandate(MANDATE["mandate_id"], idempotency_key="r1")["revoked"] is True
    assert fake.tool_calls("payments.mandates.revoke")[0].body["params"]["arguments"] == {"mandate_id": MANDATE["mandate_id"], "idempotency_key": "r1"}


def test_spend_settles_inside_a_mandate_and_replays(fake: Fake) -> None:
    def answer(call: Call) -> Reply:
        replay = len(fake.tool_calls("payments.spend")) > 1
        return tool_text({"tool": "payments.spend", "status": "settled", "authorization": "mandate", **({"replayed": True} if replay else {}),
                          "spend": spend_row("settled", authorization="mandate", mandate_id=MANDATE["mandate_id"], rail_ref="stub_psp_1")})

    fake.tool("payments.spend", answer)
    c = fake.client()
    paid = c.payments.spend(payee="cloud:hetzner", currency="USD", amount_minor=1250, memo="June", idempotency_key="inv-1")
    assert (paid.type, paid.replayed, paid.idempotency_key) == ("settled", False, "inv-1")
    assert paid.spend is not None and paid.spend["rail_ref"] == "stub_psp_1"
    assert fake.tool_calls("payments.spend")[0].body["params"]["arguments"] == {"payee": "cloud:hetzner", "currency": "USD", "amount_minor": 1250, "idempotency_key": "inv-1", "memo": "June"}
    again = c.payments.spend(payee="cloud:hetzner", currency="USD", amount_minor=1250, idempotency_key="inv-1")
    assert (again.type, again.replayed) == ("settled", True)


def test_spend_outside_a_mandate_waits_for_a_human_and_a_rejection_is_not_settled(fake: Fake) -> None:
    decided = {"v": False}
    fake.tool("payments.spend", lambda _c: tool_text({"tool": "payments.spend", "replayed": True, "status": "rejected", "next_action": "ask_human", "spend": spend_row("rejected", reason_class="rejected")})
              if decided["v"] else approval_required({"tool": "payments.spend", "spend_ref": "psp_0123456789ab", "authorization": "approval", "reason": "over_total_cap"}, 9))
    c = fake.client()
    first = c.payments.spend(payee="cloud:hetzner", currency="USD", amount_minor=1250)
    assert (first.type, first.spend_ref, first.reason) == ("approval_required", "psp_0123456789ab", "over_total_cap")
    assert first.approval is not None and first.approval.approval_id == 9
    assert first.idempotency_key.startswith("pay_")
    decided["v"] = True
    after = c.payments.spend(payee="cloud:hetzner", currency="USD", amount_minor=1250, idempotency_key=first.idempotency_key)
    assert (after.type, after.replayed) == ("not_settled", True)
    assert after.spend is not None and (after.spend["status"], after.spend["decided_by"]) == ("rejected", "human:owner")
    with pytest.raises(ShogunAgentError):
        after.status()


def test_a_rail_failure_is_a_tool_error(fake: Fake) -> None:
    fake.tool("payments.spend", tool_text({"tool": "payments.spend", "error": "payment_failed", "message": "The payment did not settle.", "next_action": "check_status", "spend": spend_row("failed")}, True))
    with pytest.raises(ToolError) as e:
        fake.client().payments.spend(payee="x", currency="USD", amount_minor=1, idempotency_key="k")
    assert (e.value.code, e.value.next_action) == ("payment_failed", "check_status")


def test_connections_status_and_disconnect(fake: Fake) -> None:
    fake.tool("connections.list", tool_text({"hosted": True, "connections": [{"service": "gcal", "state": "connected"}]}))
    fake.tool("connections.disconnect", tool_text({"disconnected": True, "service": "gcal"}))
    c = fake.client()
    assert c.connections.status()["connections"][0]["service"] == "gcal"
    assert c.connections.disconnect("gcal")["disconnected"] is True
    assert fake.tool_calls("connections.disconnect")[0].body["params"]["arguments"] == {"service": "gcal"}


def test_async_client_has_the_same_payments_surface(fake: Fake) -> None:
    fake.tool("payments.spend", approval_required({"tool": "payments.spend", "spend_ref": "psp_0123456789ab", "reason": "no_mandate"}, 4))
    fake.tool("actions.status", tool_text({"approval_id": 4, "status": "rejected"}))
    fake.tool("payments.mandates.list", tool_text({"tool": "payments.mandates.list", "mandates": []}))
    fake.tool("connections.list", tool_text({"connections": []}))

    async def go() -> None:
        c = fake.aclient()
        out = await c.payments.spend(payee="x", currency="USD", amount_minor=1, idempotency_key="k")
        assert out.type == "approval_required" and out.reason == "no_mandate"
        assert (await out.status()).status == "rejected"
        assert (await c.payments.list_mandates())["mandates"] == []
        assert (await c.connections.status())["connections"] == []
        await c.aclose()

    asyncio.run(go())
