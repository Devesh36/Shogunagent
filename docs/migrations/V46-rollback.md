# V46__traceability_web_route.sql — ロールバック手順

対象: `traceability_log.route` の CHECK に `'web'` を追加
（issue #555。チャットが YouTube 字幕 / X 投稿 oEmbed / 一般ページを読むときの台帳行）。

## 影響範囲

non-additive。V18 / V31 と同型のテーブル再作成＋全行コピー。列の削除・改名・型変更なし。
`idx_traceability_ts` を張り直す。外部キー参照なし。

`web` に `third_party` は付けない — 第三者中継を挟まず、送るのは URL だけ（Cookie も
資格情報も同行しない）。台帳にはホスト・URL の digest・バイト数のみ残る（本文は保存しない。G8）。

## ロールバック

```sql
BEGIN;

DELETE FROM traceability_log WHERE route = 'web';

CREATE TABLE traceability_log_v41 (
    id          INTEGER PRIMARY KEY,
    ts          INTEGER NOT NULL,
    route       TEXT    NOT NULL CHECK (route IN ('batch_api', 'batch_relay', 'messages_api', 'mcp', 'composio', 'billing', 'asr', 'local_agent', 'support')),
    purpose     TEXT    NOT NULL,
    destination TEXT    NOT NULL,
    chunk_bytes INTEGER NOT NULL,
    chunk_xxh64 TEXT    NOT NULL,
    third_party INTEGER NOT NULL DEFAULT 0
) STRICT;

INSERT INTO traceability_log_v41 (id, ts, route, purpose, destination, chunk_bytes, chunk_xxh64, third_party)
    SELECT id, ts, route, purpose, destination, chunk_bytes, chunk_xxh64, third_party
    FROM traceability_log;

DROP TABLE traceability_log;
ALTER TABLE traceability_log_v41 RENAME TO traceability_log;
CREATE INDEX idx_traceability_ts ON traceability_log (ts);

DELETE FROM refinery_schema_history WHERE version = 46;

COMMIT;
```

> **訂正（issue #780）**: この手順は当初 `version = 42` を消す指示になっていた。それでは V42 が未適用扱いで残り、refinery の `verify_migrations` が MissingVersion で失敗してメモリ DB が開かなくなる（新旧どちらのバイナリでも）。さらに V46 が適用済みのまま残るので、再アップグレードしても V46 が再実行されない。**消すのは `version = 46`。**
> SQL ファイル先頭のインライン手順も `version = 42` を消す指示になっているが、適用済みマイグレーションのバイト列は変えられない（refinery のチェックサムが変わる）ため、ヘッダはそのまま。**こちらの手順を正とする。**


## データ損失

`route = 'web'` の行のみ（削除しないと旧 CHECK に収まらない）。他の行は全て保持される。

## 注意

- 旧バイナリ（V45 のコード）は V46 のスキーマ上でも動作する（列は同一）。ロールバックが
  必要になるのは、`web` 行を書く新バイナリを取り消したうえで CHECK を戻したい場合だけ
- `llm_usage.route` の CHECK（V30）は変更していない。Web の要約はユーザー資格情報の
  `messages_api` / `local_agent` として記録される
