"""The A2A state-of-work handoff (docs/api/handoff.md): a strict subset of export v1 one agent
hands to another with a task. ``accept_handoff`` lands it in the receiver's own scratch namespace,
never the principal's world."""

from __future__ import annotations

from datetime import datetime, timezone
from typing import Any

from .errors import HandoffError

HANDOFF_FORMAT = "shogunagent-handoff/v1"
HANDOFF_SCHEMA_URL = "https://shogunagent.dev/schemas/handoff.schema.json"


def extract_handoff(value: Any) -> dict[str, Any]:
    """Pull the handoff out of the object, an A2A data part, an A2A message, or ``handoff()``'s result."""
    v = value
    if isinstance(v, dict) and isinstance(v.get("handoff"), dict):
        v = v["handoff"]
    if isinstance(v, dict) and isinstance(v.get("parts"), list):
        v = next(
            (p for p in v["parts"] if isinstance(p, dict) and p.get("kind") == "data" and isinstance(p.get("data"), dict) and p["data"].get("format") == HANDOFF_FORMAT),
            None,
        )
    if isinstance(v, dict) and v.get("kind") == "data" and isinstance(v.get("data"), dict):
        v = v["data"]
    if not isinstance(v, dict) or v.get("format") != HANDOFF_FORMAT:
        raise HandoffError(f"Not a {HANDOFF_FORMAT} object.", code="invalid_handoff", next_action="fix_input")
    if not isinstance(v.get("handoff_id"), str) or not isinstance(v.get("expires_at"), int) or not isinstance(v.get("entities"), list) or not isinstance(v.get("facts"), list) or not isinstance(v.get("task"), dict):
        raise HandoffError("The handoff is missing handoff_id, expires_at, task, entities or facts.", code="invalid_handoff", next_action="fix_input")
    return v


def check_handoff(h: dict[str, Any], receiver: str | None, now_ms: int) -> None:
    if h["expires_at"] <= now_ms:
        when = datetime.fromtimestamp(h["expires_at"] / 1000, tz=timezone.utc).isoformat()
        raise HandoffError(f"The handoff expired at {when}. Ask the sender for a fresh one.", code="handoff_expired", next_action="retry_after")
    if receiver is not None and h.get("to_agent") != receiver:
        raise HandoffError(f"The handoff is addressed to {h.get('to_agent')}, not {receiver}.", code="handoff_not_for_you", next_action="not_allowed")


def _provenance(p: Any) -> str:
    quoted = [f'"{x["quote"]}" (event {x.get("event_id")})' for x in (p or []) if isinstance(x, dict) and x.get("quote")]
    return f" Source: {'; '.join(quoted)}" if quoted else ""


def handoff_events(h: dict[str, Any]) -> list[dict[str, Any]]:
    """One event for the task, one per entity and fact; Medium keeps ``possibly:``; Low skipped."""
    task = h.get("task") or {}
    events = [{"kind": "handoff_task", "title": f"handoff from {h.get('from_agent')}"[:200], "content": f"Task handed over by {h.get('from_agent')}: {task.get('summary', '')}"}]
    for e in h.get("entities", []):
        if e.get("band") not in ("high", "medium"):
            continue
        mark = "possibly: " if e["band"] == "medium" else ""
        events.append({"kind": "handoff_entity", "title": f"{e.get('kind')} #{e.get('id')}", "content": f"{mark}{e.get('kind')} {e.get('name')} (confidence {e.get('confidence')}).{_provenance(e.get('provenance'))}"})
    for f in h.get("facts", []):
        if f.get("band") not in ("high", "medium"):
            continue
        mark = "possibly: " if f["band"] == "medium" else ""
        kind = str(f.get("kind", "")).replace("_", " ")
        events.append({"kind": "handoff_fact", "title": f"{f.get('kind')} #{f.get('id')}", "content": f"{mark}{kind}: {f.get('text')} (confidence {f.get('confidence')}).{_provenance(f.get('provenance'))}"})
    for ev in events:
        ev["content"] = ev["content"][:16_000]
    return events
