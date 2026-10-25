# ShogunAgent 実装計画

本書は `README.md` / `README.ja.md` の方針を、エピックとタスクの粒度まで落とした実装計画である。期間は書かない。フェーズは順序と出口条件で定義し、各タスクには「触る場所」と「受け入れ条件」を付ける。ShogunAI から移行したコードの出自は `docs/shogunai-origin/SOURCE.txt`、移行の判断は `docs/MIGRATION.md` を参照。

---

## 0. 読み方

- **E = エピック、T = タスク。** `E1-T3` のように参照する。
- **「触る場所」** はクレート・ファイル・エンドポイント名。新規は `(new)`。
- **「受け入れ条件」** は CI で機械的に確かめられる形で書く。人の目で確かめるものは「レビュー」と明記する。
- 単価・目標値はすべて推定値で、design partner の実測で改定する。

---

## 1. 前提と不変条件

### 1.1 ShogunAI から継承する不変条件

| # | 不変条件 | ShogunAgent での形 |
|---|---|---|
| 1 | データの重心は Rust コアに置く | `shogunagent-core`（Rust）が DB・推定・権限・計量の主体。`apps/api`（TS）は認証・計量・402・ルーター制御面のみで本文を触らない |
| 3 | 生データは既定で端末を出ない。クラウド経路はゼロ保持 | Local（OSS）は端末内で閉じる。hosted は本文をログ・DB・メトリクスに書かない（`traceability_log` に本文カラム無しを維持） |
| 4 | L1 に外部送信を含めない | `SendAction` は型で L3。ルーターの `run` も `scope.rs` の表に従い、表に無い操作は拒否 |
| 6 | 人間 UI と AI API は対称 | ShogunAgent には人間 UI が無いので「MCP / REST / CLI の三面対称」として維持。新機能は 3 面同時 |
| 7 | 秘密はログ・DB・平文ファイルに書かない | Keychain は無い。テナント鍵は KMS/環境変数、エージェント鍵はハッシュ保存、`Secret` 型と `check-secret-exposure.py` を維持 |
| — | マイグレーションは追加のみ・凍結 | V1〜V91 を凍結したまま使い、V92 以降で加算 |
| — | clippy deny、`unwrap()` 禁止、HTTP egress は core のみ | `scripts/check-*.py` と `check-feature-matrix.sh` を CI に残す |

### 1.2 変える前提

| ShogunAI | ShogunAgent |
|---|---|
| 1 SQLCipher ファイル = 1 ユーザー、1 プロセス | 1 SQLCipher ファイル = 1 **principal**（tenant 内の人）。1 プロセスが多数のファイルを開閉する |
| REST は `127.0.0.1:7464` 固定 | Streamable HTTP MCP と REST を公開エンドポイントで待つ。ローカルモードは loopback のまま |
| 承認はデスクトップ UI | 承認は MCP MRTR（L2）/ URL elicitation（L3）/ Web / Slack / webhook |
| 取得は macOS 取得層 | 主経路は `memory.ingest`。macOS 取得層は ShogunAI アプリに残し、sync で流す |
| 課金はライセンス・プラン | プリペイド・クレジット、402、キー別ハード上限。プランゲートは「残高」と「上限」に置き換える |
| 埋め込みは出荷しない（#237） | ローカル埋め込みを再導入（fastembed-rs）。hosted は API 埋め込みをフォールバック |

---

## 2. 目標アーキテクチャ

```
 agents (Claude Code / Codex / Cursor / custom)         tool providers
        │  MCP Streamable HTTP · REST · CLI                   │ connector defs (Monid-compatible)
        ▼                                                     ▼
 ┌──────────────────────────────── apps/api (gateway, TS, Workers) ────────────────────────────────┐
 │ agent auth (sa_ keys, OAuth/CIMD) · wallet/402/caps · Mcp-Method/Mcp-Name metering · router     │
 │ control plane (catalog, health, discover ranking) · zero-retention proxy · webhooks · Stripe     │
 └───────────────┬──────────────────────────────────────────────────┬──────────────────────────────┘
                 │ per-tenant routing                                │ run (credentials injected at transport)
                 ▼                                                   ▼
 ┌──── shogunagent-core (Rust sidecar) ────┐              ┌──── router engine (Rust or Deno) ────┐
 │ open/close per-principal SQLCipher file │              │ connector loader · JSON Schema validate│
 │ event_log → Dream Cycle → state tables  │◄── run log ──│ execute · settle usage · map output   │
 │ ApprovalQueue (L1/L2/L3)                │              └───────────────────────────────────────┘
 │ MemoryBackend → MCP tools / REST / CLI  │
 │ /memories virtual FS · export v1        │
 └─────────────────────────────────────────┘
                 ▲
                 │ sync (encrypted, user-held key)
      ShogunAI Mac app (premium capture source)
```

### 2.1 プロセスと責務

- **`shogunagent-core`（Rust、`crates/shogun-core` の `shogun_api` / `shogun_mcp` バイナリを発展）**：principal ごとの SQLCipher ファイルを開閉する。推定（Dream Cycle）、状態の読み書き、権限判定、承認キュー、`/memories` 射影、エクスポート。本文はここにしか無い。
- **`apps/api`（TS、Cloudflare Workers + Postgres）**：エージェント認証、ウォレットと 402、`Mcp-Method`/`Mcp-Name` によるヘッダ計量、ルーターの制御面（カタログ、ヘルス、`discover` のランキング）、Stripe、webhook。本文を保存しない（既存のゼロ保持ロガーを維持）。
- **router engine**：コネクタ定義のロード、入力の JSON Schema 検証、実行（プロバイダ鍵は transport で注入しエンジン・プロセスに入れない）、使用量の精算、出力のマッピング。Monid の MIT エンジン（Deno/TS）をフォークするか Rust で再実装するかは E8-T1 で決める。
- **ShogunAI Mac アプリ**：取得層。既存の Memory API を ShogunAgent にブランド統合し、ユーザー保持鍵で hosted に sync する。

### 2.2 テナンシー

- `tenant`（組織）→ `principal`（人。1 ワールドモデル）→ `agent`（鍵の主体）。
- principal ごとに 1 つの SQLCipher ファイル。鍵は principal ごとに派生（ローカルはユーザー保持、hosted は KMS 封印）。
- `event_log` / state tables に `tenant_id` 列は足さない（ファイルが境界）。足すのは **`actor`**（どのエージェント・どの人が書いたか）と **`namespace`**（`world` / `agent_scratch:<agent_id>`）。
- 共有（Team）は principal を跨ぐ読み取り ACL で実現し、ファイルは混ぜない。
- Cloudflare Agent Memory と supermemory が同じ「1 ユーザー = 1 SQLite」を選んでいる（調査ノート参照）。

### 2.3 データモデルの加算（V92〜、すべて additive）

| 版 | 内容 | 目的 |
|---|---|---|
| V92 | `event_log.actor TEXT`, `event_log.namespace TEXT DEFAULT 'world'`, index | 書き手の provenance、agent-scratch 分離 |
| V93 | `state_*` 4 表に `actor_last TEXT`、`namespace TEXT DEFAULT 'world'` | 状態の書き手 |
| V94 | `tool_runs(id, ts, actor, tool_id, task_hint, outcome, cost_micro, latency_ms, hint_followed)` | ルーター学習、per-user `discover` |
| V95 | `export_tombstones(kind, id, deleted_at, reason)` | エクスポート形式の削除伝播 |
| V96 | `principal_meta(key, value)`（schema_version 以外のメタ：export_cursor、sync_cursor） | sync と差分エクスポート |
| V97 | `relations(subject_kind, subject_id, predicate, object_kind, object_id, valid_from, valid_to, confidence, provenance_event_id)` | グラフ DB を買わない代わりの型付き関係行 |

既存 V1〜V91 は 1 バイトも変えない（`check-migrations.py` と `MIGRATION SAFETY` ゲートを継続）。capture 固有の表（`screen_frames`, `transcript_segments`, `voice_terms`, `mail_*`, `writing_voice`）は残すが、hosted では空のまま。

---

## 3. 移行済み資産の分類とプルーニング

`docs/MIGRATION.md` に全クレートの分類を置く。要点：

