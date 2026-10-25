# V29 rollback

V29 is additive: three tables holding the notch panel's persisted chat conversations —
`chat_threads`, `chat_messages` (per-thread rolling tail, with `ts` in unix ms so the windowed
privacy delete reaches it), and the `chat_state` singleton naming the active thread. Older
binaries ignore them. For a physical rollback, stop the app after creating and verifying an
encrypted backup, then:

```sql
DROP TABLE chat_messages;
DROP TABLE chat_state;
DROP TABLE chat_threads;
```

and remove version 29 from `refinery_schema_history`. This loses only the persisted chat
conversations. No other table references them.

Note: this migration was renumbered twice while PR #303 was in review — V24, then V25, and finally
V29 once main landed `V25__session_speakers` through `V28__pattern_notices`. No shipped database ever recorded the chat
tables as version 24 or 25.
