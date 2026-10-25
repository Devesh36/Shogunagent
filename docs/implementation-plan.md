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
| E4-T3 | core 21 の確定：既存 Core（会議系を除く 19 本：`memory.search`, `memory.get_context`, `memory.get_context_pack`, `memory.get_events`, `memory.read_session`, `memory.get_wrap`, `calendar.upcoming`, `memory.get_periods`, `memory.append_note`, `profile.whoami`, `state.commitments.list`, `state.open_loops.list`, `state.people.list`, `state.projects.list`, `state.people.get`, `state.projects.get`, `wiki.evidence`, `actions.execute`, `actions.status` ＋ 会議系 4 本は `desktop` へ移動）＋ `memory.ingest` ＋ `memory.recall`。wire name を `docs/api/core-tools.md` に凍結 | `shogun-mcp/src/memory_api.rs`（`tool_group`）, `toolsets.rs`, `docs/api/core-tools.md` (new) | `SHOGUN_MCP_TOOLSETS=core` のツール数が 22。名前のスナップショットテスト |
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
| E29 | プロバイダ支払い（Stripe Connect）とカタログの一般公開 | `apps/api/src/payouts.ts` |
| E30 | x402 受け入れの判断（実商取引量、精算コスト、法務）。受け入れる場合は 402 ペイロードの `accepts` に追加するだけ | `docs/decisions/` |
| E31 | Monid への per-call 掲載（記憶カテゴリができた場合） | `apps/router/connectors/` |

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
| ルーターエンジンの言語 | Monid フォーク（Deno/TS） vs Rust 再実装 | Workers で動くか、コネクタ定義互換、ゼロ保持の担保。E8-T1 の ADR |
| hosted の DB | principal ごとの SQLite ファイル（R2 + Durable Object か VM 上のファイル） | Durable Object の SQLite 上限（1GB/DO）と SQLCipher の可用性 |
| 埋め込みモデル | Qwen3-Embedding-0.6B vs nomic-embed | 日英の MTEB、サイズ、ライセンス。全腕固定が条件 |
| 承認の既定 | L2 を MRTR フォームにするか、全部 URL にするか | MCP クライアントの MRTR 対応率 |
| x402 | 受け入れる／受け入れない | 実商取引量、精算コスト、法務。Phase 3 |

---

## 10. 用語

- **principal**：1 人の人。1 ワールドモデル、1 SQLCipher ファイル。
- **tenant**：principal の集まり（組織）。ACL の単位。
- **agent**：鍵の主体。principal に束縛される。
- **actor**：書き込みの主体（agent または人）。全書き込みに付く。
- **world / agent_scratch**：名前空間。world は principal の仕事の状態、agent_scratch はエージェントの作業メモ。
- **L1 / L2 / L3**：即時 / 一押し確認 / 専用の人間承認。外部送信と決済は型で L3。
- **core 21**：凍結した wire name のツール集合。
