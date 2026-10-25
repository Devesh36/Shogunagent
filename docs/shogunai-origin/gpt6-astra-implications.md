# GPT-6 Astra が変える前提と ShogunAI への含意（2026-09-04 調査）

**Status**: Draft v1（2026-09-04）
**用途**: OpenAI の GPT-6 Astra（2026-09-03 発表）と、その直前に出た ChatGPT デスクトップの Computer History / Computer Use を踏まえ、「どんなアプリが出てくるか」「ShogunAI は何を作る・作らないか」を判断するための内部資料。外向けコピーには使わない（競合名を出しているため。`docs/positioning-category-messaging.md` の規律に従う）。
**前提**: 一次情報は openai.com が本セッションからは取得できなかった（403）。以下は OpenAI API changelog（一次）と、9to5Mac / Fortune / VentureBeat / DataCamp / Artificial Analysis / MarkTechPost / learn.chatgpt.com（ChatGPT 公式ヘルプ）等の二次情報を突き合わせたもの。数値はベンダー公表値で、**採用時に必ず公式で再確認**する。

---

## 0. 結論（先に要点）

1. **GPT-6 Astra の本体は「Computer Use と長時間タスクの実務化」**であって、会話やチャットの向上ではない。OSWorld 2.0 で 72.6%、1タスク約40分（前世代 GPT-5.6 Sol は 65.7% / 約75分）、ScreenSpot-Pro 92.7%、SRE-Bench 88%。1.05M トークン文脈、Responses API 限定の tool calling、**非同期 tool calling**、**WebSocket による途中操舵**、**推論努力の会話中変更**、「必要な時だけ質問して止まる」挙動、テンプレ準拠の成果物（文書・表・スライド）生成。
2. **ShogunAI にとっての本当の変化は Astra 単体ではなく、OpenAI が 7〜9 月で Mac 上の L1（取得）と L3（実行）を両方押さえに来たこと。** 7/9 に ChatGPT デスクトップが Chat / Work / Codex に統合、8/13 に **Computer History**（Accessibility 経由のクリック・入力・アプリ切替のイベント取得 → 一時的 Codex セッションで Markdown メモリに要約、ローカル保存）、9/3 に Astra で **Mac ではバックグラウンド動作する Computer Use**（Screen Recording + Accessibility 権限、アプリ単位承認、ロック中実行）。ShogunAI の 5 層のうち L1 と L3 の「見た目」は同じものが配布力を持って出てきた。
3. **それでも ShogunAI が持っていて OpenAI が持っていないものは明確**: (a) provenance + confidence 付きの **state tables＝ワールドモデル**（向こうはフラットな Markdown メモリ）、(b) **暗号化・有限保持・除外・機構としてのプライバシー**（向こうはメモリファイルが平文で「同一ユーザーの他プログラムから読める」と自ら明記、EEA/UK/CH では当初提供なし、セキュリティ各社が「業務 Mac では切れ」と勧告）、(c) **モデル非依存**（Claude / Codex / Gemini のサブスク委譲を既に実装済み。`crates/shogun-core/src/llm/subscription.rs`）、(d) **L1/L2/L3 と draft-stop という構造的な送信ガード**（向こうは「実行中は目を離すな」型）、(e) **Notch という常駐面**。
4. **推奨方針は「Computer Use を自作しない。Astra 級の実行エンジンにワールドモデルと承認境界を貸す側に回る」**。v1 スコープ表の「Computer Use = Phase 3」は維持し、代わりに (1) Codex 委譲で Astra を即日使えるようにする、(2) `ExternalExecute` を L3 として追加し「Codex / ChatGPT の Computer Use に仕事を渡す」ハンドオフを作る、(3) Memory API を「他社エージェントが装着する文脈層」として磨く、(4) Computer History のメモリを #384 の経路でインポートできるようにする、の 4 本を先に打つ。
5. **やらないこと**: スクリーンショット駆動の自前 CUA、Realtime 音声、Batch lane の Astra 化（$10/$50 は Haiku 級の10倍）、他アプリの資格情報・ブラウザプロファイルの読み取り（#311 の Hermes 方式は不変条件に反する）。

---

## 1. GPT-6 Astra の事実整理（何が新しいか）

### 1.1 モデルと API

