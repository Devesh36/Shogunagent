# V41 ロールバック — event_log の UPDATE トリガを列指定にする


対象: `event_log` の UPDATE トリガ3本（`event_log_au` / `period_summary_event_log_bu` /
`search_doc_event_log_au`）の発火条件を `UPDATE OF <列>` に限定し、Hot 層再構築用の
インデックス `idx_event_log_last_seen` を追加した。

## 影響範囲

テーブル・列の形は変わらない。トリガの**発火条件**だけが狭くなる:

| トリガ | 旧 | 新 |
|---|---|---|
| `event_log_au` | 全 UPDATE | `content, window_title` |
| `period_summary_event_log_bu` | 全 UPDATE | `ts, content` |
| `search_doc_event_log_au` | 全 UPDATE | `content, thread_key, session_id` |

`event_log::touch`（`last_seen_at` / `dwell_ms` のみ更新、2秒ごと）はどのトリガも起こさなくなる。
FTS の内容・期間要約の dirty 判定・検索ドキュメントの無効化は、内容が変わったときには従来どおり動く。

## ロールバック

V1 / V21 / V22 のトリガ本文をそのまま復元する（本文は各マイグレーションファイルにある）。

```sql
BEGIN;
DROP TRIGGER IF EXISTS event_log_au;
CREATE TRIGGER event_log_au AFTER UPDATE ON event_log BEGIN
    INSERT INTO event_fts (event_fts, rowid, content, window_title)
    VALUES ('delete', old.id, old.content, old.window_title);
    INSERT INTO event_fts (rowid, content, window_title)
    VALUES (new.id, new.content, new.window_title);
END;
DROP TRIGGER IF EXISTS period_summary_event_log_bu;
-- V22__period_summaries.sql の `period_summary_event_log_bu` を BEFORE UPDATE ON event_log で再作成
DROP TRIGGER IF EXISTS search_doc_event_log_au;
-- V21__search_documents.sql の `search_doc_event_log_au` を AFTER UPDATE ON event_log で再作成
DROP INDEX IF EXISTS idx_event_log_last_seen;
DELETE FROM refinery_schema_history WHERE version = 41;
COMMIT;
```

> **訂正（issue #780）**: この手順は当初 `version = 35` を消す指示になっていた。それでは V35 が未適用扱いで残り、refinery の `verify_migrations` が MissingVersion で失敗してメモリ DB が開かなくなる（新旧どちらのバイナリでも）。さらに V41 が適用済みのまま残るので、再アップグレードしても V41 が再実行されない。**消すのは `version = 41`。**


## データ損失

なし。トリガとインデックスのみで、行データには触れない。V41 適用中に touch されたイベントは
FTS に再登録されていないが、FTS の内容は変わっていない（touch は本文を変えない）ので検索結果も
変わらない。念のため FTS を作り直したい場合は
`INSERT INTO event_fts(event_fts) VALUES('rebuild');` を実行する。
