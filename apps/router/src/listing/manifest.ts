/**
 * `provider.json` — the provider's listing manifest (E21; connectors/schema.md §9).
 *
 * What a provider declares to be listed: who they are (provider id, legal name, contact), where
 * revenue goes (an OPAQUE payout account reference — an id in the payout system, never bank
 * data), what each endpoint costs (one price declaration per endpoint, which must equal the
 * endpoint's usage model) and how request data is handled (retention, zero-retention flag,
 * regions). The catalog compiler validates it whenever a provider carries one; the listing
 * pipeline (src/listing/check.ts) requires it.
 */
import { z } from "zod";
import type { ProviderDefinition, UsageModel } from "../../connectors/types.js";

export const MANIFEST_FORMAT = "shogunagent-provider/1";

const PricingDecl = z.discriminatedUnion("model", [
  z.object({ endpoint: z.string().min(1), model: z.literal("per_call"), cost_micro_usd: z.number().int().min(0) }).strict(),
  z
    .object({
      endpoint: z.string().min(1),
      model: z.literal("per_result"),
      cost_micro_usd_per_result: z.number().int().min(0),
      base_micro_usd: z.number().int().min(0).optional(),
    })
    .strict(),
  z.object({ endpoint: z.string().min(1), model: z.literal("per_unit"), unit: z.string().min(1), cost_micro_usd_per_unit: z.number().min(0) }).strict(),
]);

export const ProviderManifestSchema = z
  .object({
    $schema: z.string().optional(),
    format: z.literal(MANIFEST_FORMAT),
    provider_id: z.string().min(1),
    legal_name: z.string().min(2).max(200),
    contact: z
      .object({
        email: z.string().email().optional(),
        url: z.string().url().optional(),
      })
      .strict()
      .refine((c) => c.email !== undefined || c.url !== undefined, { message: "contact needs an email or a url" }),
    /** An id in the payout system (e.g. a Stripe Connect account id). Never an account number, IBAN or routing number. */
    payout_account_ref: z.string().min(3).max(128).regex(/^[A-Za-z0-9_:.-]+$/, "use the opaque id from the payout system (letters, digits, _ : . -)"),
    pricing: z.array(PricingDecl).min(1),
    data_handling: z
      .object({
        /**
         * "none": request data is not stored; "transient": held in memory or cache for the call only;
         * "days": stored up to retention_days; "user_account": the call creates content in the user's
         * own account on the service (a draft, an issue), kept until the user deletes it.
         */
        retention: z.enum(["none", "transient", "days", "user_account"]),
        retention_days: z.number().int().min(0).max(3650),
        zero_retention: z.boolean(),
        /** Where request data is processed: ISO 3166 codes or "eu"/"us"/"global" ("global": not region-pinned). */
        regions: z.array(z.string().min(2).max(32)).min(1),
        /** One paragraph, plain English: what is stored, where, for how long, and who can see it. */
        statement: z.string().min(20).max(600),
      })
      .strict(),
  })
  .strict();

export type ProviderManifest = z.infer<typeof ProviderManifestSchema>;
export type PricingDeclaration = z.infer<typeof PricingDecl>;

/** Shapes that are bank data, not an opaque reference. */
const BANK_DATA_PATTERNS: { re: RegExp; what: string }[] = [
  { re: /^[A-Z]{2}\d{2}[A-Z0-9]{11,30}$/i, what: "an IBAN" },
  { re: /\d{8,}/, what: "a long digit run (account or routing number)" },
  { re: /(^|[^a-z])(iban|routing|swift|bic|sort.?code|account.?n(o|umber))([^a-z]|$)/i, what: "a bank field name" },
];

export function looksLikeBankData(ref: string): string | null {
  for (const p of BANK_DATA_PATTERNS) if (p.re.test(ref)) return p.what;
  return null;
}