| 項目 | 内容 | 出典 |
|---|---|---|
| モデル ID | `gpt-6-astra`。OpenAI API と Amazon Bedrock、Azure（Microsoft Foundry） | changelog / Azure |
| 文脈 | 1.05M トークン（入力最大 922K、出力最大 128K）。MRCR v2 8-needle 512K–1M で 96.3% | DataCamp / digitalapplied |
| 価格（Standard） | 入力 $10 / キャッシュ読み $1 / キャッシュ書き $12.50 / 出力 $50（per 1M）。**272K を超える入力は入力・キャッシュが2倍、出力が1.5倍**（リクエスト全体に適用）。Batch/Flex は半額、Fast は2倍（2.5倍速） | DataCamp / Artificial Analysis |
| 入出力 | テキスト＋画像入力、テキスト出力のみ。**Realtime・音声・画像生成・embedding・fine-tuning は非対応** | changelog / DataCamp |
| API 形状 | tool calling は **Responses API 限定**（Chat Completions は移行案内）。Assistants API は 8/26 に終了 | changelog |
| 新機能 | **非同期 tool calling**（呼び出しを投げてモデルは推論を継続、後で結果を消費）、**WebSocket による途中操舵**（実行中に指示更新）、**推論努力（low/medium/high/xhigh/max。none は無し）の会話中変更でキャッシュ維持**、temperature/top_p/logprobs 不可 | changelog |
| 挙動 | 「重要な分岐では短く質問して止まる」（デモでは 20 秒で職種を確認。Sol は 13 分黙って進めた）。テンプレ準拠の成果物（文書・表・スライド）を「レビュー可能な状態」で出す | DataCamp / Azure |
| Codex | 「searchable notes」: 文脈窓を跨いで要約圧縮せずメモを保持し、過去の要件・テスト結果を検索できる（実験的、数週で既定化）。非同期質問（作業を止めずに聞く）。Mind2Web で 1.9 倍高速 | 9to5Mac / DataCamp |
| 安全 | Preparedness Framework の **サイバー Critical** に初到達。Daybreak プログラム経由で段階展開。**API では不審トラジェクトリの監視で「タスクが止まる」**（ChatGPT/Codex はレビュー要求）。ZDR は適格顧客のみ。知識カットオフ 2026-04-30 | Fortune / DataCamp / changelog |

### 1.2 ベンチ（ベンダー公表。独立検証は限定的）

| ベンチ | Astra | 比較 |
|---|---|---|
| OSWorld 2.0（デスクトップ操作） | 72.6% / 約40分 | Sol 65.7% / 約75分、Claude Opus 5 70.2% |
| ScreenSpot-Pro（画面要素の位置特定） | 92.7% | Sol 76.9%、Claude Fable 5 87.3% |
| SRE-Bench | 88.0% | Sol 55.9% |
| Terminal-Bench 4.0 | 57.7% | Fable 5.1 55.8% |
| Agents' Last Exam | 59.3% | Opus 5 55.5%（出力トークン約65%減） |
| Humanity's Last Exam (tools) | 57.2% | **Fable 5.1 65.0% に劣後** |
| Computer-use safety（低いほど良い） | 2.4% | Sol 22%、Fable 5.1 9.5% |
| ARC-AGI-3 | 99.9%（独自ハーネス） | ARC Prize の標準ハーネスでは 17–63%。鵜呑みにしない |

読み方: **「画面を見て正確に操作し、長時間迷わず、成果物を仕上げる」方向に全振り**。純粋な知識・推論では他社と拮抗か劣後する領域もある。ShogunAI が価値を置く「状態推定の精度」はこのモデルだけで自動的に解決しない。

### 1.3 同時に起きているプラットフォーム側の動き（Astra より重要）

| 日付 | 出来事 | ShogunAI への直接的意味 |
|---|---|---|
| 2026-07-09 | ChatGPT デスクトップ（Mac/Win）が Codex アプリを土台に **Chat / Work / Codex** に統合。旧アプリは「ChatGPT Classic」 | 常駐エージェントの配布面を OpenAI が取った。Free 含む全プラン |
| 2026-08-13 | **Computer History**（Mac）: Accessibility 経由で「クリック・入力・ショートカット・アプリ切替・アクセシビリティ文脈」をイベント取得（スクショ・音声なし）。イベントは 48h でローカル削除、**一時的な Codex セッションが要約して `~/.codex/memories/extensions/skysight/` に Markdown で保存**。クラウド同期なし。**平文・非暗号化**で「同一 macOS ユーザーの他プログラムが読める」と公式が明記。既定 OFF、Pro/Business/Enterprise、API 不可、除外/include-only/一時停止あり。「プロンプトインジェクションのリスクを高める」と公式が明記 | ShogunAI の L1（AX テキスト取得）と L2 の一部（メモリ）の**同型機能**。ただし設計思想は「ログを Markdown に要約」であって、state tables でも provenance でも confidence でもない |
| 2026-09-03 | **Computer Use**（ChatGPT デスクトップ、Work/Codex アカウント）: Screen Recording + Accessibility 権限、**アプリ単位の承認（Always allow）**、**Mac ではバックグラウンド実行**（Windows は前面のみ）、ロック中実行（authorization plug-in）、ターミナルと ChatGPT 自身は操作不可 | ShogunAI の L3（実行）を「MCP 構造化操作」ではなく「GUI 直接操作」で先に実装された。承認モデルはアプリ単位であって**操作の種類（送信か読み取りか）単位ではない** |
| 2026-08-26 | `whisper-1` / `gpt-4o-transcribe` 系が 2027-02 に終了、`gpt-live-transcribe` / `gpt-transcribe` へ | 会議 ASR は Deepgram BYOK で無関係。OpenAI の音声は Astra とは別系統のまま |

