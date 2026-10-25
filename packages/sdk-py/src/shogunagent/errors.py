"""Typed errors. Branch on the class (or ``code``), then on ``next_action``; never parse ``message``."""

from __future__ import annotations

import base64
import json
from typing import Any, Mapping

from .types import PaymentPayload


class ShogunAgentError(Exception):
    """Every error this SDK raises for an answer the server gave."""

    def __init__(
        self,
        message: str,
        *,
        status: int = 0,
        code: str = "error",
        next_action: str | None = None,
        retry_after: float | None = None,
        body: Any = None,
    ) -> None:
        super().__init__(message)
        self.message = message
        self.status = status
        self.code = code
        self.next_action = next_action
        self.retry_after = retry_after
        self.body = body

    def __repr__(self) -> str:  # pragma: no cover - cosmetic
        return f"{type(self).__name__}(status={self.status}, code={self.code!r}, next_action={self.next_action!r})"


class AuthenticationError(ShogunAgentError):
    """401: no key, a bad key, or a revoked one."""


class PaymentRequiredError(ShogunAgentError):
    """402: ``payment_required`` (top up, then retry unchanged) or ``cap_exceeded`` (wait or send less)."""

    def __init__(self, message: str, *, payment: PaymentPayload | None, payment_required_header: dict[str, Any] | None, **kw: Any) -> None:
        super().__init__(message, **kw)
        self.payment = payment
        self.payment_required_header = payment_required_header

    @property
    def top_up_url(self) -> str | None:
        if self.payment and self.payment.accepts:
            url = self.payment.accepts[0].get("url")
            return url if isinstance(url, str) else None
        return None

    @property
    def shortfall_micro_usd(self) -> int | None:
        p = self.payment
        if p is None or p.balance_micro_usd is None:
            return None
        return max(0, p.amount_micro_usd - p.balance_micro_usd)


class PermissionDeniedError(ShogunAgentError):
    """403: the key lacks the scope, or the action is not allowed here."""


class RateLimitError(ShogunAgentError):
    """429: wait ``retry_after`` seconds; do not spin."""


class ServiceUnavailableError(ShogunAgentError):
    """5xx: the gateway, the core or a provider is unavailable. Nothing was charged."""


class ToolError(ShogunAgentError):
    """The tool refused the call (``isError`` or a JSON-RPC error)."""


class TransportError(ShogunAgentError):
    """The request never got an HTTP answer."""


class HandoffError(ShogunAgentError):
    """A handoff that is malformed, for someone else, or expired."""


def _number(value: Any) -> float | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, (int, float)):
        return float(value)
    if isinstance(value, str):
        try:
            return float(value.strip())
        except ValueError:
            return None
    return None


def decode_payment_required_header(value: str | None) -> dict[str, Any] | None:
    """Decode the x402-style ``PAYMENT-REQUIRED`` header (base64url JSON)."""
    if not value:
        return None
    try:
        text = value.strip()
        raw = base64.urlsafe_b64decode(text + "=" * (-len(text) % 4))
        parsed = json.loads(raw.decode("utf-8"))
    except Exception:
        return None
    return parsed if isinstance(parsed, dict) else None


def error_from_response(status: int, body: Any, headers: Mapping[str, str]) -> ShogunAgentError:
    """The typed error for a non-2xx answer (gateway ``{error:{code,message,next_action}}`` or the
    local sidecar's ``{"error": "<code>"}``)."""
    obj = body if isinstance(body, dict) else {}
    code = f"http_{status}"
    message = f"The server answered {status}."
    next_action: str | None = None
    err = obj.get("error")
    if isinstance(err, dict):
        code = err.get("code") if isinstance(err.get("code"), str) else code
        message = err.get("message") if isinstance(err.get("message"), str) else message
        next_action = err.get("next_action") if isinstance(err.get("next_action"), str) else None
    elif isinstance(err, str):
        code = err
        for key in ("message", "detail"):
            if isinstance(obj.get(key), str):
                message = obj[key]
                break
    if next_action is None and isinstance(obj.get("next_action"), str):
        next_action = obj["next_action"]
    payment = PaymentPayload.from_dict(obj["payment"]) if isinstance(obj.get("payment"), dict) else None
    lower = {k.lower(): v for k, v in headers.items()}
    retry_after = _number(lower.get("retry-after"))
    if retry_after is None and payment is not None:
        retry_after = _number(payment.retry_after)
    kw: dict[str, Any] = {"status": status, "code": code, "retry_after": retry_after, "body": body}
    if status == 401:
        return AuthenticationError(message, next_action=next_action or "authenticate", **kw)
    if status == 402:
        return PaymentRequiredError(
            message,
            next_action=next_action or ("retry_after" if code == "cap_exceeded" else "top_up"),
            payment=payment,
            payment_required_header=decode_payment_required_header(lower.get("payment-required")),
            **kw,
        )
    if status == 403:
        return PermissionDeniedError(message, next_action=next_action or "not_allowed", **kw)
    if status == 429:
        return RateLimitError(message, next_action=next_action or "retry_after", **kw)
    if status >= 500:
        return ServiceUnavailableError(message, next_action=next_action or "retry_after", **kw)
    return ShogunAgentError(message, next_action=next_action, **kw)
