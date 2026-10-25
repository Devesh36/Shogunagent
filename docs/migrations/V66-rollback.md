# V66 ロールバック — translate_jobs（Issue #279）

対象: 新規テーブル `translate_jobs` と、そのインデックス `idx_translate_jobs_status`。

## 影響範囲

- 追加のみ。既存テーブルの列や制約は変わらない。
- Memory API（MCP / REST / CLI）からの翻訳・返信ドラフト依頼を 1 依頼＝1 行で持つ受け渡し用の表。
  - API 側のプロセスは Keychain を読まないので、Agent lane（ユーザー自身の資格情報）では実行できない。行を積むだけで、実行するのはデスクトップ。
  - デスクトップのノッチパネルからの翻訳・返信ドラフトはこの表を使わない（従来どおりメモリ上だけで完結する）。
- 履歴ではない（FR-TL-20）。API 側は結果を呼び出し元に返した時点で行を消す。依頼・claim のたびに 10 分より古い行を状態に関係なく消す。
- テキスト列は書き込み前に秘密値をマスクする。データ全消去（`maintenance` の delete-all）の対象に含めてある。

## ロールバック

```sql
BEGIN;
DROP INDEX IF EXISTS idx_translate_jobs_status;
DROP TABLE IF EXISTS translate_jobs;
DELETE FROM refinery_schema_history WHERE version = 66;
COMMIT;
```

FK は無いので、ドロップ順の制約は無い。

## データ損失

処理中・処理待ちの翻訳依頼（最長 10 分分）が消える。呼び出し元は `translate.status` で `not_found` を受け取り、依頼し直せばよい。恒久的なデータは持たない表なので、失われる利用者データは無い。

## 注意

旧ビルドはこのテーブルを読まない。ロールバック後、MCP / REST / CLI の `translate.text` / `translate.reply_draft` は動かない（デスクトップのノッチパネルの翻訳・返信ドラフトは影響を受けない）。