---

## 2. 前提がどう変わるか（5つ）

1. **「エージェントが GUI を操作する」が実用域に入った。** 40分/タスクは人間の代替として遅いが、バックグラウンドで並列に走るなら十分。「API がないサービスは自動化できない」という前提が崩れ、**MCP 直結を待たずに GUI で片付ける**選択肢が標準になる。
2. **文脈の問題が「窓の大きさ」から「窓に何を入れるか・いくらかかるか」に完全に移った。** 1M 窓を満たすと 1 回 $10、272K を超えると割増。キャッシュ読みは $1。したがって「凍結プレフィックス＋小さな高信号ワーキングセット」という既存方針（`docs/harness-design-and-optimization.md` R2/R9）は**むしろ正しさを増した**。差がつくのはワーキングセットの質＝ワールドモデル。
3. **モデル側が「いつ人間に聞くか」を判断し始めた。** Astra は重要分岐でだけ止まる。Codex は非同期に質問する。これはアプリ側の承認 UI が「全部止める」から「**質問を受け取って作業を続けさせる**」形へ変わることを要求する。ShogunAI の L3 承認キューは「送信前に止める」設計であり、そこは残しつつ、**L2 相当の「質問」アイテム**が必要になる。
4. **実行中に操舵できることが前提になる。** WebSocket による途中指示更新、非同期 tool calling。「ボタンを押して待つ」から「押した後に一言足せる」へ。Notch は本来この UI に向いている（常駐・非アクティベート・一言入力）。
5. **安全側が「監視で止まる」ことを公然と組み込んだ。** API では不審トラジェクトリで**タスクが止まる**。ZDR は適格顧客のみ。BYOK/サブスク委譲でユーザーの資格情報を使う ShogunAI は、この「止まった」を**ユーザーの作業を中断しない形**で表示する義務がある（不変条件のエラー処理原則）。また Computer History と Computer Use はどちらも EEA/UK/CH で当初提供なしで、**地域規制がプライバシー設計の競争軸を残した**。

---

## 3. 出てくるアプリケーション／ユースケース（予測）

### 3.1 伸びるもの

| カテゴリ | 具体 | 誰が有利か |
|---|---|---|
| **バックオフィス GUI 自動化** | 経費精算フォーム入力、CRM 更新、請求書の突合、SaaS 管理画面の設定変更、QA の手動テスト。「RPA の再発明」だが録画不要・変更耐性あり | OpenAI 自身（ChatGPT Work）、Anthropic、Azure Foundry 上の SI。**単能 RPA ベンダーは不利** |
| **「成果物まで」型のナレッジワーク** | 会議→議事録→提案書→スライドを一気通貫。テンプレ準拠・ブランド準拠の出力。「レビューする側」に人間が回る | Microsoft（Office 統合）、Notion 系、そして「文脈を持っている側」 |
| **長期タスクの委任（分〜時間）** | 「今週の未返信を全部下書きして」「このリポジトリのフレーク検出を直して」。非同期質問で人間を巻き込みつつ進む | Codex / Claude Code 系のハーネス、それを **スケジュール実行**できる側（#252 の方向） |
| **個人向けエージェント OS** | ChatGPT デスクトップの Chat/Work/Codex ＋ Computer History ＋ Computer Use ＝ OpenAI 版「個人 AI OS」。**ShogunAI と同じ盤面** | 配布力は OpenAI。差別化はワールドモデル・プライバシー・モデル非依存 |
| **セキュリティ運用** | 脆弱性検証、パッチ提案、SRE 障害対応（SRE-Bench 88%）。Daybreak で段階解放 | セキュリティ専業。ShogunAI の対象外 |
| **他社エージェントの「文脈供給層」** | エージェントは強くなったが「誰が誰で、何を約束したか」は持っていない。**MCP で文脈を貸す**ビジネス | **ShogunAI の最も自然なポジション**（Memory API は既に実装） |
| **エージェントの監査・可視化** | 何を見て何をしたかのトレース、承認境界、予算ガード。Azure も「承認ワークフロー・人間チェックポイント・監視」を並べている | ガバナンス製品。ShogunAI はトレーサビリティを既に持つ |

