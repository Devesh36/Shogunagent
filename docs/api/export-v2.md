# Export format v2 (draft proposal)

> **Status.** This is ShogunAgent's export format v2 and a **draft proposal** for the W3C AI Agent
> Memory Interoperability Community Group. The Community Group has **not** adopted it, reviewed it
> or endorsed it; nothing here is a W3C document or a CG consensus. It is offered as one input to
> the CG's charter item "a portable memory-cell format", together with a reference implementation
> and round-trip tests, so the discussion can start from something that runs.

The portable form of one principal's world model — people, projects, commitments, open loops and
the typed relations between them — with the confidence, the validity interval, the record time,
the writer, the namespace and the evidence that proves each one, plus the rows that were deleted.
v2 keeps every field of [export v1](export-v1.md) with the same name, type and meaning, and adds
what an interchange format needs: stable global ids, explicit bi-temporal fields, relations,
supersession links, verifiable provenance and a capability manifest.

- JSON Schema of every line: [`export-v2.schema.json`](export-v2.schema.json)
  (`$id` `https://shogunagent.dev/schemas/export-v2.schema.json`).
- Implementation: `crates/shogun-memory/src/export/v2.rs` (export, import),
  `crates/shogun-memory/src/export/convert.rs` (v1 ↔ v2). Tests validate every exported line
  against the schema, check every content hash against the source body, and round-trip export →
  fresh store → import with identical row counts, quotes, relations, validity intervals and record
  times (`crates/shogun-memory/src/export/tests_v2.rs`).
- Converters to and from other shapes: [`@shogunagent/interop`](../../packages/interop/README.md)
  (TypeScript, no runtime dependencies).

## Getting one