| クレート | 扱い | Phase 1 でやること |
|---|---|---|
| `shogun-agents`, `shogun-fusion`, `shogun-redact`, `shogun-license` | そのまま | `shogun-license` は verify 部のみ使う。`hardware_device_id`（IOKit）は cfg で macOS のみ |
| `shogun-memory` | そのまま＋V92〜 | capture 固有モジュールは触らない（空で動く） |
| `shogun-mcp` | 中立部を使う | `memory_api_settings.rs` / `live_tokens.rs` / `launcher_check.rs` / `client_install.rs` / `plan_source.rs` / `trial_relay.rs` を feature `desktop` に隔離し既定 off。`MemoryBackend`、`memory_api.rs`（`ApiLevel`、`tool_level`、`tool_group`）、`toolsets.rs`、`source_policy.rs`、`rate_limit.rs`、`rest_*.rs`、`server.rs`、`mcp.rs`、`approval_store.rs` を使う |
| `shogun-core` | 中立部を使い、残りを feature で隔離 | `dreamcycle/*`, `llm/*`, `ai_search/*`, `context.rs`, `chat_grounding.rs`, `daemon/{db,...}.rs`, `db_backend.rs`, `metrics.rs`, `bin/shogun_api.rs`, `bin/shogun_mcp.rs` を使う。`capture/*`, `audio/*`, `meeting/*`, `notch/*`, `forms/*`, `gmail_*`, `dictionary_*`, `voice_*`, `translate*`, `left_option_hold.rs`, `panel_events.rs`, `sound.rs`, `auto_update.rs`, `self_install.rs` を feature `desktop` に隔離（削除はしない。ShogunAI 側との同期を保つため） |
| `shogun-integrations` | そのまま | Keychain 保管（`keychain*.rs`）を `TokenStore` trait に抽象化し、hosted は KMS 封印の Postgres 実装 |
| `shogun-microphone` | 依存のため残す | 既定 off。Phase 2 で `shogun-mcp` の依存から外す |
| `shogun-cli` | そのまま | `shogunagent` にリネームし、`npx shogunagent` のラッパーから呼ぶ |
| 評価 5 種 + `spike-harness` | そのまま | memory-bench の既知の弱点（temporal recall@1 0.000、abstention false-evidence 100%）を E12 で閉じる |
| `apps/api` | そのまま拡張 | 認証主体を licence から agent key に拡張。ウォレット、402、ルーター制御面を追加 |

---

## 4. Phase 1：コアとルーター骨格

**出口条件**：外部エージェントからの `tools/call` が継続的に流れている。design partner が契約している。同一ハーネスのベンチマークを 1 本公開した。ルーター経由エージェントの状態層転換率を計測できている。

### E1. ワークスペースと CI の整備

| T | 内容 | 触る場所 | 受け入れ条件 |
|---|---|---|---|
| E1-T1 | ルート `Cargo.toml` / `Cargo.lock` の整理（desktop・mark・ax-noise-bench を除外） | `Cargo.toml`, `Cargo.lock` | `cargo check --workspace --all-targets --locked` が Linux で通る |
| E1-T2 | `check-feature-matrix.sh` から desktop 参照を除去し、全 feature 組み合わせを Linux で通す | `scripts/check-feature-matrix.sh` | スクリプトが exit 0 |
| E1-T3 | ガード 6 種（http-egress、secret-exposure、media-writes、log-hygiene、migrations、batch-source-filter）の自己テストと本検査を通す。desktop パスの参照を削る | `scripts/check-*.py` | 各 `--self-test` と本検査が exit 0 |
| E1-T4 | GitHub Actions：Linux のみ、`ci.yml`（clippy、test、feature matrix、ガード）、`migration-safety.yml`（V1〜V91 凍結＋全版アップグレード）、`audit.yml`（cargo audit 日次） | `.github/workflows/` (new) | main への PR で全ジョブが緑 |
| E1-T5 | `CLAUDE.md`（本リポジトリ用）。継承する不変条件と ShogunAgent 固有ルール | `CLAUDE.md` (new) | レビュー |
| E1-T6 | ライセンスを Apache-2.0 に（`LICENSE`、各 crate の `license.workspace`） | `LICENSE` (new), `Cargo.toml` | `cargo metadata` の license が全て Apache-2.0 |
| E1-T7 | shogun-core / shogun-mcp に feature `desktop` を導入し、macOS 固有モジュールを隔離。既定 off | `crates/shogun-core/Cargo.toml`, `lib.rs`, `crates/shogun-mcp/Cargo.toml`, `lib.rs` | 既定 feature で `target_os = "macos"` を含むモジュールがコンパイル対象に無い（`grep` ガードを追加） |

### E2. テナンシー（サイドカー）

| T | 内容 | 触る場所 | 受け入れ条件 |
|---|---|---|---|
| E2-T1 | `PrincipalStore`：principal_id → SQLCipher ファイルの開閉、LRU、同時オープン上限、鍵の解決（`KeyProvider` trait：env / KMS） | `crates/shogun-core/src/tenancy/` (new) | 1,000 principal を順に開閉して fd リークが無い（テスト） |
| E2-T2 | `shogun_api` を `--multi` モードに拡張：リクエストの principal を鍵から解決して `PrincipalStore` を引く | `crates/shogun-core/src/bin/shogun_api.rs`, `shogun-mcp/src/server.rs` | principal A の鍵で principal B の行が 1 行も読めない（`tests/invariant_tenant.rs` new） |
| E2-T3 | V92〜V96 マイグレーション＋ロールバック手順 | `crates/shogun-memory/src/migrations/V92__*.sql`〜, `docs/migrations/V92-rollback.md`〜 | `MIGRATION UPGRADE` テストが V1〜V96 で通る |
| E2-T4 | `actor` を全書き込み経路に通す（`memory.append_note`, `state.*`, `memory.ingest`, consolidator） | `shogun-memory/src/{event_log,state,loops}.rs`, `shogun-mcp/src/memory_api.rs` | actor 無しの書き込みがリポジトリ層で拒否される（`EmptyActor`） |
| E2-T5 | `agent_scratch:<agent_id>` 名前空間。`get_context_pack` と `state.*` は既定で `world` のみ、`include_scratch` で明示 | `shogun-memory/src/search/`, `shogun-fusion/src/assemble.rs` | scratch の行が既定の context pack に混ざらない（テスト） |
| E2-T6 | 移行ツール：ShogunAI の単一 `memory.db` を principal ファイルとして取り込む（スキーマ同一なのでコピー＋V92〜適用） | `crates/shogun-cli` | 既存 DB を読み込んで `state.commitments.list` が同じ結果を返す |

### E3. エージェント認証

| T | 内容 | 触る場所 | 受け入れ条件 |
|---|---|---|---|
| E3-T1 | API key（`sa_live_…` / `sa_test_…`）。発行はハッシュ保存（Argon2id）、スコープ（`read` / `write` / `run` / `admin`）、principal 束縛、有効期限、取り消し | `apps/api/src/auth.ts`, `pg-store.ts`, `sql/agent-keys.sql` (new) | 鍵の平文が DB・ログに無い（`check-secret-exposure` 拡張） |
| E3-T2 | OAuth 2.1 + CIMD（Client ID Metadata Documents）で MCP クライアントを登録。DCR は実装しない | `apps/api/src/oauth.ts` (new) | Claude Code / Cursor の MCP 設定から接続できる（手動レビュー） |
| E3-T3 | `POST /v1/agents`：メール不要の自己登録。レート制限、PoW またはターンスタイル、無料クレジット付与、`ref` 属性 | `apps/api/src/agents.ts` (new) | 1 IP あたりの登録上限が効く。登録直後の `discover` が 200 |
| E3-T4 | Ed25519 の既存トークン（ShogunAI ライセンス）を `meterSubject` として併存させ、Mac アプリからの sync に使う | `apps/api/src/auth.ts` | 既存トークンでの `/v1/review` が従来どおり通る |
| E3-T5 | 鍵 → principal → ファイルの解決をキャッシュし p95 を測る | `shogun-core/src/tenancy/` | 解決 p95 < 5ms（memory-bench に項目追加） |

### E4. Streamable HTTP MCP（2026-07-28）と core 21 の凍結

| T | 内容 | 触る場所 | 受け入れ条件 |
|---|---|---|---|
| E4-T1 | Streamable HTTP トランスポート：ステートレス、`Mcp-Method`/`Mcp-Name` 必須、`ttlMs`/`cacheScope` を list/read に付与 | `crates/shogun-mcp/src/mcp.rs`, `server.rs` | MCP 公式 conformance テストが通る |
| E4-T2 | MRTR：L2 を `resultType: "input_required"` で返し、`inputResponses` で再試行。L3 は URL モード elicitation で承認 URL を返す | `shogun-mcp/src/mcp.rs`, `shogun-agents/src/approval.rs` | `actions.execute` で L2 がフォーム確認、L3 が URL を返す（テスト） |
| E4-T3 | core 21 の確定：既存 Core（会議系を除く 19 本：`memory.search`, `memory.get_context`, `memory.get_context_pack`, `memory.get_events`, `memory.read_session`, `memory.get_wrap`, `calendar.upcoming`, `memory.get_periods`, `memory.append_note`, `profile.whoami`, `state.commitments.list`, `state.open_loops.list`, `state.people.list`, `state.projects.list`, `state.people.get`, `state.projects.get`, `wiki.evidence`, `actions.execute`, `actions.status` ＋ 会議系 4 本は `desktop` へ移動）＋ `memory.ingest` ＋ `memory.recall`。wire name を `docs/api/core-tools.md` に凍結 | `shogun-mcp/src/memory_api.rs`（`tool_group`）, `toolsets.rs`, `docs/api/core-tools.md` (new) | `SHOGUN_MCP_TOOLSETS=core` のツール数が 21。名前のスナップショットテスト |
| E4-T4 | ツール説明文をエージェント向けに書き直す（タスク名指し、除外、コスト）。`readOnlyHint`/`destructiveHint`/`openWorldHint` を `ApiLevel` から機械生成 | `shogun-mcp/src/tool_catalog.rs` | アノテーションが `tool_level` と 1 対 1（テスト）。説明文は E11 の選択 eval で評価 |
| E4-T5 | 非推奨ポリシーを文書化し、`deprecated` フィールドをツール定義に追加 | `docs/api/deprecation-policy.md` (new) | レビュー |
| E4-T6 | デスクトップ固有ツール約 100 本を feature `desktop` の下に移す | `shogun-mcp/src/memory_api.rs` | 既定 feature の `Toolsets::all()` が core + import + state + wiki のみ |

