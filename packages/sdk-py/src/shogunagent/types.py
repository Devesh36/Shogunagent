"""Wire types as dataclasses. Field names are the wire's own (snake_case). Every ``from_dict`` is
tolerant: unknown fields are kept in ``extra`` (or ignored), missing ones take a default."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Generic, Literal, TypeVar

T = TypeVar("T")

Band = Literal["high", "medium", "low"]
Namespace = Literal["world", "agent_scratch"]


def _d(value: Any) -> dict[str, Any]:
    return value if isinstance(value, dict) else {}


@dataclass
class Usage:
    unit: str
    quantity: int
    cost_micro_usd: int

    @classmethod
    def from_dict(cls, d: Any) -> Usage | None:
        if not isinstance(d, dict):
            return None
        return cls(unit=str(d.get("unit", "")), quantity=int(d.get("quantity", 0) or 0), cost_micro_usd=int(d.get("cost_micro_usd", 0) or 0))


@dataclass
class Envelope(Generic[T]):
    """The common envelope (plan §7.2) around a tool's ``result``."""

    result: T
    usage: Usage | None = None
    latency_ms: int | None = None
    hints: list[dict[str, Any]] = field(default_factory=list)
    next_action: str | None = None

    @classmethod
    def from_payload(cls, payload: Any) -> Envelope[Any]:
        p = _d(payload)
        return cls(
            result=p["result"] if "result" in p else payload,
            usage=Usage.from_dict(p.get("usage")),
            latency_ms=p.get("latency_ms") if isinstance(p.get("latency_ms"), int) else None,
            hints=list(p.get("hints") or []) if isinstance(p.get("hints"), list) else [],
            next_action=p.get("next_action") if isinstance(p.get("next_action"), str) else None,
        )


@dataclass
class RecallItem:
    kind: str
    id: int
    text: str
    confidence: float
    band: str
    quote: str | None = None
    event_id: int | None = None

    @classmethod
    def from_dict(cls, d: Any) -> RecallItem:
        d = _d(d)
        return cls(
            kind=str(d.get("kind", "")), id=int(d.get("id", 0) or 0), text=str(d.get("text", "")),
            confidence=float(d.get("confidence", 0) or 0), band=str(d.get("band", "low")),
            quote=d.get("quote") if isinstance(d.get("quote"), str) else None,
            event_id=d.get("event_id") if isinstance(d.get("event_id"), int) else None,
        )


@dataclass
class RecallEvidence:
    event_id: int
    ts_ms: int
    source: str
    excerpt: str
    title: str | None = None

    @classmethod
    def from_dict(cls, d: Any) -> RecallEvidence:
        d = _d(d)
        return cls(
            event_id=int(d.get("event_id", 0) or 0), ts_ms=int(d.get("ts_ms", 0) or 0), source=str(d.get("source", "")),
            excerpt=str(d.get("excerpt", "")), title=d.get("title") if isinstance(d.get("title"), str) else None,
        )


@dataclass
class RecallResult:
    items: list[RecallItem] = field(default_factory=list)
    facts: list[str] = field(default_factory=list)
    possibly: list[str] = field(default_factory=list)
    evidence: list[RecallEvidence] = field(default_factory=list)
    truncated: bool = False
    budget_tokens: int = 0
    used_tokens: int = 0
    include_scratch: bool = False
    envelope: Envelope[Any] | None = None

    @classmethod
    def from_payload(cls, payload: Any) -> RecallResult:
        env = Envelope.from_payload(payload)
        r = _d(env.result)
        return cls(
            items=[RecallItem.from_dict(x) for x in r.get("items") or []],
            facts=[str(x) for x in r.get("facts") or []],
            possibly=[str(x) for x in r.get("possibly") or []],
            evidence=[RecallEvidence.from_dict(x) for x in r.get("evidence") or []],
            truncated=bool(r.get("truncated", False)),
            budget_tokens=int(r.get("budget_tokens", 0) or 0),
            used_tokens=int(r.get("used_tokens", 0) or 0),
            include_scratch=bool(r.get("include_scratch", False)),
            envelope=env,
        )


