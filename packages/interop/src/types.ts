// Types of ShogunAgent export v1 and v2 (docs/api/export-v1.md, docs/api/export-v2.md). The JSON
// Schemas in docs/api/ are the contract; these types follow them field for field.

export const FORMAT_V1 = "shogunagent-export/v1";
export const FORMAT_V2 = "shogunagent-export/v2";
export const URN_PREFIX = "urn:shogunagent:";

export type Band = "high" | "medium" | "low";
export type EntityKind = "person" | "project";
export type FactKind = "commitment" | "open_loop";
export type FactSubtype =
  | "mine"
  | "theirs"
  | "reply_needed"
  | "waiting_on_them"
  | "review_pending"
  | "decision_pending"
  | "follow_up"
  | "other";
export type FactStatus = "open" | "done" | "overdue" | "cancelled" | "closed";
export type ProjectStatus = "active" | "waiting" | "paused" | "done";

export interface PointerV1 {
  event_id: number;
  quote: string | null;
}

export interface PointerV2 extends PointerV1 {
  event_uri: string;
  observed_at: number | null;
  source: string | null;
  /** `sha256:<hex>` of the source event's body, or null when unknown. */
  content_hash: string | null;
}

interface Shared {
  id: number;
  confidence: number;
  band: Band;
  valid_from: number;
  valid_to: number | null;
  updated_at: number;
  actor: string | null;
  namespace: string;
}

export interface EntityV1 extends Shared {
  type: "entity";
  kind: EntityKind;
  name: string;
  aliases: string[];
  summary: string | null;
  status: ProjectStatus | null;
  provenance: PointerV1[];
}

export interface FactV1 extends Shared {
  type: "fact";
  kind: FactKind;
  subtype: FactSubtype;
  text: string;
  status: FactStatus;
  due_at: number | null;
  counterparty_id: number | null;
  project_id: number | null;
  provenance: PointerV1[];
}

export interface TombstoneV1 {
  type: "tombstone";
  kind: string;
  id: number;
  deleted_at: number;
  reason: string;
}

export interface ManifestV1 {
  type: "manifest";
  format: typeof FORMAT_V1;
  schema_version: number;
  generated_at: number;
  since_cursor: number | null;
  cursor: number;
  counts: { entities: number; facts: number; tombstones: number };
}

export interface V2Additions {
  uri: string;
  recorded_at: number;
  superseded_by: string | null;
}

export interface EntityV2 extends Omit<EntityV1, "provenance">, V2Additions {
  provenance: PointerV2[];
}

export interface FactV2 extends Omit<FactV1, "provenance">, V2Additions {
  counterparty: string | null;
  project: string | null;
  provenance: PointerV2[];
}

export interface RelationV2 {
  type: "relation";
  uri: string;
  id: number;
  subject: string;
  predicate: string;
  object: string;
  confidence: number;
  band: Band;
  valid_from: number;
  valid_to: number | null;
  recorded_at: number;
  actor: string | null;
  namespace: string;
  provenance: PointerV2[];
}

export interface TombstoneV2 extends TombstoneV1 {
  uri: string;
}

export interface Capabilities {
  record_types: Array<"entity" | "fact" | "relation" | "tombstone">;
  entity_kinds?: string[];
  fact_kinds?: string[];
  global_ids: "urn";
  bitemporal: { valid_time: boolean; transaction_time: boolean };
  relations: boolean;
  supersession: boolean;
  provenance: { quotes: boolean; content_hash: "sha256" | null; observed_at?: boolean };
  confidence: Record<string, unknown>;
  namespaces?: string[];
  tombstones: boolean;
  differential: boolean;
  import_confidence_cap?: number;
  [extension: string]: unknown;
}

export interface ManifestV2 {
  type: "manifest";
  format: typeof FORMAT_V2;
  schema_version: number;
  generated_at: number;
  since_cursor: number | null;
  cursor: number;
  /** `urn:shogunagent:<ns>` */
  namespace: string;
  capabilities: Capabilities;
  counts: { entities: number; facts: number; relations: number; tombstones: number };
}

export type RecordV1 = EntityV1 | FactV1 | TombstoneV1;
export type RecordV2 = EntityV2 | FactV2 | RelationV2 | TombstoneV2;

export interface ExportV1 {
  manifest: ManifestV1;
  records: RecordV1[];
}

export interface ExportV2 {
  manifest: ManifestV2;
  records: RecordV2[];
}

/**
 * What a conversion could not carry. Counts only, never content. `dropped` names fields or
 * record types that had no place in the target; `approximated` names fields that were filled
 * with a stand-in (a default time, a band's floor) rather than the source value.
 */
export interface LossinessReport {
  from: string;
  to: string;
  records_in: number;
  records_out: number;
  dropped: Record<string, number>;
  approximated: Record<string, number>;
  /** Input items that have no home in the target model and were left out. */
  unmapped: number;
  notes: string[];
}
