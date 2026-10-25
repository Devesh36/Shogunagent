/**
 * Strict JSON contracts for Batch `model_class` (FR-HN-06 / #315).
 *
 * The device still sends a prompt that asks for this shape, and still runs the handwritten
 * parsers as a second layer (instruction-shaped fields, value-range checks). The relay attaches
 * `output_config.format` so Anthropic's decoder is the first layer. No `tools` array — Batch
 * stays structurally tool-free (invariant 5).
 *
 * Shapes match the device parsers:
 * - classify → `parse_batch_classification`
 * - summarize (`meeting_recap`) → none: sent without a forced shape, see `SCHEMA_BY_PURPOSE`
 * - brief (`morning_brief`, `evening_wrap`) → `brief_generation::parse_generated`: up to three
 *   `{text, source_id, evidence}` lines, each quote an exact span of the cited excerpt
 * - about_profile / daily_activity_summary → their grounded evidence parsers
 * - mail_triage → `mail_batch::parse_triage_batch` (a verdict plus the anchored actions)
 *
 * The decoder is bound by these schemas: a field the device prompt asks for but the schema does
 * not list is never emitted (`additionalProperties: false`), silently, after the batch is billed.
 * So a device contract change (a new field) ships here FIRST — the relay is deployed before or
 * with the device build that reads the field (`DEPLOY.md` §5). The anchor fields below are the
 * example: the device resolves actions by `line` / `anchor` / `anchor_len` (numbered lines,
 * PR #1022), so a shape that asked for `quote` would have every action dropped as unresolvable.
 */
import type { BatchPurpose } from "./types.js";

/** The anchor the device resolves back to a verbatim span (`BatchSession::resolve_anchor`): the
 * `[n]` number of the supporting line, the first 40 characters of that line, and how many
 * characters of it form the span. All three are asked for on every item, so all three are
 * required — the device still reads an item whose `line` is wrong (the anchor overrules it). */
const ANCHOR_PROPERTIES = {
  line: { type: "integer" },
  anchor: { type: "string" },
  anchor_len: { type: "integer" },
} as const;
const ANCHOR_REQUIRED = ["line", "anchor", "anchor_len"] as const;

const COMMITMENT = {
  type: "object",
  additionalProperties: false,
  required: ["direction", "description", ...ANCHOR_REQUIRED],
  properties: {
    direction: { type: "string", enum: ["mine", "theirs"] },
    description: { type: "string" },
    ...ANCHOR_PROPERTIES,
  },
} as const;

const OPEN_LOOP = {
  type: "object",
  additionalProperties: false,
  required: ["kind", "description", ...ANCHOR_REQUIRED],
  properties: {
    kind: {
      type: "string",
      enum: [
        "reply_needed",
        "waiting_on_them",
        "review_pending",
        "decision_pending",
        "follow_up",
        "other",
      ],
    },
    description: { type: "string" },
    ...ANCHOR_PROPERTIES,
  },
} as const;

/** An entity: `name` and `context` are exact spans of the anchored line; the device's
 * `WireEntity` is `deny_unknown_fields`, so nothing beyond these may appear (no `quote`, no
 * `confidence`). */
const ENTITY = {
  type: "object",
  additionalProperties: false,
  required: ["kind", "name", "context", ...ANCHOR_REQUIRED],
  properties: {
    kind: {
      type: "string",
      enum: [
        "person",
        "project",
        "topic",
        "tool",
        "organization",
        "employment",
        "education",
        "activity",
        "self",
      ],
    },
    name: { type: "string" },
    context: { type: "string" },
    // The device reads these (WireEntity); none is required, so an older prompt still validates.
    description: { type: "string" },
    person_class: { type: "string", enum: ["contact", "public_figure", "self", "not_a_person"] },
    project_class: { type: "string", enum: ["user_project", "media_or_public", "not_a_project"] },
    channel: { type: "string" },
    // Entity resolution: the id of the existing person/project this mention is, or null for a new one.
    existing_id: { type: ["integer", "null"] },
    // Other names this entity goes by in the text: first name, handle, nickname, misspelling.
    aliases: { type: "array", items: { type: "string" } },
    ...ANCHOR_PROPERTIES,
  },
} as const;

export const CLASSIFY_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["commitments", "open_loops", "entities"],
  properties: {
    commitments: { type: "array", items: COMMITMENT },
    open_loops: { type: "array", items: OPEN_LOOP },
    entities: { type: "array", items: ENTITY },
    // Names or handles the window shows are the user's own (device: `ParsedBatch::self_names`).
    // Optional, so an older prompt still validates.
    self_names: { type: "array", items: { type: "string" } },
  },
} as const;

/** The device parser (`brief_generation::parse_generated`) denies unknown fields and needs every
 * line grounded, so the old `[string]` + `charm_line` shape failed every brief after the V59
 * protocol (fixed 2026-10-17). */
