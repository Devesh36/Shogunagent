"""shogunagent — the Python client of ShogunAgent, the state layer for AI agents.

Framework middleware lives in ``shogunagent.adapters.{openai_agents, claude_agent_sdk, langgraph, crewai}``.
"""

from .client import (
    HOSTED_BASE_URL,
    LOCAL_BASE_URL,
    MAX_INGEST_ITEMS,
    MCP_PROTOCOL_VERSION,
    SDK_VERSION,
    ActionDeclined,
    ActionOutcome,
    ActionResult,
    ApprovalRequired,
    AsyncShogunAgent,
    InputRequired,
    PaymentOutcome,
    ShogunAgent,
    new_idempotency_key,
)
from .context import CONTEXT_CLOSE, CONTEXT_OPEN, context_block, escape_context, render_context, replace_context, strip_context
from .errors import (
    AuthenticationError,
    HandoffError,
    PaymentRequiredError,
    PermissionDeniedError,
    RateLimitError,
    ServiceUnavailableError,
    ShogunAgentError,
    ToolError,
    TransportError,
)
from .handoff import HANDOFF_FORMAT, HANDOFF_SCHEMA_URL, check_handoff, extract_handoff, handoff_events
from .middleware import AsyncShogunAgentMemory, ShogunAgentMemory, ShogunAgentOptions, turn_idempotency_key
from .types import (
    ApprovalInfo,
    ApprovalStatus,
    Envelope,
    IngestResult,
    InputRequest,
    PaymentPayload,
    QuoteResult,
    RecallEvidence,
    RecallItem,
    RecallResult,
    StateListResult,
    StateRow,
    Usage,
)

__version__ = SDK_VERSION

__all__ = [
    "HOSTED_BASE_URL", "LOCAL_BASE_URL", "MAX_INGEST_ITEMS", "MCP_PROTOCOL_VERSION", "SDK_VERSION",
    "ActionDeclined", "ActionOutcome", "ActionResult", "ApprovalRequired", "AsyncShogunAgent", "InputRequired", "PaymentOutcome", "ShogunAgent", "new_idempotency_key",
    "CONTEXT_CLOSE", "CONTEXT_OPEN", "context_block", "escape_context", "render_context", "replace_context", "strip_context",
    "AuthenticationError", "HandoffError", "PaymentRequiredError", "PermissionDeniedError", "RateLimitError", "ServiceUnavailableError",
    "ShogunAgentError", "ToolError", "TransportError",
    "HANDOFF_FORMAT", "HANDOFF_SCHEMA_URL", "check_handoff", "extract_handoff", "handoff_events",
    "AsyncShogunAgentMemory", "ShogunAgentMemory", "ShogunAgentOptions", "turn_idempotency_key",
    "ApprovalInfo", "ApprovalStatus", "Envelope", "IngestResult", "InputRequest", "PaymentPayload", "QuoteResult",
    "RecallEvidence", "RecallItem", "RecallResult", "StateListResult", "StateRow", "Usage",
]