### 3.2 縮むもの

- **「録って要約する」だけの製品**（会議ノート、画面録画系、ディクテーション）。Computer History が無料で「何をしていたか」を覚え、Astra が続きをやる。`docs/positioning-category-messaging.md` §2 の「3つの壁」論はそのまま強まる。
- **API 依存の薄い統合レイヤー**（「SaaS を繋ぐだけ」）。GUI 操作が API 直結の代替になるため、統合の価値は「繋ぐ」から「**何を繋ぐべきか知っている**」へ移る。
- **モデル固定の常駐アシスタント**。サブスクが複数化する中で「Claude でも ChatGPT でも Gemini でも動く」が前提になる。

---

## 4. ShogunAI の現状との突き合わせ（コード実査）

| 層 | 実装済み（変えない資産） | 未配線・欠落（Astra 後に効く順） |
|---|---|---|
| L1 取得 | AX 走査・除外・私的閲覧除外・dedup（`crates/shogun-core/src/capture/`）、Visual recall（CGWindow → Apple Vision OCR、暗号化 `screen_frames`、有限保持。`apps/desktop/src-tauri/src/screen_ocr.rs`）、会議 ASR（Deepgram BYOK / whisper.cpp、RAM のみ） | AXObserver 一本化（`docs/context-architecture-design.md` §4.2、#355/#357 が関連） |
| L2 ワールドモデル | state tables＋provenance＋confidence、Hot/Warm/Cold、FTS5 trigram、Context Fusion（`crates/shogun-fusion`）、lessons（L5-v1） | 二段抽出の Batch 精緻化が未配線（confidence 上限 0.4 のまま。同 §4.1、#287）。Work Patterns（#242） |
| L3 実行 | L1/L2/L3 と `OpKind`（`LocalCompute/DraftGenerate/StateWrite/ExternalRead/ExternalSend`。`crates/shogun-agents/src/presets.rs`）、承認キュー、draft-stop、`LocalAction` 7 種（`permission.rs`） | **tool loop（#81、`crates/shogun-mcp/src/tool_loop.rs`）は実装済みだがテストからしか呼ばれていない**。「第三者上の可逆な書き込み」カテゴリ未実装（`docs/cross-platform-architecture.md` D7） |
| LLM 抽象 | `BatchClient` / `AgentClient` の鍵レーン分離（`llm/mod.rs`）、Anthropic・OpenAI 互換・**サブスク委譲 3 種（claude / codex / gemini-cli）**、ストリーミング、usage・traceability sink | OpenAI 互換は **Chat Completions のみ**（`openai_compat.rs` 冒頭）。Responses API 未実装＝Astra の tool calling 不可。BYOK 既定 `gpt-4o-mini`（`inline_source.rs:132`）は世代遅れで、**設定 UI にモデル選択欄が無い**（プロバイダごとの既定を Rust から引くだけ。`App.tsx` の `PROVIDERS`）。**Codex 委譲はストリーミング無し**（`subscription.rs` `streaming_args`）。`OpenAiCompatConfig.reasoning_effort` は存在するが UI から触れない |
| API 対称 | Memory API 43 ツール（`memory.search` / `memory.get_context_pack` / `state.*` / `actions.execute` / `visual_recall.*` 等。`crates/shogun-mcp/src/memory_api.rs`）を MCP / REST / CLI の 3 面で対称に公開。LLM 向け連携ツールカタログ 16 種（`tool_catalog.rs`、送信系は常に提案止まり） | 「他社エージェントが ShogunAI の文脈を装着する」ための素材は揃っている。欠けているのは**そのための導線と文言**（Codex / Claude Code / ChatGPT の MCP 設定に 1 クリックで登録する体験） |
| UI | Notch、Full UI、承認 UI、Scribe（インライン編集） | 「質問」型の承認アイテム無し。実行中の操舵入力無し |

---

## 5. 提案（優先順）

### P0 — 今すぐ・安い（1〜2 週）

