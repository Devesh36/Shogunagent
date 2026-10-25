/**
 * The listing pipeline (E21): what a provider's connector must pass before it is listed.
 * `pnpm --filter @shogunagent/router listing:check <provider-dir>` (scripts/listing-check.ts) runs
 * it and prints the machine-readable report; `.github/workflows/router-listing.yml` runs it on
 * every PR touching `apps/router/connectors/**` and posts the report as the job summary.
 *
 * Five checks, each reporting errors (block the listing) and warnings (shown, do not block):
 *
 *   schema        provider.json present and valid, and the provider compiles into the catalog
 *                 (ids, categories, scope class, health, usage numbers, fixture inputs).
 *   fixtures      every endpoint has fixtures; each is replayed offline through the connector's
 *                 own hooks (request mapping, failure detection, output mapping, settlement) and
 *                 through the real `run` engine with its permission tier (L1 runs, L2 asks once
 *                 and then runs, L3 returns an approval URL and never reaches the transport).
 *   scope         every write declares its class; anything named like a send, post or invite is
 *                 `external_send` and anything named like a payment is `payment` — both L3; a
 *                 write verb or a DELETE/PUT/PATCH is never declared `read`.
 *   descriptions  written for agents: ≤ 400 characters, names the tasks ("Use for: …"), states
 *                 the cost class, has `exclusions`, no marketing superlatives, L2/L3 stated.
 *   price         provider.json declares exactly the endpoint's usage model, and the usage
 *                 observed when replaying fixtures follows that model; quotes far from observed
 *                 costs are flagged.
 *
 * Nothing here calls the network: the transport is a replay of the recorded fixture.
 */
import type { EndpointDefinition, Fixture, Json, ProviderDefinition, RunResponse } from "../../connectors/types.js";
import { CatalogCompileError, compileCatalog, publicUsage, quoteMicroUsd, type CompiledCatalog } from "../catalog.js";
import { contentHash } from "../hash.js";
import { InMemoryHealthStore } from "../health.js";
import { InMemoryIdempotencyStore } from "../idempotency.js";
import { silentLogger } from "../logging.js";
import { InMemoryMeteringClient } from "../metering.js";
import { defaultRequest, run, type RunDeps } from "../run.js";
import { noopRunLogSink } from "../runlog.js";
import { levelFor, type Level } from "../scope.js";
import { Secret } from "../secret.js";
import { createReplayTransport } from "../transport.js";
import { settleUsage, type Usage } from "../usage.js";
import { declarationFor, ProviderManifestSchema } from "./manifest.js";

export const LISTING_REPORT_FORMAT = "shogunagent-listing-report/1";
export const DESCRIPTION_MAX_CHARS = 400;
export const SUMMARY_MAX_CHARS = 160;

export type CheckName = "schema" | "fixtures" | "scope" | "descriptions" | "price";
export const CHECKS: readonly CheckName[] = ["schema", "fixtures", "scope", "descriptions", "price"];

export interface ListingProblem {
  check: CheckName;
  severity: "error" | "warning";
  tool_id: string | null;
  message: string;
}

export interface EndpointReport {
  tool_id: string;
  class: string | null;
  level: Level | null;
  quote_micro_usd: number | null;
  fixtures: number;
  fixtures_replayed: number;
  description_chars: number;
}

export interface ListingReport {
  format: typeof LISTING_REPORT_FORMAT;
  provider_dir: string;
  provider_id: string | null;
  ok: boolean;
  /** Set when the directory is not a self-serve listing (e.g. an imported passthrough catalog). */
  skipped: string | null;
  checks: { name: CheckName; ok: boolean; errors: number; warnings: number }[];
  problems: ListingProblem[];
  endpoints: EndpointReport[];
  /** Hash of the provider's compiled entries, when it compiled. */
  catalog_hash: string | null;
}

