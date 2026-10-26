"""``ShogunAgent`` (sync) and ``AsyncShogunAgent``: typed clients of the agent API.

Core tools go over MCP Streamable HTTP (``POST <base_url>/mcp``, one JSON-RPC request per call,
``Mcp-Method`` / ``Mcp-Name`` headers the gateway meters on); the gateway's own routes
(``/v1/quote``, ``/v1/handoff``) and the router verbs (``/v1/router/*``) go over REST. The same
client works against the local sidecar (``http://127.0.0.1:7464``, the default) and the hosted
gateway (``https://api.shogunagent.dev`` with an ``sa_live_…`` key).

The two classes share every request builder and response parser (``_Core``); they differ only in
how a request is sent.
"""

from __future__ import annotations

import asyncio
import json
import re
import time
import uuid
from dataclasses import dataclass, field
from typing import Any, Callable, Generic, Literal, TypeVar, Union

import httpx

from .errors import ShogunAgentError, ToolError, TransportError, error_from_response
from .handoff import check_handoff, extract_handoff, handoff_events
from .types import (
    ApprovalInfo, ApprovalStatus, Envelope, IngestResult, InputRequest, QuoteResult, RecallResult, StateListResult,
)

LOCAL_BASE_URL = "http://127.0.0.1:7464"
HOSTED_BASE_URL = "https://api.shogunagent.dev"
MCP_PROTOCOL_VERSION = "2026-07-28"
SDK_VERSION = "0.2.0"
MAX_INGEST_ITEMS = 200

R = TypeVar("R")


def new_idempotency_key(prefix: str = "sdk") -> str:
    """``sdk_<uuid4>``: generated when the caller passes no key."""
    return f"{prefix}_{uuid.uuid4()}"


# --- action outcomes ---


@dataclass
class ActionResult:
    result: Any
    envelope: Envelope[Any] | None = None
    type: Literal["result"] = "result"


@dataclass
class ActionDeclined:
    result: Any
    type: Literal["declined"] = "declined"


@dataclass
class InputRequired(Generic[R]):
    """L2: show ``input_requests`` to the human, then ``respond(True)`` (or ``False``)."""

    input_requests: list[InputRequest]
    _respond: Callable[[Any], R]
    type: Literal["input_required"] = "input_required"

    def respond(self, confirm: bool | list[dict[str, Any]]) -> R:
        """Resend the same call with the human's answer (sync client: the outcome; async: an awaitable)."""
        if isinstance(confirm, list):
            responses = confirm
        else:
            rid = self.input_requests[0].id if self.input_requests else "l2_confirm"
            responses = [{"id": rid, "action": "accept" if confirm else "decline", "content": {"confirm": bool(confirm)}}]
        return self._respond(responses)


@dataclass
class ApprovalRequired(Generic[R]):
    """L3: hand ``approval.url`` to the human; ``wait()`` until it leaves pending. Never report the
    action as done before the status is ``sent``."""

    approval: ApprovalInfo
    _status: Callable[[], Any]
    _wait: Callable[..., Any]
    type: Literal["approval_required"] = "approval_required"

    def status(self) -> Any:
        return self._status()

    def wait(self, *, interval: float = 2.0, timeout: float = 900.0) -> Any:
        return self._wait(interval=interval, timeout=timeout)


ActionOutcome = Union[ActionResult, ActionDeclined, InputRequired[Any], ApprovalRequired[Any]]


# --- payments (E46 / E51, docs/api/payments.md) ---


@dataclass
class PaymentOutcome:
    """What ``payments.mandates.request`` or ``payments.spend`` answered.

    ``type``:

    - ``approval_required``: an L3 waits for a human. Hand ``approval.url`` to them and poll
      ``status()`` / ``wait()``; this SDK never approves. A spend also carries ``spend_ref`` and
      ``reason`` (why no mandate covered it).
    - ``narrowed`` / ``replayed`` (a mandate request): narrower terms applied at once, or a retried
      request whose mandate a human already granted; ``mandate`` is the mandate.
    - ``settled`` (a spend): paid, ``spend`` is the record; ``replayed`` when it was a retry.
    - ``not_settled`` (a spend): a replay of a spend that is still awaiting a human, was rejected,
      expired or failed (``spend["status"]``). ``rejected`` / ``expired`` paid nothing: ask the
      human before trying again under a new key.

    Never report a payment as made unless ``type`` is ``settled``.
    """

    type: str
    idempotency_key: str
    raw: Any = None
    approval: ApprovalInfo | None = None
    mandate_id: str | None = None
    mandate: dict[str, Any] | None = None
    spend: dict[str, Any] | None = None
    spend_ref: str | None = None
    reason: str | None = None
    replayed: bool = False
    _status: Callable[[], Any] | None = field(default=None, repr=False)
    _wait: Callable[..., Any] | None = field(default=None, repr=False)

    def status(self) -> Any:
        """One ``actions.status`` read of the pending approval (async client: an awaitable)."""
        if self._status is None:
            raise ShogunAgentError("Nothing is waiting for approval.", code="not_pending", next_action="fix_input")
        return self._status()

    def wait(self, *, interval: float = 2.0, timeout: float = 900.0) -> Any:
        """Poll ``actions.status`` until the approval leaves pending (async client: an awaitable)."""
        if self._wait is None:
            raise ShogunAgentError("Nothing is waiting for approval.", code="not_pending", next_action="fix_input")
        return self._wait(interval=interval, timeout=timeout)


