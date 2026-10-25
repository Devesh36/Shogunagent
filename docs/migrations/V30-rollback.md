# V30__llm_usage.sql — ロールバック手順

対象: `llm_usage` テーブル新設（FR-HN-01 / Issue #313、`docs/requirements-harness-v1.md`）。
LLM 呼び出しごとのトークン数・キャッシュヒット・レイテンシ・結果の計上。本文列なし。

## 影響範囲

完全に additive（新テーブル＋インデックス2本のみ）。既存テーブル・既存コードへの影響なし。
V29 のコードは V30 のスキーマ上でそのまま動作する。

## ロールバック

```sql
BEGIN;

DROP TABLE llm_usage;

DELETE FROM refinery_schema_history WHERE version = 30;

COMMIT;
```

（インデックス `idx_llm_usage_ts` / `idx_llm_usage_prompt` は DROP TABLE に付随して消える。）

## データ損失

`llm_usage` の全行。中身は計測メタデータ（トークン数・レイテンシ・結果・プロンプトID）のみで、
ユーザーテキスト・キャプチャ内容・秘密情報は設計上含まれていない（本文を入れる列が存在しない）。
コスト分析の連続性を保ちたい場合は削除前にエクスポートする。

## 注意

- ロールバック後は shogun-core の `DbUsageSink` 書き込みが失敗するが、`UsageSink::record` は
  fire-and-forget 契約（失敗はカウントのみ、ユーザーの作業を止めない）なので機能影響はない。
  とはいえ V29 のコードへ戻してから運用すること。
