/** Audit 2026-10-20 C1: `/v1/review` never settles below what the provider billed. A failed review
 * whose response carried `usage` is booked at that usage; a failure whose bill is unknown keeps
 * the whole reservation; the chunk is only returned when nothing reached the provider; the retry
 * is reserved for and both attempts are booked. */
import { afterEach, describe, expect, it, vi } from "vitest";

import { UpstreamError } from "../src/gateway.js";
import { LUNA_MODEL, reserveInputTokens } from "../src/models.js";
import { REVIEW_MAX_ATTEMPTS, REVIEW_MAX_COMPLETION_TOKENS, ReviewRefused, openAiReviewer, reviewBillingOf, withReviewBilling, type Reviewer } from "../src/review.js";
import { outputFormatFor } from "../src/schemas.js";
import { DEFAULT_BATCH_PRICES, priceFor, usageMicros } from "../src/spend.js";
import type { RelayUsage } from "../src/types.js";
import { utcDate } from "../src/usage.js";
import { makeRig } from "./helpers.js";

afterEach(() => vi.unstubAllGlobals());

const NOW = new Date("2026-10-20T03:00:00Z");
const DATE = utcDate(NOW);
const LIC = "lic_test_1";
const USAGE = { prompt_tokens: 1_000, completion_tokens: 16_000, prompt_tokens_details: { cached_tokens: 0 } };
const BILLED: RelayUsage = { input_tokens: 1_000, output_tokens: 16_000, cache_read_input_tokens: 0, cache_creation_input_tokens: 0 };
const price = priceFor(DEFAULT_BATCH_PRICES, LUNA_MODEL);
/** What the route books for `usage`: the sync price, twice the batch card. */
const booked = (usage: RelayUsage): number => Math.ceil(2 * usageMicros(usage, price));
const twice = (u: RelayUsage): RelayUsage => ({
  input_tokens: 2 * u.input_tokens, output_tokens: 2 * u.output_tokens,
  cache_read_input_tokens: 2 * u.cache_read_input_tokens, cache_creation_input_tokens: 2 * u.cache_creation_input_tokens,
});
/** The route's reservation for system "s", user "u" on the consolidation schema. */
const reservation = (): number => {
  const schema = outputFormatFor("consolidation")!.schema;
  const perAttempt = Math.ceil(2 * ((reserveInputTokens("s") + reserveInputTokens("u") + reserveInputTokens(JSON.stringify(schema))) * price.input +
    REVIEW_MAX_COMPLETION_TOKENS * price.output));
  return perAttempt * REVIEW_MAX_ATTEMPTS;
};

const completion = (choice: Record<string, unknown>, usage: unknown = USAGE) => Response.json({ choices: [choice], ...(usage ? { usage } : {}) });

async function rig(reviewer: Reviewer) {
  return makeRig({
    reviewer,
    now: () => NOW,
    spendBreaker: { capMicros: 1_000_000_000, prices: DEFAULT_BATCH_PRICES },
    licenseSpendCapMicros: 1_000_000_000,
  });
}

async function review(r: Awaited<ReturnType<typeof rig>>) {
  return r.app.request("/v1/review", {
    method: "POST",
    headers: { authorization: `Bearer ${r.token}`, "content-type": "application/json" },
    body: JSON.stringify({ purpose: "consolidation", system: "s", user: "u" }),
  });
}

describe("the reviewer reports what was billed, success or failure", () => {
  it("a cut-off, filtered or refused answer carries its usage", async () => {
    for (const choice of [
      { message: { content: "{\"sess" }, finish_reason: "length" },
      { message: { content: "" }, finish_reason: "content_filter" },
      { message: { content: null, refusal: "no" }, finish_reason: "stop" },
    ]) {
      vi.stubGlobal("fetch", vi.fn(async () => completion(choice)));
      const e = await openAiReviewer("k")("s", "u", {}).catch((x: unknown) => x);
      expect(e instanceof UpstreamError || e instanceof ReviewRefused).toBe(true);
      expect(reviewBillingOf(e)).toEqual({ sent: true, complete: true, usage: BILLED });
    }
  });

  it("two invalid answers: both attempts are billed and summed", async () => {
    const fetch = vi.fn(async () => completion({ message: { content: "not json" }, finish_reason: "stop" }));
    vi.stubGlobal("fetch", fetch);
    const e = await openAiReviewer("k")("s", "u", {}).catch((x: unknown) => x);
    expect(e).toBeInstanceOf(ReviewRefused);
    expect(fetch).toHaveBeenCalledTimes(REVIEW_MAX_ATTEMPTS);
    expect(reviewBillingOf(e)).toEqual({ sent: true, complete: true, usage: twice(BILLED) });
  });

  it("a timeout, a 5xx or a 2xx without usage is an unknown bill; a definite 4xx is an unbilled one", async () => {
    vi.stubGlobal("fetch", vi.fn(async () => { throw new DOMException("timed out", "TimeoutError"); }));
    expect(reviewBillingOf(await openAiReviewer("k")("s", "u", {}).catch((x: unknown) => x))).toMatchObject({ sent: true, complete: false });
    vi.stubGlobal("fetch", vi.fn(async () => new Response("{}", { status: 500 })));
    expect(reviewBillingOf(await openAiReviewer("k")("s", "u", {}).catch((x: unknown) => x))).toMatchObject({ sent: true, complete: false });
    vi.stubGlobal("fetch", vi.fn(async () => completion({ message: { content: "{\"a\":1}" }, finish_reason: "stop" }, null)));
    expect((await openAiReviewer("k")("s", "u", {})).billing).toMatchObject({ sent: true, complete: false });
    vi.stubGlobal("fetch", vi.fn(async () => Response.json({ error: { code: "content_filter" } }, { status: 400 })));
    const filtered = await openAiReviewer("k")("s", "u", {}).catch((x: unknown) => x);
    expect(filtered).toBeInstanceOf(ReviewRefused);
    expect(reviewBillingOf(filtered)).toEqual({ sent: true, complete: true, usage: { input_tokens: 0, output_tokens: 0, cache_read_input_tokens: 0, cache_creation_input_tokens: 0 } });
  });

  it("an error that carries no billing reads as an unknown bill, never as free", () => {
    expect(reviewBillingOf(new UpstreamError("review", 500))).toMatchObject({ sent: true, complete: false });
  });
});