const BRIEF_LINES = {
  type: "array",
  // Mirrors the device cap. The length limits live in the prompt: Anthropic's decoder does not
  // take `minLength` / `maxLength`.
  maxItems: 5,
  items: {
    type: "object",
    additionalProperties: false,
    // `evidence` may be "": a line can rest on its cited source alone (FR-MB-01).
    required: ["text", "source_id", "evidence"],
    properties: {
      text: { type: "string" },
      source_id: { type: "integer" },
      evidence: { type: "string" },
    },
  },
} as const;

/** Evening wrap: lines only. */
export const BRIEF_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["what_happened"],
  properties: { what_happened: BRIEF_LINES },
} as const;

/** Morning brief: the same lines plus the optional warm line the device reads as `charm_line`
 * (best-effort there: a missing, null or invalid one is dropped). Deploy AFTER the app build that
 * reads it: older builds deny unknown fields and would fail a brief that carries it. A newer build
 * against the old schema just never sees the line. */
export const MORNING_BRIEF_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["what_happened"],
  properties: {
    what_happened: BRIEF_LINES,
    charm_line: { type: ["string", "null"] },
  },
} as const;

const GROUNDED_EVIDENCE = {
  type: "object",
  additionalProperties: false,
  required: ["source_id", "quote"],
  properties: {
    source_id: { type: "integer" },
    quote: { type: "string" },
  },
} as const;

export const ABOUT_PROFILE_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["about"],
  properties: {
    about: {
      type: "array",
      items: {
        type: "object",
        additionalProperties: false,
        required: ["section", "text", "basis", "evidence"],
        properties: {
          // The identity profile section (device `about_profile::Section`). Deploy AFTER the app
          // build that reads it: older builds reject unknown line fields. A newer build still reads
          // lines without it, so the app can ship first.
          section: {
            type: "string",
            enum: ["role_focus", "tools_stack", "habits_patterns", "interests", "writing_style", "communication_style"],
          },
          text: { type: "string" },
          basis: { type: "string", enum: ["fact", "habit", "interest"] },
          evidence: { type: "array", items: GROUNDED_EVIDENCE },
        },
      },
    },
  },
} as const;

export const DAILY_ACTIVITY_SUMMARY_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["summary", "evidence"],
  properties: {
    summary: { type: "string" },
    evidence: { type: "array", items: GROUNDED_EVIDENCE },
  },
} as const;

/** Mail triage Stage B (#319): one verdict per thread plus the actions the thread states.
 * Mirrors the contract the device prompt states verbatim (`mail_batch::triage_instructions`)
 * and `parse_triage_batch` reads back: `label` is the catalogue key, `attention` the reply
 * signal, `reasons` the allowed codes; `commitments` / `open_loops` are the same anchored items
 * the consolidation lane returns (zero-base plan L0(b): while triage is on, a Gmail thread is
 * read here and nowhere else, so this is the only place a promise made in mail can come from —
 * without these two arrays the device would store no mail commitment, silently). No `entities`:
 * the mail prompt does not ask for them. It shares the `classify` cost class with consolidation
 * but not its shape — the consolidation schema here would make the decoder emit
 * `{commitments, open_loops, entities}` with no `label`, which the device drops as a malformed
 * verdict after the batch has already been billed. */
export const MAIL_TRIAGE_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["label", "attention", "confidence", "reasons", "commitments", "open_loops"],
  properties: {
    label: { type: "string" },
    attention: { type: "string", enum: ["needs_reply", "waiting_on_them", "fyi", "none"] },
    confidence: { type: "number" },
    reasons: { type: "array", items: { type: "string" } },
    commitments: { type: "array", items: COMMITMENT },
    open_loops: { type: "array", items: OPEN_LOOP },
  },
} as const;

/** `dictionary_terms`: which words a user fixed after a dictation are names or terms. `term` is
 * copied from the user's fix, `heard` from the inserted text ("" for a capitalisation-only fix);
 * the device keeps only exact spans. */
export const DICTIONARY_TERMS_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["terms"],
  properties: {
    terms: {
      type: "array",
      maxItems: 8,
      items: {
        type: "object",
        additionalProperties: false,
        required: ["heard", "term"],
        properties: {
          heard: { type: "string" },
          term: { type: "string" },
        },
      },
    },
  },
} as const;

/** `entity_dedupe`: which stored people (or projects) are the same one. Each group lists the ids
 * to fold together and the fullest proper-cased name for the survivor. */
export const ENTITY_DEDUPE_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["groups"],
  properties: {
    groups: {
      type: "array",
      items: {
        type: "object",
        additionalProperties: false,
        required: ["ids", "canonical_name"],
        properties: {
          ids: { type: "array", items: { type: "integer" } },
          canonical_name: { type: "string" },
        },
      },
    },
  },
} as const;

/** `loops_consolidate` (`/v1/review` only): the loop consolidator's short list. At most six
 * `open` items (the device takes the first six and stores only those at confidence >= 0.84 whose
 * quote is an exact span of the cited event), `resolved` items each with the quote that shows the
 * outcome, and `duplicates` to fold. Ids are the device's `C<n>` / `L<n>`; `id` is null for a new
 * item. */
