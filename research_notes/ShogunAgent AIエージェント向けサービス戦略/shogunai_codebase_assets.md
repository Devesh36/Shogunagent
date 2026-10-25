# ShogunAI コードベース資産インベントリ — ShogunAgent（AIエージェント向けメモリ/状態推論サービス）へのピボット観点

- 調査対象: `/home/user/shogunai-`（最終コミット `ce3d2acf` 2026-10-24 "fix(rewrite): read the current conversation before drafting (#1456)"）
- 調査方法: コード・docs の読解のみ（ビルド・実行・Web 調査なし）
- 規模（実測）: `crates/` Rust 合計 **約 330,000 行**（`shogun-core` ≈160k / `shogun-memory` ≈72k / `shogun-mcp` ≈49k / `shogun-cli` 8.5k / `shogun-integrations` 8.5k / `shogun-agents` 5.7k / `shogun-fusion` 4.5k / `shogun-license` 3.7k / `shogun-redact` 2.1k）、`#[test]` **5,719**、マイグレーション **V1〜V91（91本）**、`CREATE TABLE` 計 **114**。`apps/api`（Hono relay）TS ≈6,600 行。`apps/desktop/src-tauri` ≈86k 行（macOS 専用、`#[tauri::command]` 460）。

---

## 1. メモリアーキテクチャ（`crates/shogun-memory`）

### 1.1 全体像
- **データ重心は Rust コア**（CLAUDE.md 不変条件1）。DB は **SQLite + SQLCipher**（`rusqlite` feature `bundled-sqlcipher-vendored-openssl`）、WAL、`STRICT` テーブル、`sqlite-vec`（vec0）、**FTS5 trigram**。DB 鍵は Keychain `memory-db-key`。
- マイグレーションは **refinery**（`crates/shogun-memory/src/migrations/V<n>__*.sql`）。**追加のみ**（DROP/RENAME/ALTER COLUMN 禁止、`scripts/check-migrations.py`）、main に入った時点で**凍結**（チェックサム）、各マイグレーションは履歴行と同一トランザクション（`shogun_memory::migrate_and_check`）。ロールバック手順は `docs/migrations/V<n>-rollback.md`。
- **3層メモリ**（`docs/requirements-v1.0.md` §6.3, FR-MEM-01〜04）:

| 層 | 範囲 | 実装 | 備考 |
|---|---|---|---|
| Hot | 直近 24h | RAM（`src/hot.rs` `HotLayer`、既定 200MB 上限、1 イベント content ≤64KB、起動時 Warm から再構築） | キャッシュであり真実の源ではない |
| Warm | 30 日 | SQLite 本体。`event_vec` = `vec0(embedding float[384])`（V2）総当たり | 通常のベクトル検索はここだけ |
| Cold | 全履歴 | `cold_embeddings`（V4: `event_id, partition(30日バケット), scale, dim, codes BLOB`）int8 量子化（`src/quantize.rs`、cosine ≥0.999 を保証） | **埋め込みは出荷ビルドに無い（#237）ため実質不活性**。テキスト本文と FTS は常に Warm 側に残る |

### 1.2 event log（`V1__init.sql`, `src/event_log.rs`）
- `event_log(id, ts, source, kind, app_bundle_id, window_title, content, content_hash, last_seen_at, dwell_ms, display_id, window_bounds, window_pose, gaze_target)` + V5 `thread_key`, V7 `session_id`。`source ∈ capture/gmail/gcal/gdrive/slack/notion/github/linear/granola/web/agent/user/meeting/screen_ocr/screen_text/user_screenshot…`。
- **追記のみ**。唯一の UPDATE は dedup touch（`insert_or_touch`: `content_hash`(xxhash64, `event_log::content_hash`) 一致で `last_seen_at`/`dwell_ms` 更新）。
- 近似重複: Sørensen–Dice bigram ≥0.98（`shogun-core/src/capture/dedup.rs`、`TextRevision::{Unchanged,StateOnly,Delta,Checkpoint}` で差分のみ保存、40% 行変化でチェックポイント）。
- 書き込み前サニタイズ: `sanitize::persist_body` = `strip_hidden`（ZWSP 等除去）→ `redact`（秘密マスク）→ `instruction_shaped` 拒否（P2/P4 メモリ毒性対策）。
- 索引: `event_fts`（FTS5 trigram, external content）、V81 `event_grams_fts`（漢字 bigram・カタカナ bigram・2文字 Latin トークン、`detail=none`）、V68 event link indexes。
- `threads`（V5: `thread_key, source, title, summary, participants, project_id, salience, confidence`）、`sessions`（V7: kind `meeting|call|focus`）。

### 1.3 state tables（ワールドモデル本体、`src/state.rs`）
4 テーブル `people / projects / commitments / open_loops`（FR-ST-01）。全行必須: **provenance（`state_provenance(state_table, state_id, event_id, weight, quote(V69), batch_reviewed(V69))` 多対多）＋ confidence(0..1 CHECK)＋ `created_at/updated_at/last_evidence_at`**。provenance 空の INSERT はリポジトリ層で拒否（`EmptyProvenance`）。

| テーブル | 主カラム |
|---|---|
| `people` | `display_name, aliases(JSON), emails(JSON), handles(JSON), relationship_summary, last_interaction_at, interaction_channel, pending_from_me/them(JSON)` + V37 `extraction_key`、V49 `user_pinned/user_edited_at`、V69 `wording_edited`、V84 `mention_count, last_mentioned_at, page(JSON claims), page_dirty, engagement, person_class(contact/public_figure/not_a_person)`、V86 self names |
| `projects` | `name, status(active/waiting/paused/done), summary, participants, sources` + 同上 + V82 aliases, V85 `project_class` |
| `commitments` | `direction(mine/theirs), counterparty_id, description, due_at, status(open/done/overdue/cancelled), project_id` + V80 `deferred_until`、V83 `horizon(same_day/short/project), aged_at, merged_into` |
| `open_loops` | `kind(reply_needed/waiting_on_them/review_pending/decision_pending/follow_up/other), description, counterparty_id, project_id, opened_at, staleness_days, status(open/closed)` + V83 同上 |

周辺テーブル:
- `state_disputes`（V42: 同一フィールドの矛盾 earlier/later、resolution kept_earlier/kept_later/dismissed_both/kept_both）、`state_merge_blocks` + `state_split_events`（V50: ユーザー分割後は自動統合しない）、`state_suppressions`（V49: 削除/編集済みの digest を記録し再出現を抑止）。
- `loop_signals` / `loop_signal_reads`（V83: 毎時レビューが出す typed signal `commitment|blocker|question|decision|followup` + 逐語 quote。append-only、表示されない。**commitments/open_loops を書くのは consolidator だけ**）。
- `knowledge_entities`（V37: kind `topic|tool|organization|self|employment|education|activity`、**confidence ≤0.6**）+ `knowledge_provenance(entity_id, event_id, quote)`、根拠が消えると entity も消えるトリガー、`knowledge_nodes/knowledge_links` VIEW（Wiki）。
- `period_summaries`（V22: level `hour|day|week|month`、`generation_mode extractive|model`、`content_digest, is_dirty`、event/child provenance 付き、shard runs で再開可能）。
- `search_documents`（V21: 11 kinds `person/project/commitment/open_loop/thread_summary/session_summary/meeting_recap/meeting_note/brief/lesson/period_summary` の**再構築可能な検索射影**、invalidation キュー）、`search_document_vec`（V23 vec0 384）。
- `user_memory_notes` + versions + FTS（V76: チャットの `save_memory`／`Remember that…` のユーザーノート、revision 管理・forget/undo）。
- `memory_imports` / `memory_import_items`（V52: `shogun-memory-import/v1` パッケージ、origin `chatgpt|claude|cursor|other|computer_history`、section `preference/working_style/goal/constraint/entity/relationship/open_loop`、**confidence ≤0.6、High 不可**）。
- `feedback_events` / `lessons` / `lesson_provenance`（V16: kind `style|preference|correction|pattern`）、`offer_impressions` / `work_patterns`（V27: kind `activity|focus|rhythm|avoid`）。
- `traceability_log`（V1 + V13/14/18/31/46/70/71/91: `route, purpose, destination, chunk_bytes, chunk_xxh64, third_party`、**本文カラム無し**）、`llm_usage`/`llm_usage_live`（V30/V91）、`dream_cycles` + `dream_event_arrivals`（V32: 冪等な cycle_id、lease、batch_stage）、`job_runs`（V3）、`screen_frames`（V12 暗号化 JPEG、有限保持）、`transcript_segments`/`meeting_recaps`（V9/V10）、`mail_*`（V33 以降 Mail triage）、`calendar_occurrences`（V43）、`skill_runs`（V44）、`scheduled_jobs`（V45）、`clarify_questions`（V53）、`writing_voice_*`（V35/36）、`voice_terms`（V20）。

### 1.4 confidence の規則（`shogun-fusion/src/confidence.rs` が唯一の実装）
- バンド: **High ≥0.8（事実）/ Medium 0.5〜0.8（`possibly:` 接頭で弱く渡す）/ Low <0.5（生成・アクション判断から除外）**。
- `base_confidence`（V6）× `0.5^(elapsed/half_life)` の**再計算型減衰**（累積しない、`src/recompute.rs`）。複数根拠で corroborate。ユーザー編集＝1.0（pin）。
- ローカル規則抽出の上限 `LOCAL_RULE_MAX_CONFIDENCE = 0.4`（`src/extract.rs`）。Batch レビューで引き上げには**元文の逐語 quote が必須**（2026-10-13）。「何度か見た」だけでは上げない（2026-10-02）。
- consolidator（`src/loops.rs`）: `MIN_CONFIDENCE 0.84`、`MAX_OPEN 6`、1日 `RUNS_PER_DAY 8`、horizon タイマー退役 **3 / 14 / 45 日**（same_day/short/project）。
- 争議・推定文言は 0.79 上限（`disputes.rs`）。1 セッション言及のみの人物/プロジェクトは 60 日で prune（`entity_pages.rs` `PRUNE_AFTER_MS`）。

