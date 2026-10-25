/**
 * The connector definition format (connectors/schema.md). Monid-compatible in spirit: a provider
 * file carries metadata, auth and defaults; each endpoint carries its id, an agent-readable
 * description, categories, input/output schemas, a usage model, a health baseline and recorded
 * fixtures. Hooks mirror Monid's four (`input.toRequest`, `usage` consolidation,
 * `output.fromResponse`, auth injection) — except that auth injection is NOT a hook a connector
 * can write: it is performed by src/transport.ts from the environment map, so no definition and
 * no hook ever holds a credential (invariant 7).
 */
import type { Category } from "./categories.js";

/** Plain JSON. Inputs, outputs and fixtures are JSON, never class instances. */
export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
export type JsonObject = { [key: string]: Json };

/**
 * What an operation does, which fixes the permission level it may have (src/scope.ts). A port of
 * `shogun_mcp::scope::OpClass`: an endpoint without a class is not in the table and is denied.
 */
export type OpClass =
  /** Reads the provider's data. L1. */
  | "read"
  /** Writes that touch the user's own data (state rows, notes, drafts). L2. */
  | "user_data_write"
  /** A reversible change on the provider that delivers nothing to anyone (a label, a draft). L2. */
  | "service_state_change"
  /** Sends, posts, creates an event, provisions something visible to others. Always L3. */
  | "external_send"
  /** Spends money or commits to a charge beyond the call price. Always L3. */
  | "payment";

/** An input (or output) contract: the JSON Schema agents see plus the validator the engine runs. */
export interface InputSpec {
  /** JSON Schema draft 2020-12, as returned by `inspect`. */
  jsonSchema: JsonObject;
  validate(input: unknown): { ok: true; value: Json } | { ok: false; errors: string[] };
}

/**
 * Usage models (README §7 pricing; Monid's per-call / per-result / per-unit). Costs are
 * micro-USD integers. Failures and empty results settle at 0 whatever the model says.
 */
export type UsageModel =
  | { model: "per_call"; cost_micro_usd: number }
  | {
      model: "per_result";
      cost_micro_usd_per_result: number;
      base_micro_usd?: number;
      /** Typical result count, used only for the quote shown by `discover` / `inspect`. */
      typical_results: number;
      /** Counts results in the mapped output. Default: the length of the output's `items` array. */
      results?: (output: Json) => number;
    }
  | {
      model: "per_unit";
      /** Vendor-native unit name, e.g. "1k_chars", "image", "second". */
      unit: string;
      cost_micro_usd_per_unit: number;
      typical_units: number;
      /** Reads the units consumed from the raw response (runs before `output.fromResponse`). */
      units: (ctx: { input: Json; response: RunResponse }) => number;
    };

/** Declared health, used until the health store has enough observations. */
export interface HealthBaseline {
  p50_ms: number;
  p95_ms: number;
  /** 0..1 */
  success_rate: number;
}

export type HttpMethod = "GET" | "POST" | "PUT" | "PATCH" | "DELETE";

/** The request the engine hands the transport. Carries no credential; the transport adds it. */
export interface RunRequest {
  method: HttpMethod;
  path: string;
  query?: Record<string, string>;
  headers?: Record<string, string>;
  body?: Json;
  timeout_ms?: number;
}

/** What the transport gives back. Headers are deliberately not forwarded. */
export interface RunResponse {
  status: number;
  body: Json;
}

/** A recorded exchange replayed offline by the fixture test (`pnpm test`). */
export interface Fixture {
  scenario: string;
  input: Json;
  response: RunResponse;
  expect?: {
    output?: Json;
    usage?: { quantity: number; cost_micro_usd: number };
    /** The run is expected to be an upstream error (status >= 400 or a failed vendor status). */
    error?: boolean;
  };
}

export interface EndpointMeta {
  /** One line, the task this endpoint does. */
  summary: string;
  /** Full capability text written for ranking: name the tasks the agent is trying to do, state the cost class. Indexed by `discover`. */
  description: string;
  /**
   * What this endpoint is NOT for, one line. Shown by `inspect` and NOT indexed by `discover`, so
   * naming the excluded task does not attract it (an exclusion inside `description` would).
   */
  exclusions?: string;
  /** One operational fact per entry. */
  notes?: string[];
  /** Default: the provider's `categories`. The union must be non-empty. */
  categories?: Category[];
  examples?: { input: Json; output: Json }[];
  docsUrl?: string;
}

export interface EndpointDefinition {
  /** Local name; the catalog `tool_id` is `<provider.id>.<id>`. */
  id: string;
  meta: EndpointMeta;
  request: { method: HttpMethod; path: string; timeout_ms?: number };
  input: {
    schema: InputSpec;
    /** Default: GET → input as query string; otherwise input as JSON body. */
    toRequest?: (ctx: { input: Json; request: { method: HttpMethod; path: string } }) => RunRequest;
  };
  output: {
    schema?: InputSpec;
    /** Maps the raw response body to the result the agent receives. Default: identity. */
    fromResponse?: (ctx: { input: Json; response: RunResponse }) => Json;
    /**
     * Whether the vendor reports failure inside a 2xx body (Monid: `status: "failed"`). Default:
     * any status >= 400 is a failure.
     */
    isFailure?: (response: RunResponse) => boolean;
    /** Whether the mapped output is empty (settles at 0). Default: see src/usage.ts. */
    isEmpty?: (output: Json) => boolean;
  };
  usage: UsageModel;
  scope: { class: OpClass };
  /** Default: the provider's `health`. One of the two is required. */
  health?: HealthBaseline;
  fixtures: Fixture[];
}

/**
 * The connector-token slots each principal's token store keeps (on its sidecar, E50; the
 * `<source>-tokenset` names of `shogun_integrations::token_store::CredentialSlot`).
 */
export type ConnectorSlot = "gmail-tokenset" | "gcal-tokenset" | "github-tokenset" | "linear-tokenset" | "slack-tokenset";

/**
 * How the transport authenticates to the provider. Names an environment variable, never a
 * value. `caller_bearer` forwards the agent's own bearer (the first-party state face).
 * `caller_connector` (E48) acts as the CALLING principal's own account at the vendor: the
 * transport fetches that principal's access token for `slot` from apps/api at run time
 * (docs/api/connector-tokens.md). `credential_env` is read only in development, behind
 * `ROUTER_DEV_SHARED_CREDENTIALS=1`, and never in production.
 */
export type ProviderAuth =
  | { kind: "none" }
  | { kind: "bearer"; credential_env: string }
  | { kind: "header"; header: string; credential_env: string }
  | { kind: "query"; param: string; credential_env: string }
  | { kind: "caller_bearer" }
  | { kind: "caller_connector"; slot: ConnectorSlot; credential_env: string };

export interface ProviderDefinition {
  id: string;
  name: string;
  first_party: boolean;
  meta: { description: string; docsUrl?: string; homepage?: string };
  request: {
    baseUrl: string;
    /** Environment variable that overrides `baseUrl` (configurable base URL for the first-party face). */
    baseUrlEnv?: string;
  };
  auth: ProviderAuth;
  /**
   * The provider's listing manifest (`provider.json`, E21): legal name, contact, opaque payout
   * reference, per-endpoint price declarations and the data-handling statement. Imported as JSON
   * and validated by the catalog compiler (src/listing/manifest.ts); required by the listing
   * pipeline (`listing:check`) for every listed provider. Passthrough catalogs (Monid) carry none.
   */
  manifest?: unknown;
  /** Defaults inherited by endpoints that omit them. */
  categories?: Category[];
  health?: HealthBaseline;
  endpoints: EndpointDefinition[];
}