### E5. `memory.ingest` と `memory.recall`

| T | 内容 | 触る場所 | 受け入れ条件 |
|---|---|---|---|
| E5-T1 | `memory.ingest(events[] \| turns[], source, actor, thread_key, metadata)`：`event_log` へ追記。dedup（xxhash + Dice ≥ 0.98）、redaction、instruction-shaped ゲートを通す。idempotency key 必須 | `shogun-mcp/src/memory_api.rs`, `shogun-memory/src/event_log.rs`, `sanitize.rs` | 同じ idempotency key の再送が 1 行も増やさない。秘密情報が `event_log` に残らない（pii-bench） |
| E5-T2 | `turns[]` 形式（role/content/tool_calls）をセッション化して Dream Cycle の入力にする | `shogun-core/src/dreamcycle/session.rs` | 会話ログ 1,000 ターンから commitments が抽出される（extract-eval に追加） |
| E5-T3 | `memory.recall(query, budget_tokens, include_scratch?)`：federated search → confidence バンド → 予算内に圧縮。`usage` と `latency_ms` を返す | `shogun-fusion/src/{assemble,budget,compress}.rs`, `shogun-mcp` | 返却トークン数が `budget_tokens` 以下（テスト）。Medium は `possibly:` 接頭 |
| E5-T4 | 毎時 Dream Cycle をサーバー側スケジューラで principal ごとに回す（新素材が無ければ送らない） | `shogun-core/src/dreamcycle/hourly.rs`, `apps/api` cron | 素材 0 の principal で LLM 呼び出しが 0（テスト） |
| E5-T5 | コネクタ pull（Gmail / Calendar / Slack / GitHub / Linear / Notion）を hosted で動かす。`TokenStore` trait の Postgres 実装 | `shogun-integrations/src/`, `apps/api/sql/tokens.sql` (new) | OAuth トークンの平文が DB に無い（KMS 封印） |

### E6. `/memories` 仮想ファイルシステム

| T | 内容 | 触る場所 | 受け入れ条件 |
|---|---|---|---|
| E6-T1 | `entity_pages` と state tables を `/memories/people/<slug>.md`, `/memories/projects/<slug>.md`, `/memories/commitments.md`, `/memories/open_loops.md`, `/memories/INDEX.md`（200 行 / 25KB 以内）に射影 | `shogun-core/src/memfs/` (new) | `INDEX.md` が常に 25KB 以下（テスト） |
| E6-T2 | Anthropic `memory_20250818` の `view/create/str_replace/insert/delete/rename` を実装。`world` への書き込みは `memory.ingest` に変換し、`agent_scratch` へは直接書く | `shogun-mcp/src/memfs_tool.rs` (new) | Claude Agent SDK の memory tool サンプルがそのまま動く（手動レビュー） |
| E6-T3 | 16K 文字の `view` 切り詰めとページング | 同上 | 16K 超のページが切り詰め印付きで返る |

### E7. エクスポート形式 v1

| T | 内容 | 触る場所 | 受け入れ条件 |
|---|---|---|---|
| E7-T1 | 形式定義：エンティティ、有効区間付き事実、provenance ポインタ（event_id + quote）、confidence、tombstone、namespace、actor。JSON Lines + manifest | `docs/api/export-v1.md` (new), `shogun-memory/src/export/` (new) | スキーマ（JSON Schema）が公開され、`state.export` が検証を通る |
| E7-T2 | `shogun-memory-import/v1` を双方向化（export v1 を import できる） | `shogun-memory/src/memory_import/` | export → import の往復で state 行数と quote が一致（テスト） |
| E7-T3 | 差分エクスポート（`export_cursor`）と削除伝播（tombstone） | `shogun-memory/src/export/` | 削除した事実が次回エクスポートで tombstone として出る |

### E8. ルーター骨格

| T | 内容 | 触る場所 | 受け入れ条件 |
|---|---|---|---|
| E8-T1 | 実装言語の決定：Monid の MIT エンジン（Deno/TS）をフォークして `apps/router` に置くか、Rust で再実装するか。判断基準：コネクタ定義互換性、Workers で動くか、ゼロ保持の担保 | `docs/decisions/0001-router-engine.md` (new) | ADR がレビュー済み |
| E8-T2 | コネクタ定義形式：Monid 互換（provider / endpoint / zod or JSON Schema / usage model / fixtures）。`categories` に `memory`, `state`, `approval` を追加 | `apps/router/connectors/` (new) | Monid の既存コネクタ定義を 10 本取り込んでテストが通る |
| E8-T3 | `discover(task, budget?)`：カタログをタスク適合 × 価格 × 健全性 × レイテンシでランク。`scoreBreakdown` に価格を含めて返す。first-party ラベル | `apps/router/src/discover.ts` (new) | 価格が 10 倍の端点が同点で上位に来ない（テスト） |
| E8-T4 | `inspect(tool_id)`：スキーマ、価格、p50/p95、例、`hints` | `apps/router/src/inspect.ts` (new) | 無料。認証不要 |
| E8-T5 | `run(tool_id, input, idempotency_key)`：L1/L2/L3 判定（`scope.rs` の表）→ 予算確認 → 実行 → 精算 → `usage` 返却。失敗・0 件は $0 | `apps/router/src/run.ts`, `shogun-integrations/src/scope.rs` | 表に無い操作が拒否される（`tests/invariant4.rs` 相当を移植）。失敗時の精算が 0 |
| E8-T6 | Monid hosted カタログのパススルー（Proxy/Direct-use）。価格は Monid 価格 + 5% | `apps/router/src/passthrough/monid.ts` (new) | パススルーの run がゼロ保持（本文ログ無し） |
| E8-T7 | run ログ → `tool_runs`（V94）へ principal ごとに記録。`discover` の個別化は Phase 2 | `shogun-memory/src/tool_runs.rs` (new) | run 1 件につき 1 行。本文は入れない |
| E8-T8 | first-party コネクタ：`shogunagent.state.*`（記憶・状態カテゴリ）、承認を要する書き込みの雛形 1 本 | `apps/router/connectors/shogunagent/` | `discover("what did the user commit to")` で first-party が上位に出る |
| E8-T9 | プロバイダ鍵の transport 注入（エンジン・プロセスに鍵を渡さない） | `apps/router/src/transport.ts` | エンジン側のテストで鍵が `undefined` |

### E9. ウォレット・402・上限

| T | 内容 | 触る場所 | 受け入れ条件 |
|---|---|---|---|
| E9-T1 | ウォレット（micro-USD）。既存の reserve-then-settle 台帳（`usage.ts` / `spend.ts` / `pg-store.ts`）を agent key 主体へ拡張 | `apps/api/src/{usage,spend,pg-store}.ts` | 二重精算が無い（既存テスト拡張） |
| E9-T2 | 402 ペイロード（x402 payment-required 形式互換）：金額、単位、受け付ける手段、承認 URL、`Retry-After` | `apps/api/src/billing/402.ts` (new), `docs/api/402.md` (new) | 残高 0 で全従量呼び出しが 402 と同一形式を返す |
| E9-T3 | キー別・エージェント別ハード上限（日次・月次）。既定は保守的 | `apps/api/src/caps.ts` (new) | 上限超過で 402 / `cap_exceeded` |
| E9-T4 | Stripe Checkout でカード入金。URL elicitation で人に 1 回だけ | `apps/api/src/billing/stripe.ts` | 入金 → 残高反映 → 同じ呼び出しが 200（E2E） |
| E9-T5 | 計量単位：ingest / model-op / run / storage。`Mcp-Method`/`Mcp-Name` から計量し本文を読まない | `apps/api/src/metering.ts` | 読み取り系（`state.*`, `get_context_pack`, `recall`, `search`）の精算が常に 0 |
| E9-T6 | `quote` エンドポイントと全従量呼び出しの `dryRun` | `apps/api/src/quote.ts` (new) | `dryRun` の見積りと実精算の差が 0 |
| E9-T7 | レシートと異議申し立て API | `apps/api/src/receipts.ts` (new) | 呼び出しごとに機械可読レシートが取れる |

### E10. エージェント・コマースの Day 1 成果物

| T | 内容 | 触る場所 | 受け入れ条件 |
|---|---|---|---|
| E10-T1 | `/.well-known/mcp/server-card.json`（ツール一覧とアノテーションを `tool_catalog` から生成） | `apps/api/src/wellknown/` (new) | 生成物とツール表のスナップショットが一致 |
| E10-T2 | MCP Registry の `server.json`（`io.github.torutesu/shogunagent`） | `server.json` (new) | `mcp-publisher validate` が通る |
| E10-T3 | `/.well-known/agent-card.json`（A2A v1.0、署名付き） | `apps/api/src/wellknown/` | 署名検証テスト |
| E10-T4 | `llms.txt` / `llms-full.txt`（docs から生成） | `docs/` ビルド | 生成が CI で再現可能 |
| E10-T5 | `SKILL.md`：トリガー条件・非トリガー例外・バージョン一致・setup/auth・優先順位・コスト警告・ステータス・L2/L3 の意味・番号付きルール | `SKILL.md` (new) | 選択 eval（E11）で Claude Code がタスクから自発的に導入する率を計測 |
| E10-T6 | `openapi.json`（REST 面。`rest_routing.rs` から生成） | `apps/api` | スナップショット一致 |
| E10-T7 | `/.well-known/pricing.json`、`/.well-known/status.json`、`/health`（p50/p95、エラー率、インシデント） | `apps/api` | 価格表が E9 の計量設定と単一ソース |
| E10-T8 | `benchmarks.json`（reader、judge、seed、トークン、コスト、p95） | `crates/memory-bench` 出力 | E12 の出力から生成 |
| E10-T9 | Claude Code プラグイン（`.claude-plugin/marketplace.json`）、Glama / Smithery 掲載（`smithery.yaml`、`/.well-known/mcp-config`） | リポジトリ root | 掲載が確認できる（手動） |
| E10-T10 | `npx shogunagent`：ローカルサイドカーを起こし、見つけた AI アプリ（Claude Code / Codex / Cursor）に MCP 設定を書く（ShogunAI の `client_install` を移植、`read` のみ） | `packages/cli` (new), `shogun-mcp/src/client_install.rs` | 新規 Mac/Linux で 1 コマンドで Claude Code から `profile.whoami` が返る |