**5.1 Astra を Codex 委譲で即日使えるようにする**
- 既存 `Delegate::Codex`（`codex exec -`）はモデル名を持たない設計（「その CLI がそのプランで既定にするモデル」）。Astra が Codex 既定になれば追加コードなしで乗る。確認事項: Codex CLI が Astra を既定化する時期、`-m gpt-6-astra` 相当の明示指定の可否、**推論努力（`model_reasoning_effort`）を渡す方法**。
- Codex 委譲の**ストリーミング**が `None` のため SLO-03（初トークン 1s）が満たせない。`codex exec --json`（イベント JSONL）の安定性を再調査し、`StreamFormat` に追加する。
  - **【2026-09-25 オーナー決定｜#388 / #485】** `codex exec --json` は回答を一括で返しストリーミングできないため、**Codex 委譲は SLO-03 の対象外**とする（定義は `docs/requirements-v1.0.md` NFR-SLO-03）
- BYOK 既定モデルの更新: `inline_source.rs` の `"openai" => "gpt-4o-mini"` を現行の廉価モデルへ。**Astra を BYOK 既定にしない**（Scribe のような高頻度・短文用途で $10/$50 は不適。単価はユーザー資格情報でも予算ガード R8 の対象）。
  - **【2026-09-26 実装｜#389】** プロバイダ別の厳選リストは `shogun_core::model_catalog`（Rust 所有、UI と MCP / REST / CLI の単一ソース）。各プロバイダに `default`（チャット等の既定）と `budget`（⌥ タップ下書き・Scribe・音声整形の既定）を持つ（2026-09-26 に各社価格ページで確認）: Anthropic `claude-sonnet-5` / `claude-sonnet-5`、OpenRouter `anthropic/claude-sonnet-5` / 同、**OpenAI `gpt-6-sol` / `gpt-6-luna`（2026-09-26 オーナー決定：OpenAI の既定は Sol、高頻度レーンは Luna のまま）**、Gemini `gemini-3.8-flash` / 同。deep（明示選択のみ・高コスト表示）: `gpt-6-astra`、`claude-opus-5-5`、`claude-fable-5-1` ほか。`INLINE_DRAFT` / `SCRIBE_EDIT` / `VOICE_EDIT` は `budget_only`：明示的に budget モデルを選んだとき以外は `budget` で走る（deep・自由入力は乗らない、R8）。旧既定（`gpt-4o-mini` / `anthropic/claude-sonnet-4.5` / `gemini-2.5-flash`）は起動時に `llm.json` から消して現行既定へ（DB は触らない）。リスト外の保存値は既定で走り、設定画面と `agent.source.get`（`model_choice: unknown`）に注記。書き込みはリスト外を `unknown_model` で拒否。**例外（2026-09-26 オーナー決定）: OpenRouter のみ自由入力のモデル ID を受け付ける**（形式チェックのみ、`model_choice: custom`、料金不明表示、UI は「Other model…」）。OpenAI の Chat Completions には `max_completion_tokens` を送る。ハンドオフブリーフの deep 既定は未実装（ハンドオフ自体が #5.5 の範囲）

**5.2 OpenAI クライアントを Responses API 対応にする（`openai_compat.rs` の隣に `openai_responses.rs`）**
- Astra の tool calling は Responses 限定。#81 の tool loop を Astra で動かす前提条件。
- 同時に `reasoning.effort` と `previous_response_id`（文脈継続＋キャッシュ維持）を扱う。非同期 tool calling は tool loop が並列 tool_result を一括返却できる形（R5）に揃えてから。
- **【2026-09-25 オーナー決定｜#664】`store` は既定 Off**（プロバイダ側に応答を保存させない。`OpenAiResponsesConfig::store` の既定 `false` と一致）。**ユーザーが設定で明示的に opt-in した場合のみ On** にし、そのときだけ `previous_response_id` 継続を使う。設定の追加（UI / MCP / REST / CLI の対称性を含む）は別 PR  
  - **【2026-09-26 実装】** 設定は `llm.json` の `openai_store`（`agent_source::LlmSettings`、serde 既定 `false`。旧ファイルは Off として読む）。`OpenAiResponsesConfig::for_agent` が `provider == "openai" && openai_store` のときだけ `store: true` を立てる（`build_agent` への配線は #390 の次スライス）。UI は Settings › Privacy のプロバイダ選択の下、OpenAI 選択時だけ出るトグル「Let OpenAI store responses (enables faster continuation)」と1行の開示（30日以上保持）。API は `agent.source.get` が `openai_store` を返し、`agent.source.set` の `openai_store: false` は直接（L1）、**`true` は API では拒否**（`openai_store_only_in_app`。L3 で保留にはしない。第三者保持を延ばす緩和は開示のあるトグルでのみ、`connections.composio.set` と同じ扱い）。CLI は `shogun agent-source openai-store on|off`