export const LOOPS_CONSOLIDATE_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["open", "resolved", "duplicates"],
  properties: {
    open: {
      type: "array",
      maxItems: 6,
      items: {
        type: "object",
        additionalProperties: false,
        required: ["id", "kind", "horizon", "source_event_id", "source_quote", "confidence", "text"],
        properties: {
          id: { type: ["string", "null"] },
          kind: { type: "string", enum: ["commitment", "loop", "followup", "project"] },
          horizon: { type: "string", enum: ["same_day", "short", "project"] },
          source_event_id: { type: "integer" },
          source_quote: { type: "string" },
          confidence: { type: "number" },
          text: { type: "string" },
        },
      },
    },
    resolved: {
      type: "array",
      items: {
        type: "object",
        additionalProperties: false,
        required: ["id", "event_id", "quote"],
        properties: { id: { type: "string" }, event_id: { type: "integer" }, quote: { type: "string" } },
      },
    },
    duplicates: {
      type: "array",
      items: {
        type: "object",
        additionalProperties: false,
        required: ["id", "into_id"],
        properties: { id: { type: "string" }, into_id: { type: "string" } },
      },
    },
  },
} as const;

/** `entity_page` (`/v1/review` only): one person or project page rewritten from its current page
 * and new dated evidence (device: `shogun_memory::entity_pages::parse_draft`). Each claim names how
 * it is known and the event ids it rests on; `person_class` / `project_class` let a page reclassify
 * a person or a project. */
export const ENTITY_PAGE_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["summary", "claims"],
  properties: {
    summary: { type: "string" },
    person_class: { type: "string", enum: ["contact", "public_figure", "not_a_person"] },
    project_class: { type: "string", enum: ["user_project", "media_or_public", "not_a_project"] },
    claims: {
      type: "array",
      items: {
        type: "object",
        additionalProperties: false,
        required: ["text", "basis", "weight", "event_ids"],
        properties: {
          text: { type: "string" },
          basis: { type: "string", enum: ["user_action", "user_said", "on_screen", "inferred"] },
          weight: { type: "string", enum: ["core", "detail", "noise"] },
          event_ids: { type: "array", items: { type: "integer" } },
        },
      },
    },
  },
} as const;

/** `consolidation_window` (`/v1/review` only): one call for a whole hourly window. Each element is
 * one session's consolidation answer, keyed by the `Session:` id the device sent; the device
 * splits it back into per-session results for `parse_batch_classification`. */
export const CLASSIFY_WINDOW_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["sessions"],
  properties: {
    sessions: {
      type: "array",
      items: {
        type: "object",
        additionalProperties: false,
        required: ["id", "commitments", "open_loops", "entities"],
        properties: {
          id: { type: "string" },
          ...CLASSIFY_SCHEMA.properties,
        },
      },
    },
  },
} as const;

/** The response contract is a property of the purpose, not of the cost class: two purposes can
 * share a model class (mail_triage and consolidation are both `classify`) and still expect
 * different shapes. Every purpose in `MODEL_CLASS_BY_PURPOSE` must have an entry here; `null`
 * sends the purpose without a forced shape.
 *
 * `meeting_recap` is `null` on purpose. Measured on simulated meetings (Haiku 4.5, 9 runs per
 * variant), a strict summary/decisions/next_actions/sections shape carries no
 * evidence (it has no field for it), left 2 of 9 recaps unusable (a loop to max_tokens, or an
 * all-empty object) and 5 of 9 with looping or merged action items; the same prompt without a
 * forced shape was clean 9 of 9. Adding `evidence` to the shape made it worse (5 of 6 hit
 * max_tokens). The device parser (`parse_minutes`) already tolerates prose around the JSON and
 * rejects empty minutes. */
export const SCHEMA_BY_PURPOSE: Record<BatchPurpose, object | null> = {
  consolidation: CLASSIFY_SCHEMA,
  consolidation_window: CLASSIFY_WINDOW_SCHEMA,
  morning_brief: MORNING_BRIEF_SCHEMA,
  evening_wrap: BRIEF_SCHEMA,
  meeting_recap: null,
  mail_triage: MAIL_TRIAGE_SCHEMA,
  entity_dedupe: ENTITY_DEDUPE_SCHEMA,
  loops_consolidate: LOOPS_CONSOLIDATE_SCHEMA,
  entity_page: ENTITY_PAGE_SCHEMA,
  about_profile: ABOUT_PROFILE_SCHEMA,
  daily_activity_summary: DAILY_ACTIVITY_SUMMARY_SCHEMA,
  dictionary_terms: DICTIONARY_TERMS_SCHEMA,
};

/** Anthropic Messages `output_config.format` for one Batch purpose, or `undefined` for a purpose
 * sent without a forced shape. */
export function outputFormatFor(purpose: BatchPurpose): {
  type: "json_schema";
  schema: object;
} | undefined {
  const schema = SCHEMA_BY_PURPOSE[purpose];
  return schema === null ? undefined : { type: "json_schema", schema };
}
