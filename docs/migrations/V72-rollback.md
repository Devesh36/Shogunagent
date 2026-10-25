# V72 ロールバック — ai_search_jobs（Issue #940 B2b）

対象: 新規テーブル `ai_search_jobs` と、そのインデックス `idx_ai_search_jobs_status`。

## 影響範囲

- 追加のみ。既存テーブルの列や制約は変わらない。
- Memory API（MCP / REST / CLI）の `memory.search` に `mode: "ai"` を付けた依頼を 1 依頼＝1 行で持つ受け渡し用の表。
  - API 側のプロセスは Keychain を読まないので、Agent lane（ユーザー自身の資格情報）では実行できない。行を積むだけで、実行するのはデスクトップ（V66 `translate_jobs` と同じ形）。
  - デスクトップの Full UI の検索はこの表を使わない（プロセス内で完結する）。
- 履歴ではない。API 側は結果を返した時点（または待つのをやめた時点）で行を消す。依頼・claim のたびに 10 分より古い行を状態に関係なく消す。
- テキスト列（質問・結果の抜粋）は書き込み前に秘密値をマスクする。データ全消去（`maintenance` の delete-all）の対象に含めてある。

## ロールバック

```sql
BEGIN;
DROP INDEX IF EXISTS idx_ai_search_jobs_status;
DROP TABLE IF EXISTS ai_search_jobs;
DELETE FROM refinery_schema_history WHERE version = 72;
COMMIT;
```

FK は無いので、ドロップ順の制約は無い。

## データ損失

処理中・処理待ちの AI 検索依頼（最長 10 分分）が消える。呼び出し元は待っている間ならキーワード結果を受け取る。恒久的なデータは持たない表なので、失われる利用者データは無い。

## 注意

旧ビルドはこのテーブルを読まない。ロールバック後、MCP / REST / CLI の `memory.search` の `mode: "ai"` は使えない（キーワード検索と、デスクトップ Full UI の AI 検索は影響を受けない）。
