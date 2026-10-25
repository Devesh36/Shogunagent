# V65 dictionary auto-add ownership rollback

Back up the encrypted database first. Verify the applied V65 name and checksum in `refinery_schema_history` against this build, and verify no higher migration is applied. If either check fails, stop and use the matching build.

Disable auto-add. In one transaction, drop `dictionary_autoadd_suppression`, `dictionary_autoadd_secret`, and `dictionary_autoadd_artifacts`, then remove only the verified V65 history row. This loses durable ownership and suppression; learned vocabulary remains. Do not run an older binary until its schema history matches its bundled migrations. To roll back V64 too, follow `V64-rollback.md` afterward.

Open PR #751 also proposes a different V64. Decide migration order before either PR merges; this rollback does not resolve that collision.
