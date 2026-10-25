// @shogunagent/interop: ShogunAgent export v2 (docs/api/export-v2.md) and converters to and from
// other memory shapes, each with a lossiness report. No runtime dependencies, no I/O.
export * from "./types.js";
export {
  InteropError,
  band,
  detectFormat,
  exportV1ToV2,
  exportV2ToV1,
  namespaceOf,
  parseExportV1,
  parseExportV2,
  parseUri,
  serializeExport,
  uri,
  validNamespace,
} from "./export.js";
export {
  FLAT_FORMAT,
  META,
  flatToV2,
  v2ToFlat,
  type FlatMemory,
  type FlatMemoryList,
  type FlatScalar,
  type FromFlatOptions,
  type ToFlatOptions,
} from "./flat.js";
export { TREE_FORMAT, memoriesTreeToV2, slug, v2ToMemoriesTree, type FromTreeOptions, type MemoriesTree } from "./memories-tree.js";