@dataclass
class _Raw:
    kind: str  # payload | input_required | approval_required
    payload: Any = None
    input_requests: list[InputRequest] = field(default_factory=list)
    approval: ApprovalInfo | None = None


def _parse_json(text: str) -> Any:
    if not text:
        return None
    try:
        return json.loads(text)
    except ValueError:
        return text


def _parse_sse(text: str) -> Any:
    last = None
    for block in re.split(r"\r?\n\r?\n", text):
        data = "\n".join(line[5:].lstrip(" ") for line in block.splitlines() if line.startswith("data:"))
        if data:
            last = data
    return None if last is None else _parse_json(last)


class _Core:
    def __init__(
        self,
        *,
        api_key: str | None = None,
        base_url: str = LOCAL_BASE_URL,
        router_base_url: str | None = None,
        actor: str = "agent:shogunagent-sdk",
        source: str = "shogunagent-sdk",
        timeout: float = 30.0,
        now: Callable[[], float] | None = None,
    ) -> None:
        self.api_key = api_key
        self.base_url = base_url.rstrip("/")
        self.router_base_url = (router_base_url or self.base_url).rstrip("/")
        self.actor = actor
        self.source = source
        self.timeout = timeout
        self._now = now or time.time
        self._rpc_id = 0

    def now_ms(self) -> int:
        return int(self._now() * 1000)

    def _headers(self, extra: dict[str, str] | None = None) -> dict[str, str]:
        h = {
            "content-type": "application/json",
            "accept": "application/json, text/event-stream",
            "user-agent": f"shogunagent-sdk-py/{SDK_VERSION}",
        }
        if self.api_key:
            h["authorization"] = f"Bearer {self.api_key}"
        h.update(extra or {})
        return h

    # request builders

    def _tool_request(self, name: str, args: dict[str, Any], input_responses: list[dict[str, Any]] | None, quantity: int | None) -> tuple[str, dict[str, str], dict[str, Any]]:
        self._rpc_id += 1
        params: dict[str, Any] = {"name": name, "arguments": args}
        if input_responses is not None:
            params["inputResponses"] = input_responses
        extra = {"mcp-method": "tools/call", "mcp-name": name, "mcp-protocol-version": MCP_PROTOCOL_VERSION}
        if quantity is not None:
            extra["mcp-quantity"] = str(quantity)
        return f"{self.base_url}/mcp", self._headers(extra), {"jsonrpc": "2.0", "id": self._rpc_id, "method": "tools/call", "params": params}

    # response parsers

    @staticmethod
    def _parse_http(response: httpx.Response) -> Any:
        text = response.text
        parsed = _parse_sse(text) if "text/event-stream" in response.headers.get("content-type", "") else _parse_json(text)
        if response.status_code < 200 or response.status_code >= 300:
            raise error_from_response(response.status_code, parsed, response.headers)
        return parsed

    @staticmethod
    def _parse_tool(name: str, parsed: Any) -> _Raw:
        if not isinstance(parsed, dict):
            raise ToolError(f"{name}: the server answered something that is not JSON-RPC.", status=200, code="invalid_response", body=parsed)
        if isinstance(parsed.get("error"), dict):
            message = str(parsed["error"].get("message", "JSON-RPC error"))
            m = re.match(r"^([a-z_]+):", message)
            code = m.group(1) if m else f"rpc_{parsed['error'].get('code', 'error')}"
            raise ToolError(message, status=200, code=code, next_action="retry_after" if code == "memory_update_pending" else "fix_input", body=parsed["error"])
        result = parsed.get("result") if isinstance(parsed.get("result"), dict) else {}
        if result.get("resultType") == "input_required":
            return _Raw("input_required", input_requests=[InputRequest.from_dict(x) for x in result.get("inputRequests") or []])
        if result.get("resultType") == "approval_required" and isinstance(result.get("approval"), dict):
            content = result.get("content")
            first = content[0] if isinstance(content, list) and content else None
            text = _parse_json(first["text"]) if isinstance(first, dict) and isinstance(first.get("text"), str) else None
            return _Raw("approval_required", payload=text, approval=ApprovalInfo.from_dict(result["approval"]))
        payload = result.get("structuredContent")
        if payload is None:
            content = result.get("content")
            first = content[0] if isinstance(content, list) and content else None
            payload = _parse_json(first["text"]) if isinstance(first, dict) and isinstance(first.get("text"), str) else None
        if result.get("isError") is True:
            p = payload if isinstance(payload, dict) else {}
            err = p.get("error")
            code = err if isinstance(err, str) else (err.get("code") if isinstance(err, dict) and isinstance(err.get("code"), str) else "tool_error")
            message = p.get("message") if isinstance(p.get("message"), str) else (err.get("message") if isinstance(err, dict) and isinstance(err.get("message"), str) else f"{name} refused the call ({code}).")
            next_action = p.get("next_action") if isinstance(p.get("next_action"), str) else (err.get("next_action") if isinstance(err, dict) else None)
            raise ToolError(message, status=200, code=code, next_action=next_action, body=payload)
        return _Raw("payload", payload=payload)

    @staticmethod
    def _require_payload(name: str, raw: _Raw) -> Any:
        if raw.kind != "payload":
            raise ToolError(
                f"{name} answered {raw.kind}; call it through actions.execute to handle confirmation and approval.",
                status=200, code=raw.kind, next_action="provide_input" if raw.kind == "input_required" else "approve_at_url", body=raw,
            )
        return raw.payload

    # argument builders

    @staticmethod
    def _recall_args(query: str, budget_tokens: int | None, include_scratch: bool | None) -> dict[str, Any]:
        args: dict[str, Any] = {"query": query}
        if budget_tokens is not None:
            args["budget_tokens"] = budget_tokens
        if include_scratch is not None:
            args["include_scratch"] = include_scratch
        return args

    def _ingest_args(self, events: list[dict[str, Any]] | None, turns: list[dict[str, Any]] | None, source: str | None, actor: str | None,
                     thread_key: str | None, metadata: dict[str, Any] | None, idempotency_key: str | None, namespace: str) -> tuple[dict[str, Any], int]:
        items = events if events is not None else turns or []
        if not items:
            raise ShogunAgentError("Pass at least one event or turn.", code="invalid_events", next_action="fix_input")
        if len(items) > MAX_INGEST_ITEMS:
            raise ShogunAgentError(f"At most {MAX_INGEST_ITEMS} items per ingest; split the batch.", code="invalid_events", next_action="fix_input")
        args: dict[str, Any] = {
            "source": source or self.source,
            "actor": actor or self.actor,
            "idempotency_key": idempotency_key or new_idempotency_key(),
            "namespace": namespace,
        }
        if events is not None:
            args["events"] = events
        else:
            args["turns"] = turns
        if thread_key:
            args["thread_key"] = thread_key
        if metadata:
            args["metadata"] = metadata
        return args, len(items)

    @staticmethod
    def _state_args(include_low: bool | None, limit: int | None, offset: int | None) -> dict[str, Any]:
        args: dict[str, Any] = {}
        if include_low is not None:
            args["include_low"] = include_low
        if limit is not None:
            args["limit"] = limit
        if offset is not None:
            args["offset"] = offset
        return args

    @staticmethod
    def _approval_status(approval_id: int, payload: Any) -> ApprovalStatus:
        obj = payload if isinstance(payload, dict) else {}
        if isinstance(obj.get("result"), dict):
            obj = obj["result"]
        return ApprovalStatus(approval_id=int(obj.get("approval_id", approval_id) or approval_id), status=str(obj.get("status", "unknown")), reason=obj.get("reason") if isinstance(obj.get("reason"), str) else None)

    def _handoff_body(self, task: str, to_agent: str, from_agent: str | None, query: str | None, budget_tokens: int | None, ttl_seconds: int | None, thread_key: str | None) -> dict[str, Any]:
        body: dict[str, Any] = {"task": task, "to_agent": to_agent, "from_agent": from_agent or self.actor}
        if query is not None:
            body["query"] = query
        if budget_tokens is not None:
            body["budget_tokens"] = budget_tokens
        if ttl_seconds is not None:
            body["ttl_seconds"] = ttl_seconds
        if thread_key is not None:
            body["thread_key"] = thread_key
        return body

    def _accept_plan(self, value: Any, as_agent: str | None, check_addressee: bool) -> tuple[dict[str, Any], dict[str, Any], int]:
        """Check a received handoff here first (no call for a bad one); the ``handoff.accept``
        arguments and its ``Mcp-Quantity`` (the events the core will store)."""
        h = extract_handoff(value)
        receiver = as_agent or self.actor
        check_handoff(h, receiver if check_addressee else None, self.now_ms())
        args: dict[str, Any] = {"handoff": h, "actor": receiver}
        if not check_addressee:
            args["check_addressee"] = False
        return h, args, len(handoff_events(h))

    @staticmethod
    def _accepted(h: dict[str, Any], payload: Any) -> dict[str, Any]:
        result = payload.get("result") if isinstance(payload, dict) else None
        return {"handoff": h, "accepted": result if isinstance(result, dict) else {}, "envelope": payload}

    @staticmethod
    def _action_from_payload(payload: Any) -> tuple[str, Any]:
        if isinstance(payload, dict) and payload.get("declined") is True and payload.get("ran") is False:
            return "declined", payload
        if isinstance(payload, dict) and payload.get("pending") is True and isinstance(payload.get("approval_id"), int):
            return "pending", ApprovalInfo(
                approval_id=payload["approval_id"], level=str(payload.get("level", "L3")), url=payload.get("url") if isinstance(payload.get("url"), str) else None,
                message="Hand the URL to the human, or wait for approval in the ShogunAI app; poll actions.status.",
            )
        return "result", payload

    # payments (E46 / E51) and connections

    @staticmethod
    def _mandate_list_args(mandate_id: str | None, limit: int | None) -> dict[str, Any]:
        args: dict[str, Any] = {}
        if mandate_id is not None:
            args["mandate_id"] = mandate_id
        if limit is not None:
            args["limit"] = limit
        return args

    @staticmethod
    def _mandate_list(payload: Any) -> dict[str, Any]:
        obj = payload if isinstance(payload, dict) else {}
        mandate = obj.get("mandate") if isinstance(obj.get("mandate"), dict) else None
        out: dict[str, Any] = {"mandates": obj["mandates"] if isinstance(obj.get("mandates"), list) else ([mandate] if mandate else [])}
        if mandate is not None:
            out["mandate"] = mandate
        if isinstance(obj.get("spends"), list):
            out["spends"] = obj["spends"]
        return out

    @staticmethod
    def _mandate_request_args(*, currency: str, max_per_payment_minor: int, total_cap_minor: int, purpose: str, payee: str | None, payee_class: str | None,
                              expires_in_days: int | None, expires_at_ms: int | None, single_use: bool | None, mandate_id: str | None,
                              idempotency_key: str | None) -> dict[str, Any]:
        if (payee is None) == (payee_class is None):
            raise ShogunAgentError("Give exactly one of payee (an exact payee id) or payee_class.", code="invalid_payee", next_action="fix_input")
        args: dict[str, Any] = {
            "currency": currency, "max_per_payment_minor": max_per_payment_minor, "total_cap_minor": total_cap_minor,
            "purpose": purpose, "idempotency_key": idempotency_key or new_idempotency_key("mandate"),
        }
        for name, value in (("payee", payee), ("payee_class", payee_class), ("expires_in_days", expires_in_days), ("expires_at_ms", expires_at_ms),
                            ("single_use", single_use), ("mandate_id", mandate_id)):
            if value is not None:
                args[name] = value
        return args

    @staticmethod
    def _spend_args(*, payee: str, currency: str, amount_minor: int, memo: str | None, mandate_id: str | None, idempotency_key: str | None) -> dict[str, Any]:
        args: dict[str, Any] = {"payee": payee, "currency": currency, "amount_minor": amount_minor, "idempotency_key": idempotency_key or new_idempotency_key("pay")}
        if memo is not None:
            args["memo"] = memo
        if mandate_id is not None:
            args["mandate_id"] = mandate_id
        return args

    def _payment_outcome(self, tool: str, raw: _Raw, key: str, mandate_id: str | None = None) -> PaymentOutcome:
        """One shape for both payment tools, from the MRTR approval or the core's payload."""
        payload = raw.payload
        p = payload if isinstance(payload, dict) else {}
        approval = raw.approval
        if approval is None:
            kind, value = self._action_from_payload(payload)
            approval = value if kind == "pending" else None
        if approval is not None:
            info = approval
            return PaymentOutcome(
                "approval_required", key, payload, approval=info,
                mandate_id=p.get("mandate_id") if isinstance(p.get("mandate_id"), str) else mandate_id,
                spend_ref=p.get("spend_ref") if isinstance(p.get("spend_ref"), str) else None,
                reason=p.get("reason") if isinstance(p.get("reason"), str) else None,
                _status=lambda: self._status(info.approval_id), _wait=lambda **k: self._wait(info.approval_id, **k),  # type: ignore[attr-defined]
            )
        if tool == "payments.mandates.request":
            if raw.kind != "payload" or not isinstance(p.get("mandate"), dict):
                raise ToolError(f"{tool} answered neither an approval nor a mandate.", status=200, code="invalid_response", body=payload)
            m = p["mandate"]
            return PaymentOutcome("narrowed" if p.get("narrowed") is True else "replayed", key, payload, mandate_id=m.get("mandate_id"), mandate=m, replayed=p.get("replayed") is True)
        if raw.kind != "payload" or not isinstance(p.get("spend"), dict):
            raise ToolError(f"{tool} answered neither a spend nor an approval.", status=200, code="invalid_response", body=payload)
        spend = p["spend"]
        return PaymentOutcome("settled" if spend.get("status") == "settled" else "not_settled", key, payload,
                              mandate_id=spend.get("mandate_id"), spend=spend, spend_ref=spend.get("spend_ref"), replayed=p.get("replayed") is True)


