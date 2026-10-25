/**
 * Invariant 3 over the whole compiled catalog, the router's port of
 * `crates/shogun-mcp/tests/invariant4.rs`: an L1 path can never reach an external send.
 */
import { describe, expect, it } from "vitest";
import { authorize, buildScopeTable, isExternalSend, levelFor } from "../src/scope.js";
import { run } from "../src/run.js";
import { bearer, fullCatalog, makeDeps } from "./helpers.js";

describe("scope table (E8-T5, invariant 3)", () => {
  it("an operation not in the table is denied, never defaulted", () => {
    const table = buildScopeTable([{ tool_id: "a.read", class: "read" }]);
    expect(authorize(table, "a.read")).toEqual({ kind: "allowed", level: "L1", class: "read" });
    expect(authorize(table, "a.unknown")).toEqual({ kind: "denied_unknown_op" });
    expect(authorize(table, "")).toEqual({ kind: "denied_unknown_op" });
  });

  it("the level mapping is the written rule: reads L1, user-data writes L2, sends and payments L3", () => {
    expect(levelFor("read")).toBe("L1");
    expect(levelFor("user_data_write")).toBe("L2");
    expect(levelFor("service_state_change")).toBe("L2");
    expect(levelFor("external_send")).toBe("L3");
    expect(levelFor("payment")).toBe("L3");
    expect(isExternalSend("external_send")).toBe(true);
    expect(isExternalSend("payment")).toBe(true);
    expect(isExternalSend("read")).toBe(false);
  });

  it("over the whole catalog: every external send or payment is L3 and no L1 entry is a send", () => {
    const catalog = fullCatalog();
    expect(catalog.entries().length).toBeGreaterThan(0);
    for (const e of catalog.entries()) {
      const row = catalog.scope.get(e.tool_id);
      expect(row, e.tool_id).toBeDefined();
      if (isExternalSend(e.scope.class)) expect(e.scope.level, `${e.tool_id} must be L3`).toBe("L3");
      if (e.scope.level === "L1") expect(isExternalSend(e.scope.class), `${e.tool_id} is L1 and must not send`).toBe(false);
      expect(e.scope.level).toBe(levelFor(e.scope.class));
    }
    expect(catalog.get("shogunagent.actions.execute")?.scope.level).toBe("L3");
    expect(catalog.get("monid.saperly.provision-numbers")?.scope.level).toBe("L3");
  });

  it("run never executes an L3 tool directly, whoever calls", async () => {
    const deps = makeDeps();
    const toolId = "monid.saperly.provision-numbers";
    const out = await run(deps, { input: { tool_id: toolId, input: { country: "US" }, idempotency_key: "p" }, bearer: bearer() });
    expect(out.body.resultType).toBe("approval_required");
    expect(deps.calls).toHaveLength(0);
  });
});
