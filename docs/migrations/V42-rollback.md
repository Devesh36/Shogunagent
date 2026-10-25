# V42 ロールバック — state_disputes（矛盾フラグ）

対象: 新規テーブル `state_disputes` と、そのインデックス `idx_state_disputes_open`（issue #253）。

## 影響範囲

- 追加のみ。既存テーブルの列や制約は変わらない。
- 同じ約束に見える open の commitments 2件が、違う期限を名指ししたときに1行を書く。
  2件の commitments はどちらも残り、上書きはしない。
- 読むのは次の3か所:
  - Fusion: 係争中の行を事実として扱わない
  - Memory API: `state.disputes.list` / `state.disputes.resolve`
  - desktop の Context Health の一覧
- 行に入るのは、state の id と、食い違った2つの値（期限のタイムスタンプ）だけ。キャプチャ本文は入らない。

## ロールバック

```sql
BEGIN;
DROP INDEX IF EXISTS idx_state_disputes_open;
DROP TABLE IF EXISTS state_disputes;
DELETE FROM refinery_schema_history WHERE version = 42;
COMMIT;
```

旧ビルドはこのテーブルを参照しない。旧ビルドで開く前にスキーマ履歴も揃えたい場合は、上の SQL を流す。

## データ損失

未解決・解決済みの矛盾記録だけが消える。commitments 自体は V42 の前後で一度も上書きしていないので、
ロールバックしても両方の行がそのまま残る（係争中だった2件は、区別のない2件の約束として見える）。
解決時に「片方を採用」した場合、もう片方の status は `cancelled` にしてあり、これはロールバック後も残る。