class _StateNoun:
    def __init__(self, list_fn: Callable[..., Any], get_fn: Callable[[int], Any] | None = None) -> None:
        self.list = list_fn
        if get_fn is not None:
            self.get = get_fn


class _Namespace:
    def __init__(self, **members: Any) -> None:
        self.__dict__.update(members)


class ShogunAgent(_Core):
    """Synchronous client."""

    def __init__(self, *, http_client: httpx.Client | None = None, transport: httpx.BaseTransport | None = None, **kw: Any) -> None:
        super().__init__(**kw)
        self._http = http_client or httpx.Client(timeout=self.timeout, transport=transport)
        self.state = _Namespace(
            commitments=_StateNoun(lambda **k: self._state_list("state.commitments.list", **k)),
            open_loops=_StateNoun(lambda **k: self._state_list("state.open_loops.list", **k)),
            people=_StateNoun(lambda **k: self._state_list("state.people.list", **k), lambda i: StateListResult.from_payload(self.call_tool("state.people.get", {"id": i}))),
            projects=_StateNoun(lambda **k: self._state_list("state.projects.list", **k), lambda i: StateListResult.from_payload(self.call_tool("state.projects.get", {"id": i}))),
        )
        self.actions = _Namespace(execute=self._execute, status=self._status, wait_for_approval=self._wait)
        self.router = _Namespace(discover=self._discover, inspect=self._inspect, run=self._run)
        self.payments = _Namespace(list_mandates=self._list_mandates, request_mandate=self._request_mandate, revoke_mandate=self._revoke_mandate, spend=self._spend)
        self.connections = _Namespace(status=self._connections_status, disconnect=self._disconnect)

    def close(self) -> None:
        self._http.close()

    def __enter__(self) -> ShogunAgent:
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()

    def _send(self, method: str, url: str, headers: dict[str, str], body: Any = None) -> Any:
        try:
            response = self._http.request(method, url, headers=headers, content=None if body is None else json.dumps(body))
        except httpx.HTTPError as e:
            raise TransportError(f"Could not reach {url}: {e}", code="transport_error", next_action="retry_after") from e
        return self._parse_http(response)

    def rest(self, path: str, body: Any = None, *, router: bool = False, method: str | None = None) -> Any:
        base = self.router_base_url if router else self.base_url
        return self._send(method or ("GET" if body is None else "POST"), f"{base}{path}", self._headers(), body)

    def _tool(self, name: str, args: dict[str, Any], input_responses: list[dict[str, Any]] | None = None, quantity: int | None = None) -> _Raw:
        url, headers, body = self._tool_request(name, args, input_responses, quantity)
        return self._parse_tool(name, self._send("POST", url, headers, body))

    def call_tool(self, name: str, args: dict[str, Any] | None = None, *, quantity: int | None = None) -> Any:
        """One ``tools/call`` that must answer a payload."""
        return self._require_payload(name, self._tool(name, args or {}, quantity=quantity))

    def recall(self, query: str, *, budget_tokens: int | None = None, include_scratch: bool | None = None) -> RecallResult:
        """``memory.recall``: what memory holds on a question, cut to the budget. Free."""
        return RecallResult.from_payload(self.call_tool("memory.recall", self._recall_args(query, budget_tokens, include_scratch)))

    def ingest(self, *, events: list[dict[str, Any]] | None = None, turns: list[dict[str, Any]] | None = None, source: str | None = None,
               actor: str | None = None, thread_key: str | None = None, metadata: dict[str, Any] | None = None,
               idempotency_key: str | None = None, namespace: str = "world") -> IngestResult:
        """``memory.ingest``; an idempotency key is generated when absent. Metered per event."""
        args, n = self._ingest_args(events, turns, source, actor, thread_key, metadata, idempotency_key, namespace)
        return IngestResult.from_payload(self.call_tool("memory.ingest", args, quantity=n), args["idempotency_key"])

    def context_pack(self, query: str) -> Envelope[Any]:
        """``memory.get_context_pack``. Free."""
        return Envelope.from_payload(self.call_tool("memory.get_context_pack", {"query": query}))

    def _state_list(self, tool: str, *, include_low: bool | None = None, limit: int | None = None, offset: int | None = None) -> StateListResult:
        return StateListResult.from_payload(self.call_tool(tool, self._state_args(include_low, limit, offset)))

    def _execute(self, args: dict[str, Any], *, input_responses: list[dict[str, Any]] | None = None) -> ActionOutcome:
        raw = self._tool("actions.execute", args, input_responses)
        if raw.kind == "input_required":
            return InputRequired(raw.input_requests, lambda responses: self._execute(args, input_responses=responses))
        if raw.kind == "approval_required" and raw.approval is not None:
            return self._approval(raw.approval)
        kind, value = self._action_from_payload(raw.payload)
        if kind == "declined":
            return ActionDeclined(value)
        if kind == "pending":
            return self._approval(value)
        env = Envelope.from_payload(value)
        return ActionResult(env.result, env if isinstance(value, dict) and "result" in value else None)

    def _approval(self, info: ApprovalInfo) -> ApprovalRequired[Any]:
        return ApprovalRequired(info, lambda: self._status(info.approval_id), lambda **k: self._wait(info.approval_id, **k))

    def _status(self, approval_id: int) -> ApprovalStatus:
        return self._approval_status(approval_id, self.call_tool("actions.status", {"approval_id": approval_id}))

    def _wait(self, approval_id: int, *, interval: float = 2.0, timeout: float = 900.0) -> ApprovalStatus:
        deadline = self._now() + timeout
        while True:
            s = self._status(approval_id)
            if s.status != "pending" or self._now() >= deadline:
                return s
            time.sleep(interval)

    def _discover(self, task: str, **kw: Any) -> Envelope[Any]:
        return Envelope.from_payload(self.rest("/v1/router/discover", {"task": task, **kw}, router=True))

    def _inspect(self, tool_id: str) -> Envelope[Any]:
        return Envelope.from_payload(self.rest("/v1/router/inspect", {"tool_id": tool_id}, router=True))

    def _run(self, tool_id: str, input: dict[str, Any], *, idempotency_key: str | None = None, **kw: Any) -> dict[str, Any]:
        key = idempotency_key or new_idempotency_key("run")
        out = self.rest("/v1/router/run", {"tool_id": tool_id, "input": input, "idempotency_key": key, **kw}, router=True)
        return {**(out if isinstance(out, dict) else {"result": out}), "idempotency_key": key}

    def quote(self, *, name: str | None = None, method: str = "tools/call", quantity: int | None = None, **kw: Any) -> QuoteResult:
        """``POST /v1/quote``: the exact cost of a call, nothing reserved."""
        body: dict[str, Any] = {"method": method, **kw}
        if name is not None:
            body["name"] = name
        if quantity is not None:
            body["quantity"] = quantity
        return QuoteResult.from_payload(self.rest("/v1/quote", body))

    def handoff(self, task: str, *, to_agent: str, from_agent: str | None = None, query: str | None = None, budget_tokens: int | None = None,
                ttl_seconds: int | None = None, thread_key: str | None = None) -> dict[str, Any]:
        """``POST /v1/handoff``: ``{"handoff": …, "a2a": {"part": …, "message": …}}``. Free."""
        env = Envelope.from_payload(self.rest("/v1/handoff", self._handoff_body(task, to_agent, from_agent, query, budget_tokens, ttl_seconds, thread_key)))
        return {**(env.result if isinstance(env.result, dict) else {}), "envelope": env}

    def accept_handoff(self, value: Any, *, as_agent: str | None = None, check_addressee: bool = True) -> dict[str, Any]:
        """``handoff.accept``: the core validates a received handoff and stores it in YOUR
        ``agent_scratch`` namespace (never the world). Idempotent; metered per event stored."""
        h, args, quantity = self._accept_plan(value, as_agent, check_addressee)
        return self._accepted(h, self.call_tool("handoff.accept", args, quantity=quantity))

    # payments (docs/api/payments.md): never approves; an L3 answers the URL for a human.

    def _list_mandates(self, *, mandate_id: str | None = None, limit: int | None = None) -> dict[str, Any]:
        """``payments.mandates.list``: ``{"mandates": [...]}``; with ``mandate_id`` also ``mandate`` and its ``spends``. Free."""
        return self._mandate_list(self.call_tool("payments.mandates.list", self._mandate_list_args(mandate_id, limit)))

    def _request_mandate(self, *, currency: str, max_per_payment_minor: int, total_cap_minor: int, purpose: str, payee: str | None = None,
                         payee_class: str | None = None, expires_in_days: int | None = None, expires_at_ms: int | None = None,
                         single_use: bool | None = None, mandate_id: str | None = None, idempotency_key: str | None = None) -> PaymentOutcome:
        """``payments.mandates.request`` (L3): ``approval_required`` with the URL for the human, or ``narrowed`` / ``replayed``."""
        args = self._mandate_request_args(currency=currency, max_per_payment_minor=max_per_payment_minor, total_cap_minor=total_cap_minor, purpose=purpose,
                                          payee=payee, payee_class=payee_class, expires_in_days=expires_in_days, expires_at_ms=expires_at_ms,
                                          single_use=single_use, mandate_id=mandate_id, idempotency_key=idempotency_key)
        return self._payment_outcome("payments.mandates.request", self._tool("payments.mandates.request", args), args["idempotency_key"], mandate_id)

    def _revoke_mandate(self, mandate_id: str, *, idempotency_key: str | None = None) -> dict[str, Any]:
        """``payments.mandates.revoke`` (L1): revoked at once; twice is harmless."""
        args: dict[str, Any] = {"mandate_id": mandate_id}
        if idempotency_key is not None:
            args["idempotency_key"] = idempotency_key
        return self.call_tool("payments.mandates.revoke", args)

    def _spend(self, *, payee: str, currency: str, amount_minor: int, memo: str | None = None, mandate_id: str | None = None,
               idempotency_key: str | None = None) -> PaymentOutcome:
        """``payments.spend`` (L3): ``settled`` inside a mandate, else ``approval_required``. A retry with the same key never pays twice."""
        args = self._spend_args(payee=payee, currency=currency, amount_minor=amount_minor, memo=memo, mandate_id=mandate_id, idempotency_key=idempotency_key)
        return self._payment_outcome("payments.spend", self._tool("payments.spend", args), args["idempotency_key"])

    def _connections_status(self) -> dict[str, Any]:
        """``connections.list``: the principal's connected accounts and their state. Free."""
        return self.call_tool("connections.list", {})

    def _disconnect(self, service: str) -> dict[str, Any]:
        """``connections.disconnect``: remove one connected account; reconnecting is the human's."""
        return self.call_tool("connections.disconnect", {"service": service})


