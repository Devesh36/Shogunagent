# V48 ロールバック — local_writes（Issue #397）

対象: 新規テーブル `local_writes` と、そのインデックス `idx_local_writes_status`。

## 影響範囲

- 追加のみ。既存テーブルの列や制約は変わらない。
- `LocalAction::WriteFile`（L2）によるファイル書き出し（会議 Recap を Markdown / CSV で、ユーザーが選んだフォルダへ保存）を 1 件＝1 行で持つ。
  - デスクトップの「Save to folder」も、MCP / REST / CLI の `export.write_file` も、ここに行を作る。
  - API 由来の依頼は `awaiting_confirm` で止まり、デスクトップでユーザーが確認した行だけが書き出される。
- 行はローカル書き出しのトレーサビリティ記録を兼ねる: `kind = 'local_write'`、バイト数と xxh64 ダイジェストのみ。**内容は保存しない**（列が無い）。送信経路（route）も持たない（デバイス外に出ないため `traceability_log` には書かない）。
- `folder` / `name` / `path` は会議タイトルを含み得るので、データ全消去（`maintenance` の delete-all / delete-since）の対象に含めてある。
- 書き出されたファイルそのもの（ユーザーのフォルダ）は DB と無関係で、ロールバックでも消えない。

## ロールバック

```sql
BEGIN;
DROP INDEX IF EXISTS idx_local_writes_status;
DROP TABLE IF EXISTS local_writes;
DELETE FROM refinery_schema_history WHERE version = 48;
COMMIT;
```

FK は無いので、ドロップ順の制約は無い。

## データ損失

書き出し履歴（どこへ・いつ・何バイト・ダイジェスト）が消える。書き出されたファイルは残る。

## 注意

旧ビルドはこのテーブルを読まない。ロールバック時に確認待ち（`awaiting_confirm`）だった API からの書き出し依頼は実行されずに消える。
