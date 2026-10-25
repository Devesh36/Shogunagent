/**
 * The router's permission scope (CLAUDE.md invariant 3; E8-T5). A port of the rule in
 * `crates/shogun-mcp/src/scope.rs` — "an operation not in the table is denied; ExternalSend is
 * always L3" — extended with the router's two write classes:
 *
 *   - an operation not in the table is denied, never defaulted;
 *   - any external send or payment is L3;
 *   - writes touching user data (and reversible provider-state changes) are L2;
 *   - reads are L1.
 *
 * The table is the compiled catalog's scope rows (src/catalog.ts): every endpoint declares its
 * `OpClass` or does not compile, and a `tool_id` absent from the catalog is absent from the
 * table. test/scope.test.ts asserts the invariant over the whole compiled catalog, the way
 * `crates/shogun-mcp/tests/invariant4.rs` does over the Rust tables.
 */
import type { OpClass } from "../connectors/types.js";

export type Level = "L1" | "L2" | "L3";

export interface ScopeRow {
  tool_id: string;
  class: OpClass;
  level: Level;
}

export type ScopeTable = ReadonlyMap<string, ScopeRow>;

export type Authorization =
  | { kind: "allowed"; level: Level; class: OpClass }
  | { kind: "denied_unknown_op" };

/** The level an operation class is gated at. The only place the mapping is written. */
export function levelFor(cls: OpClass): Level {
  switch (cls) {
    case "read":
      return "L1";
    case "user_data_write":
    case "service_state_change":
      return "L2";
    case "external_send":
    case "payment":
      return "L3";
  }
}

export function isExternalSend(cls: OpClass): boolean {
  return cls === "external_send" || cls === "payment";
}

export function buildScopeTable(rows: Iterable<{ tool_id: string; class: OpClass }>): ScopeTable {
  const table = new Map<string, ScopeRow>();
  for (const r of rows) {
    if (table.has(r.tool_id)) throw new Error(`scope: duplicate tool_id ${r.tool_id}`);
    table.set(r.tool_id, { tool_id: r.tool_id, class: r.class, level: levelFor(r.class) });
  }
  return table;
}

/** The only entry point `run` uses. An unknown tool id is denied, never defaulted. */
export function authorize(table: ScopeTable, toolId: string): Authorization {
  const row = table.get(toolId);
  if (!row) return { kind: "denied_unknown_op" };
  return { kind: "allowed", level: row.level, class: row.class };
}