### 1.5 検索（`src/search/*`）
- `search_federated`（`search/federated.rs`）: レーン `UserNote / Event / Derived(kind) / Meeting / Imported(platform)` を **RRF（k=60）** で融合、per-source 40 件、抜粋 480 字、`IMPORTED_SCORE_WEIGHT 0.5`、`MIN_FACT_CONFIDENCE 0.5`、corroboration 加点、answer anchors（識別子・固有名・引用句）で「回答可能性」を判定。
- クエリ計画 `plan_memory_query`（`search/query.rs`）: topic terms、stopwords、相対日付（昨日/先週）→ ローカル日境界、source focus（meeting/brief/period/thread）。
- 字句処理 `search/terms.rs`（2026-10-14 改修）: NFKC・スクリプト境界分割・ひらがな助詞/敬称除去・送り仮名折り畳み（打ち合わせ→打合）・2文字 Latin（Q3, PR）をトークン化。`search/lexical.rs`: rowid を新しい順に最大 50,000 件取り、全概念 AND → idf 上位 300 を Rust 側 BM25（title ×2、既知人物/プロジェクト ×1.5）。
- 深さ `SearchDepth warm|auto|all`、`SearchTimeRangeV1 {from_ms,to_ms}`（半開区間）、`source_kind`、`limit`（既定 20、最大 50）、クエリ ≤4,096 字（`shogun-mcp/src/backend.rs`）。
- **AI 検索**（`shogun-core/src/ai_search/*`、#940）: 埋め込みを使わず、Agent lane で (a) 質問→最大 6 本の FTS クエリ＋日付範囲へ展開、(b) 上位 30 抜粋をリランク＋引用付き回答段落。モデル呼び出し最大 2 回＋ローカル検索最大 8 回、20 秒上限、完全一致トークンは固定順。
- 埋め込み（`src/embed.rs`, `src/embed_onnx.rs`, `docs/embedding-model-setup.md`）: `Embedder` trait（`embed_passages/embed_query`, `E5_SMALL_DIM 384`）、**multilingual-e5-small ONNX（約 470MB）は開発者のみ opt-in（feature `onnx` / `local-embedder`）**。配布ビルドはモデルも ONNX Runtime も同梱・ダウンロードせず、クラウド embedding API も使わない（ADR-001 を #237 で改定）。e5 の `query:`/`passage:` 接頭辞・mask 平均プーリング・L2 正規化を実装済み。

### 1.6 dedup / identity
- `src/memory_identity.rs`（2026-10-09/10、`research/MEMORY-DEDUP.md`）: タイトルのカウンタ除去（`(2)`/`unread`）、`stable_title`、引用 `>` 除去、`Today/Yesterday/曜日` 除去、省略プレビュー `…` の重なり判定（≥20字）、画面↔コネクタの cross-surface 同定、エンティティは正規化名（Unicode lower）で解決。効果: **2.18 → 1.06 行/メモリ、誤統合ゼロ維持**（`crates/dedup-bench/BASELINE.md`）。
- `src/identity.rs`（名寄せ FR-ST-10）: 同一チャネル identity（同じメール／同プラットフォーム同ハンドル）のみ自動統合、名前一致は弱い候補。`src/entity_dedupe.rs` + モデル dedupe（`shogun-core/src/dreamcycle/entities.rs`、relay purpose `entity_dedupe`）。
- `state.merge.candidates / state.merge / state.keep_separate`（#1362）で人手統合レビュー。

### 1.7 秘匿・PII（`crates/shogun-redact`, `crates/pii-bench`, `research/pii-benchmark/`）
- `shogun-redact`（依存ゼロ、2.1k 行）: `redact`（`ISSUER_PREFIXES`: `sk-ant-, sk-, ghp_/gho_/ghu_/ghs_/ghr_, github_pat_, xox[bpas]-, AKIA, ASIA, AIza, ya29., glpat-, sq0atp-, sq0csp-, shpat_, SG., GOCSPX-` + `key/token/password` ラベル後続値、`MIN_SECRET_LEN 12`、マスク `[redacted]`）、`redact_log`（メール・URL も）、`redact_egress`（**カード番号 Luhn・SSN・国民 ID** を送信境界で除去）、`strip_hidden`、`fence_untrusted` / `fence_data` / `fence_instruction` / `neutralize_context_markers`（プロンプトフェンス）、`instruction_shaped` / `reject_instruction_shaped`（永続ゲート）。
- 方針: **secret redactor であって PII redactor ではない**（メール・氏名は `people.emails`/名寄せに必要なので DB 内は保持、暗号化 at rest と単一 traced egress が統制）。
- `pii-bench` v0.2（120 probes、3 境界 content/window_title/egress）: TargetedSecret recall **1.000**、HighRiskIdentifier egress **6/6**、**false positive 0**、既知漏れ 5 ケース（`rk_/pk_` 未対応、高エントロピー無ラベル、12字未満パスワード、`-----BEGIN PRIVATE KEY-----`、OCR 破損接頭辞）。PII-001（window_title 未サニタイズ）は修正済み。

### 1.8 context cache / provenance / traceability
- `shogun-core/src/context.rs`: `ContextPack {facts, evidence(Evidence{event_id, ts, source, title, excerpt, frame_id}), screen_frames}`、`PROMPT_FACT_LIMIT 6` / `PROMPT_EVIDENCE_LIMIT 8`。**常時プリアセンブル**（押してから収集禁止、SLO: focus 切替→300ms、ボタン提示 150ms、ローカル検索 500ms）。
- `chat_grounding.rs`: 根拠 id `E1…En`（プロンプト）+ ツール結果に続番、引用解析でソースチップ、`MEMORY_PROMPT_BUDGET_CHARS = 6×600 + 1,000`。
- `traceability_log` route 列挙（`src/traceability.rs`）: `batch_api, batch_relay, messages_api, live_relay, mcp, composio, billing, asr, local_agent, support, web`。全外部送信につき**ちょうど 1 行、本文は digest(xxh64)+bytes のみ**、成功時のみ。`llm/traceability.rs` の `TraceRecord` にテキストフィールドは存在しない。

---

## 2. Context Fusion と学習（`crates/shogun-fusion`, Dream Cycle, 継続学習）

### 2.1 shogun-fusion（4.5k 行、依存は `shogun-agents` のみ、LLM 非呼び出し＝FR-CF-01）
`f(state, screen_ctx, intent) → action candidates`:
- `assemble.rs`: ランク = relevance × band_weight(confidence) + intent ヒント。`RELEVANCE_FLOOR`、`URGENT_ONLY_SLOTS`（緊急だけで取れる枚数は 1）、最大 4 候補、空パネル禁止の fallback（Search memory 等）。全候補に `Level` を**導出**（Send 系は必ず L3）。
- `candidates.rs`: people(name/alias/email)・projects(name)・commitments/open_loops（相手が画面上／キーワード 2 語 or 固有名 1 語）→ relevance 1.0/0.5/0、urgency（overdue／24h 以内）。`deferred_until` スヌーズ除外。純関数（`now_ms` 引数）。
- `confidence.rs`: バンド規則（§1.4）。`trust.rs`: `effective_confidence = source_trust_prior × extraction_confidence × freshness_decay`、prior: Structured 0.9 / StateFact 0.9 / Summary 0.7 / Evidence 0.4 / Lesson 0.5（freshness は未配線で 1.0）。
- `budget.rs`: `HeuristicEstimator`（Latin 4 字/トークン、CJK 1.5 字/トークン）、`deep_context_tokens`。`compress.rs`: `CompressionConfig{budget_tokens 2000, mode Balanced}`（既定 OFF、`score.rs` で score→予算充填、LLM なし）。`block.rs`: `ContextBlock/SourceKind`。
- `surface.rs`: bundle id／ホスト → `Surface {Mail, Chat, Messaging, Calendar, CodeReview, IssueTracker, Docs, Code, Browser, Other}`。
- `brief.rs` / `wrap.rs`: Morning Brief / Evening Wrap の決定的組み立て（`assemble_brief`, `assemble_degraded`）。

### 2.2 設計文書の要点
- `docs/context-architecture-design.md`（2026-07）: アーキテクチャは既に**ソース対称**（AX も Gmail も `Db::ingest_integration` で同じ event_log → 同じ `extract` → 同じ state）。新ソース追加は `IntegrationTransport` 実装・FSM 登録・ingest 配線の 3 点だけ。二段抽出（ローカル即時 ≤0.4 ＋ Batch 精緻化）が芯。
- `docs/context-layer-audit-and-plan.md`（2026-07-25 監査）: 当時「検索がチャットに未接続・埋め込み無し・名寄せ未配線・DB 平文」を指摘。**その後ほぼ解消**（SQLCipher、federated search、hourly Batch、memory tools）。残: 名寄せの一部、Cold 層不活性。

