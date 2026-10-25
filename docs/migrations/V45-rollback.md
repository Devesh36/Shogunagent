# V45 ロールバック — scheduled_jobs（Issue #252）

対象: 新規テーブル `scheduled_jobs`。

## 影響範囲

- 追加のみ。既存テーブルは変わらない。
- ユーザーが作ったスケジュール（どのスキルを、どの曜日の何時に実行するか）を 1 スケジュール＝1 行で持つ。
- 実行そのものは `skill_runs`（V44）の行として積まれる（origin = 'schedule'）。このテーブルは「いつ積むか」だけを持つ。
- `input` はユーザーが書いた入力文。データ全消去（`maintenance` の delete-all）の対象に含めてある。

## ロールバック

```sql
BEGIN;
DROP TABLE IF EXISTS scheduled_jobs;
DELETE FROM refinery_schema_history WHERE version = 45;
COMMIT;
```

FK は無い。

## データ損失

スケジュールの定義が消える。過去の実行結果は `skill_runs` に残る。

## 注意

旧ビルドはこのテーブルを読まない。ロールバック後はスケジュール実行が止まるだけ。
