# Local embedding models (E24, E43)

The server's vector lane runs a local embedding model through ONNX Runtime. Nothing about it
leaves the machine: there is no cloud embedding API anywhere in the tree, and no body is ever sent
to embed it. A build without the `embed-local` feature, or a server whose model is not installed,
searches lexically (FTS5 trigram + the V81 gram index) and says so in one stderr line at start.

**Nothing downloads at build or run time.** `shogun models fetch` (a CLI built with
`--features embed-local`) is the only command anywhere that downloads a model. It is an explicit
operator step: it pulls the files of a pinned `ModelSpec` from the model's repository at a pinned
commit, through the traced egress in `shogun-core`, checks each SHA-256, and moves them into place.
A process started with `embed-local` only *looks* for the files.

The ShogunAI developer notes this replaces are kept at
`docs/shogunai-origin/embedding-model-setup.md` for reference.

## The models

| name (`SHOGUN_EMBED_MODEL`) | space id | licence | languages | dims | pooling | prefixes (query / passage) | cutoff (L2) | pinned |
|---|---|---|---|---|---|---|---:|---|
| `nomic-embed-text-v1.5` (default) | `nomic-embed-text-v1.5@384` | Apache-2.0 | English-first | 768 → 384 Matryoshka prefix (layer norm, truncate, L2) | mean | `search_query: ` / `search_document: ` | 0.70 | yes |
| `multilingual-e5-small` (E43) | `multilingual-e5-small@384` | MIT | ~100, incl. ja, zh, es, de, fr, en | 384 native | mean, L2 | `query: ` / `passage: ` | 0.63 | **no** (see below) |

Both store 384-wide vectors in the existing `float[384]` tables (`event_vec`, V2;
`search_document_vec`, V23), so neither needs a migration. The full reasoning for each sits on its
`ModelSpec` in `crates/shogun-memory/src/embed.rs`.

### Why `multilingual-e5-small` for multilingual memory

E43 needed a model that retrieves across languages and runs under the existing ONNX path (an
encoder, mean pooling over the attention mask, a `tokenizer.json`), with a permissive licence:

- **multilingual-e5-small** (chosen): MIT; 118M parameters (XLM-RoBERTa, 12 layers); the fp32
  export in the model's own repository (`onnx/model.onnx`) is about 470 MB and is the export
  ShogunAI already ran through this embedder on device (2026-07-25: hybrid recall@5 1.00 against
  lexical 0.93 on `crates/shogun-memory/tests/retrieval_eval.rs`); 384 native dims; mean pooling;
  `query: ` / `passage: ` prefixes, without which it degrades silently; 512 tokens.