### 2.3 Dream Cycle / 毎時 Batch（`shogun-core/src/dreamcycle/*`, `docs/requirements` §6.7）
- ジョブ列挙（`plan.rs` `JobKind`）: `Consolidation, Compression, StateUpdate, ConfidenceRecalc, ColdDemotion, PatternDistillation, MorningBrief, LessonDistillation`。`Full` / `Degraded`（Batch 不要）シーケンス、`job_runs`/`dream_cycles` 台帳で再開・冪等。
- **2026-10-12 #1066**: consolidation と期間要約は**毎時**（窓 ID `YYYYMMDD-hHH`、カーソルで取りこぼし無し、`hourly.rs`）。Morning Brief・Cold 降格・30 日パターンは夜間に残す。
- `session.rs`（ゼロベース原価計画 L1/L2）: 行ではなく**セッション**（app+正規化タイトル+1h）単位で送る。OCR 行は AX があれば落とす（L0a）、Mail triage ON 時は gmail 行を Stage B に任せる（L0b）、行 dedup（Dice ≥0.98 かつ同一語集合）、`SESSION_MAX_CHARS`、共有 `system` でプロンプトキャッシュ。出力は逐語 quote ではなく **anchor（行番号＋先頭 ≤40 字＋長さ）** で検証（`resolve_anchor`）。行数 1,650 → セッション 30〜80。
- `prefilter.rs`（#286）: 決定論的な UI ノイズ除去（Close/数字のみ/cookie banner/同窓再送）、全内容行は必ず 1 回は送る。
- `loops.rs`（consolidator プロンプト）: 入力 = Now・Open items・Recently closed・New signals（kind|event|text|quote）・Sources `[E<id>]`、出力 JSON `{open:[{id,kind(commitment|loop|followup|project),horizon,source_event_id,source_quote,confidence,text}], resolved:[{id,event_id,quote}], duplicates:[{id,into_id}]}`。コード側で quote 完全一致・≥0.84・≤6 を強制。
- `pages.rs`（entity page プロンプト、`PAGE_PURPOSE="entity_page"`）: 出力 `{summary ≤140字, person_class|project_class, claims:[{text, basis: user_action|user_said|on_screen|inferred, weight: core|detail|noise, event_ids}]}` 最大 10 claims、逐語コピー禁止、`[[関連ページ]]` wikilink。1 回のレビューで dirty 4 ページ（backlog 時 16）。
- `entities.rs`（`entity_dedupe`）: 人物/プロジェクトの同一判定グループ化。`jobs.rs`: `Classifier` / `Summarizer` seam（`LocalRuleClassifier` が網羅的ネットワーク無しデフォルト）。
- relay 側の**構造化出力スキーマ**（`apps/api/src/schemas.ts`）: `CLASSIFY_SCHEMA`（commitments/open_loops/entities）、`CLASSIFY_WINDOW_SCHEMA`、`LOOPS_CONSOLIDATE_SCHEMA`、`ENTITY_PAGE_SCHEMA`、`ENTITY_DEDUPE_SCHEMA`、`BRIEF_SCHEMA`/`MORNING_BRIEF_SCHEMA`、`ABOUT_PROFILE_SCHEMA`、`DAILY_ACTIVITY_SUMMARY_SCHEMA`、`MAIL_TRIAGE_SCHEMA`、`DICTIONARY_TERMS_SCHEMA`。**抽出契約は purpose ごとにサーバーが強制**（`MODEL_CLASS_BY_PURPOSE`、allowlist）。

### 2.4 学習（learned context / continual learning v2 / lessons）
- `docs/learned-context.md`: 夜間分類が people/projects/tools/organizations/topics/employment/education/activity を**逐語 quote 必須**で抽出（V37）。Wiki（`wiki.list/evidence/neighbors`）で可視化。About You プロフィールはユーザー管理、first-person 捕捉は自動上書きしない。
- `docs/continual-learning-v2-design.md`（#242、P0〜P3 実装済み）: **Work Patterns**（activity「翻訳ばかり」/ focus「ShogunAI ばかり」/ rhythm 時間帯 5 バンド early/morning/afternoon/evening/night / avoid「この文脈のこの提案は無視される」）を `work_patterns`（count_7d/30d, trend EWMA, confidence, active）に毎晩再計算、FR-CF-03 の ranking prior、14 日クールダウン、`PatternDistillation` ジョブ。新規キャプチャ無し、集計のみ。
- Lessons（L5-v1）: 承認前編集の差分 `feedback_events` → `lessons`（instruction 1 文、scope global/app/person/project、evidence_count、active）。`LESSON_TRUST_PRIOR 0.5`、レベルには影響しない。
- 文章スタイル学習 `writing_voice_*`（V35/36、相手別例文、既定 ON V79）、`rewrite_*`（V78 学習ループ）。

### 2.5 日次サマリ（`docs/daily-summaries-design.md`）
- Morning Brief（Dream 最終ジョブ、Batch lane）: Today（カレンダー）／Commitments due／Open loops／What happened／Suggested actions。劣化 brief（LLM 無し）を必ず出す（FR-MB-04）。Evening Wrap は**決定的集計のみ**（FR-EB-02）。配達判定 `daily_delivery.rs`（純関数）。API: `memory.get_wrap`、briefs。`period_summaries` は `memory.get_periods` で読める。

### 2.6 チャット側のメモリ接続
- `daemon/memory_tools.rs`: チャットモデルに常に渡す**読み取り専用 L1 ツール 7 本** `search_memory(query, from?, to?, source?)`, `get_event(id)`, `list_commitments()`, `list_open_loops()`, `find_person(name)`, `find_project(name)`, `calendar_range(from,to)`（`MAX_MEMORY_TOOL_CALLS 6`、結果 ≤6,000 字、egress redaction＋fence）。
- `save_memory`（`docs/features/chat-memory.md`）: ユーザーの耐久事実を ≤2,000 字のノートとして自動保存（1 ターン 2 回まで、意味重複は直近 20 ノートで回避）。
- `llm/prompt_cache.rs`: 凍結 system（`CHAT_SYSTEM_PREAMBLE`: 根拠 id 引用・`possibly:`・送信しない）＋ソート済み state facts をキャッシュ可能プレフィックスに。

---

## 3. Memory API 面（`crates/shogun-mcp`, `docs/memory-api-mcp.md`, `docs/specs/mcp-tool-surface.md`, `docs/mcp/`）— 最重要

### 3.1 三面対称（不変条件6）
- **MCP**（`shogun-mcp` stdio、JSON-RPC 2.0、`initialize/tools/list/tools/call`、protocol `2025-06-18 / 2025-03-26 / 2024-11-05`）、**REST**（`shogun-api`、axum、`127.0.0.1:7464` 固定 loopback、CORS なし、Bearer）、**CLI**（`shogun`）。全部 `MemoryBackend` trait（`src/backend.rs`、依存逆転：trait は API 層、実装は `shogun-core/src/db_backend.rs`）＋同じ `ApprovalQueue` に落ちる（`src/dispatch.rs`）。
- `initialize` は約 1.4KB の**ルーティングガイド**（どの質問にどのツールか、要約は索引であり引用前に元を開く、送信は自動化しない）を `instructions` で返す。
- 対応クライアント（`client_config.rs` / `client_install.rs`）: Claude Code（`claude mcp add`）、Codex CLI / ChatGPT desktop（`~/.codex/config.toml`）、Cursor（`mcp.json`）、Claude Desktop。**アプリ起動時に見つけた AI アプリへ `read` 接続を自動投入**（2026-10-14）。
- 公開ドキュメント: `apps/docs/memory-api/{overview,mcp,rest,cli}.mdx`（Mintlify）。

### 3.2 認証・権限モデル
| 要素 | 内容 | 実装 |
|---|---|---|
| 接続許可（grant） | `shogun-mcp --client <id>` のみ（秘密値無し）。Keychain 項目 `memory-api-tokens` の `grants` に `{client, scope, created_at, sensitive_sources}` | `memory_api_settings.rs` `ClientGrant`、`GrantOrigin::{User,Automatic}` |
| トークン | REST/CLI 用。SHA-256 verifier を Keychain に保存、1 回だけ表示（Concealed pasteboard、60 秒で消去）、`client` 紐付け任意 | `memory_api.rs` `TokenRegistry`、`token_clipboard.rs` |
| スコープ | `read`（既定、**158 ツール中 85 の Read のみ**）/ `full`（各ツール固有レベル）。拡大はアプリ内のみ、API からは不可 | `scope_allows` 1 述語を REST (`rest_auth.rs`) と MCP で共用。拒否 = HTTP 403 `scope_insufficient` / JSON-RPC `-32004` |
| sensitive_sources | `withheld`/`allowed`。自動接続は withheld：会議・Visual recall 系ツールを `tools/list` から隠し `-32006 source_not_allowed`、`memory.search` 等はデータ層でフィルタ。API は狭める方向のみ（`api.connections.restrict`） | `source_policy.rs` |
| 即時失効 | `memory_api_tokens.rev` マーカー stat + 60 秒ごと再読（`live_tokens.rs`）、fail-closed |
| 起動元検証 | `launcher_check.rs`（#979）: 親プロセスの Developer ID Team ID を照合 |
| プラン | Memory API は **Pro/trial**。各呼び出しで `plan_allows`、ロック時 `plan_required`（`-32003`）、トライアル満了でメモリロック（#940） | `plan_source.rs`（ライセンストークン） |
| レート制限 | モデルを起動するツール（`translate.text`, `translate.reply_draft`, `memory.search mode:"ai"`, `skills.run`, `forms.plan`）は **30 回/60 秒/クライアント**、プロセス横断ファイル `llm_rate.json`、`rate_limited` | `rate_limit.rs` |
| 確度規則 | 読み取りは confidence と `possibly` を含み <0.5 は既定除外（FR-API-06） | `memory_api::read_inclusion` |

