/**
 * The run log hook (E8-T7). One `ToolRunRecord` per `run`, handed to a pluggable sink; the state
 * layer wires it to `tool_runs` (V94) per principal, and Phase 2 personalises `discover` from it.
 *
 * The record has NO field for an input or a result (invariant 2). `task_hint` is the caller's
 * short task phrase, truncated — a ranking signal, not content — and is omitted unless the
 * caller sent one. test/zero-retention.test.ts asserts the exact key set.
 */
export type RunOutcome =
  | "ok"
  | "empty"
  | "invalid_input"
  | "denied"
  | "input_required"
  | "approval_required"
  | "authentication_required"
  | "payment_required"
  | "provider_error"
  | "transport_error"
  | "replayed";

export interface ToolRunRecord {
  ts: number;
  /** Opaque actor id (a hash of the key), or "anonymous". */
  actor: string;
  tool_id: string;
  task_hint: string | null;
  outcome: RunOutcome;
  cost_micro_usd: number;
  latency_ms: number;
  hint_followed: boolean;
}

export const TOOL_RUN_RECORD_KEYS = ["ts", "actor", "tool_id", "task_hint", "outcome", "cost_micro_usd", "latency_ms", "hint_followed"] as const;

export interface RunLogSink {
  record(record: ToolRunRecord): void | Promise<void>;
}

export class InMemoryRunLogSink implements RunLogSink {
  readonly records: ToolRunRecord[] = [];
  record(record: ToolRunRecord): void {
    this.records.push(record);
  }
}

export const noopRunLogSink: RunLogSink = { record() {} };

export const TASK_HINT_MAX = 120;

export function clampTaskHint(hint: unknown): string | null {
  if (typeof hint !== "string") return null;
  const t = hint.trim();
  return t ? t.slice(0, TASK_HINT_MAX) : null;
}