**5.3 Computer History を「引っ越し元」として扱う（#384 の拡張）**
- `~/.codex/memories/extensions/skysight/*.md` はユーザーの資産で、平文で置かれている。**ユーザーがフォルダを明示的にドロップした時だけ**読み、#384 の `shogun-memory-import/v1` 経路（ローカル検証・重複排除・provenance=`imported`・confidence 低め）へ流す。自動探索・常時読み取りは**しない**（「他アプリのファイルを読むコードを書かない」の精神。資格情報ではないが同じ規律で扱う）。
- 効果: 「ChatGPT で試した人が ShogunAI に来る」導線。差分は「Markdown の要約」→「provenance 付きの state」。

**5.4 プライバシーを「機構」として対外的に明文化する（#292 を前倒し）**
- 競合が公式に「平文・他プロセスから可読・インジェクション注意」と書いた今、ShogunAI の SQLCipher・Keychain・有限保持・除外（#383）・私的閲覧除外・「画像は Visual recall ON の時だけ暗号化 DB に有限保持」を**比較可能な形**で 1 ページにする。LP/UI では競合名を出さない（ブランド規律）。

### P1 — 今期（4〜8 週）

**5.5 `ExternalExecute` を L3 として追加し「Astra 級エージェントへのハンドオフ」を作る（Computer Use を自作しない）**
- `OpKind` に `ExternalExecute`（mandated L3）を追加。D7 の capability 軸で言えば `execute`。承認キューは `SendAction` しか受けない型保証を**緩めず**、`HandoffAction` を新設して同じキューに流す。
- 中身: Context Fusion が組んだ文脈（関係者・約束・open loop・関連スレッド、confidence 帯付き。`memory.get_context_pack` が既にほぼこの形）＋制約（送信禁止・対象アプリ・完了条件）を **1 枚のタスクブリーフ**に落とし、承認後に (a) `codex exec` / `claude -p` にそのまま渡す、または (b) クリップボード経由で ChatGPT Work / Computer Use に貼る。ShogunAI は「見て・覚えて・境界を決める」側、GUI 操作は向こうに任せる。委譲先には ShogunAI の MCP サーバー（`shogun-memory`）を道具として与え、実行中に `memory.search` で根拠を引かせる。
- Skills（#57 / #251）のマニフェストがそのままブリーフの雛形になる。#252 のスケジュール実行と組み合わせると「毎週金曜に Codex に週報を作らせる」が成立する。
- 実行エンジン側の型保証（承認キューは `SendAction` のみ、`ExecutionEngine::submit` は L3 を拒否）は緩めない。`HandoffAction` は別キューか、`ApprovalQueue` を enum 化して両方を受ける形にし、`invariant4.rs` に「ハンドオフからも送信は起きない」を足す。
- 要件更新: `docs/requirements-v1.0.md` §3.2 の「Computer Use = Phase 3」は維持し、**「委譲によるハンドオフは v1.5」**として行を足す（スコープ運用 §3.3 に従い改版を伴う）。

**5.6 承認モデルに「質問」を足す（L2 Clarify）**
- Astra / Codex は途中で短く聞く。`Disposition` に `NeedsInput{question, options}` を追加し、Notch に「質問カード」を出す。回答は L3 承認とは別の軽い操作（1 タップ）。未回答でも作業は継続し、期限で「保留」にする（非同期質問のミラー）。
- API 対称: `actions.status` に `awaiting_input` を追加、REST/CLI で回答できるようにする。