### 3.3 L1/L2/L3（`ApiLevel`、`memory_api.rs` `tool_level`）
- `Read` / `Write(L1)`（ローカル・可逆、即実行）/ `Write(L2)`（ワンタップ確認待ち、`pending`）/ `Write(L3)`（共有承認キュー `l3_approvals.json`、本文は ChaCha20-Poly1305（HKDF from DB key）で封印、専用ボタン必須、**10 分タイムアウト**、API は `{"pending":true,"approval_id":N,"level":"L3","origin":…}` / REST 202）/ `PerAction`（`actions.execute`）。
- 不変条件4: L1 に外部送信は構造的に存在しない（`SendAction` は型で L3）。「権限を広げる」操作（トークン発行・接続作成・consent 付与・除外解除・Visual recall ON）は L3 ceiling か **API で拒否**。
- `actions.execute` の kind: local `local_search, open_app, reveal_file, show_notification, copy_to_clipboard`、L3 `send_email, post_message, post_comment`（`rest_actions.rs`）。

### 3.4 ツール全リスト（wire name、158 本。`memory_api.rs` `ALL_TOOLS` / `wire_name`。R=Read）
グループは `SHOGUN_MCP_TOOLSETS` / `--toolset`（`toolsets.rs`。stdio 既定は **core 20 本 ≈3k tokens**、`all` ≈ 11〜12k tokens）。

**core / memory & profile**
- `memory.search` R — federated 検索（`limit, from_ms, to_ms, source_kind, depth, mode keyword|ai`）
- `memory.get_context` R — 現在何が起きているか（capture ログのセッション化）
- `memory.get_context_pack` R — 問いに対する confidence ゲート済み facts＋日付・出典付き根拠（Context Pack）
- `memory.get_events` R — イベント本文を id で読む（withheld は本文無し）
- `memory.read_session` R — 1 セッション（app×時間）の内容
- `memory.get_periods` R — `period_summaries` の hour/day/week/month 要約（`page_size` 既定 24）
- `memory.get_wrap` R — 今日の Evening Wrap
- `memory.append_note` L1 — ユーザーメモを event log へ追記
- `memory.notes.list` R / `memory.notes.change` L1 — チャット保存メモの一覧・save/update/forget/undo（revision 必須）
- `calendar.upcoming` R — 同期済みカレンダー予定
- `profile.whoami` R / `profile.set` L1 — identity/prefs/About You 射影
- `storage.preview` R — ストレージ内訳
- `memory.import.preview` R / `memory.import` L1 / `memory.import.list` R / `memory.import.delete` L1 — 他アシスタントからの取り込み（`shogun-memory-import/v1`）

**state**
- `state.{people,projects,commitments,open_loops}.list` R / `.get` R
- `state.create` L1 / `state.update` L1 / `state.delete` L1 / `state.export` R
- `state.disputes.list` R / `state.disputes.resolve` L1
- `state.split` L1 / `state.split.evidence` R（名寄せの分割と根拠）
- `state.merge.candidates` R / `state.merge` L1 / `state.keep_separate` L1

**actions（agents）**
- `actions.execute` per-action / `actions.status` R（approval_id の照会）
- `actions.questions` R / `actions.answer` L1（Clarify #394）/ `actions.steer` L1（実行中の run に 1 行追加、#395）

**skills / schedules**
- `skills.list` R / `skills.run` L1 / `skills.run.status` R / `skills.save` L1
- `schedules.list` R / `schedules.add` L1 / `schedules.remove` L1

**forms（Form mode、Jev）**
- `forms.inspect` L2 / `forms.plan` L1 / `forms.status` R / `forms.apply` L3 / `forms.undo` L3 / `forms.close` L1

**wiki / patterns / lessons**
- `wiki.list` R / `wiki.evidence` R / `wiki.neighbors` R
- `patterns.list` R / `patterns.set_active` L1
- `lessons.list` R / `lessons.set_active` L1

**web**（公開ページ読み取り、資格情報なし）
- `web.page` R / `web.video_transcript` R / `web.social_post` R

**meeting（会議ノート）**
- `meeting.list` R / `meeting.search` R / `meeting.recap.get` R / `meeting.transcript.get` R / `meeting.recap.export` R / `meeting.recap.edit` L1 / `meeting.recap.track` L1 / `meeting.delete` L1
- `meeting.start` L2 / `meeting.start.status` R / `meeting.system_audio.status` R / `meeting.system_audio.retry` L2
- `meeting.speakers.get` R / `.set` L1、`meeting.templates.get` R / `.set` L1、`meeting.recap_prefs.get` R / `.set` L1、`meeting.microphone.get` R / `.set` L1、`meeting.reminder.get` R / `.set` L1

**mail（Mail triage）**
- `mail.triage.list` R / `mail.triage.get` R / `mail.triage.set` L1 / `mail.triage.archive` L2 / `mail.triage.unarchive` L2
- `mail.labels.list` R / `mail.labels.upsert` L1 / `mail.labels.delete` L1
- `mail.sender_rules.list` R / `.add` L1 / `.delete` L1、`mail.history.list` R、`mail.settings.get` R / `.set` L1

**visual_recall / capture**
- `visual_recall.status` R / `visual_recall.set_enabled` L3 / `visual_recall.set_retention` L3 / `visual_recall.search_frames` R / `visual_recall.get_frame` R / `visual_recall.frame_text_lines` R / `visual_recall.rescan_frame` L1 / `visual_recall.delete_frame` L1
- `capture.status` R / `capture.set_paused` L3（再開側が L3）/ `capture_exclusions.get` R / `capture_exclusions.set` L3（削除側）

**voice / writing / translate**
- `voice_dictionary.list` R / `.create` L1 / `.update` L1 / `.delete` L1 / `.import` L1 / `.export` R / `.autoadd` R / `.autoadd_manage` L3
- `voice.microphone.get` R / `.set` L1、`writing_voice.get` R / `writing_voice.set` L3
- `translate.text` L1 / `translate.reply_draft` L1 / `translate.status` R / `translate.register.get` R / `.set` L1 / `translate.pair.get` R / `.set` L1

**export**
- `export.write_file` L2 / `export.writes.list` R / `export.folders.list` R / `export.folders.remove` L1

**settings / device / admin**
- `agent.source.get` R / `agent.source.set` L3（Agent lane の資格情報: hosted `shogun` / BYOK / サブスク委譲）
- `connections.list` R / `connections.disconnect` L1 / `connections.composio.set` L1 / `connections.gmail.set` L1（draft-only は ON 方向のみ）
- `api.tokens.list` R（tokens, connections, access 一覧）/ `api.connections.restrict` L1
- `backup.status` R / `backup.set_retention` L1、`search_index.status` R / `search_index.compact.start` L1 / `.stop` L1
- `dream.status` R / `dream.retry` L1
- `device.onboarding.get` R / `device.account.get` R / `device.release_notes.get` R / `device.update.get` R
- `settings.language.get` R / `.set` L1、`support.reports.list` R

### 3.5 REST ルート（`rest_routing.rs`、`/v1/...`、`Routed::Read|Write|Action|ApprovalStatus|Status|Metrics`）
`GET /v1/status`・`GET /v1/metrics`（無認証）、`GET /v1/memory/search|context|events|session|context_pack|wrap|periods`、`POST /v1/memory/import`（+`/preview`, `/imports`, `/imports/{id}/delete`）、`GET/POST /v1/memory/user-notes[/<id>|/change]`、`/v1/state/*`、`POST /v1/actions`・`GET /v1/actions/status/{id}`・`/v1/actions/questions|answer|steer`、`/v1/skills/*`、`/v1/forms/*`、`/v1/meeting/{list,recap,transcript,search,…}`、`/v1/mail/*`、`/v1/visual_recall/*`、`/v1/capture/{status,paused}`、`/v1/capture_exclusions`、`/v1/wiki/{list,evidence,neighbors}`、`/v1/translate/*`、`/v1/export/{file,writes,folders}`、`/v1/connections[/connect|disconnect|composio|gmail]`、`/v1/agent/source`、`/v1/api/tokens`、`/v1/api/connections/restrict`、`/v1/device/*`、`/v1/dream/status`、`/v1/backup`、`/v1/search_index/*`、`/v1/writing_voice`、`/v1/voice_dictionary/*`。

### 3.6 CLI（`crates/shogun-cli`）
`shogun search <q>` / `context` / `events` / `session` / `periods` / `wrap` / `state edit|delete|export|split|merge|keep-separate` / `actions execute|poll|questions|answer|steer` / `skills list|show|run|status|save` / `schedules` / `forms` / `meeting …` / `mail triage|labels|rules|history` / `visual-recall …` / `api status|tokens|restrict` / `agent-source use` / `connections` / `search-index status|compact|stop` / `dream` / `memory-notes list|show|change` / `import` / `translate` / `wiki` / `patterns` / `lessons` / `backup` / `export`。

### 3.7 連携ツールカタログ（モデル向け hub op、`tool_catalog.rs` + `tool_loop.rs`）
- モデルは実 MCP ツール名を見ない。hub op: `list_calendar_events`, `check_calendar_availability`, `search_mail`, `get_mail_thread`, `list_recent_drive_files`, `read_drive_file`, `search_chat_messages`, `get_slack_thread`, `search_docs`, `search_code_issues`, `list_tracker_issues`, `list_granola_meetings`, `read_granola_meeting`, `read_video_transcript`, `read_social_post`, `read_web_page`, `open_url`(Local) ＋ Propose 系（送信は常に L3 提案止まり）。`service_gate::authorize_op` で二重ゲート。
- `tool_loop.rs`: `MAX_TOOL_USES 8`、1 読み取り 10 秒、結果 16KiB、並列 read を 1 メッセージで返却、**非同期 tool calling opt-in（#396）**、全結果 `fence_untrusted`。

