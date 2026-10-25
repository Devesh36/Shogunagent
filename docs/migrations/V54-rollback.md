# V54__dream_batch_send_stats.sql — ロールバック手順

対象: issue #286（Batch に送る量を削る: 決定的な前処理フィルタと原価テレメトリ / OPEN-B2）。

- 新規テーブル `dream_batch_send_stats`（Dream Cycle 1晩 × Batch ジョブ 1つ＝1行。前処理フィルタの
  件数・行数・文字数と、フィルタ前後の推定入力トークン数。**本文・行・行のハッシュを入れる列は無い**。
  `line_drops_by_rule` は固定のルール名→整数の JSON）
- インデックス `idx_dream_batch_send_stats_recorded`

## 影響範囲

additive。新規テーブル1つとインデックス1つのみ。既存テーブル・列・インデックス・トリガーの変更なし。
書き込みは best-effort（失敗しても Dream Cycle は止まらない）。旧バイナリ（V53）は V54 のスキーマ上でも
そのまま動作し、新テーブルを参照しない。

## ロールバック

```sql
BEGIN;
DROP INDEX IF EXISTS idx_dream_batch_send_stats_recorded;
DROP TABLE IF EXISTS dream_batch_send_stats;
DELETE FROM refinery_schema_history WHERE version = 54;
COMMIT;
```

## データ損失

- 失われるのは1晩ごとの送信量の集計（件数・文字数・推定トークン数）のみ。ユーザーのデータ・状態・
  キャプチャ本文には影響しない。
- OPEN-B2 の実測に使っている期間の行は、ロールバック前に書き出しておく:
  `sqlite3 <db> -json "SELECT * FROM dream_batch_send_stats ORDER BY recorded_at"`（暗号化 DB は
  `docs/production-db-access-runbook.md` の手順で開く）。
