# V43 ロールバック — calendar_occurrences（FR-MT-06）

対象: 新規テーブル `calendar_occurrences` と、そのインデックス `idx_calendar_occurrences_time`（issue #263）。

## 影響範囲

- 追加のみ。既存テーブルの列や制約は変わらない。
- Google カレンダーの同期（`gcal`）で取り込んだ予定を、1予定＝1行の state として保持する。
  - 同期のたびに `external_id` で上書き更新する。
  - 予定がキャンセルされたら `status = 'cancelled'` にする。
- 読むのは次の2か所。
  - 会議セッションを開くとき: `sessions.calendar_occurrence_id` に、重なる予定の id を入れる。
  - 話者名の候補: 出席者を最上位の候補にする。
- `location` は会議 URL を含むことがあるので、デバイスの外には出さない（FR-MT-06）。
- `attendees` には出席者のメールアドレスが入る。データ全消去（`maintenance` の delete-all）の対象に含めてある。

## ロールバック

```sql
BEGIN;
DROP INDEX IF EXISTS idx_calendar_occurrences_time;
DROP TABLE IF EXISTS calendar_occurrences;
DELETE FROM refinery_schema_history WHERE version = 43;
COMMIT;
```

`sessions.calendar_occurrence_id` は V7 からある列で、FK も無い。ロールバック後は、存在しない id を指す値が残り得るが、
旧ビルドはこの列を読まないので害はない。気になる場合は `UPDATE sessions SET calendar_occurrence_id = NULL;` を流す。

## データ損失

予定の構造化データ（開始・終了・場所・出席者）だけが消える。予定そのものは V43 以前と同じく `event_log`
（`source = 'gcal'`）にも残っているので、検索は変わらない。次回の同期で再び作られる（V43 を再適用した場合）。