---

## 4. エージェント実行エンジン（`crates/shogun-agents`、5.7k 行、依存ゼロ）
- `permission.rs`: `Level {L1,L2,L3}`、`LocalAction {OpenApp, RevealFile, LocalSearch, ShowNotification, CopyToClipboard, UpdateState, SaveDraft, ReplyDraft, WriteFile}`、`SendAction {SendEmail, PostMessage, CreateCalendarEvent, PostComment…}`（型で L3）、`ServiceStateAction`（可逆なサービス書き込み）、`CaptureExclusionChange / PausedCapture / VisualRecallWidening`（L3 ceiling の設定変更）。
- `engine.rs` `ExecutionEngine`: L1 即実行→`action.executed`、L2 キュー→`confirm/cancel/expire`、L3 は engine では拒否して承認キューへ。`LocalEffector`/`ExecutionObserver` 注入、`now_ms` 引数。Clarify 質問キューを統合。
- `approval.rs` `ApprovalQueue`: `SendAction` のみ保持（L3-by-construction）、`Preview`（全文・宛先・route・key kind）、`ConfirmIntent::DedicatedButton`（Enter 不可）、`ApprovalOrigin {Ui, Api, Mcp, Cli}` で同一フロー、10 分タイムアウト、`Route {DirectMcp, ViaComposio}`（Composio は実行不可）。
- `producer.rs` `ProposedSend {Email, SlackPost, CalendarEvent, IssueComment}` → L3 エントリ生成。
- `presets.rs`: 7 プリセット `ReplyDrafter, MeetingPrep, TaskExtractor, FollowUpSentinel, CalendarScheduler, IssueTriage, NoteCapture`。各操作に `OpKind {LocalCompute(L1), DraftGenerate(L2), StateWrite(L2), ExternalRead(L2), ExternalSend(L3)}`、テーブルとレベル整合をテストで強制。
- `skills.rs`（#251）: 1 ページ Markdown（frontmatter `name/description/permission_level/required_connections/inputs≤8`）、≤16KB、≤200 ファイル、本文に送信語（英/日）があれば **L3 に昇格**、run は Agent lane の 1 completion（ツール無し、テキストのみ）。`skill_runs` テーブル（V44）でキュー。
- `clarify.rs`（#394）: 1 run につき 1 質問（≤200 字、選択肢 ≤3、回答 ≤500 字、10 分で期限）、回答は何も承認しない。`steer.rs`（#395）: ≤500 字、run あたり保留 3、次ターンの user 半分に fenced で追加。
- `entitlement.rs`: `Plan {Standard, Pro}`、7 日トライアル、`Entitlements {memory_api, agent_execution, local_capture, memory_access, gmail send unlock…}`、`PLAN_REQUIRED`。判定は Rust のみ。
- Forms（`shogun-core/src/forms.rs`, `forms/jev.rs`、`docs/features/form-mode.md`）: Scribe から選んだフォームの空欄だけを人が確認して記入。判断は **TypeSafe Jev（`jev-1.13.0`、`api.typesafe.ai/v1/systemone`）**、relay `/v1/ai/form_decide` 経由（会社キー）。`UndoLedger`、≤80 fields、≤2,000 字/値。

---

## 5. 連携コネクタ（`crates/shogun-integrations`, `shogun-mcp/src/scope.rs`, `docs/connector-adapter-plan.md`）
- 対象サービス（`Service` enum）: **Gmail, GoogleCalendar, GoogleDrive, Slack, Notion, GitHub, Linear, Granola, Web**。Wave 1 = Gmail/Calendar/Drive/Web、Wave 2 = Slack、Wave 3 = Notion/GitHub/Linear/Granola（`Service::wave`、未解放 Wave はゲートで自動拒否）。
- 権限表（`scope.rs`）: サービス×操作×`OpClass {Read, DraftLocal, ServiceStateChange, ExternalSend, LocalAction}`×`Gating {Background, Level}`。**表に無い操作は拒否、ExternalSend は必ず L3**（`tests/invariant4.rs`）。`service_gate::authorize_op` が Wave×接続状態×同意×draft-stop を合成。
- 経路: **Gmail は Google REST 直結**（#713、`shogun-core/src/gmail_direct.rs`、Desktop OAuth+PKCE、`gmail.modify`、Keychain `gmail-tokenset`、draft-only 既定 ON）。他は**公式リモート MCP 直結**（`endpoints.rs`: `calendarmcp.googleapis.com/mcp/v1`, `drivemcp.googleapis.com/mcp/v1`, `mcp.slack.com/mcp`, `mcp.notion.com/mcp`, `api.githubcopilot.com/mcp/`(PAT), `mcp.linear.app/mcp/readonly`, `mcp.granola.ai/mcp`）。Composio（`composio.rs`）は旧資格情報互換のみ。
- 部品: `oauth.rs`（OAuth 2.1 + PKCE 純ロジック）、`oauth_flow.rs`（loopback、feature `live`）、`token.rs`（`TokenStore`/`TokenManager` 自動リフレッシュ）、`keychain*.rs`（macOS Keychain adapter）、`toolmap.rs`（op 名→実ツール名）、`result.rs`（→`FetchedItem` 正規化）、`transport.rs`（`RemoteMcpTransport`: JSON-RPC Streamable HTTP）、`runtime.rs`（`ConnectorRuntime`: **15 分ポーリング**、`IngestSink`、`SyncCursor`、on-demand fetch、二重ゲート）、`send_bridge.rs`（確認済み送信の実行ルーティング）、`connection.rs`（connected/amber(NeedsReauth)/disconnected FSM、サービス間隔離）。
- 取り込みは `Db::ingest_integration` で `event_log.source=<service>` に入り、キャプチャと同じ抽出・state・検索・Fusion を通る（ソース対称）。`connector_sync_cursors` テーブル。Mail triage（#319: Stage L ローカル＋Stage B Batch、`mail_batch.rs`、1 スレッド 1 モデルパス）。
- 新サービス追加の拡張点: `Service` enum → `endpoints.rs` → `toolmap.rs` → `result.rs` の 4 点（`docs/mcp/01-architecture.md` §2-1）。

---

## 6. ホスト／サーバー側（`apps/api`, `apps/website`, 関連 docs）

### 6.1 Batch/Live relay（`apps/api`、Hono、**Cloudflare Workers `shogunai-relay` @ `api.shogunaios.com` + Supabase Postgres via Hyperdrive（キャッシュ無効）**、cron `*/15`）
- ルート（`src/app.ts`, `live-app.ts`, `asr-app.ts`, `boot.ts`）: `POST /v1/batch`、`GET /v1/batch/:id`、`POST /v1/batch/recover`、**`POST /v1/review`**（同期の毎時レビュー、`gpt-6-luna`、`REVIEW_TIMEOUT 90s`、`max_completion_tokens 16,000`、最大 2 試行、422 `upstream_refused`）、`POST /v1/ai/form_decide`（TypeSafe）、**`POST /v1/ai/:lane`**（9 live lanes: chat/agent/draft/scribe/rewrite/dictation_cleanup/summary_live/translate_live/quick_translate、SSE、OpenAI Responses or Groq）、`GET /v1/ai/usage`、`POST /v1/asr/grant|refresh|finish`（Deepgram 短命 JWT 60 秒）、`GET /healthz`、`GET /metrics`（bearer）、dev 用 gmail-local。
- 認証（`src/auth.ts`）: **Ed25519 署名ライセンストークン `v1.<base64url(payload)>.<sig>`**（`{v,lic,plan,status,device,iat,exp(26h),period_end,cancel_at_period_end,grace_days}`、trial は `plan:"trial"`）、`kid` による鍵ローテーション、`MAX_TOKEN_LIFETIME 48h`、`meterSubject`（アカウント／trial デバイス／licence id）、`RELAY_DENY_SUBJECTS`。同じ検証器が `crates/shogun-license`（端末側、offline grace 14 日、ハード UUID 束縛）にある。
- 計量・予算（`usage.ts`, `spend.ts`, `pg-store.ts`, `live-store.ts`）: **reserve-then-settle の micro-USD 台帳**（提出時に bytes/2≈token で上限予約、結果で provider usage に置換、冪等 `client_request_id`、`SELECT … FOR UPDATE`＋条件付き upsert で多インスタンス安全）。テーブル `relay_usage_days / relay_usage_idem / relay_usage_owners / relay_spend_days / relay_spend_batches / relay_rate_buckets / relay_live_subjects / relay_live_counters / relay_live_requests / relay_live_admin_actions / relay_asr_*`。ライセンス別日次上限・トライアル上限・全体ブレーカー `DAILY_SPEND_CAP_USD`（未設定なら起動拒否）、live は **D9 上限 trial $1/日・Standard $3/日 $40/月・Pro $6/日 $80/月**、同時 3 ストリーム/ユーザー・2/デバイス、12 req/ユーザー/lane/分、240 秒ストリーム期限。
- **ゼロ保持（D11）**: 本文を DB・ログ・メトリクスに書かない（`redactingLogger`、invocation logs OFF）、ストリームはバッファせず中継、provider `store:false`、HMAC safety/cache id。
- モデル選択はサーバー（`models.ts` `ModelClass {classify, summarize, brief, about_profile, daily_activity_summary}` → `gpt-6-luna`、`BATCH_PROVIDER=anthropic` で Haiku 5.5 / Sonnet 5.5）、出力スキーマ強制（`schemas.ts`）、`system` は既知プロンプトのみ。
- テスト: vitest 128（`test/*.test.ts`、Workers ランタイムテスト含む）。運用: `apps/api/DEPLOY.md`、`docs/ai-gateway-runbook.md`、`docs/ai-gateway-launch-checklist.md`。
- 設計文書: `docs/batch-relay-design.md`（2026-07-25、運営キーを端末に置かない・原価上限をサーバーで強制）、`docs/hosted-ai-gateway-design.md`（#1101 D1〜D12）、`docs/hosted-ai-gateway-implementation.md`、`docs/specs/issue-29-shared-service-capacity.md`（想定 1 万台、夜間 3,000 台/時、ジッタ・バックオフ・ライセンス単位レート制限）。