### E11. 選択 eval

| T | 内容 | 触る場所 | 受け入れ条件 |
|---|---|---|---|
| E11-T1 | ハーネス：タスク記述セット（50 件、英語）× モデル（Claude、GPT、Gemini、オープン 1 種）× 面（MCP Registry 掲載文、Claude Code プラグイン説明、OpenAI Plugin 掲載文、ルーター `discover`）。候補に Mem0 / supermemory / Zep の公開説明文を並べ、どれを選ぶかを記録 | `packages/selection-eval/` (new)。計画時の `crates/selection-eval/` から変更: モデル API を fetch で叩くオフラインの評価ツールであり、製品の Rust egress 規則（不変条件 9）を eval に持ち込まないため TypeScript パッケージにした（2026-10-25、コーディネーター判断） | 選択率が面 × モデルで出る。CI で週次実行 |
| E11-T2 | 説明文の変更をゲート：選択率が落ちる PR は赤 | `.github/workflows/selection-eval.yml` | ゲートが動く |
| E11-T3 | SKILL.md の自発的導入率（タスクに「記憶」や「約束」が出たときに導入を提案するか） | 同上 | 計測できる |

### E12. 同一ハーネスのベンチマーク公開

| T | 内容 | 触る場所 | 受け入れ条件 |
|---|---|---|---|
| E12-T1 | memory-bench の既知の弱点を閉じる：temporal recall@1（`since_ts`/`to_ts` を検索に通す）、abstention false-evidence（relevance floor） | `crates/memory-bench`, `shogun-memory/src/search/` | temporal recall@1 > 0、false-evidence < 10% |
| E12-T2 | OmniMemEval と Mem0 公開ハーネスのアダプタ。reader / judge を明記、≥ 10 seed、信頼区間、1 回答あたりの文脈トークン、書き込み経路コスト、p95 | `crates/memory-bench/src/harness/` (new) | 結果 JSON が `benchmarks.json` に出る |
| E12-T3 | 「open-loop tracking」ベンチ v0：異種イベントストリーム（メール・チャット・会議ノート）からのコミットメント／オープンループ追跡。公開データセットと採点器 | `crates/loop-bench/` (new), `docs/benchmarks/open-loop-tracking.md` | 他社が再現できる手順 |
| E12-T4 | 公開：記事（監査型）、ハーネスのリポジトリ、`benchmarks.json` | `docs/benchmarks/` | 公開済み |

### E13. OSS リリース

| T | 内容 | 触る場所 | 受け入れ条件 |
|---|---|---|---|
| E13-T1 | 開く範囲／閉じる範囲を `docs/OPEN_SOURCE.md` に明記。hosted sync、遠隔承認、ウォレット、hosted カタログは別リポジトリ（private） | `docs/OPEN_SOURCE.md` (new) | レビュー |
| E13-T2 | Docker イメージとバイナリ配布（Linux x86_64 / arm64、macOS arm64） | `.github/workflows/release.yml` (new) | タグ push で成果物が出る |
| E13-T3 | README の 10 秒 GIF、Show HN 用の再現可能な主張（E12 の結果）、コネクタ貢献ガイド | `README.md`, `CONTRIBUTING.md` (new) | レビュー |
| E13-T4 | テレメトリは opt-in のみ（supermemory の既定 ON を反面教師に） | `shogun-core/src/analytics/` | 既定で送信 0（テスト） |

### E14. ShogunAI との統合

| T | 内容 | 触る場所 | 受け入れ条件 |
|---|---|---|---|
| E14-T1 | Mac アプリの Memory API を ShogunAgent ブランドに統合（名前、ツール表、SKILL.md を共有） | ShogunAI リポジトリ側 | 両リポジトリのツール表スナップショットが一致 |
| E14-T2 | sync：Mac の `event_log` 差分を hosted principal へ、ユーザー保持鍵で暗号化して送る。方向は Mac → hosted のみ | ShogunAI 側 + `apps/api/src/sync.ts` (new) | hosted で本文が復号されない（ユーザー鍵でのみ復号） |
| E14-T3 | ShogunAI Pro 加入者に月 $20 相当のクレジット付与 | `apps/api/src/billing/` | 付与が台帳に出る |

---

## 5. Phase 2：承認・SDK・課金・カタログ拡大

**出口条件**：有料ウォレット利用者が増え続けている。ingest の粗利が原価の 3 倍以上。LLM 原価が売上の 15% 以下。ルーター経由の状態層転換率が改善している。

| E | 内容 | 主な場所 |
|---|---|---|
| E15 | 遠隔 L2/L3 承認：Web（承認 URL）、Slack アプリ、webhook。`ApprovalQueue` を Postgres バックに | `shogun-agents/src/approval.rs`, `apps/api/src/approvals/` |
| E16 | A2A Agent Card の引き渡しに型付き「state of work」オブジェクト（export v1 のサブセット）を載せる | `apps/api/src/wellknown/`, `docs/api/handoff.md` |
| E17 | TS / Python SDK。`withShogunAgent()` ミドルウェア（Vercel AI SDK、OpenAI Agents SDK、Claude Agent SDK、Mastra、LangGraph、CrewAI）。呼び出し前に `recall`、後に `ingest` | `packages/sdk-ts`, `packages/sdk-py` |
| E18 | agent-scratch 名前空間の ACL と監査ログ。Team：principal を跨ぐ読み取り ACL | `shogun-memory`, `apps/api/src/acl.ts` |
| E19 | Stripe 従量課金の本番化、年間コミット割引、レシートの月次集計 | `apps/api/src/billing/` |
| E20 | per-user 個別化 `discover`：`tool_runs` から tool × task × outcome を学習し、principal ごとにランキングを補正。補正は説明可能（`scoreBreakdown.personal`） | `apps/router/src/discover.ts`, `shogun-memory/src/tool_runs.rs` |
| E21 | プロバイダのセルフサーブ掲載（コネクタ PR → 自動テスト → 掲載）。プロバイダ向け `provider.json` と支払い先 | `apps/router`, `apps/api/src/providers.ts` |
| E22 | first-party コネクタの拡大（承認を要する書き込み系を L2/L3 で） | `apps/router/connectors/` |
| E23 | Anthropic directory、OpenAI Plugin directory への提出（`readOnlyHint`/`destructiveHint`/`openWorldHint`、プライバシーポリシー、デモアカウント） | `docs/directories/` |
| E24 | ローカル埋め込みの再導入（fastembed-rs、Qwen3-Embedding-0.6B / nomic）。`Embedder` trait と `event_vec` / `search_document_vec` を復活。ベンチマーク全腕で埋め込みを固定 | `shogun-memory/src/{embed,vector}.rs` |
| E25 | `shogun-microphone` を `shogun-mcp` の依存から外す。`desktop` feature の縮小 | `crates/shogun-mcp/Cargo.toml` |

---

## 6. Phase 3：Enterprise・標準・決済

**出口条件**：Enterprise 契約。NRR > 110%。同一ハーネスで multi-session / temporal カテゴリ上位 3。カタログの自前コネクタ比率が上がっている。

| E | 内容 | 主な場所 |
|---|---|---|
| E26 | BYOC / オンプレ・サイドカー（Helm chart、顧客 KMS）、国内リージョン選択、SSO/SCIM、監査エクスポート、ZDR 契約文書 | `deploy/`, `apps/api` |
| E27 | W3C AI Agent Memory Interoperability CG 参加。export v2（CG の合意形式に合わせる） | `docs/api/export-v2.md` |
| E28 | open-loop tracking ベンチ v1 の外部公開と採点受付 | `crates/loop-bench/` |
| E29 | プロバイダ支払い（Stripe Connect）とカタログの一般公開。実装済み（2026-10-25）: ルーターの run 精算シーム（`apps/router/src/settlement.ts` → `POST /v1/internal/router/settlements`、run_id で冪等、プロバイダ別・日次の accrual、5% の取り分は `RUN_MARKUP_BPS`）、Connect Express のオンボーディング、月次支払い（プロバイダ×月で冪等、最低額、紛争中は保留）、`GET /v1/providers/me/statements`、公開カタログ `GET /v1/catalog`・`/catalog`（`docs/api/payouts.md`, `docs/api/catalog.md`）。追加 SQL: `apps/api/sql/provider-payouts.sql` | `apps/api/src/payouts.ts`, `apps/router/src/open-catalog.ts` |
| E30 | x402 受け入れの判断（実商取引量、精算コスト、法務）。判断済み（2026-10-25）: 本番では受け入れない。実装は `X402_ENABLED`（既定 off）の裏に置く。402 の `accepts` に `x402` を足し、`X-PAYMENT` をファシリテータで検証、再利用は拒否、レシートは `funding: "x402"`（`docs/decisions/0002-x402.md`, `docs/api/402.md`） | `docs/decisions/0002-x402.md`, `apps/api/src/billing/x402.ts` |
| E31 | Monid への per-call 掲載（記憶カテゴリができた場合）。準備のみ（2026-10-25、未提出）: Monid 形式の provider / endpoint 定義、提出手順、未決事項（記憶カテゴリが無い、エンドユーザーごとの資格情報、run 本文の保存）、チェックリスト | `integrations/monid/` |

