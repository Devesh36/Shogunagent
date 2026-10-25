"""E36b: ``with_shogun_agent(Runner.run)`` / ``Runner.run_sync`` around the real OpenAI Agents SDK
(``openai-agents``): a real ``Agent`` and ``Runner``, with a ``Model`` implementation that records
the system instructions it was given and answers a fixed message. Tracing is disabled; no network."""

from __future__ import annotations

import asyncio
from typing import Any

from conftest import PAYMENT_402, Fake, memory_fake, recall_payload, tool_text
from frameworks_support import block_count, no_network, require  # noqa: F401  (no_network: autouse fixture)
from shogunagent import PaymentRequiredError
from shogunagent.adapters import openai_agents as adapter

agents = require("agents")
interface = require("agents.models.interface")
items = require("agents.items")
usage_mod = require("agents.usage")
response_types = require("openai.types.responses")

agents.set_tracing_disabled(True)


class RecordingModel(interface.Model):  # type: ignore[misc,name-defined]
    def __init__(self, reply: str, response_id: str) -> None:
        self.reply = reply
        self.response_id = response_id
        self.instructions: list[str | None] = []

    async def get_response(self, system_instructions: Any, input: Any, *args: Any, **kwargs: Any) -> Any:
        self.instructions.append(system_instructions)
        message = response_types.ResponseOutputMessage(
            id="msg_1",
            type="message",
            role="assistant",
            status="completed",
            content=[response_types.ResponseOutputText(type="output_text", text=self.reply, annotations=[])],
        )
        return items.ModelResponse(output=[message], usage=usage_mod.Usage(requests=1), response_id=self.response_id)

    def stream_response(self, *args: Any, **kwargs: Any) -> Any:  # pragma: no cover - not used
        raise NotImplementedError


def agent(model: RecordingModel, instructions: str = "You are helpful.") -> Any:
    return agents.Agent(name="helper", instructions=instructions, model=model)


def test_run_recalls_into_the_instructions_and_ingests_after() -> None:
    fake = memory_fake()
    model = RecordingModel("I will draft the plan.", "resp_1")
    run = adapter.with_shogun_agent(agents.Runner.run, client=fake.aclient(), thread_key="task-1")
    result = asyncio.run(run(agent(model), "What do I owe Oren?"))

    assert result.final_output == "I will draft the plan."
    instructions = model.instructions[0] or ""
    assert instructions.startswith("You are helpful.")
    assert block_count(instructions) == 1
    assert "send the rollout plan by Friday" in instructions
    assert "SECRET-LOW-ITEM" not in instructions
    assert [c.headers.get("mcp-name") for c in fake.calls] == ["memory.recall", "memory.ingest"]
    args = fake.tool_calls("memory.ingest")[0].body["params"]["arguments"]
    assert args["thread_key"] == "task-1"
    assert args["turns"] == [{"role": "user", "content": "What do I owe Oren?"}, {"role": "assistant", "content": "I will draft the plan."}]
    assert args["idempotency_key"].startswith("turn:")


def test_run_sync_composes_with_the_callers_filter_and_replays_by_response_id() -> None:
    fake = memory_fake()
    calls = {"n": 0}

    def mine(data: Any) -> Any:
        calls["n"] += 1
        md = data.model_data
        return agents.run.ModelInputData(input=md.input, instructions=(md.instructions or "") + "\nCALLER-FILTER")

    run = adapter.with_shogun_agent(agents.Runner.run_sync, client=fake.client(), thread_key="task-1")
    models = [RecordingModel("a", "resp_1"), RecordingModel("a", "resp_1"), RecordingModel("b", "resp_2")]
    for model in models:
        run(agent(model, "sys"), "status?", run_config=agents.RunConfig(call_model_input_filter=mine))
    assert calls["n"] == 3
    assert "CALLER-FILTER" in (models[0].instructions[0] or "")
    assert block_count(models[0].instructions[0] or "") == 1
    keys = [c.body["params"]["arguments"]["idempotency_key"] for c in fake.tool_calls("memory.ingest")]
    assert keys[0] == keys[1] and keys[2] != keys[0]


def test_402_on_ingest_is_surfaced_once_and_the_result_returned() -> None:
    fake = Fake().tool("memory.recall", tool_text(recall_payload())).tool("memory.ingest", PAYMENT_402)
    errors: list = []
    run = adapter.with_shogun_agent(agents.Runner.run, client=fake.aclient(), on_error=lambda e, p: errors.append((e, p)))
    result = asyncio.run(run(agent(RecordingModel("fine", "r")), "q"))
    assert result.final_output == "fine"
    assert len(fake.tool_calls("memory.ingest")) == 1
    assert len(errors) == 1 and isinstance(errors[0][0], PaymentRequiredError) and errors[0][1] == "ingest"