**5.7 実行中の操舵（Notch から一言足す）**
- Astra の WebSocket 途中操舵、Codex の途中指示。ShogunAI 側は「実行中アクションに追記メッセージを投げる」経路を `actions.execute` の続きとして設ける。Notch の一行入力を再利用。委譲先が非対応なら「次ターンに反映」にフォールバック。
  - **【2026-09-26 実装（第1スライス）｜#395】** 規則は `shogun_agents::steer`（1 行・500 字・1 ターンあたり 3 件まで、`unknown_run` / `run_finished` / `too_many_notes` などの拒否コード、user 半分へのフェンス付き追記 `append_to_user_half`。承認・権限の語彙を持たないことをソースガードで固定）。プロセス間の受け渡しは DB ではなく `steer_runs.json`（DB の隣、`l3_approvals.json` と同じロック・0600・原子的書き込み。本文は run が読むか終わるまでだけ保持）で、マイグレーションなし（#533 の V55 と番号が衝突するため）。API は `actions.steer {kind, id, text}`（L1、MCP / `POST /v1/actions/steer` / `shogun actions steer`）。配送は全経路「次ターン境界」（`delivery: next_turn`）。**未配線**: tool loop / tool chat（#533 が大改修中）での `open_run` → 各ターン境界で `take_for_turn` → `close_run` の 3 呼び出しと、Notch の実行中カードの「一言足す」入力（`App.tsx`、同じく #533）。skill run は 1 回の呼び出しでターン境界が無いため対象外
  - **【2026-09-27 配線｜#395】** #533 / #635 のマージ後に配線済み。tool chat（`apps/desktop/src-tauri/src/tool_chat.rs`）が Anthropic ソースを `steer_api::SteeredTurnSource` で包み、run 開始で `open_run`、各モデルターンの直前に `take_for_turn` → ツール結果の後ろの user 半分へ追記、終了（回答・エラー・Stop）で `close_run`。`ModelTurnSource` に `accepts_user_notes` / `push_user_note`（既定は受けない）を足したので、ノートを運べないソースでは run 自体を開かない（`unknown_run`）。Notch はツールを使うチャットの実行中だけ 1 行入力（`chat_steer_open` を 1 回受けて表示、ポーリングなし）。最後のターンの後に届いたノートは `chat_done.steer_unread` で状態行に出す。Notch の context action（`notch_exec`）は 1 回呼び出しでターン境界が無いので対象外のまま。実ループ＋偽モデルの統合テスト `crates/shogun-mcp/tests/steer_loop.rs` で「送って」と書いたノートでも送信は L3 提案で止まることを固定

**5.8 tool loop（#81）を製品に配線し、並列 tool_result を前提にする**
- 5.2 と 5.5 の土台。読み取り系 MCP（Calendar / Drive）を「Astra が使う道具」として露出。ここで初めて「Astra が ShogunAI のメモリを道具として使う」体験（`memory.search` を tool として呼ぶ）が成立する。

**5.9 「成果物」型の L2 ローカル書き出し**
- Astra はテンプレ準拠の文書・表・スライドを作れるが**出力はテキストのみ**。ShogunAI が Markdown/CSV/`.docx`/`.pptx` へローカルで組み立てる `LocalAction::WriteFile`（L2、ユーザー指定フォルダ限定）を追加。会議 Recap→議事録ファイル、Morning Brief→週次レポートの雛形。外部送信ではないので L3 に引っかからない。

### P2 — v2 に向けて

**5.10 「深い文脈」モードの予算設計**（長文脈は原則使わない、の例外を明示）
- 週次レビュー・プロジェクト回顧のような**ユーザーが明示的に押す重い操作**に限り、Warm 層のスレッドパックを大きく渡す。Rust コアで**272K 未満**（割増境界）をハードキャップにし、実行前に見積もりトークン・金額（またはサブスク枠消費の目安）を表示。日常の Notch アクションには適用しない（R9）。

**5.11 ワールドモデルの可読化・矛盾フラグ（#290 / #253）の格上げ**
- エージェントが自律になるほど、人間の仕事は「実行」から「**ワールドモデルの監査**」へ移る。Computer History の Markdown を人が読んで直す体験に対し、ShogunAI は「根拠付きで直せる」体験を出す。P1 の後の最優先。

**5.12 「止められた」の表示**
- API 監視でタスクが止まる、Codex がレビューを要求する、を **Notch インジケータの琥珀色＋公的エラー文言**で扱う。公開文言表: `docs/public-error-copy.md`。

**5.13 Windows は急がない**
- ChatGPT Computer Use は Windows では前面専有・VM 推奨。Mac のバックグラウンド実行が差別化点である間は、`docs/cross-platform-architecture.md` の「不可逆なものだけ先に」を維持。

---

## 6. やらないこと（根拠つき）

| 提案されがちなこと | 判断 | 根拠 |
|---|---|---|
| スクリーンショット駆動の自前 Computer Use | **やらない（Phase 3 のまま）** | 安全面（誤クリック・資格情報画面）を自社で負うコストが高く、Astra / Claude 側が既に持つ。ShogunAI の差分は操作ではなく文脈と境界 |
| Realtime 音声で会議に同席（#113 の方向） | **Astra では不可能** | Astra は Realtime・音声非対応。会議 ASR は Deepgram BYOK のまま |
| Batch lane（Dream Cycle / Brief / Recap）を Astra に | **やらない** | Select KK レーンは Haiku 級（$1/$5）が主。Astra は $10/$50、Batch でも $5/$25。不変条件 5 の鍵分離も変わらない |
| ブラウザプロファイルのコピーで「本人としてログイン」（#311） | **やらない** | 「他アプリの資格情報ファイル・Keychain を読むコードを書かない」に抵触。代替は 5.5 のハンドオフ（ChatGPT Computer Use はアプリ単位承認で本人セッションを使う） |
| 長文脈にメモリを詰め込んで検索を置き換える | **やらない** | 1M 窓 1 回 $10、272K 超は割増。retrieval 8K トークンなら約 $0.08。R9 のとおり |
| Visual recall のフレーム画像を Astra（画像入力可）に送って「これ何だった？」を答えさせる | **オーナー判断なしにやらない** | `screen_frames.rs` は「将来の vision 入力用」と注記しているが、画像は生データであり不変条件 3 の新しい例外になる。やるなら OCR テキストのみ送る現行方式を既定に残し、画像送信は明示 opt-in＋トレース必須 |
| Astra 向けにプロンプトを作り直す | **やらない（同一性を保つ）** | R7。委譲先が変わっても system/user の分離（`complete_split`）と fence は共通 |