---

## 6A. Phase 4：本番ローンチの準備

**出口条件**：hosted `/mcp` が 503 を返さない。ルーターの run が apps/api のウォレットで一度だけ課金される。精算が失われない。MCP 適合スイートと実フレームワークの SDK 結合テストが CI で緑。負荷試験の結果と脅威モデルが記録されている。公開（npm / PyPI、本番デプロイ、ディレクトリ提出）は手順と workflow まで用意し、実行はオーナーの確認後に行う。

| E | 内容 | 主な場所 |
|---|---|---|
| E32 | ルーターのウォレットを apps/api に配線する。run は apps/api の reserve → settle を一度だけ通し、ルーター側で別に上乗せしない（Monid 経由の二重マークアップを解消）。取り分は `RUN_MARKUP_BPS` の1か所。実装済み（2026-10-25）: `ApiWalletClient`（`apps/router/src/wallet.ts`）→ `POST /v1/internal/router/reserve`・`/settlements`（`apps/api/src/router-wallet.ts`、呼び出し元の鍵を `X-Caller-Authorization` で転送、領収書 `rcpt_run_<run_id>` で冪等）。ルーターは提供者価格だけを送り、取り分は apps/api が一度だけ足す（first-party は無し）。値は `packages/config/pricing.json` の1か所。Monid 取り込みは Monid の価格そのまま（1300 µ$ → 呼び出し元 1365 µ$、以前は 1434 µ$ になり得た）、精算に `marketplace: "monid"` を付ける。精算形式は `/2`（`receipt_id`・`marketplace` を追加、`/1` も受理） | `apps/router/src/wallet.ts`, `apps/api/src/router-wallet.ts` |
| E33 | 精算の永続アウトボックス。ルーターの精算は Postgres のアウトボックスに書いてから送り、再送は run_id で冪等。届かなかった精算は再送ジョブが拾う。実装済み（2026-10-25）: `router_settlement_outbox`（`apps/router/sql/settlement-outbox.sql`、Hyperdrive）、資金の保留前に $0 の下書き、送信前に最終精算を書く。cron（毎分）が 30 秒〜1 時間の指数バックオフで再送し、10 分放置された下書きは $0 で保留を解放。不正な行は `dead` で保持。テスト用の in-memory 版あり（`apps/router/src/outbox.ts`） | `apps/router/src/outbox.ts`, `apps/router/sql/` |
| E34 | hosted `/mcp` を本物にする。`SHOGUN_SIDECAR_URL` があるとき apps/api は principal ごとにサイドカーの Streamable HTTP へ本文を保存せず中継する（不変条件 1・2）。`memory.ingest` / `append_note` のアノテーション不一致を直す（新名と非推奨エントリ。不変条件 11）。gateway 側は実装済み（2026-10-25）: `apps/api/src/mcp-proxy.ts` が鍵の認証・スコープ・予約のあと、本文をストリームのまま principal のサイドカー `POST /mcp` へ中継する（要求・応答ヘッダは許可リストのみ、エージェントの鍵はサイドカーに渡さず principal のサイドカー bearer に差し替える。`SHOGUN_SIDECAR_PRINCIPAL_TOKENS` か `SHOGUN_SIDECAR_BEARER_SECRET` の導出 bearer `sgw1.<principal>.<key>.<mac>`、`SHOGUN_SHARD` で別レプリカの principal を `421 wrong_shard`）。未設定なら従来どおり `503 upstream_unavailable`。サイドカーへの経路は差し替え可能（`SHOGUN_SIDECAR_URL` への fetch か、Worker では `SHOGUN_SIDECAR_BINDING` のサービス / Workers VPC バインディング。サイドカーは 127.0.0.1 のみなので、ローンチ時の推奨は pod 内の Node gateway に `/mcp` を回す案。選択肢は `docs/deploy/production.md` §1）。未認証は他のヘッダより先に `401` + `WWW-Authenticate`（resource_metadata 付き）。`Mcp-Method` / `Mcp-Name` の検査はサイドカーに任せる（拒否は 0 で精算）。適合スイートを gateway 経由でローカルのサイドカーに通し、`actions.execute`（run 単位）以外は合格。課金の不具合（適合スイートで発見）を修正: 精算は要求の `Mcp-Quantity` ではなくサイドカー応答の `Mcp-Quantity`（上限は予約数）から行い、ヘッダが無い・不正・非 2xx は 0。HTTP 200 で返る JSON-RPC エラー・拒否・冪等な再送は 0（契約は `docs/api/billing.md`「Hosted `/mcp`」）。core 側も実装済み（2026-10-25）: サイドカーはすべての `tools/call` の応答に `Mcp-Quantity` を付ける（ingest は `usage.quantity`、書いた `append_note` と実行した model_op は 1、JSON-RPC エラー・`isError`・L2/L3 の拒否・冪等な再送・読み取りは 0。`mcp_http.rs` `settled_quantity`）。`sgw1` 導出 bearer を定数時間の HMAC で検証する `DerivedBearers`（`tenancy/derived_bearer.rs`、actor は `agent:<key_id>`、gateway と共通のテストベクタ）を `--multi` がトークン一覧と併せて受け付ける（`SHOGUN_SIDECAR_BEARER_SECRET` があれば `SHOGUN_PRINCIPAL_TOKENS` は不要、R4）。サーバービルドはポートが使用中なら別ポートへ逃げず起動しない（`bind_exact`、R3。`desktop` は従来どおり）。未認証の `/mcp` は HTTP 401 + `WWW-Authenticate: Bearer`（本文は JSON-RPC エラーのまま）。拒否は `next_action`（JSON-RPC の `error.data` と `isError` の本文）を、state の読み取りと `profile.whoami` は `usage`（0 円）・`latency_ms` を返す（適合スイートの警告 0）。アノテーション不一致は名前を変えずに解消: core の `tools/list` は 4 つのヒントをすべて真偽値で出しており、食い違いは L1 の `idempotentHint`（カード true、凍結済みの core false）だけだったので、派生物であるカードを core の規則に合わせた（不変条件 11・12。カードと wire-freeze スナップショットの一致をテスト）。`actions.execute` は hosted `/mcp` で無料（L3 の予約と承認は不変条件 10 で無料、送信は実行される場所で課金。`FREE_RUN_TOOLS`、`docs/api/billing.md`）。compose / helm / deploy の表に `SHOGUN_SIDECAR_BEARER_SECRET` を追加。三面の揃え（不変条件 4）も実装済み（2026-10-25）: 包みの値は `crates/shogun-mcp/src/envelope.rs` の1つの表から出し、MCP（JSON-RPC の `error.data` と `isError` の本文、`/mcp` の転送層の拒否と multi の未知の bearer も含む）・REST（4xx/5xx の本文の最上位、state の読み取りと `profile.whoami` の 200 の本文に `usage` / `latency_ms` / `next_action: null`）・CLI（REST の本文をそのまま出す）が同じ `next_action` と同じ包みを返す。`crates/shogun-core/tests/envelope_parity.rs` が同じツールを三面で呼んで包みを比べる（読み取り、`not_found`、未知の bearer、read トークンの書き込み、引数の誤り、プランのロック）。`memory.search` / `memory.get_events` は面をまたいでバイト単位で比べるため包みを付けない（E34 のまま）。値の一覧は `docs/api/envelope.md`。wire-freeze のスナップショットは不変。残り: §1 の経路をオーナーが選ぶ、承認後の送信 1 件ごとの料金を設けるか（オーナーの価格判断） | `apps/api/src/mcp-proxy.ts`, `crates/shogun-mcp` |
| E35 | handoff を三面に出す（不変条件 4）。引き渡しオブジェクトの組み立てを Rust コアへ移し（`handoff.create` / `handoff.accept`）、apps/api はサイドカーへの中継だけにする。これで apps/api が recall 本文を解析する不変条件 1 の例外が要らなくなる。実装済み（2026-10-25）: `handoff.create`（読み取り・無料）/ `handoff.accept`（L1、受け手自身の `agent_scratch` へ ingest 経路で書き、既定の冪等キーは `handoff:<id>:<actor>`、ingest として件数課金）を MCP・REST（`POST /v1/handoff`、`/v1/handoff/accept`）・CLI（`shogun handoff create|accept`）に同じ水準で出した（`crates/shogun-mcp/src/handoff_api.rs`）。principal をまたぐのは REST の `?principal=` で Team の許可があるときの create だけ（accept は書き込みなので拒否）。`apps/api/src/handoff.ts` は本文を読まずにサイドカーの `/v1/handoff` へ中継する（`mcp-proxy.ts` が要求のパスで行き先を選ぶ）。SDK の `acceptHandoff` / `accept_handoff` は `handoff.accept` を呼ぶ。脅威モデル R5 は解消。サイドカーの REST 面も `sgw1` 導出 bearer を受け付ける（E34 で REST・MCP 共通のトークン登録に検証を入れた。`crates/shogun-core/tests/derived_bearer.rs`）ので、hosted の `/v1/handoff` は導出 bearer でも動く | `crates/shogun-mcp`, `crates/shogun-cli`, `apps/api/src/handoff.ts` |
| E36 | MCP 適合スイート（2026-07-28 仕様の初期化・ツール一覧・MRTR・エラー形）と、実フレームワーク（Vercel AI SDK、OpenAI Agents SDK、Claude Agent SDK、LangGraph）での SDK 結合テスト。モデルはモックで、CI で外部に出ない。実装済み（2026-10-25）: `packages/conformance`（使い捨てサイドカーまたは `SHOGUN_CONFORMANCE_URL` の任意の URL に対して 34 項目。CI の `conformance` ジョブ）、SDK の実フレームワーク結合テスト（TS: Vercel AI SDK・OpenAI Agents SDK・Claude Agent SDK・LangGraph.js、Py: LangGraph・OpenAI Agents SDK。ネットワークは遮断）。見つかって直したもの: `memory.ingest` の `Mcp-Quantity` が拒否と再送でも要求の件数を返していた、`initialize` が 2026-07-28 を要求されても 2025-06-18 を答えていた、Python の LangGraph アダプタが `thread_id` を受け取れていなかった、TS の AI SDK ミドルウェアの型が `wrapLanguageModel` に渡せなかった。残る差分は警告として報告（未認証が HTTP 200、拒否に `next_action` が無い、state 読み取りに `usage` / `latency_ms` が無い、L2 がサーバー版に無く注釈で L1 と区別できない） | `packages/conformance`, `packages/sdk-*/test/integration` |
| E37 | 可観測性と負荷試験。メトリクス・トレースは本文を持たない（`Mcp-Method` / `Mcp-Name`、ステータス、レイテンシのみ）。gateway とサイドカーの負荷試験スクリプトと SLO 表 | `apps/api/src/observability.ts`, `bench/load/`, `docs/slo.md` |
| E38 | セキュリティレビュー。脅威モデル、パーサ（JSON-RPC、SAML、export の取り込み）のファズ、`SECURITY.md` と報告窓口、依存とシークレットの走査 | `docs/security/threat-model.md`, `fuzz/`, `SECURITY.md` |
| E39 | Phase 3 C の積み残し。サイドカーの `DELETE /v1/principals/<id>`（SCIM 削除の猶予後に実ファイルを消す）、SAML の未対応部分、メンバー鍵の発行を冪等にする。実装済み（2026-10-25）: サイドカーの `DELETE /v1/principals/<id>`（運営者の admin bearer のみ・multi のみ。ハンドルを閉じ、ストアのロックを保ったまま KMS で包んだ鍵ファイルを先に消し、DB と -wal/-shm、ディレクトリを消す。使用中は `409 principal_busy`、何も残っていなければ `200 purged:false` で冪等。Team から外し、`principal-purges.jsonl` に本文を含まない1行を追記）。apps/api の SCIM スイープは既存の管理クライアントでこれを呼ぶ。SAML: Version 2.0 のみ、Response の `Issuer` は IdP、`IssueInstant` は未来でもサインイン10分より古くもない、assertion の `Issuer` / `Subject` / `Conditions` は1つ、`NotBefore` が先の bearer 確認は不可、`EncryptedID` / `EncryptedAttribute` も拒否（XML 暗号化は Workers で検証済みのライブラリが無いので未対応と明記）。メンバー鍵: `Idempotency-Key` があれば鍵 ID を (tenant, member, key) から導き、再試行は同じ `key_id` を `key: null` で返す（秘密は二度出さない。同時実行も1本だけ発行、`insertKeyOnce`）、スコープ違いは `409 idempotency_key_reused`。コネクタ資格情報と墓標も実装済み（2026-10-25）: purge はストアのあとに principal のトークンストアの項目（`SHOGUN_TOKEN_STORE_DIR` か `<root>/.tokens/<id>/` の全スロットと一時ファイル。鍵が環境変数由来でも KMS で包んだものでも同じ）も消す（`TokenStore::purge_principal`。ファイル版はディレクトリごと、KMS 行版は全スロットの行、既定は全スロットの delete。ストアのモジュールは integrations を知らず、`MultiBackends::with_credentials` に `shogun-api --multi` が渡す）。応答と purge ログに `credentials_removed`。資格情報が消せなければ `503 credentials_unavailable` で、再試行で終わる。消す前に `<root>/.purged/<id>` に墓標を書き（ロック中・fsync）、以後その principal を名指すトークン（一覧のトークンも `sgw1` も）は `410 principal_purged`（`next_action: ask_human`、`/mcp` でも同じ）で、空のストア・鍵・ディレクトリを作らない。`shogun principal import` も拒否。再作成は運営者の意図的な操作だけ: `shogun-api --reinstate-principal <id> --multi <root>`（墓標を消すだけ。次の要求で新しい鍵の空のストア）。手順は `deploy/README.md` §6.6 | `crates/shogun-core/src/server`, `apps/api/src/enterprise/` |
| E40 | リリース工学（準備のみ）。npm / PyPI の公開 workflow は手動実行と `environment: release` の承認の裏に置き、版の付け方と変更履歴を決める。実際の公開はオーナーの確認後。準備済み（2026-10-25、未公開）: `publish-npm.yml` / `publish-pypi.yml`（手動実行のみ・main のみ・既定は dry run・`environment: release`・trusted publishing でトークンを置かない）、CI の `release-dry-run` ジョブ（`npm publish --dry-run`、`python -m build` + `twine check`、workflow の YAML 検査、版の一致検査）、各パッケージの `CHANGELOG.md` | `.github/workflows/publish-*.yml`, `docs/release.md` |
| E41 | 本番デプロイの手順書とステージング設定（準備のみ）。Workers・Postgres・サイドカーの順序、ロールバック、シークレットの一覧（値は書かない）。実デプロイはオーナーの確認後。準備済み（2026-10-25、未デプロイ）: `docs/deploy/production.md`（構成、初回と更新の順序、SQL 19 ファイルの適用順、Worker・ルーター・サイドカーの環境変数とシークレットの一覧、ルーターの Hyperdrive バインディング、スモークと適合スイート、構成要素ごとのロールバック、未決のオーナー判断を含むローンチ前チェックリスト）。wrangler に env ブロックが無いため、ステージングは別名の Worker をコピーした設定から出すコマンドで記述。`scripts/check-env-docs.mjs`（CI の invariant guards ジョブ）が apps/api・apps/router の読む環境変数が手順書に無いと落とす | `docs/deploy/production.md`, `deploy/` |
| E42 | ディレクトリ提出の前提を片付ける。OpenAI のクレジット販売の規定への対応案、提出物の差分チェック。提出そのものはオーナーの確認後。準備済み（2026-10-25、未提出）: `docs/directories/openai-credit-rule.md`（規定の記録、3 案と推奨: ChatGPT クライアントには無料の読み取りツールだけを出し、課金はリンクで外へ）、`docs/directories/submission-checklist.md`（ディレクトリごとの必要物と現状の差分、ブロッカー: hosted `/mcp` の 503、ホスト名の不一致、注釈、法務文書の DRAFT、法人名と連絡先、デモアカウント、ロゴ、npm 未公開） | `docs/directories/` |
| E48 | 本番前の堅牢化（E32〜E42 の残り）。ルーター: run の冪等ストアを Postgres に（isolate を跨ぐ再試行で二重課金しない）、ウォレット停止時の `Retry-After` を返す、Hyperdrive 未設定時の起動ログ。ゲートウェイ: レート・同時実行の制限を Postgres 停止時に閉じる側へ（R1）、RLS の無い 4 つの SQL に permissions ファイル、`live-store-access.sql` を再実行可能に、手動実行のみの deploy workflow。サイドカー: 7464 が使えないときに別ポートへ黙って逃げない（R3）、`SHOGUN_PRINCIPAL_TOKENS` を平文 env から外す（R4）、1 principal への書き込みの直列化（R7）。CI: シークレット走査（R8）、nightly の fuzz（R9）。ルーターのコネクタ資格情報は呼び出し元ごとのトークンを引く（Worker secret を全員で共有しない）。ゲートウェイ側の残りを実装（2026-10-25）: `POST /v1/internal/router/connector-token`（`apps/api/src/connector-tokens.ts`、契約は `docs/api/connector-tokens.md`）。ルーターのサービストークンを定数時間で照合してから本文を読み、principal は `X-Caller-Authorization` の鍵から引き（本文に principal は無い）、`run` スコープと tenant の admit を確かめ、`connector_tokens` の行を Rust の `KmsSealer` と同じ形式（KMS で包んだデータ鍵 + ChaCha20-Poly1305、(principal, slot) を context と AAD に束縛）で開き、`{access_token, token_type, expires_at}` だけを `Cache-Control: no-store` で返す（トークン・鍵・本文はログに出ない）。60 秒以内に切れるトークンは `ConnectorRefresher` で更新して封印し直して書き戻す。ルーターの実クライアントをプロセス内で通すテストあり。受領書に精算数量を記録（`settled_quantity`、`GET /v1/receipts` は `reserved_quantity` と `settled_quantity` を返す。`apps/api/sql/agent-receipts-settled-quantity.sql`、手順書の SQL 順 8 番）。compose / helm のゲートウェイに E34・E37・E48 の変数（`SHOGUN_SIDECAR_*`、`SHOGUN_SHARD`（helm は pod index から）、`OTEL_*`、`ROUTER_SETTLEMENT_TOKEN`）を配線。手動のみの `.github/workflows/deploy.yml`（`workflow_dispatch` のみ、component と environment を選び、environment の承認待ち、既定は `--dry-run`、確認句で本番デプロイ。`check-workflow-yaml.py` が検査。未実行）。残り: どのランタイムも KMS の `KeyWrapper` を渡していない（AWS/GCP/Vault のクライアントは Rust コアのみ）ので本番は全員 `404 connector_not_connected`、`connector_tokens` を書く hosted の接続フローが無い、OAuth broker の refresh 経路が無い（更新トークンを回転するプロバイダには行ロックが要る）。それまで一次コネクタは hosted カタログに載せない | `apps/router`, `apps/api/sql`, `apps/api/src/connector-tokens.ts`, `crates/shogun-core/src/bin`, `.github/workflows` |
| E49 | open-loop 追跡の品質（E28 の計測結果から）。解決・期限変更・取り下げを状態に反映する（現状 0.000）、言い換えの約束の再現率（hard で 0.43）、ローカル規則の候補が actionable（≥ 0.5）に届く経路（Batch レビュー込み）を loop-bench の全腕で測る 。実装済み（2026-10-25、V104 `loop_updates`）: 同じスレッドの後続メッセージから完了・取り下げ・期限変更を拾い、ローカル候補（0.5 未満・未レビュー・ユーザー未編集）だけをその場で閉じる／期限を動かす（confidence は上げない）。consolidator には「後のメッセージで気づいた結果」を渡す。言い換えの約束と、仮定・撤回の除外を追加。loop-bench（5 seed）: ローカル規則の tracking F1 は easy 0.842→1.000、medium 0.859→1.000、hard 0.540→1.000（hard の約束再現率 0.433→1.000、解決 0.940、期限変更 0.900、誤クローズ 0）。規則は生成器の文面を見て書いたので、これは上限であって汎化の証拠ではない。オフラインの scripted reviewer 腕で actionable（≥ 0.5）に届く経路を確認。残り: 未見テンプレートの評価セット、実モデル腕、スレッドの無いテキスト（画面）の扱い、consolidator の指示文の更新、本番での `consolidate_loops` の呼び出し | `shogun-memory`, `crates/loop-bench` |

