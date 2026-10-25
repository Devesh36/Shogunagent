/** Hosted live AI configuration. No provider key or price is inherited from Batch. */
export const LIVE_LANES = ["chat", "agent", "draft", "scribe", "rewrite", "dictation_cleanup", "summary_live", "translate_live", "quick_translate"] as const;
export type LiveLane = typeof LIVE_LANES[number];
export type LiveProvider = "openai" | "groq";
export type LivePlan = "trial" | "standard" | "pro";
export interface LivePolicy {
  provider: LiveProvider;
  model: string;
  inputMicrosPerToken: number;
  outputMicrosPerToken: number;
  maxOutputTokens: number;
  effort?: "none" | "low";
}
export interface LiveConfig {
  openaiKey: string;
  groqKey: string;
  hmacKey: string;
  policies: Record<LiveLane, LivePolicy>;
  globalDailyMicros: number;
  caps: Record<LivePlan, { daily: number; monthly: number }>;
}
export function isLiveLane(value: string): value is LiveLane {
  return (LIVE_LANES as readonly string[]).includes(value);
}
const object = (value: unknown): value is Record<string, unknown> => typeof value === "object" && value !== null && !Array.isArray(value);
export function liveConfig(env: Record<string, unknown>): LiveConfig {
  const required = (name: string): string => {
    const value = env[name];
    if (typeof value !== "string" || !value.trim()) throw new Error(`missing ${name}`);
    return value;
  };
  const positive = (value: unknown): value is number => typeof value === "number" && Number.isFinite(value) && value > 0 && value <= 10000;
  const raw: unknown = JSON.parse(required("LIVE_LANE_CONFIG_JSON"));
  if (!object(raw)) throw new Error("invalid LIVE_LANE_CONFIG_JSON");
  const policies = {} as Record<LiveLane, LivePolicy>;
  for (const lane of LIVE_LANES) {
    const row = raw[lane];
    if (!object(row) || typeof row.model !== "string" || !/^[a-zA-Z0-9._/-]{1,128}$/.test(row.model)
        || !positive(row.inputMicrosPerToken) || !positive(row.outputMicrosPerToken)
        || !Number.isSafeInteger(row.maxOutputTokens) || Number(row.maxOutputTokens) < 1 || Number(row.maxOutputTokens) > 16384
        || (row.effort !== undefined && row.effort !== "low" && row.effort !== "none")) throw new Error("invalid LIVE_LANE_CONFIG_JSON");
    if (lane === "rewrite" && (row.model !== "gpt-6-luna" || row.effort !== "low")) throw new Error("invalid rewrite policy");
    policies[lane] = {
      provider: lane === "dictation_cleanup" ? "groq" : "openai", model: row.model,
      inputMicrosPerToken: row.inputMicrosPerToken, outputMicrosPerToken: row.outputMicrosPerToken,
      maxOutputTokens: Number(row.maxOutputTokens), ...(row.effort ? { effort: row.effort } : {}),
    };
  }
  const cap = Number(required("LIVE_GLOBAL_DAILY_CAP_USD"));
  if (!Number.isFinite(cap) || cap < 0 || cap > 1_000_000) throw new Error("invalid LIVE_GLOBAL_DAILY_CAP_USD");
  const hmacKey = required("LIVE_HMAC_KEY");
  if (hmacKey.length < 32) throw new Error("invalid live authentication configuration");
  return {
    policies, openaiKey: required("OPENAI_LIVE_API_KEY"), groqKey: required("GROQ_LIVE_API_KEY"), hmacKey,
    globalDailyMicros: Math.floor(cap * 1_000_000),
    // Owner-approved #1101 D9. The trial cannot spend more than seven daily budgets.
    caps: { trial: { daily: 1_000_000, monthly: 7_000_000 }, standard: { daily: 3_000_000, monthly: 40_000_000 }, pro: { daily: 6_000_000, monthly: 80_000_000 } },
  };
}
