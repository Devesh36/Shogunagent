# #1101 live relay schema rollout and rollback

This adds `relay_live_subjects`, `relay_live_counters`, `relay_live_requests`, and
`relay_live_admin_actions` to the existing **relay** schema. It does not alter Batch
or account tables. Use a separate, empty database for tests.

Apply `apps/api/sql/live-store.sql` as the existing `shogun_relay` role (its configured
`search_path` is `relay`), then apply `apps/api/sql/live-store-access.sql` as the
schema owner with the same search path. The access script is transactional. Revoke
anonymous/PostgREST access; never grant the gateway the Supabase service role.
Keep Hyperdrive query caching disabled.

Before enabling, verify all four tables, grants and RLS policies, run the SQL and
Worker tests, configure the independent live provider keys and current price table,
and complete the remaining launch gates in `docs/hosted-ai-gateway-implementation.md`.

Rollback: unset `LIVE_AI_ENABLED` or set it to `0`, then deploy the previous Worker.
The live endpoints return 503; existing Batch endpoints retain their previous
behavior. **Keep every live table and counter.** Do not truncate usage, lower a
counter or refund ambiguous requests as part of rollback. This prevents re-enabling
from resetting the user's monthly budget. The old Worker does not reference these
tables. A later schema removal requires a separate retention/migration decision.