## 6B. Phase 5：拡張

**出口条件**：非英語の recall が英語と同じ水準。Mac 以外の取得元から state が育つ。first-party コネクタで主要な仕事の道具を覆う。エージェントの支払いが L3 と上限の中で完結する。

| E | 内容 | 主な場所 |
|---|---|---|
| E43 | 多言語埋め込み。モデルごとの閾値（Phase 3 A の per-model cutoff）に多言語モデルを足し、ベンチの全腕で固定する | `shogun-memory/src/embed`, `crates/memory-bench` |
| E44 | Mac 以外の取得元。コネクタの webhook（Gmail、Calendar、Slack、GitHub、Linear、Notion）から `memory.ingest` へ流す取り込み口と、ブラウザ拡張の取り込み仕様。画像・音声は保存しない。実装済み（2026-10-25）: ゲートウェイの `/v1/ingest-hooks`（登録・一覧・ローテーション〔旧秘密は 24 時間有効〕・削除、`write` スコープ、`Idempotency-Key` で冪等）と、プロバイダごとの受信口。署名は各社の方式を定数時間で検証する（GitHub `X-Hub-Signature-256`、Linear `Linear-Signature`、Slack v0 署名＋時刻 ±5 分、Notion `X-Notion-Signature`、Calendar のチャネル ID とトークン、Gmail は Pub/Sub push の Google OIDC トークン〔RS256・aud・サービスアカウント〕）。秘密は `INGEST_HOOKS_SECRET` から導出（GitHub・Calendar、世代番号だけを保存）か AES-256-GCM で封印（Linear・Slack・Notion、フック ID に束縛）、Gmail の宛先は HMAC だけ（不変条件 7、`apps/api/sql/ingest-hooks.sql`）。検証済みの本文は base64 のまま既存のサイドカー中継で新ツール `memory.ingest_webhook`（Rust コア、MCP・REST `POST /v1/memory/ingest-webhook`・CLI `shogun ingest-webhook` の三面、L1・ingest 課金）に渡し、apps/api は本文を保存・記録・返送しない（不変条件 1・2）。解析の例外は Slack の `url_verification`、Notion の検証トークン、Gmail の Pub/Sub 封筒の 3 つだけで、何も残らないことをテストで確かめる。冪等キーは `hook:<provider>:<配信 ID>`。コアは GitHub（push・issue・コメント・PR・レビュー・リリース）、Linear（Issue・Comment・Project・ProjectUpdate）、Slack（message・app_mention）を出典（プロバイダ・配信 ID・URL）付きのテキストにし、ID しか来ない Notion・Calendar・Gmail は「変更」通知として 0 件で確定する（内容の取得はコアのコネクタ同期、E50 の資格情報が要る）。課金は保存件数の ingest、ハンドシェイクは無料、不正署名は無料で IP とフックごとに 10 分 30 回まで（超えると 429）。コアの再試行可能な失敗は新ヘッダ `Mcp-Error-Code` で見分けて 503 を返す。ブラウザ拡張は仕様のみ（`docs/api/ingest-sources.md` §3: 見えている本文・URL・タイトルだけ、スクリーンショット無し、パスワードマネージャー・プライベートウィンドウ等を除外、`write` のみの鍵、レート上限） | `apps/api/src/ingest-hooks/`, `crates/shogun-mcp/src/webhook_ingest.rs`, `docs/api/ingest-sources.md` |
| E45 | first-party コネクタの拡大。書き込み系は L2/L3、スコアは第三者と同じ式（不変条件 13） | `apps/router/connectors/` |
| E46 | エージェントの支払い。支払いの委任（上限・期限・用途）を型で L3 にし、承認台帳と結びつける。実装済み（2026-10-25、`docs/api/payments.md`）: `SendAction::Pay` / `GrantPaymentMandate`（どちらも型で L3）と境界の判定（`shogun_agents::payment`、拡大の判定は `MandateTerms::widens` の 1 か所）。委任の付与・拡大・延長は承認キューの L3（承認 URL、人の decide だけが解放し、ストアは V103 台帳に `approve` が無ければ書かない）、縮小と取り消しは直接（L1、全面）。委任内の支払いは付与の決定を L3 の事前承認として rail へ（`send_exec` 経由で traceability を先に書く）、範囲外は支払いごとの L3 承認へ戻る。支払いは rail を呼ぶ前に principal のストアへ actor・冪等キー・付与の承認 id と決定者つきで記録（V105、`payment_spends`。V104 は同時期の open-loop が使用）。4 ツール `payments.mandates.list|request|revoke` / `payments.spend` を MCP・REST（`/v1/payments/...`）・CLI（`shogun payments ...`）に同水準で出した（非コア、toolset `payments`、wire-freeze は不変）。rail は stub のみ（`SHOGUN_PAYMENT_RAIL=stub`、お金は動かない。実 rail に要るものは同文書）。課金は `apps/api/src/pricing.ts` だけを変えた: list は無料の read、他 3 つは `run` で呼び出し時 $0（stub の rail 手数料 0）。`apps/api/src/billing/mandates.ts` は作らなかった（ゲートウェイは本文を読まず中継するだけで、委任の状態はコアのもの） | `shogun-agents`, `shogun-mcp/src/payments_api.rs`, `shogun-core/src/payment_rail.rs`, `apps/api/src/pricing.ts` |
| E47 | export 形式の標準化。export v2 を CG 向けの草案に整える（提出はオーナーの確認後）。草案を書いた（2026-10-25、未提出）: `docs/standards/export-v2-cg-draft.md`（要約・適合性の用語・データモデル・未決事項・他形式との対応表（コミュニティ形式の欄は未検証と明記）・テストスイート・IPR/ライセンスはオーナー記入欄）。JSON-LD の `@context`（`docs/api/export-v2.context.jsonld`）と、v2 パッケージがその下で展開できることのテストを `packages/interop` に追加 | `docs/api/export-v2.md`, `docs/standards/`, `packages/interop` |
| E50 | ホスト型コネクタの資格情報。サイドカーが principal のコネクタ行を開いて期限切れなら更新する内部エンドポイント（KMS とプロバイダへの送信は Rust コアの単一 egress のまま）、ゲートウェイの `connector-token` ルートはそれを呼ぶ。ホスト型の接続フロー（OAuth 開始・コールバック、トークンはサイドカーの principal ストアにだけ保存）、行ごとの更新ロック。実装（2026-10-25）: サイドカー（`shogun-api --multi`）に `POST /v1/internal/connectors/{token,connect/start,connect/finish}`（`crates/shogun-mcp/src/connector_vault.rs`）。呼べるのは管理資格情報（ゲートウェイの `SHOGUN_SIDECAR_ADMIN_TOKEN`）と principal 自身のサイドカー bearer（`X-Shogun-Principal-Authorization`、派生 `sgw1` か列挙トークン）の両方を出したときだけで、principal は bearer から決まり本文には無い。エージェントの bearer だけなら `403 admin_required`。応答はすべて `no-store`。保管庫は `shogun_integrations::hosted_connect`: principal の既存のトークンストア（principal の KMS 由来の鍵で封印したファイル、承認実行と同じストア）を開き、60 秒以内に切れるなら core の `HttpTokenExchange`（単一 egress）で更新して書き戻し、`{access_token, token_type, expires_at}` だけを返す。(principal, slot) ごとのロックの中でストアを読み直すので、回転する更新トークンを同時の run が二重に使わない（並行テストあり）。ベンダーが拒否した更新は `404`、届かない更新は有効な間は旧トークン・切れたら `503`、更新失敗で grant を消さない。接続フロー: core が既に話せる認可コード + PKCE（Google の Gmail・Calendar、Slack の user token、Linear の user actor）を Web コールバック（`SHOGUN_CONNECT_REDIRECT_URI`）で。verifier はサイドカーのメモリに 10 分、ゲートウェイの不透明な state のハッシュで引き、finish は一度きりで開始した principal に束縛。コードはサイドカーで交換し tokenset は principal のストアへ直接書く（ゲートウェイに返さない）。Google は全スコープの付与と refresh token を要求。GitHub は core が device flow でしか接続しないので token のみ（接続は未対応）。OAuth クライアントは `SHOGUN_CONNECT_*`（`deploy/README.md` §3.3）。ゲートウェイ: `connector-token` ルートの既定の裏側をサイドカー（`sidecarConnectorVault`、`/mcp` 中継と同じ transport・binding・bearer）にし、状態は `docs/api/connector-tokens.md` どおりに写す（サイドカーがゲートウェイの資格情報を拒否したら `401` ではなく `503`、purge 済みは `403`、別シャードは `503`）。E48 の `KeyWrapper` / `ConnectorRefresher` / TypeScript の `KmsSealer` / `connector_tokens` 表（`apps/api/sql/connector-tokens*.sql`）は削除（どのランタイムも埋められず、principal データの所有を二つに割るため。不変条件 1・9）。人が接続する: `POST /v1/connectors/{provider}/connect`（`run` スコープ）はリンクだけを返す（`202`、`next_action: ask_human`。エージェントは自分で接続できない、L3 扱い）→ 人のページ（CSRF クッキー付きの Continue）→ ベンダー → `GET /v1/connectors/callback`（Continue したブラウザのフロークッキーが必要）。リンクと state は `CONNECTOR_CONNECT_SECRET` 由来の鍵で AES-256-GCM 封印、ゲートウェイは何も保存せずコードもログに出さない（`apps/api/src/connector-connect.ts`）。テスト: `hosted_connect` の単体、`crates/shogun-core/tests/connector_vault.rs`（実サーバー、principal 間の漏れ無し）、`apps/api/test/connector-tokens.test.ts`（ルーターの実クライアントを含む）、`connector-connect.test.ts`、workerd。手順書 §9・脅威モデル (10)・R10 を更新。残り: ベンダーの OAuth アプリ（Google Web クライアントと `gmail.modify` の検証・セキュリティ評価、Slack、Linear）とコールバック登録、Worker → サイドカーの経路と NetworkPolicy のトークンエンドポイントへの egress、ステージングでの接続・更新・取り消しの確認、GitHub の hosted フロー、切断と状態の読み取り（三面）、接続・更新の監査行（R10）。それまで一次コネクタは hosted カタログに載せない | `crates/shogun-mcp/src/connector_vault.rs`, `crates/shogun-integrations/src/hosted_connect.rs`, `crates/shogun-core/src/bin/shogun_api.rs`, `apps/api/src/connector-tokens.ts`, `apps/api/src/connector-connect.ts` |

