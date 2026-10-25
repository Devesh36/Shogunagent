# Company speech admission rollback

Disable `ASR_ENABLED` on `shogunai-relay`, deploy, and verify `/v1/asr/grant` and
`/v1/asr/refresh` return 503. Existing Deepgram sockets can remain connected until
their client session deadline; disabling mint does not revoke accepted JWTs.
Restore the preceding desktop build if necessary. Optional personal keys remain
independent of this switch.

Retain `relay_asr_subjects`, `relay_asr_months`, and `relay_asr_sessions` in the
isolated `relay` schema so usage reservations and blocks survive a rollback.
There is no destructive down migration. Re-enabling uses those same counters.
Apply `apps/api/sql/asr-store.sql` and `asr-store-permissions.sql` as
`shogun_relay`, with `search_path=relay`, before enabling speech grants.

Provider usage upgrade: apply `apps/api/sql/asr-provider-usage.sql` as the relay owner. It adds only aggregate counter floors. Disable ASR to roll back; retain this table and its RLS, and never clear its global block without investigating provider misuse.