---

## 7. オーナー判断が必要な事項

1. **5.5 のハンドオフ先の既定**: Codex CLI（ヘッドレス、テキストのみ）か、ChatGPT Work / Computer Use（GUI 操作、貼り付け経由）か。前者は API 対称にしやすく、後者は「画面を操作する」体験が出る。
2. **`ExternalExecute` の課金位置**: エージェント実行（2026-09-24 以降 Standard 以上）に含めるか、別ゲートか。委譲先の枠を消費するため、5.10 と同じ「実行前見積もり」を必須にするか。
3. **Computer History インポート（5.3）の扱い**: 「ユーザーが明示的にドロップ」で不変条件の精神を満たすと判断してよいか。
4. **要件書 §3.2 の改版**: 「委譲によるハンドオフ = v1.5」行の追加と、`docs/skills-concept-and-ui-design.md` へのブリーフ雛形の追記。
5. **BYOK 既定モデルの更新方針**: OpenAI 側の廉価モデル（現行世代）を確認して差し替える。Astra は「深いタスク」用の明示選択肢に留める。

---

## 8. 出典

一次:
- OpenAI API changelog（2026-09-03 GPT-6 Astra、Responses API 非同期 tool calling・WebSocket 操舵・推論努力変更、2026-08-26 transcribe 系終了）: https://developers.openai.com/api/docs/changelog
- ChatGPT 公式ヘルプ Computer Use: https://learn.chatgpt.com/docs/computer-use
- ChatGPT 公式ヘルプ Computer History: https://learn.chatgpt.com/docs/customization/computer-history
- OpenAI 発表ページ（本セッションでは 403 で未取得）: https://openai.com/index/gpt-6-astra/

二次:
- 9to5Mac（ChatGPT/Codex への影響、Mac バックグラウンド動作、Codex notes）: https://9to5mac.com/2026/09/03/openai-releasing-major-upgrade-to-chatgpt-and-codex-with-gpt-6-astra-details-here/
- Fortune（Computer Use、Brockman 発言、Daybreak、Critical）: https://fortune.com/2026/09/03/openai-debuts-gpt-6-astra-computer-use-greg-brockman-says-start-of-agi/
- DataCamp（ベンチ表、価格、Codex 機能、ARC-AGI-3 の注記）: https://www.datacamp.com/blog/gpt-6-astra
- digitalapplied（価格明細、272K 割増、推論努力段階、非対応機能）: https://www.digitalapplied.com/blog/gpt-6-astra-price-benchmarks-guide
- Artificial Analysis: https://artificialanalysis.ai/models/gpt-6-astra
- VentureBeat（企業向け、ガバナンス）: https://venturebeat.com/technology/welcome-to-the-agi-era-openai-launches-gpt-6-astra
- Microsoft Azure Blog（Foundry、用途、価格帯）: https://azure.microsoft.com/en-us/blog/gpt-6-astra-frontier-intelligence-for-work-now-available-in-microsoft-foundry/
- dev.to（1M 窓のコスト試算）: https://dev.to/gabrielanhaia/filling-gpt-6-astras-1m-token-window-costs-10-a-call-1m46
- Help Net Security（Computer History のセキュリティ評価）: https://www.helpnetsecurity.com/2026/08/19/openai-computer-history-privacy-risks/
- Elephas / The AI Career Lab（Computer History の挙動整理）: https://elephas.app/resources/chatgpt-desktop-computer-history-privacy / https://theaicareerlab.com/blog/chatgpt-computer-history-mac-privacy-2026
- Codex と ChatGPT デスクトップ統合（2026-07-09）: https://help.openai.com/en/articles/20001276-moving-to-the-new-chatgpt-desktop-app