---

## 7. API 仕様の骨子

### 7.1 core 21（wire name 凍結）

読み取り（L1、無料）：`memory.search`, `memory.get_context`, `memory.get_context_pack`, `memory.get_events`, `memory.read_session`, `memory.get_wrap`, `memory.get_periods`, `memory.recall`, `calendar.upcoming`, `profile.whoami`, `state.commitments.list`, `state.open_loops.list`, `state.people.list`, `state.projects.list`, `state.people.get`, `state.projects.get`, `wiki.evidence`, `actions.status`
書き込み（L1、従量）：`memory.append_note`, `memory.ingest`
行動（L1/L2/L3 はアクション種別で決まる）：`actions.execute`
会議系 4 本（`meeting.*`）は `desktop` feature へ移し、core から外す。

### 7.2 共通レスポンス

```json
{
  "result": {},
  "usage": { "unit": "events", "quantity": 12, "cost_micro_usd": 6000 },
  "latency_ms": 41,
  "hints": [{ "tool_id": "...", "reason": "cheaper", "estimated_cost_micro_usd": 1200 }],
  "next_action": null
}
```

エラー時は `error.code`、`error.message`、`error.next_action`（`top_up` / `approve_at_url` / `retry_after` / `narrow_query` など）。

### 7.3 402

