# V26__meeting_templates.sql — ロールバック手順

対象: `sessions.template` / `meeting_recaps.sections` の2列追加（会議テンプレート / FR-MT-16 追補）。

## 影響範囲

additive のみ（nullable 列2つ）。旧ビルドは両列を読まず、NULL のままでも全機能が従来どおり動く。
テンプレート未選択・テンプレート無しの議事録は両列とも NULL で、V26 以前の行と区別できない
（意図した設計）。

## Rollback

V26 が refinery 履歴に残ったまま V25 ビルドを起動しないこと。実行前に暗号化 DB をバックアップする。
SQLite の `ALTER TABLE ... DROP COLUMN` を使う（本リポジトリの rusqlite が同梱する SQLite は
3.35+ であり対応している）。

1. SHOGUN のデスクトップ / API / MCP プロセスをすべて停止する。
2. 暗号化 DB ファイルを複製して保管する。
3. V26 対応ツールが入っている間に、以下のトランザクションを実行する。
4. `refinery_schema_history` に version 22 の行が無いこと、`PRAGMA foreign_key_check` が
   空であることを確認する。
5. その後に V25 ビルドを導入・起動する。

```sql
BEGIN;

ALTER TABLE meeting_recaps DROP COLUMN sections;
ALTER TABLE sessions DROP COLUMN template;

DELETE FROM refinery_schema_history WHERE version = 26;

COMMIT;
```

> **訂正（issue #780）**: この手順は当初 `version = 22` を消す指示になっていた。それでは V22 が未適用扱いで残り、refinery の `verify_migrations` が MissingVersion で失敗してメモリ DB が開かなくなる（新旧どちらのバイナリでも）。さらに V26 が適用済みのまま残るので、再アップグレードしても V26 が再実行されない。**消すのは `version = 26`。**


## データ損失

各会議のテンプレート選択と、テンプレート整形された議事録セクションが失われる。要約・決定事項・
next actions（V10 の既存列）は影響を受けない。次回 Recap 再生成でセクションは再取得できる
（文字起こしが残っている限り）。
