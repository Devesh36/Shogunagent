# V51__meeting_recap_edits.sql — ロールバック手順

対象: `meeting_recaps.edited_at`（nullable INTEGER, ms）の 1 列追加（議事録のその場編集 /
docs/meeting-gap-analysis-2026-09.md D1）。

## 影響範囲

additive のみ（nullable 列 1 つ）。旧ビルドはこの列を読まず、NULL のままでも全機能が従来どおり動く。

ユーザーの編集は `summary` / `decisions` / `next_actions` の**既存列に書き込まれ**、`edited_at`
はその行が人の手で書かれた版であることを示すマーカー。V51 対応ビルドの `meeting_recaps::save`
（再生成）は `edited_at IS NOT NULL` の行を上書きしない。編集時は検索プロジェクションの鮮度判定
（`created_at`）も進める。

## Rollback

V51 が refinery 履歴に残ったまま V50 ビルドを起動しないこと。実行前に暗号化 DB をバックアップする。
SQLite の `ALTER TABLE ... DROP COLUMN` を使う（本リポジトリの rusqlite が同梱する SQLite は
3.35+ であり対応している）。

1. SHOGUN のデスクトップ / API / MCP プロセスをすべて停止する。
2. 暗号化 DB ファイルを複製して保管する。
3. V51 対応ツールが入っている間に、以下のトランザクションを実行する。
4. `refinery_schema_history` に version 51 の行が無いこと、`PRAGMA foreign_key_check` が
   空であることを確認する。
5. その後に V50 ビルドを導入・起動する。

```sql
BEGIN;

ALTER TABLE meeting_recaps DROP COLUMN edited_at;

DELETE FROM refinery_schema_history WHERE version = 51;

COMMIT;
```

## データ損失

「編集済み」マーカーだけが失われる。編集した本文そのもの（`summary` / `decisions` /
`next_actions`）は既存列に残るので消えない。ただし V50 ビルドでは再生成（テンプレート変更や
話者名の確定）がその本文をモデルの出力で上書きするようになる — V51 で守っていた「人が書いた版を
モデルが上書きしない」保証がロールバック後は効かない点に注意。