/** Words a marketing listing uses and an agent cannot act on. Matched case-insensitively as words. */
export const MARKETING_SUPERLATIVES = [
  "best", "best-in-class", "leading", "industry-leading", "world-class", "revolutionary", "cutting-edge", "state-of-the-art",
  "next-generation", "next-gen", "seamless", "seamlessly", "powerful", "ultimate", "unparalleled", "unmatched", "blazing",
  "blazing-fast", "lightning-fast", "game-changing", "amazing", "incredible", "magical", "effortless", "effortlessly",
  "ai-powered", "second brain", "#1", "number one", "unbeatable", "superior",
] as const;

/** Id segments that name a send: must be `external_send` (L3). */
const SEND_SEGMENTS = new Set(["send", "post", "publish", "invite", "notify", "broadcast", "reply", "forward", "share", "tweet"]);
/** Id segments that name a payment or provisioning: must be L3 (`payment`, or `external_send` for provisioning). */
const PAY_SEGMENTS = new Set(["pay", "payment", "charge", "purchase", "buy", "transfer", "refund", "checkout", "order", "provision", "subscribe"]);
/** Id segments that name a write: never `read`. */
const WRITE_SEGMENTS = new Set(["create", "update", "delete", "remove", "add", "set", "write", "insert", "upload", "archive", "label", "move", "close", "merge", "assign", "execute", "draft", "drafts", "edit", "patch", "put"]);

function segments(id: string): string[] {
  return id.toLowerCase().split(/[._/-]+/).filter(Boolean);
}