@dataclass
class IngestResult:
    idempotency_key: str
    event_ids: list[int] = field(default_factory=list)
    inserted: int = 0
    deduplicated: int = 0
    replayed: bool = False
    thread_key: str | None = None
    extra: dict[str, Any] = field(default_factory=dict)
    envelope: Envelope[Any] | None = None

    @classmethod
    def from_payload(cls, payload: Any, key: str) -> IngestResult:
        env = Envelope.from_payload(payload)
        r = _d(env.result)
        known = {"event_ids", "inserted", "deduplicated", "replayed", "thread_key", "idempotency_key"}
        return cls(
            idempotency_key=key,
            event_ids=[int(x) for x in r.get("event_ids") or [] if isinstance(x, int)],
            inserted=int(r.get("inserted", 0) or 0),
            deduplicated=int(r.get("deduplicated", 0) or 0),
            replayed=bool(r.get("replayed", False)),
            thread_key=r.get("thread_key") if isinstance(r.get("thread_key"), str) else None,
            extra={k: v for k, v in r.items() if k not in known},
            envelope=env,
        )


@dataclass
class StateRow:
    text: str
    confidence: float
    possibly: bool
    table: str | None = None
    id: int | None = None
    extra: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_dict(cls, d: Any) -> StateRow:
        d = _d(d)
        return cls(
            text=str(d.get("text", "")), confidence=float(d.get("confidence", 0) or 0), possibly=bool(d.get("possibly", False)),
            table=d.get("table") if isinstance(d.get("table"), str) else None,
            id=d.get("id") if isinstance(d.get("id"), int) else None,
            extra={k: v for k, v in d.items() if k not in {"text", "confidence", "possibly", "table", "id"}},
        )


@dataclass
class StateListResult:
    results: list[StateRow]
    raw: Any = None

    @classmethod
    def from_payload(cls, payload: Any) -> StateListResult:
        p = _d(payload)
        rows = p.get("results")
        if not isinstance(rows, list) and isinstance(p.get("result"), dict):
            rows = p["result"].get("results")
        return cls(results=[StateRow.from_dict(x) for x in rows or []], raw=payload)


@dataclass
class InputRequest:
    id: str
    mode: str
    message: str
    requested_schema: dict[str, Any] | None = None
    url: str | None = None
    raw: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_dict(cls, d: Any) -> InputRequest:
        d = _d(d)
        return cls(
            id=str(d.get("id", "")), mode=str(d.get("mode", "form")), message=str(d.get("message", "")),
            requested_schema=d.get("requestedSchema") if isinstance(d.get("requestedSchema"), dict) else None,
            url=d.get("url") if isinstance(d.get("url"), str) else None, raw=d,
        )


@dataclass
class ApprovalInfo:
    approval_id: int
    level: str
    url: str | None
    message: str
    origin: str | None = None

    @classmethod
    def from_dict(cls, d: Any) -> ApprovalInfo:
        d = _d(d)
        return cls(
            approval_id=int(d.get("approval_id", 0) or 0), level=str(d.get("level", "L3")),
            url=d.get("url") if isinstance(d.get("url"), str) else None, message=str(d.get("message", "")),
            origin=d.get("origin") if isinstance(d.get("origin"), str) else None,
        )


@dataclass
class ApprovalStatus:
    approval_id: int
    status: str
    reason: str | None = None


@dataclass
class PaymentPayload:
    amount_micro_usd: int
    unit: str
    accepts: list[dict[str, Any]]
    retry_after: float
    balance_micro_usd: int | None = None
    cap: dict[str, Any] | None = None

    @classmethod
    def from_dict(cls, d: Any) -> PaymentPayload:
        d = _d(d)
        return cls(
            amount_micro_usd=int(d.get("amount_micro_usd", 0) or 0), unit=str(d.get("unit", "")),
            accepts=[x for x in d.get("accepts") or [] if isinstance(x, dict)], retry_after=float(d.get("retry_after", 0) or 0),
            balance_micro_usd=d.get("balance_micro_usd") if isinstance(d.get("balance_micro_usd"), int) else None,
            cap=d.get("cap") if isinstance(d.get("cap"), dict) else None,
        )


@dataclass
class QuoteResult:
    quote: Usage | None
    sufficient: bool
    balance_micro_usd: int
    next_action: str | None
    raw: dict[str, Any]

    @classmethod
    def from_payload(cls, payload: Any) -> QuoteResult:
        p = _d(payload)
        return cls(
            quote=Usage.from_dict(p.get("quote")), sufficient=bool(p.get("sufficient", False)),
            balance_micro_usd=int(p.get("balance_micro_usd", 0) or 0),
            next_action=p.get("next_action") if isinstance(p.get("next_action"), str) else None, raw=p,
        )
