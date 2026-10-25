# V49__state_user_edits.sql — ロールバック手順

対象: issue #290 slice 1（ワールドモデルをユーザーが読める・直せる）。

- `people` / `projects` / `commitments` / `open_loops` に `user_pinned INTEGER NOT NULL DEFAULT 0`
  と `user_edited_at INTEGER` を追加
- 新規テーブル `state_suppressions`（削除・書き換えた信念を、同じ古いイベントから再抽出しないための
  ダイジェスト。本文は持たない）

## 影響範囲

additive。列の追加と新規テーブル1つのみ。既存行は `user_pinned = 0`（従来どおり自動パスの対象）
として読める。旧バイナリはどちらも参照しないので、V48 のコードは V49 のスキーマ上でも動作する。
外部キー・既存インデックスの変更なし。

## ロールバック

このリポジトリの方針（列は消さない）に従い、**列を残したまま**テーブルと履歴だけを戻す:

```sql
BEGIN;
DROP TABLE IF EXISTS state_suppressions;
-- user_pinned / user_edited_at は残す（旧ビルドは読まない。0 / NULL のまま無害）。
DELETE FROM refinery_schema_history WHERE version = 49;
COMMIT;
```

どうしても列を消す必要がある場合のみ（バックアップ取得後。SQLite 3.35+）:

```sql
ALTER TABLE people      DROP COLUMN user_pinned;
ALTER TABLE projects    DROP COLUMN user_pinned;
ALTER TABLE commitments DROP COLUMN user_pinned;
ALTER TABLE open_loops  DROP COLUMN user_pinned;
ALTER TABLE people      DROP COLUMN user_edited_at;
ALTER TABLE projects    DROP COLUMN user_edited_at;
ALTER TABLE commitments DROP COLUMN user_edited_at;
ALTER TABLE open_loops  DROP COLUMN user_edited_at;
```

## データ損失

- `state_suppressions` を落とすと、ユーザーが削除した信念が次の Dream Cycle で同じ古いイベントから
  再抽出されうる（削除そのものは取り消されない）。
- 列を残す手順ならユーザーの修正内容は残る。旧ビルドは `user_pinned` を知らないので、修正済みの行も
  減衰・自動更新の対象に戻る（値は上書きされうるが、行は消えない）。
- 列を消す手順では「ユーザーが修正した」という印だけが失われる。修正後の値そのものは残る。