### 6.2 ライセンス／アカウント API（`apps/website`、Next.js、Supabase Postgres）
- `/api/license/verify|claim|trial`（Ed25519 発行）、`/api/admin/license/{reissue,revoke,seats/reset}`、`/api/admin/ops-licenses`、`/api/stripe/webhook`、`/api/update/{target}/{current}`（Tauri updater）、`/api/account/*`（**WebAuthn パスキー** register/login/reauth/recover、Google OAuth start/callback/complete、`desktop/authorize|exchange`（PKCE loopback）、sessions refresh、devices、identities、billing portal/cancel、email confirm）。
- `docs/account-passkey-design.(en.)md`（#148, D7）: アカウントは課金・プラン・デバイス・内容を含まない使用量/上限/ブロックのため。**クラウドにメモリを置かない（NFR-PRV-04）**。
- `docs/ops-licenses.md`: 運営用 Pro コード（SHA-256 のみ Worker secret、上限 30、365 日、IP 10/時）。

### 6.3 クロスプラットフォーム設計
- `docs/cross-platform-architecture.md`（2026-08-25/10-19）: crate 別 OS 依存表 — **`shogun-agents` / `shogun-fusion` / `shogun-redact` / `shogun-license`(verify) は macOS 依存なし、`shogun-memory` は `embed_onnx.rs` のみ、`shogun-mcp` は `plan_source.rs` のみ、`shogun-integrations` は Keychain、`shogun-core` は capture/audio/meeting/notch が macOS**。XP-01（境界ガード）・XP-02（`event_log.uid` 端末横断 ID）・XP-03（`packages/protocol`）は未着手。
- `docs/windows-architecture.md`（2026-10-19 決定）: Tauri v2 + Rust Core + React を流用、UIA/Graphics Capture/WASAPI/Credential Manager+TPM アダプタのみ新規。
- `docs/mobile-architecture.md`（2026-10-19）: ネイティブ Swift/Kotlin + UniFFI の Rust データ層、**クラウド同期しない**、同一 LAN の E2E 直接やり取り、中継は暗号文のみで保存しない、`event_log` に ULID/`device_id`/`platform` を足す加算マイグレーション案。

---

## 7. 評価・ベンチマーク（`crates/memory-bench`, `fusion-eval`, `extract-eval`, `pii-bench`, `dedup-bench`, `ax-noise-bench`, `docs/search-eval.md`, `docs/extract-eval.md`, `docs/memory-quality-verification.md`）

| ハーネス | 何を測るか | 主要数値 |
|---|---|---|
| `memory-bench`（`BASELINE.md`, `baselines/*.json`, `MemoryBackend` seam） | 書込/検索 p50/p95/p99、recall@k/MRR、write amplification、staleness。ワークロード `clean / duplicate / temporal / redundant / abstention / multi-session / preference`、`--path hybrid|federated`、corpus slicer | 100k events: write p95 **0.36ms**、query p95 **40.79ms**（SLO 500ms の 8%）、**~640 bytes/row**、recall@5 1.000。**temporal: recall@1 0.000 / MRR 0.500 / stale 100%**（知識更新で古い値が常に 1 位）。dup: 完全一致 55.1% 収束、write amp 1.19。**abstention: false-evidence 100%**（relevance floor 無し）。multi-session coverage@5 0.854 / @10 0.938。preference: 例外が習慣を上回る 75%。slicer day/800: 行 44% 削減で fact retention 100% |
| `docs/search-eval.md`（`shogun-core/tests/search_golden_eval.rs`、132 問、主に日本語） | R@5/@10、MRR、zero-result、answerable、false-answerable | 2026-10-14 改修で **R@5 0.653→0.985、MRR 0.634→0.948、zero-result 0.278→0.000、answerable 0.048→0.873、false-answerable 0→0** |
| `pii-bench`（`BASELINE.md`、40 cases×3 境界） | 秘密/PII の残存 | §1.7 参照（TargetedSecret 1.000、FP 0、egress HighRisk 6/6） |
| `dedup-bench`（`BASELINE.md`、25〜27 シナリオ） | 1 メモリあたり行数、誤統合 | **2.18→1.06 行/メモリ**、22/25→24/27 pass、誤統合 1→2（文書化済みトレードオフ） |
| `extract-eval`（`goldens/day-01..06`、`replay/*.json`、`docs/extract-eval.md`） | SynthDay ゴールデンでの state 抽出 P/R/F1（hallucination/omission 分離）、provenance dangling/unsupported、確度較正 ECE、バンド約束違反。失敗タクソノミー T1〜T19・未カバー N1〜N8 | ローカル規則は話者取り違え 4/4 等多数誤抽出、ECE 0.36〜0.46（過小確信）、約束違反なし。CI ゲート `tests/gate.rs` + `shogun-core/tests/extract_eval.rs` |
| `fusion-eval`（`scenarios/{en,ja,hard}.json` ≥30） | 画面×state → 候補の正しさ（relevant/first/generic） | CI ゲート `tests/gate.rs` |
| `ai_search_eval` / `answer_eval`（`shogun-core/tests/`） | AI 検索の recall@k/MRR（fake は CI、live は `#[ignore]`）、チャット回答のプロンプト契約 | 数値は fake のみ |
| `ax-noise-bench`（#302） | AX キャプチャのノイズ・トークン・事実保持 | — |
| `docs/memory-quality-verification.md` | 統合検証 | 713 memory tests / 1,708 core db tests / 1,833 core exec tests、日次クエリ 1.78ms p50 |

研究側（`research/`）の結論は §8。LongMemEval のカテゴリ分類（single-session user/assistant/preference, multi-session, knowledge-update, temporal-reasoning）をワークロードとして取り込み済み。

---

## 8. 既存の調査・戦略文書の結論（エージェント向けメモリ製品に関係するもの）

- **`research/COMPARISON.md`（2026-09-19）**: ReMe（agentscope）は BM25+vector RRF・daily→digest 統合・CLI/HTTP/MCP と**設計がほぼ同型**。LongMemEval 500 問中 **knowledge-update 78 + temporal-reasoning 133 = 42% を SHOGUN は当時失敗**（最新値が 1 位にならない／as-of が表現できない）。推奨: データセットではなくタクソノミーを採用、abstention/multi-session/preference ワークロード追加（→ 実装済み）。Sentient OS は SSN/カードの regex backstop を持つ（→ `redact_egress` で実装）。
- **`research/BENCHMARK-RESULTS.md`（2026-09-29）**: `since_ts/to_ts` は既に API にあり temporal gap は閉じていた；memory-bench が計測していた `search_hybrid_with_options` は製品で未使用（federated へ）；**abstention 100% 失敗**が最悪の結果（relevance floor が無く「無い」が表現できない）；coverage と recall の乖離。
- **`research/MEMORY-DEDUP.md`（2026-10-09）**: 重複の 8 原因と修正（§1.6）、次は identity index（スキャン O(n) 回避）・既存重複の統合ジョブ・言い換え統合はモデル判断に委ねる。
- **`docs/sentient-os-competitive-analysis.md`（2026-08-25）**: SHOGUN が勝る点 = リアルタイム文脈、provenance+confidence の構造化ワールドモデル、会議、検索 SLO、L1/L2 段階的自律、ビジネス。借りる価値 = 蓋閉じ夜間実行、**読める・直せる・消せるナレッジベース**、**E2E 暗号 MCP ミラーでスマホ到達**、夜間ジョブ安全パターン、egress 前オンデバイス triage。
- **`docs/audits/2026-10-12-shogun-vs-goldfish.md`**: Goldfish（競合メモリ製品）は 17 MCP tools（`search_memory, get_working_memory, get_chronicle, search_wiki, get_promises, get_work_context, get_attention_context…`）、4.5GB **平文** SQLite、2,491 wiki ページ・575k エッジ KG、毎時 T+1 分の要約で**鮮度と豊かさで優位**。SHOGUN が強いのは **一箇所で強制されるゲーティング、quote 検証済みレビュー、supersession（disputes/decay/pin）、原価・egress 制御、権限モデル、型付き予算**。この監査が「Goldfish parity」の一連のオーナー決定（毎時レビュー、entity pages、consolidator）を生んだ。
- **`docs/gpt6-astra-implications.md`（2026-09-04）**: 結論4「**Computer Use を自作せず、Astra 級実行エンジンにワールドモデルと承認境界を貸す側に回る**」「**他社エージェントの文脈供給層（MCP で文脈を貸す）が ShogunAI の最も自然なポジション**」。OpenAI が持たないもの = provenance+confidence の state tables、機構としてのプライバシー、モデル非依存。提案: Responses API 対応（済）、`ExternalExecute` L3 ハンドオフ、Memory API を「他社エージェントが装着する文脈層」として導線化。
- **`docs/jev-ecosystem-research-and-adoption.md`（2026-09-22）**: TypeSafe Jev（選択式判断、$0.0002/step、分類で Haiku を大差で上回り原価 1/50、`jev-recall` の関連度スコアは 238 件 $0.00044・0.35s）。「モデルに座標もセレクタも生成させない」契約、ガードレール規則（決定論先、Jev は狭い意味判断、fail-open 読み/fail-closed 書き、シャドー既定）。Fusion の relevance は現状プレースホルダ（0.95 固定）で Jev 候補。
- **`docs/select-kk-cost-zero-base-plan.md`（2026-09-22）**: 原価の第一要因は「送る必要の無いものを送る」こと → **セッション化で桁削減**、Jev は残余の門、目標 LLM 原価 ≤ 月額の 15%。
- **`docs/unit-economics.md`（2026-10-12）**: ARPU $63.62、LLM 原価/人・月 **現行 $67.94（139%）→ Haiku 5.5 で $9.08（19%）→ セッション化併用で $2.08（4%）**、固定費 $58〜83/月、推奨 cap Standard $0.50/日・trial $0.25/日。
- **`docs/llm-combined-product-decisions.md`**: ツールは Anthropic BYOK から、質問駆動の読み取りのみ自動、認可 revision で tool 会話全体を無効化、8 tool calls/10s/16KiB、記憶 facts は untrusted としてフェンス、Brief の内容規則。
- **`docs/harness-design-and-optimization.md`（#103）**: 「配管は一級品（レーン分離の型強制、本文を持てない traceability、SLO ヒストグラム、モック可能トランスポート）だが頭脳と目が無い」→ 計測→キャッシュ/構造化出力→評価基盤→ルーティングの順。R1〜R10 は `docs/requirements-harness-v1.md`（FR-HN/FR-EV）。
- **`docs/positioning-category-messaging.md`**: 5 層構造 **L1 取得 / L2 ハーネス＝ワールドモデル / L3 アプリ上で実行 / L4 24 時間実行 / L5 自己改善**、単一アプリが越えられない「文脈・実行・記憶の 3 つの壁」。
- **`docs/mcp/00-05`**: MCP ハブ 3 責務（レジストリ・接続状態・ルーティング）は 2 クレートに分散実装済み、独立サーバーは存在しない。

