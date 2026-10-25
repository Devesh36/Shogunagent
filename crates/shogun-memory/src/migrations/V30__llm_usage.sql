-- LLM usage accounting (FR-HN-01 / #313, docs/requirements-harness-v1.md).
--
-- 1呼び出し = 1行。トークン数・キャッシュヒット・レイテンシ・結果だけを持ち、
-- **本文列は存在しない**（traceability_log と同じ規律: 入れる場所がなければ間違えようがない）。
-- route の語彙は traceability_log.route と共有する。prompt_id / prompt_version は
-- shogun-core の llm::prompts レジストリ（FR-HN-02）。estimated=1 の行はバイト長推定
-- （委譲CLI・relay 経路はプロバイダが usage を返さない）で、正確値と区別できる。
-- model 列は書き込み側（UsageRecord::new）で redact_secrets を通した後の文字列のみ。
--
-- 追加のみの additive migration。ロールバック: docs/migrations/V25-rollback.md。

CREATE TABLE llm_usage (
    id                 INTEGER PRIMARY KEY,
    ts                 INTEGER NOT NULL,
    route              TEXT    NOT NULL CHECK (route IN ('batch_api', 'batch_relay', 'messages_api', 'mcp', 'composio', 'billing', 'asr', 'local_agent')),
    prompt_id          TEXT    NOT NULL,
    prompt_version     INTEGER NOT NULL,
    model              TEXT    NOT NULL,
    input_tokens       INTEGER NOT NULL DEFAULT 0,
    output_tokens      INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens  INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    estimated          INTEGER NOT NULL DEFAULT 0,
    first_token_ms     INTEGER,
    total_ms           INTEGER NOT NULL DEFAULT 0,
    outcome            TEXT    NOT NULL CHECK (outcome IN ('ok', 'error', 'rate_limited', 'unauthorized', 'quota', 'parse'))
) STRICT;

CREATE INDEX idx_llm_usage_ts ON llm_usage (ts);
CREATE INDEX idx_llm_usage_prompt ON llm_usage (prompt_id, ts);
