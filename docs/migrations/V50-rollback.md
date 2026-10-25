# V50__state_merge_blocks.sql — ロールバック手順

対象: issue #290 / FR-ST-10（誤った名寄せをユーザーが分割する）。

- 新規テーブル `state_merge_blocks`（「この2行は同一エンティティではない」というユーザー判断。id のみ）
- 新規テーブル `state_split_events`（その分割で移したイベント id。抽出器が同じイベントを元の行へ
  再リンクしないためのリダイレクト表）
- インデックス `idx_state_split_events_event`

## 影響範囲

additive。新規テーブル2つとインデックス1つのみ。既存テーブル・列・インデックスの変更なし。
旧バイナリはどちらも参照しないので、V49 のコードは V50 のスキーマ上でも動作する。
分割で作られた新しい `people` / `projects` 行と、移動した `state_provenance` 行は通常の行であり、
ロールバック後もそのまま残る。

## ロールバック

```sql
BEGIN;
DROP INDEX IF EXISTS idx_state_split_events_event;
DROP TABLE IF EXISTS state_split_events;
DROP TABLE IF EXISTS state_merge_blocks;
DELETE FROM refinery_schema_history WHERE version = 50;
COMMIT;
```

## データ損失

- 分割の結果（新しい行・移したエビデンス・参照の付け替え）は失われない。
- 失われるのは「再び統合しない」という判断の記録のみ。ロールバック後、Dream Cycle が同じ古い
  イベントを再抽出すると、抽出キーが一致するイベントが元の行へ再リンクされうる（新しい行から
  リンクが消えることはないが、両方の行に同じイベントが付く）。