- **Qwen3-Embedding-0.6B** (the plan's other candidate, §9): Apache-2.0 and stronger on
  multilingual MTEB, but a decoder that needs last-token pooling and an instruction template on the
  query, published as ONNX only through community conversions with extra inputs (`position_ids`,
  past key values), and about 1.2 GB at fp16. It needs a new pooling path and a heavier runtime
  budget; it stays a candidate for a later spec, never mixed into an existing space.
- **multilingual-e5-base**: MIT, better than small, but 768 dims that are not a Matryoshka space:
  a 384 prefix is not a valid embedding, and a 768 table would need a migration.
- **nomic-embed-text-v1.5** stays the default: pinned, English-first, Matryoshka.

The default does not change in E43. A principal whose memory is mostly not English opts in with
`SHOGUN_EMBED_MODEL=multilingual-e5-small`; making it the default waits for the owner's
calibration run (below).

## Fetch, verify, list

```bash
cargo build --release -p shogun-cli --features embed-local
shogun models list                     # * marks the default; state per model
shogun models fetch                    # the default model
shogun models fetch --model nomic-embed-text-v1.5
shogun models verify --model nomic-embed-text-v1.5
shogun models path                     # SHOGUN_MODELS_DIR, else $XDG_DATA_HOME/shogun/models
```

Layout: `<models dir>/<model name>/{model.onnx, tokenizer.json}`. The ONNX Runtime library
(1.22.x, `ort` is built with `load-dynamic`) is found through `ORT_DYLIB_PATH` or the usual system
directories; without it the model does not load and search stays lexical.

`models list` states per model: `installed` / `not installed` (pinned), `unpinned` /
`hand-placed` (placeholder pins), `manual` (no files listed).

## Pinning `multilingual-e5-small` (one-time, operator)

The commit and SHA-256s of `intfloat/multilingual-e5-small` could not be read when E43 was written
(no network to model hubs), so its `ModelSpec` carries placeholders (`UNPINNED`). Until they are
replaced:

- `shogun models fetch --model multilingual-e5-small` **refuses** before creating a directory or
  sending a request (`not pinned in this build …`), because a placeholder would download whatever
  the repository serves today;
- `shogun models verify --model multilingual-e5-small` refuses: there is nothing to verify against;
- files placed by hand still load (`installed` only checks presence), so a developer can evaluate
  the model before pinning it.

To pin it, on a machine with access to Hugging Face:

```bash
# 1. The commit to pin: the current head of the repository's main branch.
git ls-remote https://huggingface.co/intfloat/multilingual-e5-small refs/heads/main
# 2. The two files at exactly that commit, and their hashes.
REV=<40-hex commit from step 1>
curl -fL -o model.onnx     "https://huggingface.co/intfloat/multilingual-e5-small/resolve/$REV/onnx/model.onnx"
curl -fL -o tokenizer.json "https://huggingface.co/intfloat/multilingual-e5-small/resolve/$REV/tokenizer.json"
sha256sum model.onnx tokenizer.json
```

3. In `crates/shogun-memory/src/embed.rs`, `MULTILINGUAL_E5_SMALL`: set `revision` to the commit
   and each file's `sha256` to its hash (lowercase hex). `ModelSpec::is_pinned` then holds, and
   the test `the_multilingual_model_is_a_complete_spec_that_waits_for_its_pins` must be updated to
   expect it pinned (it asserts the placeholders on purpose, so the pin is a reviewed change).
4. Rebuild; `shogun models fetch --model multilingual-e5-small` now downloads and checks both
   files, and `verify` checks them.

## Switching models: one space per store, re-embedded, never mixed

Every stored vector records the model that wrote it (`vector_models`, V102) and a store has one
active space (`embedding_space`). Switching a server from one model to another **re-embeds**; it
never refuses and never mixes:

1. Until the new model's embed job first runs, the store's space is still the old model's. The new
   model's query does not match it, so `Db::query_embedding` returns nothing and the vector lane
   sits out (lexical search only). Old vectors are never compared with a new query.
2. The job's first batch makes the new model the active space (`embedding_space::activate`). From
   then on every vector query reads only the new model's rows (`ACTIVE_EVENT_ROWS` and the
   document and Cold counterparts), so mid-switch the vector lane sees the re-embedded subset.
3. Rows of the old model read as pending and are re-embedded in place, oldest first; nothing is
   deleted. At the end every row is in the new space.
4. The vector lane's relevance cutoff is read from the active space's `ModelSpec` on every search
   (`embed::max_l2_distance_for`), so the new model's cutoff applies from the moment it is active.

Tests: `embed_job::switching_to_the_multilingual_model_re_embeds_and_never_mixes`,
`embed_job::a_model_switch_re_embeds_instead_of_mixing`,
`search::federated_tests::the_multilingual_models_cutoff_applies_when_its_space_is_active`,
`vector::knn_never_mixes_models`.

## The cutoff and its calibration

`ModelSpec::max_l2_distance` is the largest L2 distance between a query vector and a stored unit
vector that still counts as a semantic match (`d = sqrt(2 − 2·cos)`). Neither real model's cutoff
was measured here, because neither model can be fetched in this environment; both are set
conservatively, favouring abstention. e5's 0.63 (cosine ≥ 0.80) is the strictest: e5's low training
temperature packs even unrelated text into cosine 0.7–1.0, so the 1.0 it carried before E43
(cosine ≥ 0.5) let nearly every row through.

How to calibrate on a machine with the model is in `docs/benchmarks/README.md`, "Multilingual
retrieval (E43)" (e5) and "The per-model relevance cutoff" (nomic).
