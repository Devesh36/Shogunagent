// A small export v2 in the shape crates/shogun-memory/src/export/v2.rs writes: two people, a
// project, an open commitment (high), a duplicate folded into it (superseded), a medium open
// loop, a low candidate, a closed loop, a relation and a tombstone. Content hashes are real
// SHA-256 of the event bodies below.
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import type { ExportV2, PointerV2 } from "../src/index.js";

export const NS = "5f0c2a9e7d1b4c3a";
const u = (kind: string, id: number) => `urn:shogunagent:${NS}:${kind}:${id}`;
const sha = (body: string) => `sha256:${createHash("sha256").update(body, "utf8").digest("hex")}`;

export const EVENTS: Record<number, string> = {
  481: "Oren Vale: I'll send the rollout plan by Friday.",
  482: "Still waiting on Tamsin for the cost estimate.",
  483: "Oren Vale: the rollout plan will come Friday, as promised.",
  484: "maybe someone should look at the dashboards",
  485: "Tamsin sent the vendor list, thanks.",
};

function p(event: number, quote: string | null, observed: number, source = "gmail"): PointerV2 {
  return { event_id: event, event_uri: u("event", event), quote, observed_at: observed, source, content_hash: sha(EVENTS[event] ?? "") };
}

const base = { actor: "system:loop_consolidator", namespace: "world" } as const;

export function fixture(): ExportV2 {
  return {
    manifest: {
      type: "manifest",
      format: "shogunagent-export/v2",
      schema_version: 102,
      generated_at: 10_000,
      since_cursor: null,
      cursor: 10_000,
      namespace: `urn:shogunagent:${NS}`,
      capabilities: {
        record_types: ["entity", "fact", "relation", "tombstone"],
        entity_kinds: ["person", "project"],
        fact_kinds: ["commitment", "open_loop"],
        global_ids: "urn",
        bitemporal: { valid_time: true, transaction_time: true },
        relations: true,
        supersession: true,
        provenance: { quotes: true, content_hash: "sha256", observed_at: true },
        confidence: { scale: "0..1", bands: { high: 0.8, medium: 0.5 } },
        namespaces: ["world", "agent_scratch:*"],
        tombstones: true,
        differential: true,
        import_confidence_cap: 0.6,
      },
      counts: { entities: 3, facts: 5, relations: 1, tombstones: 1 },
    },
    records: [
      { type: "entity", kind: "person", id: 1, uri: u("person", 1), name: "Oren Vale", aliases: ["Oren"], summary: "Leads the Heron rollout.", status: null, confidence: 0.9, band: "high", valid_from: 1_000, valid_to: null, recorded_at: 1_000, updated_at: 1_000, superseded_by: null, ...base, provenance: [p(481, null, 900)] },
      { type: "entity", kind: "person", id: 2, uri: u("person", 2), name: "Tamsin Ekberg", aliases: [], summary: null, status: null, confidence: 0.7, band: "medium", valid_from: 2_000, valid_to: null, recorded_at: 2_000, updated_at: 2_000, superseded_by: null, ...base, provenance: [p(482, "Still waiting on Tamsin", 1_900, "slack")] },
      { type: "entity", kind: "project", id: 1, uri: u("project", 1), name: "Heron Rollout", aliases: [], summary: "Q3 rollout.", status: "active", confidence: 0.85, band: "high", valid_from: 1_000, valid_to: null, recorded_at: 1_000, updated_at: 1_000, superseded_by: null, ...base, provenance: [p(481, null, 900)] },
      { type: "fact", kind: "commitment", id: 7, uri: u("commitment", 7), subtype: "theirs", text: "send the rollout plan by Friday", status: "open", due_at: 1_792_281_600_000, counterparty_id: 1, project_id: 1, counterparty: u("person", 1), project: u("project", 1), confidence: 0.9, band: "high", valid_from: 1_000, valid_to: null, recorded_at: 1_000, updated_at: 1_001, superseded_by: null, ...base, provenance: [p(481, "I'll send the rollout plan by Friday.", 900), p(483, "the rollout plan will come Friday", 2_900)] },
      { type: "fact", kind: "commitment", id: 8, uri: u("commitment", 8), subtype: "theirs", text: "the rollout plan will come Friday", status: "open", due_at: 1_792_281_600_000, counterparty_id: 1, project_id: null, counterparty: u("person", 1), project: null, confidence: 0.7, band: "medium", valid_from: 3_000, valid_to: null, recorded_at: 3_000, updated_at: 3_001, superseded_by: u("commitment", 7), ...base, provenance: [p(483, "the rollout plan will come Friday", 2_900)] },
      { type: "fact", kind: "open_loop", id: 3, uri: u("open_loop", 3), subtype: "waiting_on_them", text: "cost estimate from Tamsin", status: "open", due_at: null, counterparty_id: 2, project_id: null, counterparty: u("person", 2), project: null, confidence: 0.6, band: "medium", valid_from: 1_500, valid_to: null, recorded_at: 2_000, updated_at: 2_000, superseded_by: null, ...base, provenance: [p(482, "Still waiting on Tamsin for the cost estimate.", 1_900, "slack")] },
      { type: "fact", kind: "open_loop", id: 4, uri: u("open_loop", 4), subtype: "other", text: "maybe someone should look at the dashboards", status: "open", due_at: null, counterparty_id: null, project_id: null, counterparty: null, project: null, confidence: 0.35, band: "low", valid_from: 4_000, valid_to: null, recorded_at: 4_000, updated_at: 4_000, superseded_by: null, ...base, provenance: [p(484, "maybe someone should look at the dashboards", 3_900)] },
      { type: "fact", kind: "open_loop", id: 5, uri: u("open_loop", 5), subtype: "waiting_on_them", text: "vendor list from Tamsin", status: "closed", due_at: null, counterparty_id: 2, project_id: null, counterparty: u("person", 2), project: null, confidence: 0.85, band: "high", valid_from: 2_500, valid_to: 5_000, recorded_at: 2_500, updated_at: 5_000, superseded_by: null, ...base, provenance: [p(485, "Tamsin sent the vendor list", 4_900)] },
      { type: "relation", uri: u("relation", 1), id: 1, subject: u("person", 1), predicate: "works_on", object: u("project", 1), confidence: 0.9, band: "high", valid_from: 1_000, valid_to: 6_000, recorded_at: 1_200, actor: "agent:exporter", namespace: "world", provenance: [p(483, null, 2_900)] },
      { type: "tombstone", kind: "commitments", id: 9, uri: u("commitment", 9), deleted_at: 9_000, reason: "deleted_by:human:owner" },
    ],
  };
}

const root = join(dirname(fileURLToPath(import.meta.url)), "../../..");

/** A published schema from docs/api. */
export function schema(name: string): Record<string, unknown> {
  return JSON.parse(readFileSync(join(root, "docs/api", name), "utf8")) as Record<string, unknown>;
}
