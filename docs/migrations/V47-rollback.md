# V47__sessions_in_person.sql — ロールバック手順

対象: `sessions.in_person INTEGER NOT NULL DEFAULT 0` の追加（issue #262 FR-MC-01。手動開始の
「対面」フラグ。音声レーンが system tap を開かず、マイクの話者分離ラベルを使う）。

## 影響範囲

additive。列の追加のみ。既存行は 0（オンライン）として読める。旧バイナリはこの列を読まないので
V46 のコードは V47 のスキーマ上でも動作する。外部キー・インデックスの変更なし。

## ロールバック

SQLite 3.35+ は `DROP COLUMN` を持つが、このリポジトリの方針（列は消さない）に従い、
**列を残したまま refinery の履歴だけ戻す**のが正しい手順:

```sql
BEGIN;
-- 列は残す（0 のまま無害）。履歴だけを V46 に戻す。
DELETE FROM refinery_schema_history WHERE version = 47;
COMMIT;
```

> **訂正（issue #780）**: この手順は当初 `version = 43` を消す指示になっていた。それでは V43 が未適用扱いで残り、refinery の `verify_migrations` が MissingVersion で失敗してメモリ DB が開かなくなる（新旧どちらのバイナリでも）。さらに V47 が適用済みのまま残るので、再アップグレードしても V47 が再実行されない。**消すのは `version = 47`。**


どうしても列を消す必要がある場合のみ（バックアップ取得後）:

```sql
ALTER TABLE sessions DROP COLUMN in_person;
```

## データ損失

なし（列を残す手順）。列を消す場合は「対面で開始した」という事実だけが失われる。
文字起こし・ノート・リキャップは影響を受けない。