---

## 9. 事業・価格で決まっていること（`docs/paid-terms-decisions.md`, `docs/ops-licenses.md`, `docs/unit-economics.md`, `docs/prepaid-credits-design.md`, CLAUDE.md §プラン構成）
- **Free プランなし**（2026-07-26）。**7 日トライアル = Pro 相当、カード必須**（Stripe Checkout `trial_period_days: 7`、未解約で自動移行）。満了後は新規キャプチャ停止＋メモリロック（#940）、データは消さない。
- 価格（USD）: **Standard $49/月（年払い $588）／$62 月払い、Pro $99/月（年払い $1,188）／$124 月払い**（`apps/website/src/i18n/dictionaries.ts`, `pricing.ts`）。
- Standard: キャプチャ＋3 層メモリ＋検索＋Notch＋第 1 層連携（読み取り、Gmail 直結含む）＋Dream Cycle＋Morning Brief＋**エージェント実行（L1/L2/L3 チャット・アクション・ドラフト・Scribe、2026-09-24 に Pro から移動）**。Pro: ＋**Memory API（MCP/CLI/REST）**＋Gmail 実送信解放＋優先サポート。会議ノートは全プラン。
- 会社キー hosted live（#1101 D1）: 予算 D9（trial $1/日・$7/期間、Standard $3/日・$40/月、Pro $6/日・$80/月）。Batch（分類・要約・Dream・Brief・review・consolidation）は会社キー専用。
- プリペイドクレジット（#1334、設計のみ）: Stripe 一回払い、プラン枠消費後に使用、Batch には使わない、資金決済法（自家型前払式、残高 1,000 万円超で届出・供託、期限 ≤6 か月なら適用除外）の分析あり。
- 運営用ライセンス（Pro 無償、30 件・365 日・10/IP/時）、Stripe Customer Portal、オフライン猶予 14 日、決済失敗 7 日猶予、PostHog opt-out 既定 ON、返金 14 日（サブスク）。Stripe live account 情報は `paid-terms-decisions.md` §2-0。

---

## 10. 持ち越せない制約 vs プラットフォーム中立な Rust

### 10.1 macOS／単一ユーザー前提（ShogunAgent には持ち越せない）
- **取得層全部**: Accessibility API ウォーク（`shogun-core/src/capture/*`、desktop `axcache.rs`）、Visual recall（ScreenCaptureKit/Apple Vision OCR、`screen_frames`）、会議検知・Core Audio tap・Deepgram live ASR（`audio/*`, `meeting`）、NSPanel ノッチ、グローバル入力モニタ。`event_log.source` の語彙はキャプチャ中心。
- **秘密保管**: macOS Keychain（`shogun-integrations/src/keychain*.rs`, `security-framework`）、`shogun-license::hardware_device_id`（IOKit）、`plan_source.rs`。Memory API の grant/トークンも Keychain 項目。
- **ローカル単一テナント**: SQLCipher ファイル 1 個＝1 ユーザー（`SHOGUN_DB_PATH`）。`event_log` は `INTEGER PRIMARY KEY`（端末横断 ID 無し、XP-02 未着手）、テナント列無し。プロセス横断の状態がファイル（`l3_approvals.json`, `steer_runs.json`, `llm_rate.json`, `memory_api.json`, `connections_status.json`）。REST は loopback 固定。承認 UI はデスクトップ。
- **マイグレーションは SQLite 方言**（STRICT、FTS5 trigram、vec0、トリガー、`refinery` rusqlite）。Postgres へ移すなら 91 本の書き直し。
- Tauri デスクトップ（86k 行、460 コマンド）、配布・署名・更新・CI ランナー（Mac mini）。
- `target_os="macos"` を含むファイル: `shogun-core` 16、`shogun-integrations` 2、`shogun-mcp` 2、`shogun-memory` 1、`shogun-license` 1（Linux CI は `--exclude shogun-desktop-spike` で全クレート通る）。

### 10.2 プラットフォーム中立（サーバー／サイドカー／ライブラリとして流用可）
- **`shogun-agents`（依存ゼロ）、`shogun-fusion`（agents のみ）、`shogun-redact`（依存ゼロ）、`shogun-license`（検証部）**: そのまま。
- **`shogun-memory`**: bundled SQLite で Linux 動作。state/provenance/confidence/loops/entity_pages/knowledge/memory_identity/search/period_summaries/sanitize/traceability/memory_import は中立。capture 固有（`screen_frames`, `transcript_segments`, `voice_terms`, `mail_*`, `writing_voice`）は不要。
- **`shogun-mcp`**: MCP サーバー（`mcp.rs`）、REST ルーティング（`rest*.rs`、axum feature `server`）、`MemoryBackend` trait、`ApiLevel`/`scope_allows`/`Toolsets`/`source_policy`/`rate_limit`、tool_catalog + tool_loop、scope 表・service_gate・connection FSM は中立（Keychain 読み書きの `memory_api_settings`/`live_tokens`/`launcher_check` は差し替え）。
- **`shogun-core` の中立部**: `dreamcycle/*`（plan/hourly/session/prefilter/loops/pages/entities）、`llm/*`（`BatchClient`/`AgentClient` 型分離、Anthropic/OpenAI Responses/Groq、relay client、SSE、retry、usage、prompt_cache、`PromptTag` 約 30 種）、`ai_search`, `context`, `chat_grounding`, `chat_prompt`, `forms`, `daily_delivery`, `memory_health`, `metrics`。feature `db`/`net`/`exec` でゲート。**`src/bin/shogun_api.rs` / `shogun_mcp.rs` は既に「DB パスを指定して立つ単体メモリサーバー」**。
- **`apps/api`**: Workers + Postgres、Ed25519 トークン検証、reserve/settle 計量、レート制限、ゼロ保持ロギング、構造化出力スキーマ、同期 review エンドポイント — 全部ポータブル（マルチテナント化は認証主体の差し替えのみ）。
- **評価ハーネス 6 種**: 全部 Linux CI。

---

## Pivot-relevant assessment（ShogunAgent への含意）

### A. 本当に差別化されている資産（競合 mem0 / Zep / Letta / supermemory 系が一般に持たないもの）
1. **provenance＋confidence＋逐語 quote 検証付きの状態モデルと supersession**: `state_provenance.quote/batch_reviewed`、base_confidence 半減期減衰、disputes（矛盾の明示）、user_pinned=1.0、merge blocks/split、suppressions、horizon タイマー退役、`loop_signals`（append-only シグナル）→ consolidator（≤6、≥0.84）の二段構え。「記憶が嘘をつかない」ための機構が一箇所（`shogun-fusion/confidence.rs`）で強制され、API 読み取り（FR-API-06）にも同じ規則が適用される。これは **"agent memory with calibrated truth"** として売れる中核。
2. **書き込み権限モデル L1/L2/L3 と approval queue**: エージェントによるメモリ変更・外部送信を型で分類し、人の承認キューに落とす仕組み（`shogun-agents`）。Human-in-the-loop が必要なエージェント基盤で差になる（「エージェントが勝手に記憶を書き換えない」）。
3. **低コスト抽出パイプライン**: セッション化・prefilter・anchor 検証・毎時冪等サイクル・relay 側スキーマ強制・micro-USD 計量。unit economics で**LLM 原価 $68→$2/人・月**まで下げる設計がそのまま「メモリ抽出 API の粗利」になる。
4. **ゼロ保持ゲートウェイ＋digest-only traceability＋redact/fence/instruction-shaped ゲート**: 企業向けエージェントメモリで最初に聞かれる「本文はどこに残るか」「プロンプトインジェクションで記憶が汚染されないか」への回答が実装済み（`pii-bench`, `poison_persist.rs`）。
5. **評価ハーネス**: LongMemEval タクソノミー準拠の memory-bench（abstention/temporal/multi-session/preference）、dedup-bench、pii-bench、extract-eval（provenance 妥当性・較正）、日英 golden search eval。「数字で語れるメモリ製品」の土台。
6. **日本語品質**: NFKC・送り仮名・助詞処理・漢字 bigram 索引で R@5 0.985（JA 中心 132 問）。日本市場向けエージェントメモリとして明確な差。
7. **他アシスタントからの記憶インポート形式** `shogun-memory-import/v1`（ChatGPT/Claude/Cursor/Computer History、低 confidence・provenance=imported）＝ユーザー移行導線。
8. **Memory API の多面対称と MCP 実装**: 158 ツール、core/full toolsets、scope、source policy、ルーティングガイド。Claude Code/Codex/Cursor/Claude Desktop への 1 クリック装着。