describe("/v1/review books what the provider billed (C1)", () => {
  it("a failing reviewer (length / filter / invalid JSON / timeout) leaves spend > 0 and keeps the chunk", async () => {
    const cases: Array<{ name: string; reply: () => Response | Promise<Response>; status: number; spent: () => number }> = [
      { name: "length", reply: () => completion({ message: { content: "{\"sess" }, finish_reason: "length" }), status: 502, spent: () => booked(BILLED) },
      { name: "filter", reply: () => completion({ message: { content: "" }, finish_reason: "content_filter" }), status: 422, spent: () => booked(BILLED) },
      { name: "invalid JSON", reply: () => completion({ message: { content: "nope" }, finish_reason: "stop" }), status: 422, spent: () => booked(twice(BILLED)) },
      { name: "timeout", reply: () => { throw new DOMException("timed out", "TimeoutError"); }, status: 502, spent: reservation },
      { name: "5xx", reply: () => new Response("{}", { status: 503 }), status: 502, spent: reservation },
    ];
    for (const c of cases) {
      vi.stubGlobal("fetch", vi.fn(async () => c.reply()));
      const r = await rig(openAiReviewer("k"));
      const res = await review(r);
      expect(res.status, c.name).toBe(c.status);
      const spent = await r.usage.spentOn(DATE);
      expect(spent, c.name).toBeGreaterThan(0);
      expect(spent, c.name).toBe(c.spent());
      expect(await r.usage.licenseSpentOn(LIC, DATE), c.name).toBe(spent);
      expect(await r.usage.usedOn(LIC, DATE), c.name).toBe(1); // the chunk is kept
    }
  });

  it("an empty first answer then a good one: the retry was reserved for and both usages are booked", async () => {
    const replies = [
      completion({ message: { content: "" }, finish_reason: "stop" }),
      completion({ message: { content: "{\"ok\":1}" }, finish_reason: "stop" }),
    ];
    vi.stubGlobal("fetch", vi.fn(async () => replies.shift()!));
    const r = await rig(openAiReviewer("k"));
    // The reservation covers both attempts at their full answer budget.
    expect(reservation()).toBeGreaterThanOrEqual(REVIEW_MAX_ATTEMPTS * booked({ ...BILLED, input_tokens: 0 }));
    const res = await review(r);
    expect(res.status).toBe(200);
    expect(await r.usage.spentOn(DATE)).toBe(booked(twice(BILLED)));
    expect(await r.usage.usedOn(LIC, DATE)).toBe(1);
  });

  it("a review that never reached the provider returns the chunk and the estimate", async () => {
    const r = await rig(async () => { throw withReviewBilling(new UpstreamError("review", 0), { sent: false }); });
    expect((await review(r)).status).toBe(502);
    expect(await r.usage.spentOn(DATE)).toBe(0);
    expect(await r.usage.usedOn(LIC, DATE)).toBe(0);
  });

  it("a definitely rejected request (4xx) books nothing billed but keeps the chunk", async () => {
    vi.stubGlobal("fetch", vi.fn(async () => Response.json({ error: { code: "invalid_request_error" } }, { status: 400 })));
    const r = await rig(openAiReviewer("k"));
    expect((await review(r)).status).toBe(502);
    expect(await r.usage.spentOn(DATE)).toBe(0);
    expect(await r.usage.usedOn(LIC, DATE)).toBe(1);
  });

  it("a successful answer without a readable bill keeps the whole reservation", async () => {
    vi.stubGlobal("fetch", vi.fn(async () => completion({ message: { content: "{\"a\":1}" }, finish_reason: "stop" }, null)));
    const r = await rig(openAiReviewer("k"));
    expect((await review(r)).status).toBe(200);
    expect(await r.usage.spentOn(DATE)).toBe(reservation());
  });
});
