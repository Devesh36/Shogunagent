# V67__shared_service_observability.sql — ロールバック手順

対象: issue #29（大量のユーザーが同時に触ってもパフォーマンスが落ちない）スライス A の端末側観測
（`docs/specs/issue-29-shared-service-capacity.md` §4-6 / §5）。

- `traceability_log.kind` 列の追加（`'egress'` / `'retry'` / `'fell_back'`、既定 `'egress'`）。既存行は
  すべて `'egress'` として読まれ、意味は変わらない。`retry` / `fell_back` は共有サービス経路の目印で、
  チャンクを持たない（`chunk_bytes = 0`、`chunk_xxh64 = ''`）
- 新規テーブル `shared_service_rtt`（共有サービスへの HTTP 往復 1 回 = 1 行。経路・ミリ秒・HTTP ステータス
  のみ。**URL・ID・本文を入れる列は無い**）とインデックス `idx_shared_service_rtt_ts`

## 影響範囲

additive。列の追加（定数の既定値つき）と、新規テーブル1つ・インデックス1つのみ。既存列・インデックス・
トリガーの変更なし。旧バイナリ（V66 以前）は V67 のスキーマ上でもそのまま動作する: `traceability_log` への
INSERT は `kind` を指定しないので既定の `'egress'` が入り、新テーブルは参照しない。

以後 `traceability_log` の route CHECK を広げるテーブル再作成（V18 / V46 型）を書くときは、`kind` 列も
新テーブルへ引き継ぐこと。

## 番号について

当初 V64 として書いたが、`main` に V64（`dictionary_autoadd`）/ V65（`dictionary_autoadd_ownership`）が先に入ったため V67 に振り直した。V66 は PR #755（`translate_jobs`）が使う。refinery は既定で `abort_missing = true` なので、**V67 を適用済みの DB に後から V66 が届くと `MissingVersion` で起動時のマイグレーションが止まる**。V65 → V67 の飛び番そのものは受け付ける（現在より大きい版は適用対象）。したがって #755（V66）を先にマージすること。

## ロールバック

SQLite は列の削除に再作成を要し、`traceability_log` は監査記録なので、**列はそのまま残す**のが既定の
ロールバック（旧バイナリは `kind` を読まず、書くときも既定値で埋まる）。目印行だけを消し、新テーブルを
落とす:

```sql
BEGIN;
DELETE FROM traceability_log WHERE kind <> 'egress';
DROP INDEX IF EXISTS idx_shared_service_rtt_ts;
DROP TABLE IF EXISTS shared_service_rtt;
DELETE FROM refinery_schema_history WHERE version = 67;
COMMIT;
```

`refinery_schema_history` から V67 を消した DB を V67 のバイナリで再度開くと、`ADD COLUMN kind` が
「列が既にある」で失敗する。再適用が必要なら、上の手順の代わりにバックアップ（V67 適用前）から復元する。

## データ損失

- 失われるのは共有サービスの往復時間（ミリ秒・ステータス）と、retry / fell back の目印行のみ。ユーザーの
  データ・状態・キャプチャ本文・送信（egress）の監査行には影響しない。
