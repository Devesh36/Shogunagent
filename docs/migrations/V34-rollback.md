# V34__mail_triage_settings.sql — ロールバック手順

対象: メール仕訳のマスタートグル `mail_triage_settings`（issue #319、仕様 §9 の残タスク）。

## 影響範囲

additive のみ。新規テーブル1つ（1行固定）で、既存テーブルへの変更はない。V33 の
`mail_labels.enabled`（ラベル単位）とは独立で、そちらには触れていない。

## ロールバック

```sql
BEGIN;
DROP TABLE IF EXISTS mail_triage_settings;
DELETE FROM refinery_schema_history WHERE version = 34;
COMMIT;
```

## データ損失

ユーザーが「仕訳を止める」を選んでいた場合、その1ビットだけが失われる。ロールバック後の
コードは V33 時点の挙動、すなわち**常に仕訳する**に戻るため、止めていた人は再び仕訳が
走る状態で復帰する。分類結果（`mail_triage`）・ラベル・送信者ルール・フィードバックは
このテーブルと無関係なのでいずれも残る。
