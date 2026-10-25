# V91 rollback — live gateway route

V91 preserves all traceability and usage rows (including `kind`, `url`, IDs and indexes) and adds `live_relay` to their route CHECK constraints. No content column is added. The table rebuild is required because SQLite cannot extend a CHECK in place.

Stop the app and all Memory API processes. Prefer disabling the hosted source and selecting the prior BYOK/delegate; keep V91 and its ledger intact. Do not run an older binary against the newer schema or delete the refinery history row alone.

If an older binary is essential, restore the encrypted pre-migration snapshot using the normal restore procedure before launching it. This loses records created since that snapshot; retain a copy of the current encrypted database for recovery. Never delete or relabel live gateway sends as direct BYOK traffic.
