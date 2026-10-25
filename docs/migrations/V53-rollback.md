# V53__clarify_questions.sql — ロールバック手順

対象: issue #394（L2 Clarify: エージェントの run が Notch で質問を 1 つだけ出し、回答で同じ run が再開する）。

- 新規テーブル `clarify_questions`（質問 1 件＝1 行。`run_kind` / `run_id` が質問した run＝provenance。
  状態は `awaiting_input` → `answered` | `expired` | `cancelled`。回答は `answer` / `answer_choice` /
  `answer_origin`（`ui` / `api` / `mcp`））
- インデックス `idx_clarify_questions_status` / `idx_clarify_questions_run`
- 部分一意インデックス `idx_clarify_questions_one_open_per_run`（run ごとに未回答の質問は 1 件まで）

## 影響範囲

additive。新規テーブル 1 つとインデックス 3 つのみ。既存テーブル・列・インデックス・トリガーの変更なし。
L3 承認キュー（`l3_approvals.json`）とは別物で、このテーブルは何のアクションも保持しない。
旧バイナリ（V52）は V53 のスキーマ上でもそのまま動作し、新テーブルを参照しない。

## ロールバック

```sql
BEGIN;
DROP INDEX IF EXISTS idx_clarify_questions_one_open_per_run;
DROP INDEX IF EXISTS idx_clarify_questions_run;
DROP INDEX IF EXISTS idx_clarify_questions_status;
DROP TABLE IF EXISTS clarify_questions;
DELETE FROM refinery_schema_history WHERE version = 53;
COMMIT;
```

## データ損失

- 失われるのは質問と回答の記録のみ。未回答の質問があった run は、ロールバック後は回答を受け取れず、
  期限切れと同じ扱い（途中結果をドラフトとして残し、その先の手順は実行しない）になる。
- 回答はユーザー自身の文章であり、ローカル DB 以外には保存されていない。残したい場合はロールバック前に
  `SELECT * FROM clarify_questions` で書き出しておく。
