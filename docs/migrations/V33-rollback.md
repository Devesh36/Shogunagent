# V33__mail_triage.sql — ロールバック手順

対象: メール仕訳（Issue #319、`docs/specs/issue-319-mail-triage-spec.md` §4-2）の新設テーブル 9 つ
（`mail_messages` / `mail_labels` / `mail_triage` / `mail_triage_batch_items` / `mail_triage_feedback` /
`mail_sender_rules` / `connector_sync_cursors` / `mail_identities` / `mail_write_log`）と索引。

## 影響範囲

additive のみ。既存テーブルへの変更はない。`mail_messages.event_id` は `event_log(id)` を
ON DELETE CASCADE で参照する（イベント削除に追従して消える）。`threads` への参照は意図的に持たない
（`threads` は Dream Cycle が再構築できるキャッシュで、maintenance が全消しすることがある）。

## ロールバック

```sql
BEGIN;
DROP INDEX IF EXISTS idx_mail_write_log_ts;
DROP TABLE IF EXISTS mail_write_log;
DROP INDEX IF EXISTS idx_mail_triage_feedback_undistilled;
DROP INDEX IF EXISTS idx_mail_triage_batch_pending;
DROP INDEX IF EXISTS idx_mail_triage_stage;
DROP INDEX IF EXISTS idx_mail_triage_label;
DROP INDEX IF EXISTS idx_mail_triage_bucket;
DROP INDEX IF EXISTS idx_mail_messages_event;
DROP INDEX IF EXISTS idx_mail_messages_from;
DROP INDEX IF EXISTS idx_mail_messages_thread;
DROP TABLE IF EXISTS mail_identities;
DROP TABLE IF EXISTS connector_sync_cursors;
DROP TABLE IF EXISTS mail_sender_rules;
DROP TABLE IF EXISTS mail_triage_feedback;
DROP TABLE IF EXISTS mail_triage_batch_items;
DROP TABLE IF EXISTS mail_triage;
DROP TABLE IF EXISTS mail_labels;
DROP TABLE IF EXISTS mail_messages;
DELETE FROM refinery_schema_history WHERE version = 33;
COMMIT;
```

## データ損失

失われるのは**仕訳の推定結果・ユーザーの修正・カスタムラベル・送信者ルール・同期カーソル**である。
メール本文は `event_log`（V1）にあり影響を受けない。`needs_reply` から派生した `open_loops` 行も
state table 側に残る（`mail_triage.open_loop_id` の参照が消えるだけ）。同期カーソルが消えると
次回同期は既定のバックフィル窓（7 日）から取り直すが、`content_hash` 重複排除で二重登録はしない。

## 注意

V1（`event_log`）と V5（`threads` の `thread_key` 規約）に依存する。それらを落とす場合は V33 を先に落とす。
V16（`lessons`）とは独立（仕訳フィードバックの蒸留は `lessons` に書くが FK はない）。