class AsyncShogunAgent(_Core):
    """Asynchronous client (same surface; every call is awaited)."""

    def __init__(self, *, http_client: httpx.AsyncClient | None = None, transport: httpx.AsyncBaseTransport | None = None, **kw: Any) -> None:
        super().__init__(**kw)
        self._http = http_client or httpx.AsyncClient(timeout=self.timeout, transport=transport)
        self.state = _Namespace(
            commitments=_StateNoun(lambda **k: self._state_list("state.commitments.list", **k)),
            open_loops=_StateNoun(lambda **k: self._state_list("state.open_loops.list", **k)),
            people=_StateNoun(lambda **k: self._state_list("state.people.list", **k), lambda i: self._state_get("state.people.get", i)),
            projects=_StateNoun(lambda **k: self._state_list("state.projects.list", **k), lambda i: self._state_get("state.projects.get", i)),
        )
        self.actions = _Namespace(execute=self._execute, status=self._status, wait_for_approval=self._wait)
        self.router = _Namespace(discover=self._discover, inspect=self._inspect, run=self._run)
        self.payments = _Namespace(list_mandates=self._list_mandates, request_mandate=self._request_mandate, revoke_mandate=self._revoke_mandate, spend=self._spend)
        self.connections = _Namespace(status=self._connections_status, disconnect=self._disconnect)

    async def aclose(self) -> None:
        await self._http.aclose()

    async def __aenter__(self) -> AsyncShogunAgent:
        return self

    async def __aexit__(self, *exc: Any) -> None:
        await self.aclose()

    async def _send(self, method: str, url: str, headers: dict[str, str], body: Any = None) -> Any:
        try:
            response = await self._http.request(method, url, headers=headers, content=None if body is None else json.dumps(body))
        except httpx.HTTPError as e:
            raise TransportError(f"Could not reach {url}: {e}", code="transport_error", next_action="retry_after") from e
        return self._parse_http(response)

    async def rest(self, path: str, body: Any = None, *, router: bool = False, method: str | None = None) -> Any:
        base = self.router_base_url if router else self.base_url
        return await self._send(method or ("GET" if body is None else "POST"), f"{base}{path}", self._headers(), body)

    async def _tool(self, name: str, args: dict[str, Any], input_responses: list[dict[str, Any]] | None = None, quantity: int | None = None) -> _Raw:
        url, headers, body = self._tool_request(name, args, input_responses, quantity)
        return self._parse_tool(name, await self._send("POST", url, headers, body))

    async def call_tool(self, name: str, args: dict[str, Any] | None = None, *, quantity: int | None = None) -> Any:
        return self._require_payload(name, await self._tool(name, args or {}, quantity=quantity))

    async def recall(self, query: str, *, budget_tokens: int | None = None, include_scratch: bool | None = None) -> RecallResult:
        return RecallResult.from_payload(await self.call_tool("memory.recall", self._recall_args(query, budget_tokens, include_scratch)))

    async def ingest(self, *, events: list[dict[str, Any]] | None = None, turns: list[dict[str, Any]] | None = None, source: str | None = None,
                     actor: str | None = None, thread_key: str | None = None, metadata: dict[str, Any] | None = None,
                     idempotency_key: str | None = None, namespace: str = "world") -> IngestResult:
        args, n = self._ingest_args(events, turns, source, actor, thread_key, metadata, idempotency_key, namespace)
        return IngestResult.from_payload(await self.call_tool("memory.ingest", args, quantity=n), args["idempotency_key"])

    async def context_pack(self, query: str) -> Envelope[Any]:
        return Envelope.from_payload(await self.call_tool("memory.get_context_pack", {"query": query}))

    async def _state_list(self, tool: str, *, include_low: bool | None = None, limit: int | None = None, offset: int | None = None) -> StateListResult:
        return StateListResult.from_payload(await self.call_tool(tool, self._state_args(include_low, limit, offset)))

    async def _state_get(self, tool: str, id: int) -> StateListResult:
        return StateListResult.from_payload(await self.call_tool(tool, {"id": id}))

    async def _execute(self, args: dict[str, Any], *, input_responses: list[dict[str, Any]] | None = None) -> ActionOutcome:
        raw = await self._tool("actions.execute", args, input_responses)
        if raw.kind == "input_required":
            return InputRequired(raw.input_requests, lambda responses: self._execute(args, input_responses=responses))
        if raw.kind == "approval_required" and raw.approval is not None:
            return self._approval(raw.approval)
        kind, value = self._action_from_payload(raw.payload)
        if kind == "declined":
            return ActionDeclined(value)
        if kind == "pending":
            return self._approval(value)
        env = Envelope.from_payload(value)
        return ActionResult(env.result, env if isinstance(value, dict) and "result" in value else None)

    def _approval(self, info: ApprovalInfo) -> ApprovalRequired[Any]:
        return ApprovalRequired(info, lambda: self._status(info.approval_id), lambda **k: self._wait(info.approval_id, **k))

    async def _status(self, approval_id: int) -> ApprovalStatus:
        return self._approval_status(approval_id, await self.call_tool("actions.status", {"approval_id": approval_id}))

    async def _wait(self, approval_id: int, *, interval: float = 2.0, timeout: float = 900.0) -> ApprovalStatus:
        deadline = self._now() + timeout
        while True:
            s = await self._status(approval_id)
            if s.status != "pending" or self._now() >= deadline:
                return s
            await asyncio.sleep(interval)

    async def _discover(self, task: str, **kw: Any) -> Envelope[Any]:
        return Envelope.from_payload(await self.rest("/v1/router/discover", {"task": task, **kw}, router=True))

    async def _inspect(self, tool_id: str) -> Envelope[Any]:
        return Envelope.from_payload(await self.rest("/v1/router/inspect", {"tool_id": tool_id}, router=True))

    async def _run(self, tool_id: str, input: dict[str, Any], *, idempotency_key: str | None = None, **kw: Any) -> dict[str, Any]:
        key = idempotency_key or new_idempotency_key("run")
        out = await self.rest("/v1/router/run", {"tool_id": tool_id, "input": input, "idempotency_key": key, **kw}, router=True)
        return {**(out if isinstance(out, dict) else {"result": out}), "idempotency_key": key}

    async def quote(self, *, name: str | None = None, method: str = "tools/call", quantity: int | None = None, **kw: Any) -> QuoteResult:
        body: dict[str, Any] = {"method": method, **kw}
        if name is not None:
            body["name"] = name
        if quantity is not None:
            body["quantity"] = quantity
        return QuoteResult.from_payload(await self.rest("/v1/quote", body))

    async def handoff(self, task: str, *, to_agent: str, from_agent: str | None = None, query: str | None = None, budget_tokens: int | None = None,
                      ttl_seconds: int | None = None, thread_key: str | None = None) -> dict[str, Any]:
        env = Envelope.from_payload(await self.rest("/v1/handoff", self._handoff_body(task, to_agent, from_agent, query, budget_tokens, ttl_seconds, thread_key)))
        return {**(env.result if isinstance(env.result, dict) else {}), "envelope": env}

    async def accept_handoff(self, value: Any, *, as_agent: str | None = None, check_addressee: bool = True) -> dict[str, Any]:
        h, args, quantity = self._accept_plan(value, as_agent, check_addressee)
        return self._accepted(h, await self.call_tool("handoff.accept", args, quantity=quantity))

    async def _list_mandates(self, *, mandate_id: str | None = None, limit: int | None = None) -> dict[str, Any]:
        return self._mandate_list(await self.call_tool("payments.mandates.list", self._mandate_list_args(mandate_id, limit)))

    async def _request_mandate(self, *, currency: str, max_per_payment_minor: int, total_cap_minor: int, purpose: str, payee: str | None = None,
                               payee_class: str | None = None, expires_in_days: int | None = None, expires_at_ms: int | None = None,
                               single_use: bool | None = None, mandate_id: str | None = None, idempotency_key: str | None = None) -> PaymentOutcome:
        args = self._mandate_request_args(currency=currency, max_per_payment_minor=max_per_payment_minor, total_cap_minor=total_cap_minor, purpose=purpose,
                                          payee=payee, payee_class=payee_class, expires_in_days=expires_in_days, expires_at_ms=expires_at_ms,
                                          single_use=single_use, mandate_id=mandate_id, idempotency_key=idempotency_key)
        return self._payment_outcome("payments.mandates.request", await self._tool("payments.mandates.request", args), args["idempotency_key"], mandate_id)

    async def _revoke_mandate(self, mandate_id: str, *, idempotency_key: str | None = None) -> dict[str, Any]:
        args: dict[str, Any] = {"mandate_id": mandate_id}
        if idempotency_key is not None:
            args["idempotency_key"] = idempotency_key
        return await self.call_tool("payments.mandates.revoke", args)

    async def _spend(self, *, payee: str, currency: str, amount_minor: int, memo: str | None = None, mandate_id: str | None = None,
                     idempotency_key: str | None = None) -> PaymentOutcome:
        args = self._spend_args(payee=payee, currency=currency, amount_minor=amount_minor, memo=memo, mandate_id=mandate_id, idempotency_key=idempotency_key)
        return self._payment_outcome("payments.spend", await self._tool("payments.spend", args), args["idempotency_key"])

    async def _connections_status(self) -> dict[str, Any]:
        return await self.call_tool("connections.list", {})

    async def _disconnect(self, service: str) -> dict[str, Any]:
        return await self.call_tool("connections.disconnect", {"service": service})

