# V32__dream_cycles.sql — ロールバック手順

対象: Dream Cycle の耐久 ID・入力範囲の凍結・Batch ライフサイクル（`dream_cycles` /
`dream_event_arrivals` / `dream_batch_items` / `dream_cycle_inputs` / `dream_brief_effects`）と、
`event_log` に張る4本のトリガ。

（issue #780 で追加。V32 だけ手順書が無く、SQL 末尾のインライン手順は
`refinery_schema_history` の行を消していなかった。適用済みマイグレーションのバイト列は変えられない
ため、SQL 側はそのまま。**こちらの手順を正とする。**）

## 影響範囲

additive。新しいテーブル5つ・インデックス2つ・`event_log` 上のトリガ4本だけで、既存の列・制約は
変えていない。`dream_event_arrivals` は適用時に既存の `event_log` 全行から作られる派生データ。

後続のマイグレーションでこれらのテーブルを参照するものは無い（V54 の `dream_batch_send_stats` は
意図的に `dream_cycles` への外部キーを持たない）。

## ロールバック

```sql
BEGIN;

DROP TRIGGER IF EXISTS dream_event_arrivals_ad;
DROP TRIGGER IF EXISTS dream_event_arrivals_ai;
DROP TRIGGER IF EXISTS dream_cycle_inputs_pin_update;
DROP TRIGGER IF EXISTS dream_cycle_inputs_pin_delete;

DROP TABLE IF EXISTS dream_brief_effects;
DROP TABLE IF EXISTS dream_cycle_inputs;
DROP TABLE IF EXISTS dream_batch_items;
DROP TABLE IF EXISTS dream_event_arrivals;
DROP TABLE IF EXISTS dream_cycles;

DELETE FROM refinery_schema_history WHERE version = 32;

COMMIT;
```

トリガは `dream_cycle_inputs` と `dream_event_arrivals` を参照するので、テーブルより先に消す。
インデックスはテーブルと一緒に消える。

## データ損失

- 実行中・未完了の Dream Cycle の状態（どの範囲を送ったか、Batch の provider id、受け取り待ちの
  custom id）。ロールバック後の最初のサイクルは新しいサイクルとして始まり、送信済みで未回収の
  Batch があればその結果は取り込まれない（Batch 側で期限切れになる）
- `dream_cycle_inputs` に退避していた入力テキストのコピー。元の `event_log` 行はそのまま残る
- `dream_event_arrivals` の到着順。V32 を再適用すると、その時点の `event_log` から作り直される

ユーザーのイベント・状態テーブル（`people` / `projects` / `commitments` / `open_loops`）は
変わらない。

## 注意

- **V33 以降を適用したまま V32 だけを戻さない。** 新しいバイナリの Dream Cycle は
  これらのテーブルを前提に動く。V32 を戻すときは `LATEST_SCHEMA_VERSION`
  （`crates/shogun-memory/src/lib.rs`）を 31 へ戻したバイナリと組み合わせ、V33 以降も
  降順で先に戻すこと
- 戻す前に `docs/migrations/README.md` の手順どおり DB のコピーを取る
