# V28__pattern_notices.sql — ロールバック手順

対象: `work_patterns.noticed_at` 列の追加（気づきモーメント / FR-PAT-06b、design §5e）。

## 影響範囲

additive のみ（nullable 列1つ）。旧ビルドはこの列を読まない。V28 が無い環境では
気づきモーメントが出ないだけで、プロファイル・蒸留・ranking prior は影響を受けない。

## Rollback

V28 が refinery 履歴に残ったまま V27 ビルドを起動しないこと。

1. SHOGUN のデスクトップ / API / MCP プロセスをすべて停止する。
2. 暗号化 DB ファイルを複製して保管する。
3. V28 対応ツールが入っている間に、以下のトランザクションを実行する。
4. `refinery_schema_history` に version 24 の行が無いことを確認する。
5. その後に V27 ビルドを導入・起動する。

```sql
BEGIN;

ALTER TABLE work_patterns DROP COLUMN noticed_at;

DELETE FROM refinery_schema_history WHERE version = 28;

COMMIT;
```

> **訂正（issue #780）**: この手順は当初 `version = 24` を消す指示になっていた。それでは V24 が未適用扱いで残り、refinery の `verify_migrations` が MissingVersion で失敗してメモリ DB が開かなくなる（新旧どちらのバイナリでも）。さらに V28 が適用済みのまま残るので、再アップグレードしても V28 が再実行されない。**消すのは `version = 28`。**


## データ損失

「どのパターンを気づきとして提示済みか」の記録のみ。再適用後は既存パターンが
もう一度（1回だけ）気づきとして提示され得る。
