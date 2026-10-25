# Portable World-Model Export (export v2) — Community Group draft

> **NOT SUBMITTED.** This is a working draft prepared by ShogunAgent for possible contribution to
> the W3C AI Agent Memory Interoperability Community Group. It has **not** been submitted to the
> Community Group, presented to it, reviewed by it or adopted by it. It is not a W3C document, not
> a Community Group Report and not a consensus of anyone. Submission waits for the owner's
> decision and for the licensing and IPR commitments in [§12](#12-ipr-and-licensing-placeholder),
> which are not made here.

| | |
|---|---|
| This version | draft, 2026-10-25 (repository `docs/standards/export-v2-cg-draft.md`) |
| Built from | [`docs/api/export-v2.md`](../api/export-v2.md), the format ShogunAgent ships |
| Schema | [`docs/api/export-v2.schema.json`](../api/export-v2.schema.json) (`$id` `https://shogunagent.dev/schemas/export-v2.schema.json`) |
| JSON-LD context | [`docs/api/export-v2.context.jsonld`](../api/export-v2.context.jsonld) |
| Test suite | [`packages/interop`](../../packages/interop/README.md) and `crates/shogun-memory/src/export/tests_v2.rs` (§10) |
| Editors | *to be named by the owner* |
| Feedback | *not open: the draft has not been submitted* |

## Abstract

This document specifies a line-oriented JSON format for moving one principal's **world model**
between agent memory systems: the people and projects a person works with, the commitments made
and the open loops left unresolved, the typed relations between them, and the rows that were
deleted. Each record carries a stable global identifier, a confidence and its band, a validity
interval and a record time (bi-temporal), the writer and namespace, and provenance pointers whose
quotes a receiver can verify against the source document by hash without the export carrying the
source. A manifest at the top declares what the producer models, so a receiver can tell "not
modelled" from "modelled, and there is none". The format is an archive format; it is meant to be
carried by whatever wire protocol the Community Group adopts.

## Status of this document

This section describes the status of this document at the time of writing. It is a draft
proposal by one implementer, offered as **one input** to the Community Group's charter item "a
portable memory-cell format" (charter adopted June 2026). It is published in the implementer's
repository only. The Community Group has not discussed it. Normative statements below describe
what the reference implementation does and what this draft proposes others do; they bind nobody
until a group adopts them. Where this draft and `docs/api/export-v2.md` disagree, the API document
describes the shipped behaviour and this draft is to be corrected.

## 1. Introduction

Agent memory products store memories in shapes that do not move: a July 2026 hands-on test of
seven memory products found every one of them locked users in, and at least five community
proposals for an interchange format (MIF, memorywire, Open Memory Protocol, OAMP, PLUR's Engram)
appeared in the first half of 2026, none adopted (`research_notes/…/agent_infra_and_protocols.md`).
Most proposals model one undifferentiated "memory item". This draft argues for a typed core —
entities, facts with status and due dates, relations — because what agents most need to hand each
other is not a pile of remembered sentences but *what is true now, since when, how sure, and on
what evidence*.

### 1.1 Design goals

1. **Typed state, not text.** Commitments and open loops have a status, a due date and a
   resolution; a receiver can act on them without re-reading prose.
2. **Evidence travels, content does not have to.** Every record points at the event that proves
   it, with an exact quote and a content hash; the source bodies stay with their owner.
3. **Time is explicit.** Valid time and transaction time are separate fields, so a receiver can
   ask "what was believed on date X" and "what was true on date X".
4. **Honest capabilities.** A producer that does not model something says so in the manifest.
5. **Forward compatible.** Unknown record types and members are ignored, never fatal.
6. **Erasure is representable.** Deletion is a content-free tombstone.

### 1.2 Non-goals

The event log (raw captured text), embeddings, retrieval scores, agent configuration (system
prompts, tools; see Letta's Agent File) and a wire protocol are out of scope.

## 2. Conformance

The key words MUST, MUST NOT, REQUIRED, SHALL, SHALL NOT, SHOULD, SHOULD NOT, RECOMMENDED, NOT
RECOMMENDED, MAY and OPTIONAL in this document are to be interpreted as described in BCP 14
(RFC 2119, RFC 8174) when, and only when, they appear in all capitals, as shown here.

This draft defines two conformance classes:

- A **conforming producer** emits exports in which every line validates against the schema
  (§9) and that satisfy every MUST in §3–§8 addressed to producers.
- A **conforming consumer** reads any export a conforming producer may emit, and satisfies every
  MUST in §3–§8 addressed to consumers, in particular the ignore rules (§3.2), the band rule
  (§6.2), the capability rule (§8.2) and the tombstone rule (§7.2).

Sections marked *non-normative*, examples and notes are not part of conformance.

## 3. Container

1. An export is UTF-8 JSON Lines: one JSON object per line, each line terminated by `\n`. The
   first non-empty line MUST be the manifest (§8). Records follow in the order entities, facts,
   relations, tombstones; a consumer MUST NOT depend on the order within a group.
2. Every record MUST carry `type`. A consumer MUST ignore record types it does not know (and
   SHOULD count them), and MUST ignore object members it does not know. A producer MUST NOT
   change the meaning of a member defined here.
3. Times are integers, Unix milliseconds, UTC.

## 4. Data model

### 4.1 Records

| `type` | What it is | Key members |
|---|---|---|
| `manifest` | One per export, first line | `format` = `shogunagent-export/v2`, `schema_version`, `generated_at`, `since_cursor`, `cursor`, `namespace`, `capabilities`, `counts` |
| `entity` | A person or a project | `kind` (`person` / `project`), `name`, `aliases`, `summary`, `status` |
| `fact` | A commitment or an open loop | `kind` (`commitment` / `open_loop`), `subtype`, `text`, `status`, `due_at`, `counterparty`, `project` |
| `relation` | A typed edge between two records | `subject`, `predicate`, `object` |
| `tombstone` | A deleted row | `kind`, `uri`, `deleted_at`, `reason` |

Entities, facts and relations additionally carry `uri`, `confidence`, `band`, `valid_from`,
`valid_to`, `recorded_at`, `actor`, `namespace` and `provenance`; entities and facts also
`updated_at` and `superseded_by`. The integer fields `id`, `counterparty_id` and `project_id` are
kept for readers of the earlier version (export v1) and are local to the producing store.

### 4.2 Identity

1. Every entity, fact, relation and tombstone MUST carry `uri`, a global identifier of the form
   `urn:shogunagent:<ns>:<kind>:<local-id>`, where `<ns>` is the producing store's namespace id
   (`[0-9a-z-]{1,64}`, scoped to one principal, stable for the life of the store), `<kind>` one
   of `person`, `project`, `commitment`, `open_loop`, `relation`, `event`, and `<local-id>` an
   integer. A producer MUST emit the same `uri` for the same row in every export of the same store.
2. References between records (`counterparty`, `project`, `subject`, `object`, `superseded_by`,
   a pointer's `event_uri`) MUST be URIs. A consumer SHOULD use the URIs, not the integer fields.
3. A URI names the row in the producing store. A consumer that restores an export assigns its own
   ids and SHOULD keep the mapping from the producer's URI to its own id for as long as it may
   receive later differential exports from the same producer.

The `urn:shogunagent:` namespace is not registered (open question Q1).

### 4.3 Time

1. `valid_from` / `valid_to` are **valid time**: when the record became true or current as the
   producer understands it, and when it stopped (`null` while current). `valid_to` MUST NOT be
   earlier than `valid_from`.
2. `recorded_at` is **transaction time**: when the producing store recorded the row. It MUST be
   present on every entity, fact and relation. A producer that does not track valid time
   separately MUST set `valid_from` to its best estimate and MAY set it equal to `recorded_at`; it
   MUST declare `capabilities.bitemporal.transaction_time: false` when `recorded_at` is not a true
   record time.
3. `updated_at` is the last change of the row; differential exports filter on it.
4. A fact that stops being current (done, cancelled, closed, superseded) MUST NOT be removed from
   an export to say so: its `valid_to` is set and, for supersession, `superseded_by`. Removal is
   expressed only by a tombstone.

### 4.4 Facts, entities, relations

1. The vocabularies of `kind`, `subtype` and `status` are the closed sets of the schema. A
   producer with another vocabulary MUST map to the closest member and SHOULD keep its own value
   in an extension member.
2. A relation is `(subject, predicate, object)` between two record URIs with its own validity
   interval, `recorded_at`, confidence and provenance. `predicate` is a non-empty string; this
   draft fixes no predicate vocabulary (Q2).
3. `superseded_by` names the record that replaces this one. The superseded record MUST stay in the
   export with its own provenance. `superseded_by` MUST NOT form a cycle.

## 5. Provenance

1. Every entity, fact and relation SHOULD carry at least one provenance pointer
   `{event_id, event_uri, quote, observed_at, source, content_hash}`.
2. `quote`, when not `null`, MUST be an exact substring of the body of the event the pointer
   names. `observed_at` is that event's time; `source` its origin label (`gmail`, `slack`,
   `import`, …).
3. `content_hash`, when not `null`, MUST be `sha256:` followed by the lowercase hex SHA-256 of the
   event body's UTF-8 bytes. A receiver holding the same source document can verify a quote by
   hashing the body, comparing, and checking that `quote` is a substring of it.
4. Event bodies MUST NOT be required to read an export.

## 6. Confidence

1. `confidence` is a number in `[0, 1]`. `band` is derived from it: `high` (≥ 0.8), `medium`
   (≥ 0.5), `low` (otherwise).
2. A consumer MUST NOT present a `low` record as fact and SHOULD present a `medium` one as a
   possibility.
3. A consumer that restores an export SHOULD cap imported confidence below its own fact threshold
   until it has observed evidence itself, and a producer that does so on import declares the cap
   in `capabilities.import_confidence_cap`.

## 7. Deletion

1. A tombstone `{type, kind, id, uri, deleted_at, reason}` says the row named by `uri` was deleted.
   It MUST NOT carry any of the deleted content.
2. A consumer that holds a copy of that URI's row MUST retire it (Q7).

## 8. Manifest and capabilities

1. The manifest MUST carry `capabilities`: `record_types`, `global_ids`,
   `bitemporal.{valid_time, transaction_time}`, `relations`, `supersession`,
   `provenance.{quotes, content_hash, observed_at}`, `confidence`, `tombstones`, `differential`.
2. A consumer MUST distinguish "the producer does not model X" (capability `false` or `null`)
   from "the producer models X and there is none" (capability `true`, no records), and MUST NOT
   infer the second from the first. Unknown capability members MUST be ignored.
3. `since_cursor` and `cursor` make an export differential: a producer that declares
   `differential: true` emits, for `since_cursor = t`, every record changed after `t` (a relation
   counts as changed when recorded or ended after `t`) and every tombstone after `t`.

## 9. Serializations

### 9.1 JSON Lines (normative)

Every line MUST validate against `export-v2.schema.json` (JSON Schema 2020-12). Examples of every
record type are in `docs/api/export-v2.md` §Shape.

### 9.2 JSON-LD (non-normative in this draft)

Each line is also a JSON-LD 1.1 node object under the context
`https://shogunagent.dev/schemas/export-v2.context.jsonld` (repository:
`docs/api/export-v2.context.jsonld`), which a consumer applies out of band (lines do not carry
`@context`, so plain-JSON consumers see no change). Under it:

- `uri` is the node's `@id` and `type` its `@type` (`entity`, `fact`, `relation`, `tombstone`,
  `manifest`, in the vocabulary `https://shogunagent.dev/ns/export/v2#`);
- `counterparty`, `project`, `subject`, `object`, `superseded_by` and a pointer's `event_uri` are
  IRI references, so the export is a graph whose edges are the references of §4.2;
- every other member expands into the same vocabulary; `capabilities` and `counts` are JSON
  literals (`@json`); `confidence` is an `xsd:double`; `provenance` pointers are blank nodes;
- `predicate` stays a literal string, not an IRI, until Q2 is settled.

The vocabulary IRI is not dereferenceable yet, and no RDF vocabulary document is published.
Whether the Community Group wants a JSON-LD form at all is part of Q8.

## 10. Test suite

The reference implementation's tests are the starting point for a CG test suite; none of them is
a CG artefact yet.

| What | Where | Checks |
|---|---|---|
| Schema validity | `packages/interop/test/interop.test.ts` (ajv, JSON Schema 2020-12) and `crates/shogun-memory/src/export/tests_v2.rs` | Every line of a fixture export and of every converted export validates against `export-v2.schema.json`. |
| Provenance verification | same | Every `content_hash` equals the SHA-256 of the fixture's source body; every `quote` is a substring of it. |
| Round trips | same | v2 → v1 → v2 exact for every v1 field; v2 → flat list → v2 exact for typed fields, the first quote and both time axes; export → fresh store → import with identical row counts, quotes, relations, validity intervals and record times. |
| Lossiness reports | `packages/interop` | Every converter reports what it dropped, approximated or could not map (counts only). |
| JSON-LD expansion | `packages/interop/test/jsonld.test.ts` | Every schema member expands to an absolute IRI or a keyword; a fixture export expands with no member lost, `@id` = `uri`, references as IRIs, `capabilities` as a JSON literal. The test carries a minimal expander for the context features used and guards that the context uses no other; running the same fixture through a full JSON-LD processor is still to do. |

A CG suite would add: a corpus of producer exports from more than one implementation; negative
cases for every MUST (cycles in `superseded_by`, `valid_to` < `valid_from`, a quote that is not a
substring, a tombstone with content); consumer tests for the ignore rules and the band rule; and
the full-processor JSON-LD check.

## 11. Mapping to other formats (non-normative)

**Caveat.** The columns for the five community proposals (MIF, memorywire, OMP, OAMP, Engram) are
written from their **public descriptions**, not from a field-by-field reading of their
specifications, which the authors of this draft have not reviewed. **Every cell in those columns
is unverified.** They map concepts: a cell naming a field means "the description names this
concept", not "the specification has a field of this name", and "not described" means only that
the description does not mention it. They MUST be checked against each specification, ideally by
its authors, before the Community Group relies on them. The last three columns are shapes the
reference implementation defines precisely and converts (`@shogunagent/interop`).

| export v2 | MIF † | memorywire † | OMP † | OAMP † | Engram (PLUR) † | Graphiti / Zep | flat memories list | `/memories` tree |
|---|---|---|---|---|---|---|---|---|
| `uri` | not described | not described | not described | record id | engram id | node / edge uuid | `id` (local) — lossy | file path — lossy |
| entity | memory item | memory item | memory | memory record | engram | entity node | one string — lossy | `people/<slug>.md`, `projects/<slug>.md` |
| fact (`status`, `due_at`) | memory item | memory item | memory | memory record | engram | fact edge | one string; status in metadata — lossy | a line in `commitments.md` / `open_loops.md` — lossy |
| relation | not described | not described | not described | not described | not described | edge | dropped | dropped |
| `valid_from` / `valid_to` | not described | not described | not described | not described | not described | `valid_at` / `invalid_at` | `created_at` only — lossy | none |
| `recorded_at` | not described | not described | not described | not described | not described | `created_at` / `expired_at` | `created_at` | none |
| `superseded_by` | not described | not described | not described | not described | not described | edge invalidation | none | none |
| `confidence` / `band` | not described | not described | not described | not described | not described | not described | `score` (a retrieval score) — lossy | `possibly:` prefix — lossy |
| provenance (quote, hash) | not described | not described | not described | not described | not described | episode reference | none | quote as text — lossy |
| tombstone | not described | not described | not described | erase (REST verb) | not described | edge invalidation | none | file deletion — lossy |
| capability manifest | not described | not described | not described | not described | not described | none | none | none |

† Unverified: from public descriptions only (see the caveat above).

The Graphiti / Zep column is from its public documentation of the bi-temporal graph and was not
tested against a running instance either. The flat list and tree columns are exact for the
reference implementation's converters; the flat list is modelled on memory APIs that store one
string per memory and is not claimed to be any vendor's wire format.

## 12. IPR and licensing (placeholder)

**To be completed by the owner before any submission. Nothing in this section is a commitment.**

- [ ] Contributor: which legal entity contributes, and whether it has signed the W3C Community
      Contributor License Agreement (CLA) for this Community Group.
- [ ] Specification licence: the W3C Community Final Specification Agreement (FSA) at final
      stage, and the licence of drafts in the meantime (for example W3C Software and Document
      License or CC BY 4.0).
- [ ] Patent commitments: whether any patent or application of the contributor reads on the
      format, and the licensing commitment offered.
- [ ] Code and test licence: the reference implementation and tests are in this repository under
      its licence (`packages/interop` declares Apache-2.0); confirm whether the test suite is
      contributed under the CG's software licence.
- [ ] Trademarks: whether "ShogunAgent" in the URN prefix and the format name is acceptable to the
      group, or is replaced by a neutral name (Q1).
- [ ] Third-party material: the mapping table names other projects; confirm the descriptions are
      fair and ask their authors to review the columns marked †.

## 13. Open questions

- **Q1. Identifier scheme.** `urn:shogunagent:` is not a registered URN namespace (RFC 8141).
  Register one, use `urn:uuid:` per record, or a `did:`-rooted principal namespace? References
  must survive a move between stores.
- **Q2. Predicate vocabulary.** A small core set (`works_on`, `reports_to`, `owns`, `blocks`,
  `mentions`) with an extension mechanism, or open strings? Who maintains it?
- **Q3. Commitment semantics.** Is "commitment / open loop" a CG-level concept or an extension of
  a generic memory cell?
- **Q4. Foreign ids after import.** Should a restored record carry the producer's URI
  (`same_as`), so later differential exports update it instead of duplicating it?
- **Q5. Quote verification without disclosure.** Should the format allow a salted or keyed hash,
  so the hash of a short message cannot be confirmed by guessing?
- **Q6. Confidence calibration.** Bands are the producer's; should the manifest carry a
  calibration statement or a reference benchmark?
- **Q7. Erasure.** Is "MUST retire" enough, or does the group need an erasure receipt that
  reaches copies a consumer already restored?
- **Q8. Wire protocol and serializations.** This is an archive format; it defers to MCP today
  (`state.export`, `memory.import`). How does it map onto the group's protocol verbs, and does
  the group want the JSON-LD form of §9.2 to be normative?

## 14. References

- RFC 2119, RFC 8174 — requirement key words. RFC 8141 — URNs. RFC 8259 — JSON.
- JSON Lines (jsonlines.org). JSON Schema 2020-12. JSON-LD 1.1 (W3C Recommendation).
- W3C AI Agent Memory Interoperability Community Group, charter (June 2026):
  https://www.w3.org/community/ai-agent-memory-interop/
- ShogunAgent export v1 and v2: `docs/api/export-v1.md`, `docs/api/export-v2.md`.
