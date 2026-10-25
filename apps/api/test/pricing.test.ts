/** E9-T5 / E10-T7: one rate table — the classification the metering enforces and the document
 * `/.well-known/pricing.json` publishes come from the same module. */
import { describe, expect, it } from "vitest";

import { DEFAULT_CAPS } from "../src/caps.js";
import { FREE_ALLOWANCE_MICROS, RATE_MICROS, RUN_MARKUP_BPS, classifyCall, pricingDocument, quoteCost } from "../src/pricing.js";
import { makeAgentRig } from "./agent-helpers.js";

describe("classifyCall", () => {
  it("reads are free, ingest and runs are named, everything else on tools/call is a model op", () => {
    for (const name of ["memory.search", "memory.get_context_pack", "memory.recall", "state.commitments.list", "state.people.get", "approvals.decide", "profile.whoami", "router.discover", "router.inspect", "actions.status"]) {
      expect(classifyCall("tools/call", name)).toBe("read");
    }
    expect(classifyCall("tools/call", "memory.ingest")).toBe("ingest");
    expect(classifyCall("tools/call", "memory.append_note")).toBe("ingest");
    expect(classifyCall("tools/call", "router.run")).toBe("run");
    expect(classifyCall("tools/call", "actions.execute")).toBe("run");
    expect(classifyCall("tools/call", "memory.summarize")).toBe("model_op");
    expect(classifyCall("tools/call", undefined)).toBe("model_op");
    for (const method of ["initialize", "tools/list", "ping", "resources/list", "notifications/initialized", undefined]) {
      expect(classifyCall(method, "memory.ingest")).toBe("protocol");
    }
  });

  it("prices flat units per quantity and runs at provider price + 5%, rounded up", () => {
    expect(quoteCost("ingest", 1000)).toEqual({ unit: "ingest", quantity: 1000, cost_micro_usd: 500_000 });
    expect(quoteCost("model_op", 1000).cost_micro_usd).toBe(100_000);
    expect(quoteCost("read", 50).cost_micro_usd).toBe(0);
    expect(quoteCost("run", 1, 1_000).cost_micro_usd).toBe(1_050);
    expect(quoteCost("run", 1, 1).cost_micro_usd).toBe(2);
    expect(quoteCost("run", 3, 1_000).cost_micro_usd).toBe(3_150);
    expect(quoteCost("run", 1, 0).cost_micro_usd).toBe(0);
    expect(() => quoteCost("run", 1)).toThrow("run price missing");
    expect(quoteCost("ingest", 0).quantity).toBe(1);
  });
});

describe("GET /.well-known/pricing.json", () => {
  it("publishes the enforced table, caps and allowance, without auth", async () => {
    const rig = makeAgentRig();
    const res = await rig.app.request("/.well-known/pricing.json");
    expect(res.status).toBe(200);
    const doc = (await res.json()) as Record<string, unknown>;
    expect(doc).toEqual(pricingDocument({ caps: DEFAULT_CAPS }));
    const units = doc.units as Record<string, Record<string, unknown>>;
    expect(units.ingest!.micro_usd).toBe(RATE_MICROS.ingest);
    expect(units.ingest!.usd_per_1000).toBe(0.5);
    expect(units.model_op!.usd_per_1000).toBe(0.1);
    expect(units.run!.markup_bps).toBe(RUN_MARKUP_BPS);
    expect(units.read!.micro_usd).toBe(0);
    expect((units.read!.tools as string[])).toContain("memory.recall");
    expect(doc.free_allowance_micro_usd).toBe(FREE_ALLOWANCE_MICROS);
    expect(doc.caps).toEqual({ key: { day_micro_usd: 1_000_000, month_micro_usd: 20_000_000 }, principal: { day_micro_usd: 5_000_000, month_micro_usd: 100_000_000 } });
    expect(rig.logLines[0]).toMatch(/^GET \/\.well-known\/pricing\.json 200/);
  });

  it("reflects configured caps", async () => {
    const rig = makeAgentRig({ caps: { key: { day: 1, month: 2 }, principal: { day: 3, month: 4 } } });
    const doc = (await (await rig.app.request("/.well-known/pricing.json")).json()) as { caps: unknown };
    expect(doc.caps).toEqual({ key: { day_micro_usd: 1, month_micro_usd: 2 }, principal: { day_micro_usd: 3, month_micro_usd: 4 } });
  });
});
