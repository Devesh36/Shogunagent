# V23 rollback

V23 is additive and stores only rebuildable local vectors plus content digests. Older binaries
ignore both objects. For a physical rollback, stop the app after creating and verifying an
encrypted backup, then drop search_document_vec, search_document_embeddings, and the two
search_document_vectors triggers. Canonical memory and full-text search remain unchanged.