```json
{
  "error": { "code": "payment_required", "next_action": "top_up" },
  "payment": {
    "amount_micro_usd": 6000,
    "unit": "events",
    "accepts": [{ "rail": "card", "url": "https://.../topup?k=..." }],
    "retry_after": 0
  }
}
```

x402 の `payment-required` ヘッダ形式と互換のフィールド名を使う。`accepts` に後から `x402` を足せる。

### 7.4 ルーター 3 動詞

- `discover(task, budget_micro_usd?, limit?)` → 候補配列（`tool_id`, `score`, `scoreBreakdown{task_fit, price, health, latency, personal}`, `price`, `p50_ms`, `p95_ms`, `first_party`）。無料・認証不要。
- `inspect(tool_id)` → スキーマ、価格、例、`hints`。無料・認証不要。
- `run(tool_id, input, idempotency_key, dryRun?)` → `result` + `usage`。L1/L2/L3 判定を先に返す（L2 は `input_required`、L3 は承認 URL）。

### 7.5 REST

`/v1/state/*`, `/v1/memory/*`, `/v1/actions/*`, `/v1/export`, `/v1/agents`, `/v1/wallet`, `/v1/quote`, `/v1/receipts`, `/v1/router/{discover,inspect,run}`。既存の `rest_routing.rs` のうち desktop 固有（`/v1/capture/*`, `/v1/visual_recall/*`, `/v1/meeting/*`, `/v1/device/*`）は `desktop` feature へ。

---

## 8. テスト・eval・CI ゲート

| ゲート | 内容 | いつ |
|---|---|---|
| `ci.yml` | clippy（deny）、test、feature matrix、ガード 6 種、relay の vitest | 全 PR |
| `migration-safety.yml` | V1〜V91 凍結、全版アップグレード原子性 | 全 PR |
| `tenant-boundary` | principal 跨ぎの読み取りが 0 件（`tests/invariant_tenant.rs`） | 全 PR |
| `wire-freeze` | core 21 の名前・スキーマ・アノテーションのスナップショット | 全 PR |
| `zero-retention` | ログ・DB・メトリクスに本文が無いことのスキャン（`check-log-hygiene.py` 拡張） | 全 PR |
| `selection-eval` | 選択率が落ちない | 説明文・SKILL.md・掲載文を触る PR |
| `memory-bench` | p95、temporal、abstention、dedup、pii | 週次と release |
| `audit.yml` | cargo audit、pnpm audit | 日次 |

---

## 9. 設計上の未決と判断の期限条件

| 未決 | 選択肢 | 判断材料 |
|---|---|---|
| handoff の組み立て場所 | apps/api（不変条件 1 の例外） vs Rust コア | 判断済み（E35）: Rust コアで組み立て、apps/api は中継だけ |
| ルーターエンジンの言語 | Monid フォーク（Deno/TS） vs Rust 再実装 | Workers で動くか、コネクタ定義互換、ゼロ保持の担保。E8-T1 の ADR |
| hosted の DB | principal ごとの SQLite ファイル（R2 + Durable Object か VM 上のファイル） | Durable Object の SQLite 上限（1GB/DO）と SQLCipher の可用性 |
| 埋め込みモデル | Qwen3-Embedding-0.6B vs nomic-embed | 日英の MTEB、サイズ、ライセンス。全腕固定が条件 |
| 承認の既定 | L2 を MRTR フォームにするか、全部 URL にするか | MCP クライアントの MRTR 対応率 |
| x402 | 受け入れる／受け入れない | 判断済み（E30）: 本番では受け入れない（フラグの裏に実装）。再判断の条件は `docs/decisions/0002-x402.md` |

---

## 10. 用語

- **principal**：1 人の人。1 ワールドモデル、1 SQLCipher ファイル。
- **tenant**：principal の集まり（組織）。ACL の単位。
- **agent**：鍵の主体。principal に束縛される。
- **actor**：書き込みの主体（agent または人）。全書き込みに付く。
- **world / agent_scratch**：名前空間。world は principal の仕事の状態、agent_scratch はエージェントの作業メモ。
- **L1 / L2 / L3**：即時 / 一押し確認 / 専用の人間承認。外部送信と決済は型で L3。
- **core 21**：凍結した wire name のツール集合。