/** The declaration a usage model implies, for comparison and for generating provider.json. */
export function declarationFor(endpointId: string, u: UsageModel): PricingDeclaration {
  switch (u.model) {
    case "per_call":
      return { endpoint: endpointId, model: "per_call", cost_micro_usd: u.cost_micro_usd };
    case "per_result":
      return { endpoint: endpointId, model: "per_result", cost_micro_usd_per_result: u.cost_micro_usd_per_result, ...(u.base_micro_usd !== undefined ? { base_micro_usd: u.base_micro_usd } : {}) };
    case "per_unit":
      return { endpoint: endpointId, model: "per_unit", unit: u.unit, cost_micro_usd_per_unit: u.cost_micro_usd_per_unit };
  }
}

function sameDeclaration(a: PricingDeclaration, b: PricingDeclaration): boolean {
  if (a.model !== b.model) return false;
  if (a.model === "per_call" && b.model === "per_call") return a.cost_micro_usd === b.cost_micro_usd;
  if (a.model === "per_result" && b.model === "per_result") return a.cost_micro_usd_per_result === b.cost_micro_usd_per_result && (a.base_micro_usd ?? 0) === (b.base_micro_usd ?? 0);
  if (a.model === "per_unit" && b.model === "per_unit") return a.unit === b.unit && a.cost_micro_usd_per_unit === b.cost_micro_usd_per_unit;
  return false;
}

/**
 * Validates a provider's manifest against the schema and against the provider definition.
 * Appends human-readable problems; returns the parsed manifest when the schema parse succeeded.
 */
export function validateManifest(provider: ProviderDefinition, problems: string[]): ProviderManifest | undefined {
  const where = `provider ${provider.id}: provider.json`;
  const parsed = ProviderManifestSchema.safeParse(provider.manifest);
  if (!parsed.success) {
    for (const i of parsed.error.issues) problems.push(`${where}: ${i.path.length ? i.path.join(".") : "$"}: ${i.message}`);
    return undefined;
  }
  const m = parsed.data;
  if (m.provider_id !== provider.id) problems.push(`${where}: provider_id "${m.provider_id}" does not match the provider id "${provider.id}"`);
  const bank = looksLikeBankData(m.payout_account_ref);
  if (bank) problems.push(`${where}: payout_account_ref looks like ${bank}; give the opaque id from the payout system, never bank data`);
  const dh = m.data_handling;
  if (dh.zero_retention && !(dh.retention === "none" || dh.retention === "transient")) problems.push(`${where}: zero_retention is true, so retention must be "none" or "transient" with retention_days 0`);
  if (dh.retention === "days" && dh.retention_days === 0) problems.push(`${where}: retention "days" needs retention_days > 0`);
  if (dh.retention !== "days" && dh.retention_days !== 0) problems.push(`${where}: retention_days must be 0 unless retention is "days"`);

  const endpoints = new Map(provider.endpoints.map((e) => [e.id, e]));
  const seen = new Set<string>();
  for (const decl of m.pricing) {
    if (seen.has(decl.endpoint)) problems.push(`${where}: pricing declares "${decl.endpoint}" twice`);
    seen.add(decl.endpoint);
    const ep = endpoints.get(decl.endpoint);
    if (!ep) {
      problems.push(`${where}: pricing declares "${decl.endpoint}", which is not an endpoint of this provider`);
      continue;
    }
    const actual = declarationFor(ep.id, ep.usage);
    if (!sameDeclaration(decl, actual)) problems.push(`${where}: pricing for "${decl.endpoint}" (${JSON.stringify(decl)}) differs from the endpoint's usage model (${JSON.stringify(actual)})`);
  }
  for (const id of endpoints.keys()) if (!seen.has(id)) problems.push(`${where}: no pricing declaration for endpoint "${id}"`);
  return m;
}

/** What the compiled catalog publishes per entry from the manifest. Contact and payout stay out. */
export interface PublicDataHandling {
  retention: "none" | "transient" | "days" | "user_account";
  retention_days: number;
  zero_retention: boolean;
  regions: string[];
  statement: string;
}

export function publicDataHandling(m: ProviderManifest | undefined): PublicDataHandling | null {
  if (!m) return null;
  const { retention, retention_days, zero_retention, regions, statement } = m.data_handling;
  return { retention, retention_days, zero_retention, regions: [...regions], statement };
}
