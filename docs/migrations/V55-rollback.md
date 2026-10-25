# V55 ロールバック — Morning Brief の要求台帳 `brief_generation`

対象: `crates/shogun-memory/src/migrations/V55__brief_generation.sql`。
Morning Brief の Batch 要求を日付ごとに 1 行で追跡する台帳テーブルと、その入力ソース
（event_log の行）が消えた・変わったときに生成済みの文章を無効化する 3 本のトリガを追加した。
番号の経緯: 当初 V44 → main の `skill_runs` に譲って V51 → main が V51〜V54
（`meeting_recap_edits` / `memory_imports` / `clarify_questions` / `dream_batch_send_stats`）を
先に使ったので、この台帳は V55。未リリースなので番号の付け替えだけで済む（旧番号で適用済みの
開発 DB は refinery のチェックサム／番号不一致で起動を拒むので、リセットすること）。

## 影響範囲

| 追加物 | 役割 |
|---|---|
| `brief_generation`（STRICT） | 日付 PK、入力窓、出力言語、ソース識別子の manifest（本文は持たない）、状態機械、プロバイダ handle、世代番号、ランダムな `request_id` |
| `brief_generation_request_id_unique` | `request_id` の一意索引 |
| `brief_generation_new_identity` | 同日を削除→再作成したとき古いワーカーを fence する |
| `brief_generation_source_delete` / `brief_generation_source_update` | manifest がその event を含む行を `expired` にし、対応する `briefs` の生成文を消す |

既存テーブルの形は変えない。V51〜V54 とは独立（互いのテーブル・トリガを参照しない）。

## ロールバック

アプリを止めてから実行する。

```sql
BEGIN;
DROP TRIGGER IF EXISTS brief_generation_source_delete;
DROP TRIGGER IF EXISTS brief_generation_source_update;
DROP TRIGGER IF EXISTS brief_generation_new_identity;
DROP INDEX IF EXISTS brief_generation_request_id_unique;
DROP TABLE IF EXISTS brief_generation;
DELETE FROM refinery_schema_history WHERE version = 55;
COMMIT;
```

## データ損失

- `brief_generation` の行は失われる。ユーザーの本文は含まれない。
- `briefs` の生成済み文章はロールバックでは消えない。
- 進行中だった Batch 要求の結果は、台帳が無い旧バージョンには取り込まれない。

## 注意

- 旧バージョンに戻したあと再び V55 へ上げると、台帳は空から作り直される。`briefs` の既存行はそのまま。
- FK は無い。ドロップ順はトリガ→索引→テーブル。
