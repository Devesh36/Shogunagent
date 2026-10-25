# V52__memory_imports.sql — ロールバック手順

対象: issue #384（`shogun-memory-import/v1` パッケージの取り込み）/ issue #391（ChatGPT Computer
History Markdown の取り込み）。設計メモは `docs/specs/memory-import.md`。

- 新規テーブル `memory_imports`（取り込み1回＝1行。取り込んだ項目すべての provenance。
  `provenance = 'imported:' || origin` を CHECK。パッケージ本体は保存せず、サイズと xxh64 のみ）
- 新規テーブル `memory_import_items`（ユーザーが選んだ項目。confidence は CHECK で 0.6 以下＝
  High にならない。既存レコードへのマージは `linked_table` / `linked_id` によるリンクのみで、
  観測済みの行は書き換えない）
- インデックス `idx_memory_import_items_import` / `idx_memory_import_items_match`

## 影響範囲

additive。新規テーブル2つとインデックス2つのみ。既存テーブル・列・インデックス・トリガーの変更なし。
取り込み項目は `people` / `projects` / `open_loops` / `event_log` / `state_provenance` に一切書かない
ため、旧バイナリ（V51）は V52 のスキーマ上でもそのまま動作し、新テーブルを参照しない。

## ロールバック

```sql
BEGIN;
DROP INDEX IF EXISTS idx_memory_import_items_match;
DROP INDEX IF EXISTS idx_memory_import_items_import;
DROP TABLE IF EXISTS memory_import_items;
DROP TABLE IF EXISTS memory_imports;
DELETE FROM refinery_schema_history WHERE version = 52;
COMMIT;
```

## データ損失

- 失われるのは取り込んだメモリ（`memory_import_items`）と取り込み記録（`memory_imports`）のみ。
  これらはユーザーが別アシスタントのエクスポートや Computer History から持ち込んだ主張であり、
  SHOGUN が観測したデータ（イベントログ・state tables）には影響しない。
- 取り込み項目が既存レコードへリンクしていた場合も、リンク先の行は取り込み時に変更されていないため、
  ロールバック後も元のまま残る。
- ロールバック前に残したい場合は、設定のデータエクスポート（`export_json` の `memory_imports` /
  `memory_import_items`）で書き出しておく。
