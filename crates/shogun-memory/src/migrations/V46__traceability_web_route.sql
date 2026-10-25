-- Web reads for chat (issue #555): add 'web' to the traceability_log.route CHECK.
--
-- チャットが URL を読むとき（YouTube の字幕トラック / X 投稿の oEmbed / 一般ページ）、デバイスから
-- 出るのは URL だけで、台帳には host と URL の digest とバイト数を残す（本文は保存しない。G8）。
-- 不変条件3のとおり送信箇所には台帳行を残す。third_party は付けない: 第三者中継を挟まず、
-- ユーザーのデータは何も同行しない（Cookie も資格情報も送らない）。
--
-- non-additive-ok: SQLite は CHECK 制約を ALTER できないため、テーブル再作成 + コピー
-- （データ保全リビルド。列の削除・改名・型変更なし）。traceability_log を参照する外部キーは
-- 存在しないので FK 無効化は不要。DROP TABLE は付随インデックスも落とすため
-- idx_traceability_ts を張り直す（V18 / V31 と同型）。
--
-- ロールバック手順:
--   1. 下と同じ手順で CHECK から 'web' を除いたテーブルを作り直す
--   2. コピー前に  DELETE FROM traceability_log WHERE route = 'web';
--      （トレース行は監査記録であり、消すのは CHECK に収まらない行だけに限ること）
--   3. refinery_schema_history から version = 42 の行を削除する
-- 破壊的な列削除・改名は行っていないため、V45 のコードは V46 のスキーマ上でも動作する。
-- 手順の全文は docs/migrations/V46-rollback.md。

CREATE TABLE traceability_log_v42 (
    id          INTEGER PRIMARY KEY,
    ts          INTEGER NOT NULL,
    route       TEXT    NOT NULL CHECK (route IN ('batch_api', 'batch_relay', 'messages_api', 'mcp', 'composio', 'billing', 'asr', 'local_agent', 'support', 'web')),
    purpose     TEXT    NOT NULL,
    destination TEXT    NOT NULL,
    chunk_bytes INTEGER NOT NULL,
    chunk_xxh64 TEXT    NOT NULL,
    third_party INTEGER NOT NULL DEFAULT 0
) STRICT;

INSERT INTO traceability_log_v42 (id, ts, route, purpose, destination, chunk_bytes, chunk_xxh64, third_party)
    SELECT id, ts, route, purpose, destination, chunk_bytes, chunk_xxh64, third_party
    FROM traceability_log;

DROP TABLE traceability_log;

ALTER TABLE traceability_log_v42 RENAME TO traceability_log;

CREATE INDEX idx_traceability_ts ON traceability_log (ts);