### B. コモディティ（差にならない／外部で代替可能）
- FTS5 + RRF の字句検索、SQLite ストレージ、MCP/JSON-RPC 配管、OAuth PKCE、tool loop 自体。
- 埋め込み/ベクトル検索は**意図的に出荷していない**（#237）。mem0/Zep/supermemory はこれを売りにしているため、エージェント向け製品では再導入が前提（`Embedder` trait と vec0 テーブルは残っている）。
- Dream Cycle のスケジューラ／job ledger は堅牢だが一般的。

### C. マルチテナントなエージェント向け API に欠けているもの
| 欠落 | 現状 | 必要な変更 |
|---|---|---|
| テナンシー | 1 SQLCipher ファイル＝1 ユーザー、テナント列なし、INTEGER PK | (a) **テナントごとに SQLite ファイル**を Rust サイドカー（`shogun-api`/`shogun-mcp` バイナリ相当）で持つ（Turso/LiteFS 的）か、(b) Postgres へ移植（91 マイグレーション書き直し、FTS/vec0 の置換）。(a) が既存資産の再利用率が最も高い |
| エージェント認証 | Keychain の grant/トークン、loopback REST、起動元の code signature | API key / OAuth client credentials / 署名付き短命トークン（`apps/api/auth.ts` の Ed25519 形式と `meterSubject` を流用可）、テナント×エージェント×エンドユーザーの 3 軸スコープ |
| 汎用取り込み API | 書き込み経路は `memory.append_note`・`memory.import`・`state.create/update`・コネクタ同期のみ。`source` 語彙はキャプチャ中心 | `memory.add(events[], source, thread_key, actor, metadata)` 的な一般エンドポイント＋会話ログ（role/turn）取り込み、Webhook/ストリーム |
| 意味検索 | 出荷なし（AI 検索は 2 回のモデル呼び出しで代替） | サーバー側 embedding（コスト・ZDR 要確認）or 軽量モデル。`search_document_vec`/`event_vec` は再利用可 |
| 承認フロー（HITL）の遠隔化 | 承認はデスクトップ UI、`l3_approvals.json` | Web/Slack/コールバック承認、approval webhook。`ApprovalQueue`/`ApprovalOrigin` はそのまま使える |
| SDK | MCP stdio・REST localhost・CLI のみ。TS/Python SDK なし | TS/Python SDK、OpenAPI 定義（REST ルート表から生成可能）、Streamable HTTP MCP（現状 stdio のみ、FR-API-01 は HTTP も予定） |
| 共有／ACL | 単一ユーザー前提。`sensitive_sources` が唯一のソース別アクセス制御 | 名前空間・共有メモリ・エージェント間 ACL・監査ログ（traceability を拡張） |
| 課金 | Stripe サブスク（Standard/Pro）、relay の micro-USD 計量 | 従量（ingest 量・検索回数・抽出回数）課金。計量台帳はそのまま、Stripe metered billing の追加 |
| 観測・運用 | SLO メトリクス（端末）、relay `/metrics` | テナント別ダッシュボード、管理 API、エクスポート |
| 24h スケジューラの実行主体 | 端末（Mac が起きている時） | サーバー側ジョブ（Workers cron は既にある。Dream の毎時サイクルを relay 側で動かす） |

### D. 再利用率の見積り
| 領域 | 再利用の見方 | 概算 |
|---|---|---|
| データモデル・マイグレーション（`shogun-memory`） | 概念・テーブル設計はほぼそのまま（state/provenance/signals/pages/period_summaries/imports/notes/traceability）。テナント列や uid の追加マイグレーションが必要。SQLite 継続なら高、Postgres 移行なら低 | SQLite 継続 **60〜70%** / Postgres 移行 **25%** |
| 検索・identity・dedup・redact・sanitize | 中立コード | **80〜90%** |
| `shogun-fusion` / `shogun-agents` / `shogun-redact` | 依存ゼロ、純関数 | **90%** |
| `shogun-mcp` | MCP/REST/CLI 対称層、Level/scope/toolsets/source_policy、tool_loop は流用。158 ツール中デスクトップ固有（meeting/mail/visual_recall/voice/capture/device/settings ≈ 100 本）は不要 | **40%** |
| `shogun-core` | dreamcycle・llm・ai_search・context・chat_grounding・forms は流用、capture/audio/notch/gmail_direct/visual recall は不要 | **25〜30%** |
| `apps/api` + `apps/website` ライセンス API | 認証・計量・ゼロ保持・スキーマ・review エンドポイント・Stripe は流用、認証主体をテナント/API key に | **70%** |
| 評価ハーネス | 全部 | **80%** |
| デスクトップ（Tauri）・capture・macOS アダプタ | 流用不可 | **0%** |

**全体**: 行数ベースでは **約 30〜40%**（macOS/Tauri 層が大半を占めるため）。ただし「難しい部分」＝状態推定の品質規則、ゲーティング、抽出契約、原価設計、評価の資産は **70% 以上**が持ち越せる。最短経路は **「`shogun-api` バイナリをテナントごとの SQLCipher ファイルで多重化し、`apps/api` の認証・計量を前段に置く」** 構成で、新規開発は (1) 汎用 ingest API と conversation 取り込み、(2) テナント/エージェント認証と SDK、(3) サーバー側埋め込み、(4) 遠隔 HITL 承認、(5) 従量課金に集中できる。

---

### 参照パス一覧（主要）
- 要件・不変条件: `/home/user/shogunai-/CLAUDE.md`, `docs/requirements-v1.0.md`（§6.3 メモリ、§6.4 state、§6.5 Fusion、§6.6 エージェント、§6.7 Dream、§6.11 Memory API、§6.12 課金）
- メモリ: `crates/shogun-memory/src/{lib.rs,event_log.rs,state.rs,extract.rs,loops.rs,entity_pages.rs,knowledge.rs,memory_identity.rs,identity.rs,recompute.rs,period_summaries.rs,search_documents.rs,memory_import/,search/{federated,query,terms,lexical,retrieval}.rs,embed.rs,cold.rs,hot.rs,quantize.rs,sanitize.rs,traceability.rs}`、`src/migrations/V1..V91`
- Fusion/学習: `crates/shogun-fusion/src/*`、`crates/shogun-core/src/dreamcycle/{plan,hourly,session,prefilter,loops,pages,entities,jobs}.rs`、`docs/{context-architecture-design,context-layer-audit-and-plan,learned-context,continual-learning-v2-design,daily-summaries-design}.md`
- Memory API: `crates/shogun-mcp/src/{memory_api.rs,mcp.rs,rest_routing.rs,rest_auth.rs,backend.rs,dispatch.rs,toolsets.rs,scope.rs,service_gate.rs,source_policy.rs,rate_limit.rs,live_tokens.rs,memory_api_settings.rs,tool_catalog.rs,tool_loop.rs}`、`docs/memory-api-mcp.md`, `docs/specs/mcp-tool-surface.md`, `docs/mcp/*`, `apps/docs/memory-api/*.mdx`
- エージェント: `crates/shogun-agents/src/*`、`crates/shogun-core/src/{forms.rs,forms/jev.rs,daemon/memory_tools.rs,chat_grounding.rs,llm/prompt_cache.rs}`
- 連携: `crates/shogun-integrations/src/*`、`docs/connector-adapter-plan.md`
- ホスト: `apps/api/src/{app.ts,live-app.ts,auth.ts,usage.ts,spend.ts,pg-store.ts,live-store.ts,review.ts,models.ts,schemas.ts}`、`apps/api/{DEPLOY.md,wrangler.jsonc,sql/*.sql}`、`apps/website/src/app/api/**`、`docs/{batch-relay-design,hosted-ai-gateway-design,hosted-ai-gateway-implementation,account-passkey-design.en,cross-platform-architecture,windows-architecture,mobile-architecture}.md`
- 評価: `crates/{memory-bench,pii-bench,dedup-bench,extract-eval,fusion-eval}/`、`research/{COMPARISON,BENCHMARK-RESULTS,MEMORY-DEDUP}.md`、`research/pii-benchmark/docs/*`、`docs/{search-eval,extract-eval,memory-quality-verification}.md`
- 戦略: `docs/{gpt6-astra-implications,jev-ecosystem-research-and-adoption,llm-combined-product-decisions,sentient-os-competitive-analysis,positioning-category-messaging,select-kk-cost-zero-base-plan,harness-design-and-optimization}.md`、`docs/audits/2026-10-12-shogun-vs-goldfish.md`
- 事業: `docs/{paid-terms-decisions,ops-licenses,unit-economics,prepaid-credits-design}.md`