| Face | Call |
|---|---|
| MCP | `state.export` with `{"format": "export-v2"}`, plus `"differential": true` or `"since_cursor": <ms>` |
| REST | `POST /v1/export` with `{"format": "export-v2", …}` (the body's `format` defaults to `export-v1`) |
| CLI | `shogun state export --format export-v2 [--differential \| --since-cursor <ms>]` |

The envelope is the export v1 envelope (`result.jsonl`, `result.cursor`, `result.since_cursor`,
`result.records`, `result.format` = `shogunagent-export/v2`, a free `usage`, `latency_ms`).
`format` is an additive value of the existing `state.export` argument; no tool or property was
renamed. Full, `since_cursor` and `differential` behave exactly as in v1; a relation counts as
changed after `t` when it was recorded after `t` or ended after `t`.

Importing: `memory.import` / `POST /v1/memory/import` / `shogun memory import` accept an export v2
in the `package` slot exactly like a v1 (see [Importing](#importing-one)).

## Normative part

The key words MUST, MUST NOT, SHOULD, SHOULD NOT and MAY are to be read as in RFC 2119 / RFC 8174
when, and only when, they appear in all capitals.

### Container

1. An export is UTF-8 JSON Lines: one JSON object per line, `\n`-terminated. The first non-empty
   line MUST be the manifest. Records follow in the order entities, facts, relations, tombstones.
   A consumer MUST NOT depend on the order within a group.
2. Every record MUST carry `type`. A consumer MUST ignore record types it does not know (and
   SHOULD count them), and MUST ignore object members it does not know. A producer MUST NOT
   change the meaning of a member defined here.
3. Times are integers, Unix milliseconds, UTC.

### Identity

4. Every entity, fact, relation and tombstone MUST carry `uri`, a global id of the form
   `urn:shogunagent:<ns>:<kind>:<local-id>`. `<ns>` is the producing store's namespace id
   (`[0-9a-z-]{1,64}`), scoped to one principal and stable for the life of the store; `<kind>` is
   `person`, `project`, `commitment`, `open_loop`, `relation` or `event`; `<local-id>` is an
   integer. A producer MUST emit the same `uri` for the same row in every export of the same store.
5. References between records (`counterparty`, `project`, `subject`, `object`, `superseded_by`,
   a pointer's `event_uri`) MUST be URIs. v1's integer reference fields (`counterparty_id`,
   `project_id`, `id`) are kept for v1 readers; a v2 consumer SHOULD use the URIs.
6. A URI names the row **in the producing store**. A consumer that restores an export into another
   store assigns its own ids; it SHOULD keep the mapping from the producer's URI to its own id for
   as long as it may receive later differential exports of the same producer.

### Time (bi-temporal)

7. `valid_from` / `valid_to` are **valid time**: when the record became true or current in the
   world as the producer understands it, and when it stopped (`null` while current). `valid_to`
   MUST NOT be earlier than `valid_from`.
8. `recorded_at` is **transaction time**: when the producing store recorded the row. It MUST be
   present on every entity, fact and relation. A producer that does not track valid time
   separately MUST set `valid_from` to its best estimate and MAY set it equal to `recorded_at`; it
   MUST declare `capabilities.bitemporal.transaction_time: false` when `recorded_at` is not a true
   record time (for example after a conversion from a format without one).
9. `updated_at` is the last change of the row; differential exports filter on it.
10. A fact that stops being current (done, cancelled, closed, superseded) MUST NOT be deleted from
    the export to say so: its `valid_to` is set (and, for supersession, `superseded_by`). Deletion
    is expressed only by a tombstone.

### Facts, entities, relations

11. An entity has `kind` `person` or `project`; a fact has `kind` `commitment` or `open_loop`
    with a `subtype` (`mine` / `theirs`, or an open-loop kind) and a `status`. The vocabularies
    are the closed sets of the schema; a producer with another vocabulary MUST map to the closest
    member and SHOULD keep its own value in an extension member.
12. A relation is `(subject, predicate, object)` between two record URIs, with its own validity
    interval, `recorded_at`, confidence and provenance. `predicate` is a non-empty string; this
    draft does not fix a predicate vocabulary (see open questions).
13. `superseded_by` names the record that replaces this one (a duplicate folded into another, a
    restated commitment). The superseded record MUST stay in the export with its own provenance.
    `superseded_by` MUST NOT form a cycle.

### Confidence

14. `confidence` is a number in `[0, 1]`. `band` is derived from it: `high` (≥ 0.8), `medium`
    (≥ 0.5), `low` (otherwise). A consumer MUST NOT present a `low` record as fact and SHOULD
    present a `medium` one as a possibility.
15. A consumer that restores an export SHOULD cap imported confidence below its own fact
    threshold until it has observed evidence itself (this implementation caps at 0.6 and declares
    it in `capabilities.import_confidence_cap`).

### Provenance

16. Every entity, fact and relation SHOULD carry at least one provenance pointer; this
    implementation always does. A pointer is
    `{event_id, event_uri, quote, observed_at, source, content_hash}`.
17. `quote`, when not `null`, MUST be an exact substring of the body of the event the pointer
    names. `observed_at` is that event's time; `source` its origin label (`gmail`, `slack`,
    `import`, …).
18. `content_hash`, when not `null`, MUST be `sha256:` followed by the lowercase hex SHA-256 of
    the event body's UTF-8 bytes. A receiver that holds the same source document (the email, the
    transcript) can verify a quote without the export carrying the body: hash the body, compare,
    then check that `quote` is a substring of it. Event bodies MUST NOT be required to read an
    export; this format exports the world model, not the log.

### Deletion

19. A tombstone `{kind, id, uri, deleted_at, reason}` says the row named by `uri` was deleted. It
    MUST NOT carry any of the deleted content. A consumer that holds a copy of that URI's row
    MUST retire it.

### Capabilities

20. The manifest MUST carry `capabilities`, which states what the producer models:
    `record_types`, `global_ids`, `bitemporal.{valid_time, transaction_time}`, `relations`,
    `supersession`, `provenance.{quotes, content_hash, observed_at}`, `confidence`, `tombstones`,
    `differential`. A consumer MUST distinguish "the producer does not model X" (capability false
    or null) from "the producer models X and there is none" (capability true, no records), and
    MUST NOT infer the second from the first. Unknown capability members MUST be ignored.

## Shape

### `manifest`

```json
{"type":"manifest","format":"shogunagent-export/v2","schema_version":102,"generated_at":1792000000000,
 "since_cursor":null,"cursor":1792000000000,"namespace":"urn:shogunagent:5f0c2a9e7d1b4c3a8e6f2d1c0b9a8e7d",
 "capabilities":{"record_types":["entity","fact","relation","tombstone"],"entity_kinds":["person","project"],
   "fact_kinds":["commitment","open_loop"],"global_ids":"urn","bitemporal":{"valid_time":true,"transaction_time":true},
   "relations":true,"supersession":true,"provenance":{"quotes":true,"content_hash":"sha256","observed_at":true},
   "confidence":{"scale":"0..1","bands":{"high":0.8,"medium":0.5}},"namespaces":["world","agent_scratch:*"],
   "tombstones":true,"differential":true,"import_confidence_cap":0.6},
 "counts":{"entities":2,"facts":3,"relations":1,"tombstones":1}}
```

### `fact`

```json
{"type":"fact","kind":"commitment","id":7,"uri":"urn:shogunagent:5f0c…:commitment:7","subtype":"theirs",
 "text":"send the rollout plan by Friday","status":"open","due_at":1792300000000,
 "counterparty_id":12,"project_id":null,"counterparty":"urn:shogunagent:5f0c…:person:12","project":null,
 "confidence":0.9,"band":"high","valid_from":1791990000000,"valid_to":null,"recorded_at":1791990000000,
 "updated_at":1791995000001,"superseded_by":null,"actor":"system:loop_consolidator","namespace":"world",
 "provenance":[{"event_id":481,"event_uri":"urn:shogunagent:5f0c…:event:481",
   "quote":"I'll send the rollout plan by Friday.","observed_at":1791989000000,"source":"gmail",
   "content_hash":"sha256:9b1c…"}]}
```

Entities add `uri`, `recorded_at` and `superseded_by` to the v1 entity. For an open loop,
`valid_from` is when it opened and `recorded_at` when the store recorded it; they can differ.

### `relation`

```json
{"type":"relation","uri":"urn:shogunagent:5f0c…:relation:3","id":3,
 "subject":"urn:shogunagent:5f0c…:person:12","predicate":"works_on","object":"urn:shogunagent:5f0c…:project:4",
 "confidence":0.9,"band":"high","valid_from":1791000000000,"valid_to":null,"recorded_at":1791000500000,
 "actor":"system:review","namespace":"world","provenance":[{"event_id":470,"event_uri":"…","quote":null,
 "observed_at":1790999000000,"source":"slack","content_hash":"sha256:…"}]}
```

### `tombstone`

```json
{"type":"tombstone","kind":"commitments","id":9,"uri":"urn:shogunagent:5f0c…:commitment:9",
 "deleted_at":1791999000000,"reason":"deleted_by:human:owner"}
```

## Importing one

Everything export v1 import does (new ids, one evidence event per pointer whose text is the
quote, confidence capped at 0.6, the importer as writer, tombstones counted and not applied),
plus:

- rows keep their `recorded_at` as the new row's record time, and evidence events keep each
  pointer's `observed_at`, so both time axes survive the move;
- relations are restored between the new rows (their evidence is an evidence event like any
  other); a relation whose subject or object is not in the export is skipped and counted;
- `superseded_by` becomes the store's own supersession link (`merged_into`) on the new row;
- the report adds `relations` and `superseded` to the v1 counts.

The import does not keep the producer's URIs on the new rows: this store has no column for a
foreign id, and adding one is a schema change (open question 4).

## Conversion between v1 and v2

`shogun_memory::export::convert` (and `@shogunagent/interop`'s `exportV1ToV2` / `exportV2ToV1`):

| Direction | What happens | Lossy |
|---|---|---|
| v1 → v2 | URIs are built from the given namespace id and the v1 ids; `recorded_at` = `valid_from`; `superseded_by` = `null`; pointers get `event_uri`, and `observed_at` / `source` / `content_hash` = `null`. The manifest declares `relations: false`, `supersession: false`, `transaction_time: false`, `content_hash: null`. | No content is lost; record time, hashes and relations are unknown and declared so. |
| v2 → v1 | Every v1 field passes through unchanged (tested: the result equals the store's own v1 export). | Relations, `superseded_by`, `recorded_at` and the pointer additions are dropped; the report counts each. |

## Mapping to other formats (non-normative)

No memory portability format has been adopted anywhere as of October 2026. A July 2026 hands-on
test of seven memory products found that all seven lock users in and that a real standard needs
both an archive format and a wire protocol. At least three interchange proposals appeared in H1
2026 (MIF, memorywire, Open Memory Protocol), OAMP is an MIT-licensed JSON Schema + REST contract,
and PLUR's Engram Specification (Apache-2.0) targets the data layer
(`research_notes/…/agent_infra_and_protocols.md`).

**How to read this table.** The rows for the five community proposals are written from their
**public descriptions** (announcement posts and summaries), not from a field-by-field reading of
their specifications, which this repository has not reviewed. They map *concepts*; a cell that
names a field says "the description names this concept", not "the spec has a field of this
name". They must be checked against each specification before the CG uses them. The last three
columns are shapes this repository defines precisely and converts (see `@shogunagent/interop`).

| export v2 | MIF | memorywire | OMP | OAMP | Engram (PLUR) | Graphiti / Zep (bi-temporal graph) | flat memories list (Mem0 / supermemory-style) | Anthropic `/memories` tree |
|---|---|---|---|---|---|---|---|---|
| `uri` (global id) | not described | not described | not described | record id (JSON Schema) | engram id | node / edge uuid | `id` (vendor-local) — **lossy**: not global | file path — **lossy**: path, not id |
| entity (`person` / `project`) | memory item | memory item | memory | memory record | engram | entity node | one memory string per entity — **lossy**: kind in `metadata` only | `people/<slug>.md`, `projects/<slug>.md` |
| fact (`commitment` / `open_loop`, `subtype`, `status`, `due_at`) | memory item | memory item | memory | memory record | engram | fact edge | one memory string — **lossy**: status / due only in `metadata` | a line in `commitments.md` / `open_loops.md` — **lossy**: status and due date as text |
| relation (`subject`, `predicate`, `object`) | not described | not described | not described | not described | not described | edge (`source`, `name`, `target`) | none — **lossy**: dropped | none — **lossy**: dropped (a link line at most) |
| `valid_from` / `valid_to` | not described | not described | not described | not described | not described | `valid_at` / `invalid_at` | `created_at` only — **lossy** | none — **lossy** |
| `recorded_at` | not described | not described | not described | not described | not described | `created_at` (and `expired_at`) | `created_at` | none — **lossy** |
| `superseded_by` | not described | not described | not described | not described | not described | invalidation of the old edge | none — **lossy** | none — **lossy** |
| `confidence` / `band` | not described | not described | not described | not described | not described | not described | `score` (retrieval score, a different quantity) — **lossy** | `possibly:` prefix for medium; low omitted — **lossy** |
| `actor`, `namespace` | not described | not described | not described | not described | not described | `group_id` (namespace-like) | `user_id` / `agent_id` (namespace-like) | directory (`scratch/<agent>`) |
| provenance (`quote`, `observed_at`, `content_hash`) | not described | not described | not described | not described | not described | episode reference (no quote, no hash) | none — **lossy** | quote as text after `—`; no hash — **lossy** |
| tombstone | not described | not described | not described | erase (REST verb) | not described | edge invalidation | none — **lossy** | file deletion (no record) — **lossy** |
| capability manifest | not described | not described | not described | not described | not described | none | none | none |

Two further formats sit next to this one rather than under it: Letta's Agent File (`.af`) exports
a whole agent (system prompt, memory blocks, tools, message history; archival passages not yet)
and is an agent format, not a world-model format; Anthropic's memory tool defines operations on a
client-side `/memories` directory and no schema at all, which is why the tree column above is a
file layout (the one ShogunAgent projects, `crates/shogun-core/src/memfs/`), not a vendor schema.

## Open questions for the Community Group

1. **Identifier scheme.** `urn:shogunagent:` is not a registered URN namespace (RFC 8141). Should
   the CG register one, use `urn:uuid:` per record, or use a `did:`-rooted principal namespace?
   Whatever is chosen, references must survive a move between stores.
2. **Predicate vocabulary.** v2 leaves `predicate` open. A small core set (`works_on`,
   `reports_to`, `owns`, `blocks`, `mentions`) with an extension mechanism would make relations
   comparable across producers; who maintains it?
3. **Commitment semantics.** Is "commitment / open loop" a CG-level concept or an extension of a
   generic memory cell? Most proposals have one undifferentiated memory item; this one argues that
   status, due date and resolution are what agents most need to share.
4. **Foreign ids after import.** Should a restored record carry the producer's URI (`same_as`), so
   later differential exports from the same producer can update it instead of duplicating it?
5. **Quote verification without disclosure.** `content_hash` lets a holder of the same source
   check a quote. Should the format also allow a salted or keyed hash, so a hash of a short
   message cannot be confirmed by guessing?
6. **Confidence calibration.** Bands (0.8 / 0.5) are this producer's; a receiver cannot know how a
   producer calibrated. Should the manifest carry a calibration statement or a reference
   benchmark (for example the open-loop tracking benchmark, `docs/benchmarks/open-loop-tracking.md`)?
7. **Erasure.** A tombstone carries no content, but a GDPR erasure must also reach copies a
   consumer already restored. Is "MUST retire" enough, or does the CG need an erasure receipt?
8. **Wire protocol.** This is an archive format. The portability test cited above argues a
   standard needs a wire protocol too; v2 defers to MCP (`state.export`, `memory.import`) and
   would map onto any CG protocol verb set.
