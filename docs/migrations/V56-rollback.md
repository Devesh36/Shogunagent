# V56 rollback

Stop Shogun and back up the encrypted database before changing its schema. Do not downgrade
while relay attempts are pending: older clients cannot recover these requests and may resubmit
paid work. Recover or operator-reconcile every unfinished row first.

```sql
SELECT request_id, provider_id FROM relay_attempts WHERE finished = 0;
-- Only after the query is empty and the backup is verified:
DROP TABLE relay_attempts;
DELETE FROM refinery_schema_history WHERE version = 56;
```

The table contains only request fingerprints and provider handles; meeting records remain intact.
