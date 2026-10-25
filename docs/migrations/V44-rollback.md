# V44 ロールバック — skill_runs（Issue #251）

対象: 新規テーブル `skill_runs` と、そのインデックス `idx_skill_runs_status`。

## 影響範囲

- 追加のみ。既存テーブルの列や制約は変わらない。
- ユーザー作成スキル（`<app-data>/skills/*.md`）の実行依頼と結果を 1 実行＝1 行で持つ。
  - デスクトップの Skills カードからの実行も、MCP / REST / CLI からの実行依頼も、ここに行を作る。
  - 実行するのはデスクトップ（Agent lane）。API 側のプロセスは行を積むだけ。
- `output` は生成テキスト（メモリの内容を引用し得る）。データ全消去（`maintenance` の delete-all）の対象に含めてある。
- スキルファイルそのものは DB に入らない（ユーザーのフォルダにあるプレーンファイル）。

## ロールバック

```sql
BEGIN;
DROP INDEX IF EXISTS idx_skill_runs_status;
DROP TABLE IF EXISTS skill_runs;
DELETE FROM refinery_schema_history WHERE version = 44;
COMMIT;
```

FK は無いので、ドロップ順の制約は無い。

## データ損失

スキル実行の履歴と結果テキストが消える。スキルファイル（ユーザーのフォルダ）は消えないので、同じスキルは再実行できる。

## 注意

旧ビルドはこのテーブルを読まない。ロールバック後に実行待ち（`pending` / `awaiting_confirm`）だった依頼は実行されずに消える。