function escapeRe(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/** Superlatives found in a text (whole words, case-insensitive). */
export function findSuperlatives(text: string): string[] {
  const found: string[] = [];
  for (const w of MARKETING_SUPERLATIVES) {
    const re = new RegExp(`(^|[^a-z0-9#-])${escapeRe(w)}($|[^a-z0-9-])`, "i");
    if (re.test(text)) found.push(w);
  }
  return found;
}

/** What replaying a fixture through the connector's own hooks produces (no engine, no scope). */
export interface FixtureReplay {
  failed: boolean;
  output: Json | null;
  usage: Usage | null;
  /** The request the connector built (so a hook that throws is caught here). */
  request_ok: boolean;
}

export function replayFixture(endpoint: EndpointDefinition, fixture: Fixture): FixtureReplay {
  const validated = endpoint.input.schema.validate(fixture.input);
  if (!validated.ok) return { failed: true, output: null, usage: null, request_ok: false };
  const input = validated.value;
  try {
    if (endpoint.input.toRequest) endpoint.input.toRequest({ input, request: { method: endpoint.request.method, path: endpoint.request.path } });
    else defaultRequest(endpoint, input);
  } catch {
    return { failed: true, output: null, usage: null, request_ok: false };
  }
  const response: RunResponse = fixture.response;
  const failed = endpoint.output.isFailure ? endpoint.output.isFailure(response) : response.status >= 400;
  if (failed) return { failed: true, output: null, usage: null, request_ok: true };
  const output = endpoint.output.fromResponse ? endpoint.output.fromResponse({ input, response }) : response.body;
  return { failed: false, output, usage: settleUsage(endpoint, { input, response, output }), request_ok: true };
}

const LISTING_KEY = "sa_listing_check_key";

function engineDeps(catalog: CompiledCatalog, fixture: Fixture, calls: { n: number }): RunDeps {
  return {
    catalog,
    transport: createReplayTransport(() => {
      calls.n++;
      return fixture.response;
    }),
    metering: new InMemoryMeteringClient({ defaultBalanceMicroUsd: 1_000_000_000 }),
    runLog: noopRunLogSink,
    idempotency: new InMemoryIdempotencyStore(),
    health: new InMemoryHealthStore(),
    logger: silentLogger,
    approvalBaseUrl: "https://approve.listing-check.invalid/approve",
  };
}

async function engineReplay(catalog: CompiledCatalog, toolId: string, level: Level, fixture: Fixture, add: (m: string) => void): Promise<void> {
  const calls = { n: 0 };
  const deps = engineDeps(catalog, fixture, calls);
  const input = fixture.input as Record<string, unknown>;
  const first = await run(deps, { input: { tool_id: toolId, input, idempotency_key: `listing:${fixture.scenario}` }, bearer: new Secret(LISTING_KEY) });
  if (level === "L3") {
    if (first.body.resultType !== "approval_required") add(`"${fixture.scenario}": an L3 run must return approval_required, got ${first.body.resultType}`);
    if (calls.n !== 0) add(`"${fixture.scenario}": an L3 run reached the transport without approval`);
    return;
  }
  let out = first;
  if (level === "L2") {
    if (first.body.resultType !== "input_required") add(`"${fixture.scenario}": an L2 run must first return input_required, got ${first.body.resultType}`);
    if (calls.n !== 0) add(`"${fixture.scenario}": an L2 run reached the transport before confirmation`);
    out = await run(deps, { input: { tool_id: toolId, input, idempotency_key: `listing:${fixture.scenario}:confirmed`, inputResponses: { confirm: true } }, bearer: new Secret(LISTING_KEY) });
  }
  if (fixture.expect?.error) {
    if (out.body.resultType !== "error") add(`"${fixture.scenario}": expected an upstream error through run, got ${out.body.resultType}`);
    else if (out.body.usage.cost_micro_usd !== 0) add(`"${fixture.scenario}": a failed run must cost 0`);
    return;
  }
  if (out.body.resultType !== "result") {
    add(`"${fixture.scenario}": expected a result through run, got ${out.body.resultType}${out.body.resultType === "error" ? ` (${out.body.error.code})` : ""}`);
    return;
  }
  if (fixture.expect?.output !== undefined && contentHash(out.body.result) !== contentHash(fixture.expect.output)) add(`"${fixture.scenario}": run's result differs from expect.output`);
}

function usageFollowsModel(endpoint: EndpointDefinition, u: Usage): string | null {
  const m = endpoint.usage;
  if (u.quantity === 0) return u.cost_micro_usd === 0 ? null : `an empty result settled at ${u.cost_micro_usd} µ$, must be 0`;
  switch (m.model) {
    case "per_call":
      return u.quantity === 1 && u.cost_micro_usd === m.cost_micro_usd ? null : `per_call should settle 1 × ${m.cost_micro_usd} µ$, settled ${u.quantity} × → ${u.cost_micro_usd} µ$`;
    case "per_result": {
      const want = (m.base_micro_usd ?? 0) + u.quantity * m.cost_micro_usd_per_result;
      return u.cost_micro_usd === want ? null : `per_result with ${u.quantity} results should settle ${want} µ$, settled ${u.cost_micro_usd} µ$`;
    }
    case "per_unit": {
      const want = Math.round(u.quantity * m.cost_micro_usd_per_unit);
      return u.cost_micro_usd === want ? null : `per_unit with ${u.quantity} ${m.unit} should settle ${want} µ$, settled ${u.cost_micro_usd} µ$`;
    }
  }
}

/** Runs the five checks over one provider definition. `dir` is only echoed into the report. */
export async function checkListing(provider: ProviderDefinition, dir: string): Promise<ListingReport> {
  const problems: ListingProblem[] = [];
  const push = (check: CheckName, severity: "error" | "warning", tool_id: string | null, message: string) => problems.push({ check, severity, tool_id, message });
  const toolId = (e: EndpointDefinition) => `${provider.id}.${e.id}`;

  // ---- schema
  if (provider.manifest === undefined) push("schema", "error", null, "provider.json is missing: add it next to provider.ts and import it as `manifest` (connectors/schema.md §9)");
  let catalog: CompiledCatalog | undefined;
  try {
    catalog = compileCatalog([provider]);
  } catch (e) {
    if (!(e instanceof CatalogCompileError)) throw e;
    for (const p of e.problems) push("schema", "error", null, p);
  }

  const endpoints: EndpointReport[] = provider.endpoints.map((e) => {
    const entry = catalog?.get(toolId(e));
    return {
      tool_id: toolId(e),
      class: e.scope?.class ?? null,
      level: e.scope?.class ? levelFor(e.scope.class) : null,
      // The provider's own typical price (what its provider.json declares), before the router's take.
      quote_micro_usd: entry?.provider_quote_micro_usd ?? null,
      fixtures: e.fixtures.length,
      fixtures_replayed: 0,
      description_chars: e.meta.description?.length ?? 0,
    };
  });

  // ---- fixtures
  for (const [i, e] of provider.endpoints.entries()) {
    const id = toolId(e);
    const rep = endpoints[i];
    if (e.fixtures.length === 0) {
      push("fixtures", "error", id, "no fixtures: record at least one successful exchange (prefix synthetic ones with synthetic-)");
      continue;
    }
    if (!e.fixtures.some((f) => !f.expect?.error)) push("fixtures", "error", id, "no successful fixture: at least one fixture must replay to a result");
    if (!e.fixtures.some((f) => f.expect?.error)) push("fixtures", "warning", id, "no failure fixture: record one vendor error so failure detection (and $0 settlement) is tested");
    for (const f of e.fixtures) {
      const r = replayFixture(e, f);
      if (!r.request_ok) {
        push("fixtures", "error", id, `"${f.scenario}": the input fails the schema or the request hook threw`);
        continue;
      }
      if (f.expect?.error) {
        if (!r.failed) push("fixtures", "error", id, `"${f.scenario}": expected a failure but the connector treats the response as a success (check output.isFailure)`);
      } else if (r.failed) {
        push("fixtures", "error", id, `"${f.scenario}": the connector treats this response as a failure`);
      } else {
        if (f.expect?.output !== undefined && contentHash(r.output) !== contentHash(f.expect.output)) push("fixtures", "error", id, `"${f.scenario}": mapped output differs from expect.output`);
        if (f.expect?.usage && r.usage && (r.usage.quantity !== f.expect.usage.quantity || r.usage.cost_micro_usd !== f.expect.usage.cost_micro_usd)) {
          push("fixtures", "error", id, `"${f.scenario}": settled ${r.usage.quantity} → ${r.usage.cost_micro_usd} µ$, expect.usage says ${f.expect.usage.quantity} → ${f.expect.usage.cost_micro_usd} µ$`);
        }
      }
      if (catalog && e.scope?.class) await engineReplay(catalog, id, levelFor(e.scope.class), f, (m) => push("fixtures", "error", id, m));
      if (rep) rep.fixtures_replayed++;
    }
  }

  // ---- scope
  for (const e of provider.endpoints) {
    const id = toolId(e);
    const cls = e.scope?.class;
    if (!cls) {
      push("scope", "error", id, "no scope.class: every endpoint, and every write in particular, must declare its class");
      continue;
    }
    const segs = segments(e.id);
    if (segs.some((s) => SEND_SEGMENTS.has(s)) && cls !== "external_send" && cls !== "payment") {
      push("scope", "error", id, `the id names a send (${segs.filter((s) => SEND_SEGMENTS.has(s)).join(", ")}); declare external_send (L3), not ${cls}`);
    }
    if (segs.some((s) => PAY_SEGMENTS.has(s)) && cls !== "payment" && cls !== "external_send") {
      push("scope", "error", id, `the id names a payment or provisioning (${segs.filter((s) => PAY_SEGMENTS.has(s)).join(", ")}); declare payment (L3), not ${cls}`);
    }
    if (cls === "read" && segs.some((s) => WRITE_SEGMENTS.has(s))) push("scope", "error", id, `the id names a write (${segs.filter((s) => WRITE_SEGMENTS.has(s)).join(", ")}) but the class is read; declare user_data_write, service_state_change or external_send`);
    if (cls === "read" && (e.request.method === "DELETE" || e.request.method === "PUT" || e.request.method === "PATCH")) push("scope", "error", id, `${e.request.method} is a write; a read must not use it`);
    const level = levelFor(cls);
    if ((cls === "external_send" || cls === "payment") && level !== "L3") push("scope", "error", id, `${cls} must be L3, the table maps it to ${level}`);
  }

  // ---- descriptions
  for (const e of provider.endpoints) {
    const id = toolId(e);
    const d = e.meta.description ?? "";
    const s = e.meta.summary ?? "";
    if (d.length > DESCRIPTION_MAX_CHARS) push("descriptions", "error", id, `description is ${d.length} characters; keep it ≤ ${DESCRIPTION_MAX_CHARS} (operational facts go in notes)`);
    if (s.length > SUMMARY_MAX_CHARS) push("descriptions", "error", id, `summary is ${s.length} characters; keep it ≤ ${SUMMARY_MAX_CHARS}, one line naming the task`);
    if (!/\bUse for:/.test(d)) push("descriptions", "error", id, 'description must name the tasks an agent is doing, as "Use for: …"');
    if (!e.meta.exclusions?.trim()) push("descriptions", "error", id, 'meta.exclusions is required: one line, "Not for …", naming what to use instead');
    else if (!/^not\b/i.test(e.meta.exclusions.trim())) push("descriptions", "warning", id, 'exclusions should start with "Not for …"');
    if (d.split(/(?<=[.!?])\s+/).some((x) => /^not\b/i.test(x.trim()))) push("descriptions", "warning", id, 'description contains a "Not …" sentence; move exclusions to meta.exclusions (they are stripped from ranking anyway)');
    const sup = findSuperlatives(`${s} ${d} ${e.meta.exclusions ?? ""}`);
    if (sup.length) push("descriptions", "error", id, `marketing superlatives are not agent-readable: ${sup.join(", ")}`);
    if (!/(\$\d|\bfree\b|\bmetered\b|\bL[123]\b)/i.test(d)) push("descriptions", "error", id, "description must state the cost class (free, $ per call/result, or metered)");
    const cls = e.scope?.class;
    if (cls && cls !== "read" && !d.includes(levelFor(cls))) push("descriptions", "warning", id, `a ${levelFor(cls)} endpoint should say ${levelFor(cls)} (and what it means) in its description`);
  }

  // ---- price
  const parsedManifest = ProviderManifestSchema.safeParse(provider.manifest);
  for (const e of provider.endpoints) {
    const id = toolId(e);
    if (parsedManifest.success) {
      const decl = parsedManifest.data.pricing.find((p) => p.endpoint === e.id);
      if (!decl) push("price", "error", id, "provider.json declares no price for this endpoint");
      else if (contentHash(decl as unknown as Json) !== contentHash(declarationFor(e.id, e.usage) as unknown as Json)) {
        push("price", "error", id, `declared ${JSON.stringify(decl)} but the endpoint's usage model is ${JSON.stringify(declarationFor(e.id, e.usage))}`);
      }
    }
    let quote: number;
    try {
      quote = quoteMicroUsd(publicUsage(e.usage));
    } catch {
      continue;
    }
    const observed: number[] = [];
    for (const f of e.fixtures) {
      if (f.expect?.error) continue;
      const r = replayFixture(e, f);
      if (r.failed || !r.usage) continue;
      const bad = usageFollowsModel(e, r.usage);
      if (bad) push("price", "error", id, `"${f.scenario}": ${bad}`);
      if (r.usage.cost_micro_usd > 0) observed.push(r.usage.cost_micro_usd);
    }
    if (quote > 0 && observed.length === 0) push("price", "warning", id, "the endpoint is priced but no fixture settles a non-zero cost; record a non-empty success so the price is observed");
    if (observed.length) {
      const max = Math.max(...observed);
      const min = Math.min(...observed);
      if (quote > 10 * max) push("price", "warning", id, `the quote (${quote} µ$) is over 10× the highest observed cost (${max} µ$); check typical_results / typical_units`);
      if (quote * 10 < min) push("price", "warning", id, `the quote (${quote} µ$) is under a tenth of the lowest observed cost (${min} µ$); budgeted agents will be surprised`);
    }
    if (quote > 1_000_000) push("price", "warning", id, `a typical run costs ${quote} µ$ (over $1); confirm this is intended`);
  }

  const checks = CHECKS.map((name) => {
    const mine = problems.filter((p) => p.check === name);
    const errors = mine.filter((p) => p.severity === "error").length;
    return { name, ok: errors === 0, errors, warnings: mine.length - errors };
  });
  return {
    format: LISTING_REPORT_FORMAT,
    provider_dir: dir,
    provider_id: provider.id,
    ok: checks.every((c) => c.ok),
    skipped: null,
    checks,
    problems,
    endpoints,
    catalog_hash: catalog?.manifest.hash ?? null,
  };
}

export function skippedReport(dir: string, reason: string): ListingReport {
  return { format: LISTING_REPORT_FORMAT, provider_dir: dir, provider_id: null, ok: true, skipped: reason, checks: [], problems: [], endpoints: [], catalog_hash: null };
}

export function failedReport(dir: string, message: string): ListingReport {
  return {
    format: LISTING_REPORT_FORMAT,
    provider_dir: dir,
    provider_id: null,
    ok: false,
    skipped: null,
    checks: CHECKS.map((name) => ({ name, ok: name !== "schema", errors: name === "schema" ? 1 : 0, warnings: 0 })),
    problems: [{ check: "schema", severity: "error", tool_id: null, message }],
    endpoints: [],
    catalog_hash: null,
  };
}

/** The job-summary rendering of one or more reports. */
export function renderMarkdown(reports: ListingReport[]): string {
  const lines = ["## Router listing check", "", "| provider | dir | result | schema | fixtures | scope | descriptions | price | endpoints |", "|---|---|---|---|---|---|---|---|---|"];
  for (const r of reports) {
    if (r.skipped) {
      lines.push(`| ${r.provider_id ?? "—"} | \`${r.provider_dir}\` | skipped | | | | | | |`);
      continue;
    }
    const cell = (n: CheckName) => {
      const c = r.checks.find((x) => x.name === n);
      if (!c) return "";
      return `${c.ok ? "pass" : `**${c.errors} error(s)**`}${c.warnings ? `, ${c.warnings} warning(s)` : ""}`;
    };
    lines.push(`| ${r.provider_id ?? "—"} | \`${r.provider_dir}\` | ${r.ok ? "pass" : "**FAIL**"} | ${cell("schema")} | ${cell("fixtures")} | ${cell("scope")} | ${cell("descriptions")} | ${cell("price")} | ${r.endpoints.length} |`);
  }
  for (const r of reports) {
    if (r.skipped) {
      lines.push("", `- \`${r.provider_dir}\` skipped: ${r.skipped}`);
      continue;
    }
    if (r.endpoints.length) {
      lines.push("", `### ${r.provider_id}`, "", "| tool_id | class | level | quote µ$ | fixtures replayed | description chars |", "|---|---|---|---|---|---|");
      for (const e of r.endpoints) lines.push(`| \`${e.tool_id}\` | ${e.class ?? "—"} | ${e.level ?? "—"} | ${e.quote_micro_usd ?? "—"} | ${e.fixtures_replayed}/${e.fixtures} | ${e.description_chars} |`);
    }
    if (r.problems.length) {
      lines.push("");
      for (const p of r.problems) lines.push(`- ${p.severity === "error" ? "**error**" : "warning"} [${p.check}]${p.tool_id ? ` \`${p.tool_id}\`` : ""}: ${p.message.replace(/\|/g, "\\|")}`);
    }
  }
  return `${lines.join("\n")}\n`;
}
