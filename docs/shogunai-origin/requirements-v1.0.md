# SHOGUN 要件定義書 v1.0

> **2026-10-13 #1101 改定:** hosted AI / browser sign-in は承認済みの目標仕様で、実装完了の記録ではない。移行順序・ホスティングの相違・未決事項は [gateway 設計](hosted-ai-gateway-design.md) を参照。

| 項目 | 内容 |
|---|---|
| 文書ID | requirements-v1.0 |
| 対象プロダクト | SHOGUN (ShogunAI) — macOSアプリ |
| ステータス | Draft for Phase 1（Phase 0 Go判定待ち） |
| 最終更新 | 2026-08-17（実装済み機能の要件を追補: §6.3.5 スレッド／Context Pack、§6.17 Evening Wrap、§6.18 参照解決、§6.19 AIセッション取り込み、§6.20 Patterns記録、§6.21 視覚キャプチャ） |
| 上位文書 | `/CLAUDE.md`（運用ルール。本書と矛盾する場合はCLAUDE.mdの絶対不変条件が優先） |
| 関連文書 | `docs/notch-ui-prototype-spec.md`（ノッチUIスパイク仕様）、`docs/phase0-dev-instructions.md`（Phase 0 開発指示書） |

## 本書の位置付け（必読）

- 現在の開発フェーズは **Phase 0（ノッチUIスパイク）** である。本書は **Phase 0のGo判定後に始まるPhase 1（v1）実装** の要件を定義する。Phase 0の間、本書の要件を先行実装してはならない。
- Phase 0の詳細仕様（常駐安定性・展開100ms・cache300ms+CPU5%・ホバー誤発火の4つの問い、およびNo-Go時のメニューバー＋パレット方式への転換基準）は `docs/notch-ui-prototype-spec.md` に完全準拠する。本書ではノッチUIスパイクの内容を重複記述しない。
- 本書の各要件（FR-xx / NFR-xx）は、実装者（人間またはAIエージェント）が**受け入れ基準**として参照できる粒度で書かれている。要件の変更はConventional Commits（`docs:`）でバージョン管理し、CLAUDE.mdの絶対不変条件7項に抵触する変更は行わない。
- 記法: **MUST** = 必須（違反は受け入れ不可）、**SHOULD** = 強い推奨（逸脱には判断記録が必要）、**MAY** = 任意。
- FR/NFR番号は安定識別子である。要件の統合・削除で欠番が生じても再採番しない（既存参照を壊さないため）。

---

## 1. プロダクト定義とビジョン

### 1.1 一言定義

ユーザーの仕事の**ワールドモデル**を構築し、Macのノッチから「ボタンを押して仕事が終わる」体験を提供する、ローカルファーストのAI OS。

SHOGUNは記録ツールではない。**状態の推定と実行**のプロダクトである。

### 1.2 ワールドモデルとは何か

SHOGUNにおけるワールドモデルとは、「ユーザーの仕事世界の現在状態の構造化表現」である。具体的には次の4つの問いに常時答えられるデータ構造を指す:

1. **誰と**仕事をしているか（people）
2. **何の**プロジェクトが動いているか（projects）
3. **何を約束**しているか（commitments）
4. **何が未完了**のまま開いているか（open_loops）

生のイベントログ（画面で何が起きたか）と、そこから推定された状態（state tables）を分離し、状態には必ず根拠（provenance）と確度（confidence）を付与する。これが「記録」と「ワールドモデル」の違いである。

### 1.3 「ボタンを押して仕事が終わる」

SHOGUNの中核体験は次のループである:

```
キャプチャ（Accessibility API・テキストのみ）
  → メモリ（event log + 3層メモリ）
  → 状態推定（state tables + confidence）
  → Context Fusion: f(state, screen_ctx, intent) → action
  → Notch UIに文脈アクションを提示（150ms以内）
  → ユーザーが押す（またはL1なら自動実行）
  → エージェントが実行（L1/L2/L3の権限モデル下で）
  → 結果とトレーサビリティログ
```

ユーザーが「AIに文脈を説明する」工程をゼロにする。文脈は常にSHOGUN側がプリアセンブル済みであり（context cache）、「押してから収集」は禁止である。

### 1.4 プロダクト原則（設計判断の優先順位）

| # | 原則 | 意味 |
|---|---|---|
| P1 | ローカルファースト | 生データはデバイス外に出ない。クラウドに出るのは処理用チャンクのみ |
| P2 | 状態の推定 > 記録 | 検索できるログではなく、行動可能な状態を作る |
| P3 | 実行 > 提案 | 最終成果は「終わった仕事」。ただし外部不可逆操作は必ずL3 |
| P4 | 人間UIとAI APIの完全対称 | 新機能はUI・MCP/CLI/RESTの両方から同一権限モデルで呼べる |
| P5 | 割り込まない | エラーも通知も、ユーザーの作業を中断させない形で伝える |
| P6 | メモリは年単位で生きる | 後方互換を破らない。スキーマはspatial-ready |

### 1.5 v1のゴール（成功条件）

- v1のユーザーが1日1回以上、Notchのコンテキストアクションから仕事を完了させる（実行系アクションの日次利用率をテレメトリのイベント数のみで計測。内容は送らない。§7.7参照）。
- 7日間トライアル→有料転換のファネルが計測可能な状態でリリースされている。
- CLAUDE.mdのSLO表（§7.1で詳細化）を全項目満たしている。

### 1.6 CLAUDE.md絶対不変条件との対応表

CLAUDE.mdの絶対不変条件7項と、本書でそれを具体化した要件の対応。実装レビュー時のチェックリストとして使う。

| 不変条件 | 内容 | 本書での具体化 |
|---|---|---|
| 1 | データの重心はRustコア | AR-03〜05（プロセス構成・webview禁止事項）、NFR-SLO-00（計測はRust側） |
| 2 | 画像・音声ファイルを保存しない（音声はRAM処理・テキストのみ永続化）。**2026-08-02 例外**: Visual recall ON時のみ、OCR用JPEGを暗号化DBに≤72時間保持 | FR-CAP-01、NFR-PRV-01、FR-MT群、**FR-VIS群（§6.21）** |
| 3 | 生データはデバイス外に出さない＋トレーサビリティ | AR-11/12、NFR-PRV-02、FR-TR-01〜04 |
| 4 | L1に外部送信を含めない。送信・投稿・カレンダー作成はL3 | FR-AG-01/02、FR-AG-14（カレンダー作成=L3）、§6.9サービス別表 |
| 5 | キーの分離（会社 Batch / 会社 live、BYOK・委譲は live 任意） | FR-DC-02、FR-MB-01、FR-AG-06、FR-BIL-02/04 |
| 6 | 人間UIとAI APIの完全対称＋同一L1/L2/L3 | FR-AG-04、FR-API-01〜06 |
| 7 | secretsはKeychainのみ | FR-INT-02、FR-C2-05、NFR-SEC-01/02 |

---

## 2. 用語集

| 用語 | 定義 |
|---|---|
| **Notch UI** | MacBookのノッチ領域（および擬似ノッチ）を常駐UIとして使うSHOGUNのメインサーフェス。NSPanelで実装。状態機械は§6.1 |
| **擬似ノッチ** | ノッチ非搭載Mac・外部ディスプレイで、メニューバー中央に表示する同型のフローティングパネル。挙動・状態機械はノッチと同一 |
| **3層メモリ** | Hot（直近24h、RAM常駐）/ Warm（30日、SQLite）/ Cold（全履歴、int8量子化＋期間パーティション）の3層構造。§6.3 |
| **event log** | キャプチャ・連携から得た生イベントの追記型ログ。不変（immutable）。状態はここから推定される |
| **state tables** | `people` / `projects` / `commitments` / `open_loops` の4テーブル。ワールドモデルの本体。§6.4 |
| **provenance** | stateレコードから根拠イベント（event log行）への参照。全stateレコードに必須 |
| **confidence** | stateレコードの確度。0.0〜1.0。閾値と生成物への渡し方の規則は§6.4.6 |
| **open_loops** | 「開いたまま」の事項。返信待ち、未読の重要スレッド、放置中のPRなど。ユーザーの明示的タスクとは限らない |
| **commitments** | ユーザーが他者に（または他者がユーザーに）明示的に約束した事項。期日を持ちうる |
| **Context Fusion** | `f(state, screen_ctx, intent) → action`。現在の状態・画面文脈・推定意図から次のアクションを合成する層。crates/shogun-fusion |
| **context cache** | 現在フォーカス中の文脈に対して常時プリアセンブルされるコンテキストパッケージ。フォーカス切替から300ms以内に更新 |
| **screen_ctx** | 現在フォーカス中のアプリ・ウィンドウ・テキストから抽出した即時文脈 |
| **intent** | ユーザーの直近の行動系列から推定される目的（例: 「この人に返信しようとしている」） |
| **L1（自動実行）** | 通知のみで自動実行されるアクション。**ローカル完結・可逆な操作のみ**。外部送信は絶対に含めない |
| **L2（ワンタップ承認）** | Notchに提案が出て1クリックで実行。デバイス外への影響が読み取り系または下書き系に限られるもの |
| **L3（明示確認）** | 実行内容のプレビュー（送信先・全文）を提示し、明示的な確認操作を要求。送信・投稿・カレンダー作成・削除など外部に不可逆な影響を持つ操作すべて |
| **Dream Cycle** | 夜間バッチ処理。統合・圧縮・state更新・confidence再計算・Cold層への降格。Select KKキー（Batch API）で実行。§6.7 |
| **Morning Brief** | Dream Cycleの成果物として毎朝生成される、その日の状態サマリと推奨アクション。§6.8 |
| **Evening Wrap** | 1日の終わりに提示する日次サマリ。Morning Briefのローカル版で、**LLM生成もegressも伴わない**決定的集計のみ。「夜のWrapで見た状態が翌朝のBriefで整理されて返る」1日のループを作る。§6.17 |
| **thread_key** | event log行が属する会話・スレッドの識別子。ソース別に安定した値を持つ（Gmail=threadId / Slack=channel+thread_ts / GitHub=issue URL / ai_session=session id / キャプチャ=正規化したアプリ＋ウィンドウタイトル）。§6.3.5 |
| **Context Pack** | ハイブリッド検索の結果・state facts・現在画面を予算内で束ねた、モデルへ渡すコンテキストの単位。UIチャットもMemory APIも同一の組み立てを使う（不変条件6）。§6.3.5 |
| **salience** | スレッドを参照先候補としてランク付けするスコア。直近性・未処理度・現在画面との一致・語句一致の加重。再計算値であり正本ではない。§6.3.5 |
| **参照解決** | 「あの件」「さっきの」等の指示的な問いから、どのスレッドを指しているかを判定すること。一意に絞れない場合は**推測せず聞き返す**。§6.18 |
| **ai_session** | Claude Code等のローカルAIセッションログを取り込んだイベントのsource値。取り込みはオプトイン。§6.19 |
| **視覚キャプチャ（Visual recall）** | AXテキストで拾えない画面内容を、イベント駆動のキーフレーム＋オンデバイスOCRで取得する経路。既定OFF。連続録画はしない。§6.21 |
| **Select KKキー** | 運営会社（Select KK）が保有するAnthropic APIキー。インデックス・分類・Dream Cycle・Morning BriefのBatch API処理**のみ**に使う |
| **BYOK** | Bring Your Own Key。ユーザー自身のAnthropic APIキー。エージェント推論・チャット・ドラフト生成**のみ**に使う。v1はAnthropicのみ対応 |
| **第1層連携** | 各サービスの**公式リモートMCP**への直接接続。OAuthはユーザー→サービス直接、トークンはKeychain。v1はGmail / Google Calendar / Slack / Notion / GitHub / Linearの6つ |
| **第2層連携** | Composio経由の連携（オプトイン）。v1ではGmail送信のみ。トレーサビリティ画面に「第三者経由」を明示 |
| **Memory API** | SHOGUNのメモリ・状態への外部AIアクセス面。MCPサーバー / CLI / ローカルREST。人間UIと完全対称。§6.11 |
| **トレーサビリティログ** | デバイス外への全送信の記録。送信先・送信内容の要約・経路（直接/第三者経由）・承認レベルを含む。§6.14 |
| **ドラフト止まりモード** | 外部送信系エージェントの出力をドラフト作成までに制限する設定。Gmail送信（Composio）に必須で用意 |
| **spatial-ready** | 将来の空間コンピューティング対応（Phase 3: visionOS）に備え、`window_pose` / `gaze_target` / `dwell_ms` / `display_id` / `window_bounds` のカラム余地をスキーマに最初から確保する方針 |
| **Wave** | 第1層連携の段階導入単位。Wave 1 = Gmail + Google Calendar、Wave 2 = Slack、Wave 3 = Notion + GitHub + Linear |
| **FTS5 trigram** | SQLite FTS5のtrigramトークナイザによる全文検索。日本語等の分かち書き不要言語を含む多言語対応のため採用 |
| **refinery** | Rustのマイグレーション管理ライブラリ。スキーマ変更は必ずバージョン管理されたマイグレーションで行う |

---

## 3. スコープ定義

本表は「v1に足すかどうか」の判断根拠である。CLAUDE.mdはスコープ外要求を断る根拠として本表を参照する。**本表にないものをv1に足す場合は、本書の改版を伴わなければならない。**

### 3.1 v1に含むもの

| 領域 | v1に含む |
|---|---|
| キャプチャ | Accessibility API経由のテキストキャプチャ、除外リスト、一時停止 |
| 視覚キャプチャ（既定OFF） | イベント駆動キーフレーム＋オンデバイスOCR（Apple Vision）。OCR用JPEGは≤72時間で自動削除。連続録画はしない（FR-VIS群、§6.21） |
| メモリ | 3層メモリ、event log、FTS5 trigram検索、Warm層ベクトル検索（ローカルONNX embedding）、スレッド（thread_key / threads）とContext Pack（§6.3.5） |
| 状態 | state tables 4種（people/projects/commitments/open_loops）、provenance + confidence |
| UI | Notch UI（実ノッチ＋擬似ノッチ）、Full UI（検索・トレーサビリティ・設定）、Morning Brief表示、Evening Wrap表示 |
| Fusion | context cache常時プリアセンブル、コンテキストアクション提示、参照解決（§6.18） |
| AIセッション取り込み（既定OFF） | Claude Code等のローカルセッションログを`ai_session`として取り込む（FR-AIS群、§6.19） |
| 会議ノート（全プラン。トライアル中から使える。文字起こしはユーザーの音声認識キーが必要） | 会議検知（session区間）、Notch Offered/Recordingピル、会議中ノート、オンデバイスASR（音声はRAMのみ・テキスト永続化）、Recap（要約＋決定事項＋約束候補）、三段オフ＋オプトイン（FR-MT群、`docs/meeting-notes-ui-design.md`） |
| エージェント | L1/L2/L3実行エンジン、プリセットエージェント7種、チャット、ドラフト生成（Standard以上。2026-09-24 改定） |
| バッチ | Dream Cycle、Morning Brief |
| 連携（第1層） | Gmail / Google Calendar / Slack / Notion / GitHub / Linear（公式リモートMCP、Wave 1→3） |
| 連携（第2層） | Composio: Gmail送信のみ（オプトイン、Pro） |
| API | Memory API: MCPサーバー / CLI / ローカルREST（Pro） |
| 課金 | Stripe、7日間フルトライアル、Standard / Pro、ライセンス検証、オフライン猶予 |
| 配布 | Developer ID + notarization、Tauri updater |
| 言語 | UI英語のみ（i18n-ready構造）、生成物の出力言語はユーザー設定 |

### 3.2 v1に含まないもの（時期を含めて明記）

| 項目 | 予定 | v1で断る根拠 |
|---|---|---|
| 録音ファイル・録画ファイルの保存 | **恒久的に対象外** | 不変条件2。回避策も設計しない。会議音声は聞いてテキストのみ残す（下記FR-MT群） |
| 会議URL/カレンダー経由のbot会議参加 | **恒久的に対象外** | 取得するのはMac出力音声のみ。会議への自動送信・投稿もしない（Issue #7 Non Goal） |
| ナレッジグラフ（エンティティ間グラフ構造） | **v2** | v1のワールドモデルはstate tables 4種で表現する |
| マルチデバイス同期・クラウドバックアップ | **v2** | ローカルファースト優先。同期はE2E暗号化設計が前提のためv2 |
| メタメモリ（記憶についての記憶・自己評価） | **v2** | Dream Cycleのconfidence再計算までがv1 |
| Computer Use（画面操作の自動実行） | **Phase 3**（設計は確定済み: `docs/computer-use-phase3-design.md`） | v1の実行はMCP経由の構造化操作のみ。2026-09-09のオーナー決定で「指示駆動Run・無人完遂・精度・上限付き事前承認」のPhase 3設計を先に確定させたが、**v1スコープは動かさない**（同書§13が持ち込まない範囲を列挙） |
| 選択フォームのテキスト記入 | **v1 限定例外（2026-10-09 オーナー承認）** | Scribe 起点、空欄・AX 対応テキストのみ、人の最終確認、同一宛先と読戻し検証。判断は専用のユーザー Keychain キーで TypeSafe Jev Decisions API を呼ぶ。送信・クリック・決済・秘密情報・無人実行は対象外。【2026-10-14 オーナー決定】右 ⌥ の1回押しは単一欄への下書きを維持。フォーム全体の記入は Scribe で明示的に依頼し、Jev が意味と文脈から自動選択する（スコープボタン・固定トリガー句は不要）。記入後10秒以内の右 ⌥ は、今も SHOGUN の値のままの検証済みの欄だけを元に戻す。明示操作で開くのはユーザー自身の除外だけで、既定の除外（パスワードマネージャー・認証ダイアログ・プライベートウィンドウ等）では記入しない。`docs/features/form-mode.md` |
| visionOS対応 | **Phase 3** | v1はspatial-readyなスキーマ確保に留める |
| Intel Mac対応 | 将来判断（未決事項 §9） | Apple Siliconのみ公式サポート（付録A ADR-005） |
| Windows / Linux対応 | 予定なし | macOSネイティブAPI（AXUIElement等）に依存 |
| iOSコンパニオンアプリ | 予定なし（v2以降で再検討） | — |
| Freeプラン | 予定なし | 課金方針（§6.12）。トライアルで代替 |
| チームプラン・組織管理 | 未決事項（§9） | v1は個人ワーカー向け |
| Slack以外のチャット（Discord, Teams等） | 予定なし（要望次第でv2検討） | 第1層は6サービスに固定 |
| クラウドembedding API | 予定なし | ローカルONNX固定（付録A ADR-001） |
| Anthropic以外のBYOKプロバイダ | 将来判断 | v1はAnthropicのみ。trait抽象化のみ確保（付録A ADR-002） |
| App Store配布 | 予定なし | Developer ID直接配布（CLAUDE.md確定） |
| モバイル通知・リモート通知 | 予定なし | 通知面はNotch UIとmacOS通知のみ |

### 3.3 スコープ判断の運用

- 実装中にスコープ外の要望・アイデアが出た場合、実装者は本表の該当行を引用して確認を取る。**確認なしにv1スコープへ追加しない。**
- 本表に該当行がない新規要望は「v1に含まない」をデフォルトとし、§9（未決事項）に追記して判断を待つ。

---

## 4. 想定ユーザーと主要ユーザーストーリー

### 4.1 想定ユーザー

**AIネイティブな個人ワーカー。** 具体的には:

- ファウンダー / インディーハッカー: 複数プロジェクト・複数の相手と並行して動き、返信漏れ・フォローアップ漏れが直接損失になる
- ビルダー / エンジニア: GitHub / Linear / Slackを横断し、コンテキストスイッチのコストが最大の敵
- リサーチャー / ナレッジワーカー: 大量の読み物・スレッド・ドキュメントから「自分が何を追っていたか」を失いやすい

共通特性: 既にAIチャットを日常的に使っており、「毎回文脈を説明し直す」ことに不満を持つ。BYOKに抵抗がない層を含む。macOSユーザー。仕事言語は英語または英語＋母語（生成物の出力言語設定が効く）。

### 4.2 ユーザーストーリー（受け入れシナリオ形式）

各ストーリーは Given / When / Then 形式。関連FRを併記する。

**US-01: 文脈を説明しない返信ドラフト**
- Given: ユーザーがGmail（第1層接続済み）で取引先からのメールスレッドを画面に表示している。SHOGUNのstate tablesにはこの相手（people）と関連プロジェクト（projects）のレコードがある
- When: Notchにホバーする
- Then: 150ms以内に「Draft reply」アクションが表示され、押すと1s以内に最初のトークンがストリーミング表示される。ドラフトはスレッド内容とstate（過去の約束・プロジェクト状況）を反映している。confidenceが閾値未満の状態は断定でなく可能性として表現されている
- 関連: FR-CF-01〜03, FR-AG-10, FR-ST-20

**US-02: 朝、今日やるべきことが既に並んでいる**
- Given: 前夜にDream Cycleが完了している。Google Calendar接続済み
- When: 朝、Macを開いてNotchのMorning Briefインジケータを開く
- Then: 今日の予定・期限が近いcommitments・放置中のopen_loops・推奨アクション（それぞれL2/L3ボタン付き）が1画面で表示される。生成言語はユーザー設定の出力言語である
- 関連: FR-MB-01〜05, FR-DC-01

**US-03: 会議前の自動準備**
- Given: 15分後にCalendar上の会議がある。参加者はstate tablesのpeopleに存在する
- When: 会議の15分前になる
- Then: L1として「Meeting Prep準備完了」の通知がNotchに出る（自動実行はローカルでの資料集約のみ）。開くと、参加者ごとの直近のやり取り・未解決のcommitments・関連ドキュメントへの参照が表示される。外部への送信は一切発生していない
- 関連: FR-AG-11, FR-CF-02

**US-04: 送信は必ず自分が最終確認する**
- Given: Pro + Composio（第2層）オプトイン済み。ドラフト止まりモードはOFF
- When: 「Send reply」アクションを実行する
- Then: 送信先アドレス・件名・本文全文のプレビューがL3確認ダイアログで表示され、明示的な確認操作をするまで送信されない。送信後、トレーサビリティ画面に「第三者経由（Composio）」の記録が残る
- 関連: FR-AG-03, FR-C2-02〜04, FR-TR-01

**US-05: パスワードマネージャは絶対に記録されない**
- Given: 既定の除外リストが有効
- When: パスワードマネージャアプリを開いて操作する
- Then: その間のキャプチャイベントは一切生成されない（保存もされない）。Notchのインジケータがキャプチャ一時停止状態の色になる。アプリを閉じるとキャプチャが再開する
- 関連: FR-CAP-05〜07

**US-06: 「あれどこだっけ」が500msで返る**
- Given: 過去2週間に読んだ資料の内容の断片（日本語）だけ覚えている
- When: Full UIの検索に断片を入力する
- Then: 500ms以内にFTS5 trigram＋Warm層ベクトル検索のハイブリッド結果が返り、当時のアプリ名・ウィンドウタイトル・前後の文脈が表示される
- 関連: FR-MEM-20〜22, NFR-SLO-04

**US-07: 返信し忘れをSHOGUNが覚えている**
- Given: 3日前にSlackで「明日返します」と書いたスレッドが未返信
- When: Dream Cycleがcommitmentsの期限超過を検出する
- Then: Morning Briefと Notchに「Follow-up」提案（L2）が出る。押すとドラフトが生成され、Slack投稿はL3確認を経る。Slack接続がWS管理者承認で不可の場合は、ドラフトのクリップボードコピーにフォールバックする
- 関連: FR-AG-13, FR-INT-30, FR-ST-12

**US-08: 外部AIから自分のメモリを使う**
- Given: Proユーザー。外部のAIコーディングエージェントにSHOGUNのMCPサーバーを登録済み
- When: 外部エージェントが `memory.search` ツールを呼ぶ
- Then: 人間UIの検索と同一の結果が返る。外部エージェントが書き込み系ツールを呼んだ場合、人間UIと同一のL1/L2/L3判定が適用され、L3操作はNotchでの明示確認なしに完了しない
- 関連: FR-API-01〜06

**US-09: トライアルから納得して課金する**
- Given: 初回起動から7日間、Pro相当の全機能を使った
- When: 8日目に起動する
- Then: 機能がロックされ、Standard / Proの選択画面（年払い/月払い価格併記）が表示される。選択→Stripe決済完了で1分以内に機能が解放される。ローカルのメモリデータは消えていない
- 関連: FR-BIL-02〜05

**US-10: 権限を拒否しても壊れない**
- Given: 初回起動でAccessibility権限を拒否した
- When: オンボーディングを続行する
- Then: アプリはクラッシュせず、キャプチャ以外の機能（連携の読み取り統合、Morning Brief、検索は連携データのみ）が動作する。Notchインジケータが権限不足の状態色を示し、設定画面から権限付与への導線が常設される（2026-10-03 オーナー決定 #778: 設定・チャット・会議ノート・Connections も使え、導線は展開パネルの常設バナー。詳細は FR-CAP-08 / FR-OB-04）
- 関連: FR-CAP-08, FR-OB-04

**US-11: フルスクリーン作業を邪魔しない**
- Given: ユーザーが動画編集アプリをフルスクリーンで使用中
- When: SHOGUNがL2提案を生成した
- Then: Notchパネルは自動表示されず、提案はキューに保持される。フルスクリーン解除後、Notchインジケータの色で提案の存在が示される
- 関連: FR-NU-08

---

## 5. システムアーキテクチャ

### 5.1 リポジトリ構成

既存のpnpmモノレポ（`apps/website` 等を含む）のルートに **Cargo workspaceを追加統合**する。JS側（pnpm/turbo）とRust側（cargo）は同一リポジトリで共存し、`apps/desktop`（Tauri）が両者の接合点となる。

```
（リポジトリルート）
├── package.json / pnpm-workspace.yaml / turbo.json   # 既存JSモノレポ
├── Cargo.toml                                        # [workspace] 追加
├── crates/
│   ├── shogun-core/      # デーモン: キャプチャ、DB所有、context cache、イベントバス
│   ├── shogun-memory/    # スキーマ、refineryマイグレーション、3層メモリ、state tables、検索
│   ├── shogun-fusion/    # Context Fusion: f(state, screen_ctx, intent) → action
│   ├── shogun-agents/    # L1/L2/L3実行エンジン、プリセットエージェント、LLMプロバイダtrait
│   ├── shogun-mcp/       # MCPクライアント（第1層）＋MCPサーバー/REST（Memory API）
│   └── shogun-cli/       # shogunコマンド
├── apps/
│   ├── desktop/          # Tauri v2アプリ（Notchパネル、Full UI、設定）React + TS
│   └── website/          # 既存
└── docs/
```

**AR-01（MUST）**: 上記のcrate分割・責務配置に従う。crate間の依存方向は `core → memory/fusion/agents/mcp` を集約点とし、循環依存を作らない。

**AR-02（MUST）**: JSワークスペースとCargoワークスペースのビルドは独立して成立する（`pnpm build` がRustツールチェーン無しで、`cargo build` がNode無しで通る。`apps/desktop` のフルビルドのみ両方を要求してよい）。

### 5.2 プロセス構成

**AR-03（MUST）**: v1は「**同居構成**」を採用する。shogun-coreは独立OSデーモン（launchd常駐）ではなく、Tauriアプリプロセス内のRustバックエンドとしてホストされる。ただし:

- shogun-coreはライブラリとして**プロセス境界を前提としない設計**（内部イベントバス経由の疎結合）にし、将来のデーモン分離（launchd化）をアーキテクチャ変更なしに行えること。
- Tauriアプリはログイン項目（Login Item）として自動起動し、ウィンドウを全て閉じてもプロセスは常駐する（メニューバー/Notch常駐アプリとして振る舞う）。
- 判断理由: v1では配布・権限（Accessibility権限はアプリ単位）・更新（Tauri updater）の単純さがプロセス分離の利益を上回る。分離は将来のCLI/外部API常時稼働要件が強まった時点で再判断する。

**AR-04（MUST）**: **データの重心はRust側に置く**（CLAUDE.md不変条件1）。DB・キャプチャ・context cache・SLO計測はRust（shogun-core以下）が単独所有する。webview（React）側は表示とユーザー入力のみを担い、以下を**禁止**する:
- webviewからのSQL発行・DBファイルアクセス
- webviewでの検索ランキング・状態推定・キャッシュ組み立てロジック
- webviewでのsecrets保持（トークン・APIキーをJSに渡さない。§7.5）

**AR-05（MUST）**: UIとRust間の通信はTauri command（要求/応答）とTauri event（Rust→UIのプッシュ）に限定する。UIに渡すデータは表示用に整形済みのDTOとし、event logの生レコードをそのまま渡さない。

### 5.3 内部イベントバス

**AR-06（MUST)**: shogun-core内に単一のイベントバス（Rust、mpsc/broadcastベース）を置き、以下のイベント種別を流す:

| イベント | 発行元 | 主な購読者 |
|---|---|---|
| `capture.text` | キャプチャ | memory（event log書き込み） |
| `focus.changed` | キャプチャ（NSWorkspace） | fusion（context cache再構築） |
| `cache.updated` | fusion | UI（Notchアクション更新） |
| `state.updated` | memory / Dream Cycle | fusion、UI |
| `action.proposed` | fusion / agents | UI（L2/L3提示） |
| `action.executed` | agents | memory（event log）、トレーサビリティ |
| `integration.synced` | mcp（第1層） | memory |
| `error.raised` | 全コンポーネント | UI（インジケータ色） |

**AR-07（MUST）**: イベントバスの購読者が遅延・失敗しても発行者（特にキャプチャ）をブロックしない（バックプレッシャは有界キュー＋古いイベントのドロップで処理し、ドロップ数をメトリクスに記録する）。

### 5.4 context cacheの常時プリアセンブル

**AR-08（MUST）**: context cacheは「押してから収集」を禁止し、以下のトリガで**常時先行更新**する:
- `focus.changed`（アプリ/ウィンドウ切替）→ 300ms以内に再構築完了（NFR-SLO-05）
- `capture.text`（同一ウィンドウ内の内容変化）→ デバウンス500msで増分更新
- `state.updated` → 影響範囲のみ差分更新

**AR-09（MUST）**: context cacheの内容は最低限、次を含む: screen_ctx（フォーカス中アプリ・ウィンドウタイトル・可視テキストの抽出）、関連stateレコード（people/projects/commitments/open_loopsのうち関連度上位、各confidence付き）、直近のHot層イベント要約、実行可能アクション候補（L1/L2/L3タグ付き）。

**AR-10（MUST）**: context cacheはRAM上に保持し、ディスクへ永続化しない（クラッシュ後は再構築する。再構築はフォーカスイベントを待たず起動時に即実行）。

### 5.5 データフロー上の境界（プライバシー境界）

**AR-11（MUST）**: デバイス外への通信は以下の5経路のみとし、それ以外の外部通信をコードに書かない:

| 経路 | 内容 | キー/認証 | トレーサビリティ |
|---|---|---|---|
| Anthropic Batch API | Dream Cycle・Morning Brief・インデックス/分類の処理用チャンク | Select KKキー | 必須 |
| Anthropic Messages API | エージェント推論・チャット・ドラフト | ユーザーBYOK | 必須 |
| 第1層公式リモートMCP | 各サービスの読み書き | ユーザー→サービスOAuth | 必須 |
| Composio（第2層） | Gmail送信のみ（v1） | オプトイン接続 | 必須（「第三者経由」明示） |
| Stripe / ライセンスAPI / updater | 課金・ライセンス検証・更新確認 | アカウント認証 | 課金系は対象外（キャプチャ内容を含まないため）。ただし通信自体は§7.7の方針に従う |

**AR-12（MUST）**: 上記いずれの経路でも、event logの生レコード全体・スクリーン全文の無差別送信を行わない。送信されるのは目的に必要な**処理用チャンク**（抽出・整形済みの断片）のみである。

### 5.6 Phase 1実装順序（マイルストーン）

CLAUDE.mdのPhase 1定義に従い、以下の順序で実装する。各マイルストーンは前段の受け入れ基準充足を開始条件とする。

| M | 内容 | 主な対応要件 | 完了条件 |
|---|---|---|---|
| M1 | Notch UI本実装（Phase 0スパイクの製品化） | §6.1 | 状態機械全遷移＋NFR-SLO-01/02の計測合格 |
| M2 | キャプチャ＋メモリ＋state tables | §6.2〜6.4 | 24h連続稼働・検索SLO・マイグレーションCI合格 |
| M3 | Context Fusion＋L1/L2エージェント＋Dream Cycle/Morning Brief | §6.5〜6.8 | US-01/02/03のE2E合格 |
| M4 | 第1層MCP連携（Wave 1→順次）＋L3実行＋第2層 | §6.9〜6.11 | 許可範囲表・トレーサビリティのテスト合格 |
| M5 | 課金＋トライアル＋オンボーディング＋配布 | §6.12〜6.13、§7.6 | トライアル→課金E2E・notarization済みビルド |
| M6 | 会議ノート（MT1〜MT5、下記） | §6.16 | §6.16受け入れ基準7項目の合格（特に「音声がディスクに書かれない」検査） |

**M6の内訳（MT採番。Phase 1のM1〜M5とは別系統）** — 詳細は `docs/meeting-notes-ui-design.md` §7:

| MT | 内容 | 音声 | 依存 |
|---|---|---|---|
| MT1 | `sessions` + 会議検知 + Notch の Offered/Recording ピル | **なし** | M1（Notch）、M2（DB） |
| MT2 | 会議中ノート（`session_notes`）+ 終了検知 + 縮退Recap | **なし** | MT1 |
| MT3 | オンデバイスASR + `transcript_segments` + リングバッファ上限 | あり | MT2 + OPEN-07/08 |
| MT4 | Recap本体（Batch要約 + 候補抽出 + `[Track]`確定） | — | MT3 + Select KKキー |
| MT5 | 三段オフ・除外リスト・オンボーディングのオプトイン | — | MT1と並行可 |

**MT1/MT2は音声なしで出荷できる。** 検知・器・UIが先に立ち上がっていれば、ASRは「解像度を上げる差し替え」で済む。**逆順にしない。**

---

## 6. 機能要件（FR群）

各FRは「目的 / 詳細要件 / プラン / SLO / エラー時挙動 / 受け入れ基準」の観点を含む。プラン列の意味: **Std** = Standard以上、**Pro** = Proのみ、**All** = トライアル含む全プラン。

### 6.1 Notch UI

**目的**: SHOGUNの主要サーフェス。常駐しつつ作業を邪魔せず、文脈アクション・状態・エラーを最小の視覚コストで提示する。

#### 6.1.1 状態機械

**FR-NU-01（MUST, All）**: Notch UIは以下の状態機械に従う。図中の遷移以外を実装しない。

```
                 hover 120ms持続
   ┌─────────┐ ───────────────→ ┌─────────┐  click / ⌘⇧Space
   │  Idle   │                  │  Hover  │ ───────────────→ ┌──────────┐
   │(常駐表示)│ ←─────────────── │(プレビュー)│                 │ Expanded │
   └─────────┘   マウス離脱200ms └─────────┘ ←─────────────── └──────────┘
        ↑                                      ESC / 外側クリック      │
        │                                     / 20s無操作            │ "Open Full UI"
        │ フルスクリーン解除                                          ↓
   ┌─────────┐                                            ┌──────────────┐
   │ Hidden  │ ←── フォーカスアプリがフルスクリーン化          │ Full UI(別窓) │
   └─────────┘                                            └──────────────┘
```

- **Idle**: ノッチ形状に沿った最小表示。インジケータ（FR-NU-06）のみ描画
- **Hover**: ホバー120ms持続で遷移（誤発火防止。Phase 0の計測結果で60〜200msの範囲で調整可）。次のアクション候補1件と状態サマリ1行をプレビュー
- **Expanded**: クリックまたはグローバルホットキー（既定 `⌘⇧Space`、変更可）で遷移。Idle→Expanded直接遷移も可（ホットキー時）。コンテキストアクション最大4件、Morning Brief導線、チャット入力欄（Pro）を表示
- **Hidden**: フルスクリーン時（FR-NU-08）

**FR-NU-02（MUST, All）**: Idle→Expandedの最初の開いたフレームは展開処理開始から**150ms以内**（NFR-SLO-01）。形状モーフのCSS durationは150ms。Expandedの初回表示時点でアクション候補が確定していること（cacheプリアセンブルの帰結。プレースホルダやスピナーで代替しない）。

**FR-NU-03（MUST, All）**: NSPanel実装は `.nonactivatingPanel` + `.canJoinAllSpaces` + `.fullScreenAuxiliary` を使用し、Notch操作でフォーカス中アプリのキーフォーカスを奪わない（チャット入力欄クリック時のみ例外的にキー入力を受ける）。

#### 6.1.2 擬似ノッチとマルチディスプレイ

**FR-NU-04（MUST, All）**: ノッチ非搭載Mac、および外部ディスプレイでは、**メニューバー中央**に実ノッチと同型・同状態機械のフローティングパネル（擬似ノッチ）を表示する。実ノッチと擬似ノッチでUI・挙動・SLOに差を設けない。

**FR-NU-05（MUST, All）**: マルチディスプレイ時の表示規則:
- パネルは**キーウィンドウのあるディスプレイ**（＝ユーザーの作業中ディスプレイ）に1つだけ表示する
- フォーカスがディスプレイ間を移動したら500ms以内にパネルが追従する
- 内蔵ディスプレイ（実ノッチ）と外部ディスプレイ（擬似ノッチ）の間を移動しても状態（Expanded中の内容等）を維持する
- 設定で「内蔵ディスプレイ固定」を選択可能

#### 6.1.3 インジケータとエラー通知

**FR-NU-06（MUST, All）**: Idle状態のインジケータは色で状態を伝える。モーダルやバナーでエラーを出さない（CLAUDE.md: エラーはユーザーの作業を中断させない）。

| 色 | 状態 |
|---|---|
| 白（微光） | 正常・キャプチャ稼働中 |
| ゴールド | 新しい提案あり（L2/L3待ち）または Morning Brief未読 |
| グレー | キャプチャ一時停止中（手動 / 除外アプリ表示中 / 権限なし） |
| アンバー | 劣化動作（連携の一部失敗、Batch API失敗、ライセンス猶予中など。機能は継続） |
| 赤 | 要対応エラー（DB書き込み失敗、ライセンス失効等）。Hoverで内容と対処導線を表示 |

**FR-NU-07（MUST, All）**: アンバー/赤の全状態はHoverプレビューで1行説明＋対処アクションに到達できること。エラー詳細はFull UIのログ画面へ導線を張る。

#### 6.1.4 フルスクリーン時挙動

**FR-NU-08（MUST, All）**: フォーカス中アプリがフルスクリーンの間、パネルはHiddenに遷移し自動表示しない。生成された提案はキューに保持し、フルスクリーン解除後にインジケータ色（ゴールド）で存在を示す。グローバルホットキーによる明示呼び出し時のみ、フルスクリーン上にも `.fullScreenAuxiliary` でExpanded表示してよい。

**受け入れ基準（6.1）**: 状態機械の全遷移がUIテストで検証されている。実ノッチ機・非ノッチ機・外部ディスプレイ接続の3構成で FR-NU-04/05 が手動テストされている。NFR-SLO-01の計測コードが同梱されている。

### 6.2 キャプチャ

**目的**: ユーザーの作業文脈を、プライバシー不変条件の範囲内で受動的に取得する。

**FR-CAP-01（MUST, All）**: キャプチャは **Accessibility API（AXUIElement）経由のテキストのみ**。スクリーンショット・画像・動画・OCRを取得・保存するコードを書かない（CLAUDE.md不変条件2）。**【2026-08-02 明示的例外｜Visual recall】** Visual recall が On のときにかぎり、フォーカス中ウィンドウがあるディスプレイの圧縮 JPEG（除外対象のウィンドウは取得時点で除く。2026-10-06 #969 更新、FR-VIS-11）を暗号化済みメモリ DB（`screen_frames`）にユーザーが選択した有限期間（プリセット1〜7日＋上限3,650日のカスタム、既定・フォールバック3日）だけ保持し、期限による自動削除は常時有効（無効化不可）。バイト予算での早期削除はせず、ストア上限（2GiB）到達時は既存フレームを保持したまま新規取得を停止する。保持期間中はローカルで閲覧・削除できる。既定は Off、クラウド送信なし、音声は対象外（CLAUDE.md 不変条件2の例外、2026-08-20 更新）。会議音声は本節のキャプチャレーンとは**別レーン**であり、FR-MT群（§6.16）の条件下（Recording中のみ・RAM内処理・テキストのみ永続化）でのみ扱う。**常時キャプチャの一部として音声を取得しない。**取得対象: フォーカス中ウィンドウのアプリ名（bundle id）・ウィンドウタイトル・可視テキスト（AX階層から抽出）・フォーカス要素のロール、およびNSWorkspaceからのアプリ切替イベント。

**FR-CAP-02（MUST, All）**: 取得タイミング: (a) フォーカス切替時、(b) 同一ウィンドウでのAX通知（値変更・タイトル変更）を500msデバウンスで。ポーリングを行う場合は2s間隔を下限とし、アイドル時CPU 5%制約（NFR-SLO-06）に収める。

**FR-CAP-03（MUST, All）**: 重複抑制: 直前キャプチャとのテキスト差分が閾値未満（正規化後の類似度98%以上）の場合はevent logに新規行を作らず、既存行の `last_seen_at` と `dwell_ms` を更新する。

**FR-CAP-04（MUST, All）**: `AXSecureTextField`（パスワード入力欄）の値は、いかなるアプリでも**読み取り自体を行わない**。〔2026-09-24 追記｜Issue #283 OPEN-283-02〕サブロールが `AXSecureTextField` の欄、およびテキスト欄系ロール（`AXTextField` / `AXTextArea` / `AXComboBox` / `AXSearchField`）のうちプレースホルダー・タイトル・説明・識別子が password / PIN / card number / CVC / one-time code 等（日本語含む）を示す欄も同じ扱い（値を読まず、キャプチャ walk ではサブツリーごとスキップ）。判定は `shogun_core::capture::secret_field`（純粋関数）で、すべての AX 読み取り経路が同じ関数を使う。

**FR-CAP-05（MUST, All）**: **既定の除外リスト**（該当アプリ/ウィンドウがフォーカスの間、キャプチャイベントを一切生成しない）:
- パスワードマネージャ: 1Password, Bitwarden, KeePassXC, Dashlane, Enpass, Keychain Access（キーチェーンアクセス）, パスワード（Passwords。macOS 15 以降の標準パスワードマネージャ）
- プライベートブラウジングウィンドウ: Safari / Chrome / Edge / Brave / Arc / Firefox（ウィンドウタイトルおよびAX属性でプライベートモードを判定。判定不能なブラウザは通常キャプチャとし、既知ブラウザのみ対応）
- macOSの認証ダイアログ（SecurityAgent）
- SHOGUN自身の設定画面のうちBYOKキー入力画面
- 【2026-10-19 オーナー決定】ターミナルは既定の除外に含めない（Goldfish と同じく既定でキャプチャし、秘密情報のマスキングは従来どおり掛かる）。外したいユーザーは FR-CAP-06 のアプリ単位の除外として追加する（UI / API 対称）。フォーム記入（Scribe）は引き続きターミナルを読まず記入しない

**FR-CAP-06（MUST, All）**: ユーザーは除外リストにアプリ単位・ドメイン単位（ブラウザのURL/タイトルパターン）で追加・削除できる。既定項目のうちパスワードマネージャとSecureTextFieldは**削除不可**（UI上グレーアウト）。

**FR-CAP-07（MUST, All）**: 一時停止:
- グローバルホットキー（既定 `⌃⌥⇧P`、変更可）で即時トグル
- 停止中はインジケータがグレー、Notchに「Paused」表示
- 再開方法: 同ホットキー / Notchから / 時限再開（15分・1時間・今日中）を選択可
- 停止中はevent log書き込み・context cache更新への入力が発生しない（連携同期は継続する）

**FR-CAP-08（MUST, All）**: Accessibility権限のグレースフルデグラデーション:
- 権限なしでもクラッシュ・エラーダイアログ連発をしない。キャプチャ系のみ停止し、連携統合・検索（連携データ）・Morning Briefは動作する
- インジケータはグレー、Hoverで「Enable capture」導線（システム設定の該当ペインを直接開く）
- 権限付与を1時間ごとに再検出し、付与され次第自動でキャプチャ開始する
- **【2026-10-03 オーナー決定｜Issue #778 オプションC】** 縮退状態で止まるのは画面読み取り系（コンテキストキャプチャ、画面テキストを要するコンテキストアクション、その他Accessibility権限を要するもの）のみ。設定・チャット・会議ノート・Morning Brief・連携（Connections）は使える。展開したパネルには権限付与を促す**常設・非ブロッキングのバナー**を出し、ボタンでシステム設定の該当ペイン（`open_accessibility_settings`）を開く（Hoverの「Enable capture」導線はこのバナーと折りたたみ時の警告表示で満たす）。権限付与の再検出は1時間ごとではなく数秒間隔の無音チェック（プロンプトを出さない）で行い、付与され次第アプリの再起動なしに縮退状態を自動で終える。判定は `shogun_core::capture::accessibility_gate`（純粋関数）

**FR-CAP-09（MUST, All）**: キャプチャデーモンは絶対に落とさない: AX API呼び出しの失敗・タイムアウト（個別呼び出し500ms上限）は該当イベントのスキップとして処理し、`unwrap()` を使わない。パニックが起きた場合もキャプチャスレッドのみ再起動し、アプリ全体を巻き込まない。

**受け入れ基準（6.2）**: 除外リスト既定値の全アプリで「イベントが1件も生成されない」ことの自動テスト（AXモック）。権限なし起動の統合テスト。24時間連続稼働でクラッシュ0・CPU 5%以下（1分平均）の計測ログ。

### 6.3 メモリ（3層メモリ + event log）

**目的**: 年単位で生きるメモリ基盤。書き込みは絶対に失わず、検索は500msで返す。

#### 6.3.1 3層構造

**FR-MEM-01（MUST, All）**: 3層メモリを実装する:

| 層 | 範囲 | 置き場所 | 内容 |
|---|---|---|---|
| **Hot** | 直近24h | RAM（shogun-core内） | 直近イベントの構造化バッファ＋要約。context cacheの主材料 |
| **Warm** | 直近30日 | SQLite | 全イベント行＋float32相当のembedding（sqlite-vec）＋FTS5索引。**ベクトル検索の対象はこの層のみ** |
| **Cold** | 全履歴 | SQLite（期間パーティション） | 圧縮済みイベント＋int8量子化embedding。月単位パーティション |

**FR-MEM-02（MUST, All）**: Hot層はRAM専用だが、**元データは常にWarm層（ディスク）に先に書かれている**こと。Hot層はキャッシュであり、プロセス再起動時はWarm層から直近24h分を再構築する（起動から10s以内にバックグラウンド完了）。Hot層のRAM使用は200MBを上限とし、超過時は古い順に要約へ畳み込む。

**FR-MEM-03（MUST, All）**: 通常のベクトル検索はWarm層のみを対象にする（sqlite-vecは総当たりのため。CLAUDE.mdデータモデル原則）。Cold層の検索は (a) FTS5 trigram全文検索は全期間対象、(b) ベクトル検索はユーザーが明示的に期間指定した場合のみ該当パーティションに対して実行、の2形態に限る。

**FR-MEM-04（MUST, All）**: 層間移動はDream Cycle（§6.7）が行う: Warm層で30日を超えた行は、Dream Cycleで要約・統合された上でCold層パーティションへ移動し、embeddingはint8量子化される。移動はトランザクション内で行い、失敗時は元の状態を維持する。

#### 6.3.2 event log

**FR-MEM-10（MUST, All）**: event logは**追記型（immutable）**とする。UPDATEは `last_seen_at` / `dwell_ms` の重複抑制更新（FR-CAP-03）のみ許可。DELETEはユーザー明示の削除操作（FR-SET-07）とCold層への移動時のみ。

**FR-MEM-11（MUST, All）**: event logの主要カラム（スキーマ概要。詳細はマイグレーションが正）:

| カラム | 型 | 説明 |
|---|---|---|
| `id` | INTEGER PK | — |
| `ts` | INTEGER (unix ms) | 発生時刻 |
| `source` | TEXT | `capture` / `gmail` / `gcal` / `slack` / `notion` / `github` / `linear` / `agent` / `user` |
| `kind` | TEXT | `text` / `focus` / `message` / `event` / `action_executed` など列挙 |
| `app_bundle_id` | TEXT NULL | キャプチャ時のアプリ |
| `window_title` | TEXT NULL | — |
| `content` | TEXT | 抽出テキスト本体 |
| `content_hash` | TEXT | 重複抑制用 |
| `last_seen_at` | INTEGER | 重複抑制更新 |
| `dwell_ms` | INTEGER | 滞在時間（spatial-ready） |
| `display_id` | INTEGER NULL | spatial-ready |
| `window_bounds` | TEXT NULL (JSON) | spatial-ready |
| `window_pose` | TEXT NULL (JSON) | spatial-ready（v1では常にNULL可） |
| `gaze_target` | TEXT NULL | spatial-ready（v1では常にNULL可） |

**FR-MEM-12（MUST, All）**: spatial-readyカラム（`window_pose` / `gaze_target` / `dwell_ms` / `display_id` / `window_bounds`）は**v1初回マイグレーションから存在**させる。v1で値を書かないカラムはNULLでよいが、後付けマイグレーションにしない。

#### 6.3.3 検索

**FR-MEM-20（MUST, Std）**: ローカル検索はハイブリッド: FTS5（trigramトークナイザ）＋Warm層ベクトル検索（sqlite-vec）の結果をスコア統合（Reciprocal Rank Fusion）して返す。p95 500ms以内（NFR-SLO-04）。

**FR-MEM-21（MUST, Std）**: embeddingは**同梱のローカルONNX多言語モデル**で生成する（付録A ADR-001）。要件として固定する性質: ローカル実行・多言語（最低限、英語と日本語を含むCJK）・オフライン動作・推論の追加限界費用ゼロ・出力がsqlite-vecに格納可能。モデルの最終選定（候補: multilingual-e5-small等）はPhase 1着手時のベンチで確定する（§9）。クラウドembedding APIは使わない。

**FR-MEM-22（MUST, Std）**: embedding生成は書き込みパスをブロックしない（event log書き込み後の非同期ジョブ。遅延許容は5分。未embedding行はFTS5のみで検索対象になる）。

**FR-MEM-23（MUST, All）**: 検索結果の各行は出典（source・アプリ・時刻）を表示し、キャプチャ由来と連携由来を視覚的に区別する。

#### 6.3.4 マイグレーションと互換性

**FR-MEM-30（MUST）**: スキーマ変更はrefineryによるバージョン管理マイグレーションのみで行う。手書きの `ALTER TABLE` をアプリコードに埋め込まない。各マイグレーションにはロールバック手順（ドキュメント）を必須添付する。

**FR-MEM-31（MUST）**: **後方互換を破るマイグレーションを書かない**: カラム削除・意味変更・型の非互換変更を禁止。必要なら新カラム追加＋書き込み二重化＋読み取り移行の3段階で行う。アプリのダウングレード時に旧バージョンがDBを開けなくなる変更は、メジャーバージョンアップ＋明示の告知なしに行わない。

#### 6.3.5 スレッドとContext Pack

**目的**: event logは平坦で、1通のメールとその返信と それを読んでいたウィンドウは無関係な行として並ぶ。「あの件どうなった」に答えるには、ランク付けと回答の単位となるグルーピングが要る。

**FR-MEM-40（MUST, Std）**: event log行は `thread_key`（会話・スレッド識別子）を持ちうる。生成規則はソース別に安定していること: Gmail=threadId / Slack=channel+thread_ts / GitHub=issue URL / ai_session=session id / キャプチャ=正規化したアプリ＋ウィンドウタイトル。**追加はadditiveに行う**（既存行はNULLのままで従来どおり振る舞い、thread_keyを読む側はNULLを必ず扱う）。

**FR-MEM-41（MUST, Std）**: `threads` テーブルをstateとして持つ（`thread_key` / `source` / `title` / `summary` / `participants` / `project_id` / `first_activity_at` / `last_activity_at` / `event_count` / `salience` / `confidence`）。`summary` はDream Cycleが更新し、それまではNULL。`salience`（直近性・未処理度・現在画面との一致・語句一致の加重）は**再計算値であり正本ではない**。threadsは推定物なので、他のstate行と同じくconfidenceを持ち、確度ゲート（FR-ST-20）未満を事実として提示しない。

**FR-MEM-42（MUST, Std）**: チャット・ドラフト・Memory APIへ渡すコンテキストは、単一の組み立て関数（Context Pack）で作る。入力は問い（query）と予算（件数・抜粋長）、出力はstate factsと、ハイブリッド検索で得た**日付・出典つきの根拠行**。1つの巨大なウィンドウキャプチャがプロンプトを食い潰さないよう、件数と1件あたりの抜粋長の双方を上限で抑える。UIチャットとMemory APIは同一の組み立てを使う（不変条件6）。

**FR-MEM-43（SHOULD, Std）**: Context Packには圧縮版の経路を持てる。圧縮は**ローカル処理のみ**（LLMを呼ばない）とし、時間予算を超過または失敗した場合は非圧縮版へフォールバックする。

**受け入れ基準（6.3.5）**: 問いに答えるイベントが検索で取れること、直近ではなく**問いに関連する**イベントが選ばれること、巨大キャプチャが抜粋予算に収まること、一致がなくてもstate factsだけは返ること。Context Pack組み立てのp95をNFR-SLO-04（500ms）に対して計測する。

**受け入れ基準（6.3）**: マイグレーションを空DBと「v1初版スキーマのダミーデータ入りDB」の両方に適用するCIテスト。10万イベント投入時の検索p95計測。プロセスkill→再起動でHot層が再構築されevent logに欠損がないこと。

### 6.4 State tables

**目的**: ワールドモデルの本体。event logから推定された「現在の状態」を、根拠と確度付きで保持する。

#### 6.4.1 共通規則

**FR-ST-01（MUST, Std）**: state tablesは `people` / `projects` / `commitments` / `open_loops` の4テーブルとし、event logと**物理的に分離**する（CLAUDE.mdデータモデル原則）。

**FR-ST-02（MUST, Std）**: 全stateレコードに以下を必須とする:
- **provenance**: 根拠となるevent log行への参照（1レコードにつき1件以上。中間テーブル `state_provenance(state_table, state_id, event_id, weight)` で多対多）
- **confidence**: REAL、0.0〜1.0
- `created_at` / `updated_at` / `last_evidence_at`（最後に根拠イベントを得た時刻）

provenanceが空のstateレコードをINSERTするコードパスを作らない（DB制約またはリポジトリ層のアサーションで担保）。

**FR-ST-03（MUST, Std）**: stateの更新経路は次の3つのみ:

| 経路 | タイミング | キー |
|---|---|---|
| インライン抽出（軽量分類） | 連携同期・キャプチャ後の非同期ジョブ | Select KKキー（Batch API）またはローカルルール |
| Dream Cycle（統合・再計算） | 夜間バッチ | Select KKキー（Batch API） |
| ユーザー明示編集 | Full UIから | なし（confidence=1.0、provenanceは編集イベント） |

エージェント推論（BYOK）の出力からstateを直接書き換えない（エージェントの提案→ユーザー承認（L2）→ユーザー編集扱い、の経路は可）。

#### 6.4.2 people

**目的**: ユーザーが仕事で関わる人物の現在状態。

| フィールド | 説明 |
|---|---|
| `id`, `display_name`, `aliases` (JSON) | 名寄せ用。メールアドレス・Slackハンドル等をaliasesに保持 |
| `emails` / `handles` (JSON) | 連携アカウント識別子 |
| `relationship_summary` | この人物との関係・進行中の文脈の要約（Dream Cycleが更新） |
| `last_interaction_at` / `interaction_channel` | 最終接触 |
| `pending_from_me` / `pending_from_them` (JSON) | 相互の返信待ち事項（open_loops参照のキャッシュ） |
| provenance / confidence / 各種タイムスタンプ | 共通規則 |

**FR-ST-10（MUST, Std）**: 名寄せ: 同一人物が複数チャネル（Gmailアドレス・Slackハンドル・GitHubアカウント）に現れる場合の統合はDream Cycleで行い、統合の確度もconfidenceに反映する。誤統合のユーザー修正（分割）をFull UIから可能にする。

#### 6.4.3 projects

**目的**: 進行中のプロジェクト・仕事のまとまりの現在状態。

| フィールド | 説明 |
|---|---|
| `id`, `name`, `status` | status: `active` / `waiting` / `paused` / `done`（列挙） |
| `summary` | 現況要約（Dream Cycleが更新） |
| `participants` (JSON) | people参照 |
| `sources` (JSON) | 関連するGitHubリポジトリ・Linearプロジェクト・Notionページ・Slackチャンネル等の識別子 |
| `last_activity_at` | — |
| provenance / confidence | 共通規則 |

#### 6.4.4 commitments

**目的**: 明示的な約束。「誰が・誰に・何を・いつまでに」。

| フィールド | 説明 |
|---|---|
| `id`, `direction` | `mine`（自分が約束した）/ `theirs`（相手が約束した） |
| `counterparty_id` | people参照 |
| `description` | 約束内容 |
| `due_at` (NULL可) | 期日 |
| `status` | `open` / `done` / `overdue` / `cancelled`。`overdue` はDream Cycleが `due_at` 超過で遷移させる |
| `project_id` (NULL可) | projects参照 |
| provenance / confidence | 共通規則 |

**FR-ST-11（MUST, Std）**: commitmentsの生成は「明示的な約束表現」（"I'll send it by Friday" 等）を根拠に持つ場合のみ。推測ベースの義務はcommitmentsでなくopen_loopsに入れる。

**FR-ST-12（MUST, Std）**: `overdue` への遷移はDream Cycleで判定し、Morning Briefとフォローアップ提案（FR-AG-13）の入力になる。

#### 6.4.5 open_loops

**目的**: 開いたままの事項。返信待ち・読みかけ・放置中のレビュー・宙に浮いた決定など。

| フィールド | 説明 |
|---|---|
| `id`, `kind` | `reply_needed` / `waiting_on_them` / `review_pending` / `decision_pending` / `follow_up` / `other`（列挙） |
| `description` | — |
| `counterparty_id` / `project_id` (NULL可) | 参照 |
| `opened_at` / `staleness_days` | 放置日数（Dream Cycleが更新） |
| `status` | `open` / `closed`。closeの根拠（返信を検出した等）もprovenanceに残す |
| provenance / confidence | 共通規則 |

**FR-ST-13（MUST, Std）**: open_loopsの自動クローズ: 対応する完了イベント（返信送信・PRマージ等）を検出したらDream Cycleまたはインライン抽出でclosedへ遷移させる。自動クローズのconfidenceが0.8未満の場合はclosedにせず、Morning Briefで「解決した可能性」として提示する。

#### 6.4.6 confidenceの規則

**FR-ST-20（MUST, Std）**: confidenceの解釈と生成物への渡し方を全コンポーネントで統一する:

| confidence | 解釈 | Context Fusion / 生成物での扱い |
|---|---|---|
| 0.8 〜 1.0 | 高確度 | 事実として使用可 |
| 0.5 〜 0.8未満 | 中確度 | **「〜の可能性」として弱く渡す**。プロンプト内で `possibly:` プレフィクス等の明示マーク必須。生成物内でも断定表現にしない |
| 0.5未満 | 低確度 | 生成物・アクション判断に**使用しない**。検索結果・Full UIの状態閲覧でのみ表示（低確度マーク付き） |

低confidenceの状態をContext Fusionが事実として生成物に混ぜてはならない（CLAUDE.md不変条件）。この規則の適用はshogun-fusionのプロンプト組み立て層で一元実装し、各エージェントに個別実装させない。

**FR-ST-21（MUST, Std）**: confidenceの初期値は抽出器が決め、Dream Cycleが再計算する。再計算の入力: Batchレビュー済みの独立した根拠引用・新しさ（`last_evidence_at` からの経過）・矛盾イベントの有無・ユーザー修正履歴。ユーザーが明示編集したレコードは1.0とし、以後の自動更新はユーザー値を上書きしない（新しい根拠で変化を検出した場合は「変更提案」として提示）。

**2026-10-02 owner-approved FR-ST-21 clarification:** Local extraction produces pending candidates below 0.5. Dashboard actions, briefs, and context actions wait for the scheduled AI Batch review (or an explicit user correction); raw capture and search remain available. If Batch is unavailable, those candidates stay pending rather than silently becoming facts. Only distinct, live, Batch-reviewed quotations can strengthen confidence or refresh evidence age; rereading the same message, layout changes, and repeated capture counts cannot. Automatic corroboration stays below 0.8. Pins remain authoritative; date/status edits preserve supporting links, while changed wording does not claim the old quotation as proof. Ambiguous screen text without a message identity stays separate rather than merging different senders. Unsupported automatic rows are hidden; each local maintenance pass removes at most 100 unpinned rows with no surviving source, clears incoming references, and leaves raw events and user-deletion tombstones untouched.

**受け入れ基準（6.4）**: provenance空のINSERTが失敗するテスト。confidence帯域ごとのプロンプト出力（事実/possibly/除外）のスナップショットテスト。名寄せ分割操作の統合テスト。

### 6.5 Context Fusion

**目的**: `f(state, screen_ctx, intent) → action`。「今この瞬間に押せる正しいボタン」を常時計算しておく。

**FR-CF-01（MUST, Std）**: Context Fusionは純粋に**ローカルで**アクション候補を決定する（LLM呼び出しをアクション提示の同期経路に入れない）。入力: context cache（state関連レコード＋screen_ctx＋Hot層要約）、intentヒューリスティック（直近の行動系列: 例「メールスレッドを30秒以上閲覧」→ reply系intent）。出力: アクション候補リスト（各候補にL1/L2/L3タグ・対象エージェント・優先度スコア）。

**FR-CF-02（MUST, Std）**: コンテキストアクションボタンの提示は、Notch Expanded表示時点から**150ms以内**（NFR-SLO-02）。実体はcache更新時（フォーカス切替から300ms以内、NFR-SLO-05）に計算済みであること。

**FR-CF-03（MUST, Std）**: アクション候補の上限は4件。優先度スコアは（intent一致度、state緊急度（overdue等）、直近の同種アクション採択率）の加重で決め、決定ロジックはユニットテスト可能な純関数として実装する。

**FR-CF-04（MUST, Std）**: screen_ctxに関連stateが見つからない場合（未知の相手・新規文脈）でも、汎用アクション（Save note / Search memory / Extract tasks）を提示し、空のパネルを出さない。

**FR-CF-05（MUST, Std）**: FusionはStandardで提示も実行も動作する（**2026-09-24 改定**: エージェント実行が Standard に含まれたため、Standard でのロック表示は無くなった）。ロック表示が出るのは**トライアル失効・未課金**のみで、その場合もアクション候補4件のうち最大1件までとし、体験を広告面にしない。

**エラー時挙動**: cache構築失敗時は前回の有効cacheを使い、インジケータをアンバーにする。300msを超過した場合も提示は行い、超過をメトリクスに記録する。

**受け入れ基準（6.5）**: 代表シナリオ10件（US-01〜US-11から抽出）でのアクション候補のスナップショットテスト。150ms/300msの計測コード同梱。LLM呼び出しがFR-CF-01の同期経路に存在しないことのアーキテクチャテスト（依存グラフ検査）。

### 6.6 エージェント実行

**目的**: 提示されたアクションを、L1/L2/L3の権限モデル下で安全に実行する。

#### 6.6.1 権限モデル

**FR-AG-01（MUST）**: 全アクションは実行前に必ずL1/L2/L3のいずれかに分類される。未分類のアクションは実行できない（enumで型的に強制）。定義:

| レベル | 承認 | 許可される影響範囲 |
|---|---|---|
| **L1 自動実行** | なし（通知のみ） | ローカル完結・可逆な操作のみ。外部送信を**絶対に**含めない（CLAUDE.md不変条件4） |
| **L2 ワンタップ承認** | Notchで1クリック | デバイス外への影響が**読み取り系または下書き系**に限られるもの（例: Gmailドラフト作成、外部サービスの読み取り取得） |
| **L3 明示確認** | 実行内容プレビュー（送信先・全文）＋明示的確認操作 | 外部に**不可逆**な影響を持つ操作すべて: 送信・投稿・カレンダーイベント作成・削除・イシュー作成・ページ編集など |

**FR-AG-02（MUST）**: レベル判定は「操作の種類」に対して静的に定義する（`shogun-agents` 内の単一の許可テーブル）。実行時の動的引き下げ（L3→L2）を行うコードパスを作らない。引き上げ（L2→L3）は設定で可能（FR-SET-05）。

**FR-AG-03（MUST）**: L3確認UIの必須表示項目: 操作種別、送信先（アドレス/チャンネル/リポジトリ等の完全表記）、送信内容の**全文**（スクロール可の全文であり要約ではない）、経路（直接MCP / 第三者経由Composio）、使用キー種別（BYOK）。確認操作は専用ボタンのクリックとし、Enterキー単独では確定しない。

**FR-AG-04（MUST）**: AI API（MCP/CLI/REST）経由の操作にも**同一のレベル判定・同一の承認UI**を適用する（CLAUDE.md不変条件6）。外部AIがL3操作を要求した場合、Notch/Full UIに承認要求が表示され、ユーザーが確認するまでAPI呼び出しはpendingを返す（タイムアウト10分でrejected）。

**FR-AG-05（MUST）**: L1実行は事後通知をNotchインジケータ＋実行履歴に残す。L1の全操作は「SHOGUNローカルDB・ファイルへの書き込み等、SHOGUN内で取り消し可能」なものに限り、実行履歴からワンクリックで取り消せる（undo可能期間: 7日）。

#### 6.6.2 実行エンジン

**FR-AG-06（MUST, Std/Pro）【2026-10-13 #1101 D1】**: エージェント推論・チャット・ドラフト・Scribe 等の live lane は gateway の会社キーを既定とする。会社キーを端末へ配らない。サーバーが lane ごとの provider / model / effort / tier / 出力上限を決める。rewrite は OpenAI `gpt-6-luna` / `low` / `service_tier: "priority"`、dictation cleanup は Groq。`form_decide` はユーザーの TypeSafe Jev キーのまま。詳細: [gateway 設計](hosted-ai-gateway-design.md)。

**FR-AG-06f（MUST）【2026-10-14 #1101 D10｜2026-10-15 改定: OpenAI のクレジットを先に使う】**: OpenAI モデルを使う lane（live・Batch）は OpenAI 直結を第一経路とする。Azure OpenAI は免除（NFR-PRV-05）の承認後に自動フォールバックとして加え、OpenAI のクレジットが尽きたら第一経路を Azure に切り替える。切り替えは最初のバイトより前の 429・5xx・タイムアウト・未デプロイ、または rewrite で priority が3回続けて付かなかったときだけ（5分後に戻す）。ストリーム開始後は再試行せず、文字を二重に出さない。provider ごとのサーキットブレーカーを gateway に置く。Batch は OpenAI Batch → Azure Global Batch（切り替え後は逆）。応答ごとに実際の service tier を記録する。第一経路の入れ替えは lane 設定の変更で行う（アプリのリリース不要）。Azure は NFR-PRV-05 の承認前に実ユーザーの通信を受けない。dictation cleanup（Groq）と ASR（Deepgram）は対象外。

- **サブスク委譲（任意）**: ログイン済みの公式 CLI（`claude` / `codex` / `gemini`）を opt-in で起動する既存経路。SHOGUN は他アプリの資格情報ファイル・Keychain エントリを読まず、コンシューマ OAuth を自前実装しない。
- **BYOK（任意）**: ユーザーの API キーを Keychain に保存。レーンごとに BYOK / 委譲を選択した場合、そのレーンは gateway を呼ばない。

**FR-AG-06a（MUST）**: Batch は会社キー専用で、BYOK・サブスク委譲を使わない。サブスク委譲は Agent lane 専用とする。Batch lane（インデックス・分類・Dream Cycle・Morning Brief）には使わない。委譲先が消費するのは**ユーザー本人のプラン枠**であり、その枠がどの課金制度で運用されていても（現行=対話利用と共有の5時間ローリング＋週次上限／発表後に施行停止された月次クレジット制ならその有限クレジット）、バッチ量はそれを最速で溶かす作業である——現行制度では**ユーザー本人の対話用 Claude Code まで数分で止め**、クレジット制なら Agent lane を月替わりまで殺す。どちらの制度でも、Select KK が既にバッチ単価でカバーしている作業と引き換えにするには壊滅的すぎる（不変条件5の型レベル分離で担保: `SubscriptionAgentClient` は `AgentClient` のみ実装し `BatchClient` を実装しない）。

> **前提となるポリシー（変更前に必ず確認すること｜2026-08-24 に一次情報で再確認済み）**: Anthropic の方針は 2026 年に3回動いている。(1) 2026年2〜4月: 第三者からのサブスクOAuth利用を禁止・サーバ側でブロック。(2) 2026-05-13 発表: 6月15日から Agent SDK / `claude -p` / 第三者アプリの利用を月次の Agent SDK クレジット（Pro $20 / Max 5x $100 / Max 20x $200 相当、API標準レート課金）に分離すると告知。(3) **2026-06-15（施行予定日当日）: この変更は施行前に一時停止され、以後も施行されていない**。現行の挙動は「Agent SDK・`claude -p`・**ユーザーのサブスクで認証する第三者アプリ**は、プランの通常の利用上限（5時間ローリングウィンドウ＋週次上限）から消費する」であり、対話利用と同一プールである（Help Center「Use the Claude Agent SDK with your Claude plan」）。委譲が公式に名指しされた正当な利用面であることと、**サブスクOAuthトークンの自前発行・窃取による直接API呼び出しが恒久的に禁止**であることは全変遷を通じて不変。**課金制度は今後も動きうるため、製品文言・コードに回復時期（「月替わり」「5時間」等）をハードコードしてはならない**。OpenAI 側には第三者利用に関する同等の公式声明が確認できないため、`codex exec` はあくまで「ユーザー自身がインストール・ログインしたCLIをユーザー自身が動かす」位置づけとする。

**FR-AG-06b（MUST）**: サブスク委譲の利用開始には**明示的opt-in同意**を必須とする。コンシューマ向けプランのデータ取り扱いはメーターAPI経路と同一ではなく、その差はユーザーが引き受ける判断であるため、既定ONにしない。同意なしで委譲先が選択されている場合、Agent lane は生成を行わない。

**FR-AG-06c（MUST）**: 委譲経由の送信もegressであり、トレーサビリティに `route = local_agent` として記録する（AR-11。記録は従来どおりダイジェスト＋バイト長のみ）。トレーサビリティ画面は「ローカルCLI経由」と委譲先ベンダーを表示する。第三者バッジ（FR-C2-04）は付けない——ユーザー自身が契約したベンダーであり、Composioのような中継ではないため。

**FR-AG-06d（MUST）**: プロンプトは委譲先プロセスの**標準入力**で渡す。argvは同一マシンの全プロセスから `ps` で読めるため、キャプチャ由来テキストを置いてはならない。また子プロセスの環境からSHOGUN側のAPIキー（`ANTHROPIC_API_KEY` 等）を除去する——残すと委譲先がサブスクではなくメーター課金で認証し、ユーザーが避けたはずの従量課金が黙って発生する。

**FR-AG-06e（MUST）**: プラン枠の使い切りは、資格情報の問題（401）とも SHOGUN の障害とも区別して提示する。文言はどのプランの枠かを明示し、SHOGUN側の失敗と読めてはならない。**回復時期を約束しない**——枠が時間窓か月次クレジットかはベンダーの課金制度であり、2026年中に複数回変わっている（FR-AG-06a のポリシー注記）。「プラン自身のスケジュールで回復する」旨と、**BYOK（APIキー）へのフォールバック導線**を必ず出す。実行時（実際のドラフト/チャット失敗）に検出した枠切れは Settings の委譲先状態にも反映する（`--version` 再検出では観測できないため、ラッチする）。

資格情報が未設定のProユーザーには、該当機能の起動時に設定導線を出す（検出済みのサブスク委譲先を第一選択として提示し、APIキー入力は副次選択に置く）。

**FR-AG-07（MUST, Pro）**: アクション実行→初トークン表示は**1s以内、ストリーミング必須**（NFR-SLO-03）。非ストリーミングの一括応答UIを作らない。（2026-09-25: 初トークンは「最初に見える1文字」で計測し、Codex 委譲は SLO-03 の対象外。§7.1 の NFR-SLO-03 計測定義を参照）

**FR-AG-08（MUST, Pro）**: LLMプロバイダは trait（`LlmProvider`: 補完・ストリーミング・ツール呼び出しの抽象）で分離する。v1の実装はAnthropicのみ（付録A ADR-002）。trait境界にAnthropic固有型を漏らさない。

**FR-AG-09（MUST, Pro）**: エージェントのプロンプトに渡すstateはFR-ST-20のconfidence規則を通す。エージェントがツール（第1層MCP等）を呼ぶ場合、各ツール呼び出しにもFR-AG-01のレベル判定を適用する（エージェント内部からのL3操作も個別に明示確認を要求）。

#### 6.6.3 プリセットエージェント（v1で7種）

各プリセットの「実行レベル」は操作ごとに記す。すべてPro機能（実行はBYOK）。提示（Fusion）はStandardでも行われる（FR-CF-05）。

| ID | 名称 | 内容 | レベル |
|---|---|---|---|
| **FR-AG-10** | Reply Drafter | フォーカス中のメール/Slackスレッドへの返信ドラフト生成。state（相手・約束・プロジェクト）を反映 | ドラフト生成=L2 / 送信=L3（送信はGmailのみ、Composio経由・オプトイン） |
| **FR-AG-11** | Meeting Prep | 次の会議の参加者・議題に関する状態集約ブリーフィングをローカル生成・表示 | 集約・表示=L1（外部送信なし。LLM整形を使う場合はL2起動） |
| **FR-AG-12** | Task Extractor | 画面中/スレッド中のテキストからタスク・約束を抽出し、open_loops / commitmentsへの追加を提案 | 抽出提案=L2（承認でstate書き込み） |
| **FR-AG-13** | Follow-up Sentinel | overdueなcommitments・staleなopen_loopsを検出し、フォローアップドラフトを提案 | 検出・提示=L1相当（Dream Cycle成果の表示） / ドラフト生成=L2 / 送信・投稿=L3 |
| **FR-AG-14** | Calendar Scheduler | スレッド文脈から会議候補を抽出し、Google Calendarイベント作成を提案 | 空き時間読み取り=L2 / イベント作成=**L3**（カレンダー作成は不可逆扱い、CLAUDE.md不変条件4） |
| **FR-AG-15** | Issue Triage | 画面文脈からGitHub/Linearのイシュー・コメントのドラフトを作成 | ドラフト表示=L2 / イシュー作成・コメント投稿=L3 |
| **FR-AG-16** | Note Capture | 現在の文脈をNotionページ/データベース行として保存する下書きを作成 | 下書き=L2 / Notionへの書き込み=L3 |

**FR-AG-17（MUST, Std/Pro）**: 自由入力チャット: Notch Expanded / Full UIからBYOKでのチャットを提供する。チャットはcontext cacheを自動添付し（添付内容はユーザーが展開して確認可能）、チャット内からのツール実行にもFR-AG-01を適用する。

**FR-AG-18（MUST, Std/Pro）**: 実行履歴: 全エージェント実行（L1含む）を実行履歴に記録し、Full UIで閲覧可能にする。記録項目: 時刻、エージェントID、レベル、承認方法、結果（成功/失敗/キャンセル）、外部送信の有無（有ならトレーサビリティログへのリンク）。

**エラー時挙動**: BYOKの401/403はインジケータ赤＋キー再設定導線。レート制限（429）は指数バックオフ（最大3回）後にL2/L3提案を「再試行」ボタン付きで保持。実行途中の失敗は部分実行の内容を明示する（「ドラフトは作成済み・送信は未実行」等）。

**受け入れ基準（6.6）**: 許可テーブルの網羅テスト（全操作種別にレベルが定義されている）。「L1に分類された操作が外部送信APIに到達しない」ことのアーキテクチャテスト。L3確認なしで外部送信が発生しないことのE2Eテスト（モックMCP）。初トークン1sの計測コード同梱。

### 6.7 Dream Cycle

**目的**: 夜間バッチで「今日の生データ」を「明日使えるワールドモデル」へ変換する。

**FR-DC-01（MUST, Std）**: Dream Cycleは1日1回、既定02:00〜06:00（ローカル時刻、変更可）のウィンドウで実行する。実行条件: (a) ユーザーがアイドル（入力なし15分以上）または画面ロック中、(b) 電源接続中またはバッテリー30%以上。条件を満たさないままウィンドウを過ぎた場合、次回アイドル時に縮退版（state更新のみ）を実行し、フルサイクルは翌夜に持ち越す。Macがスリープしていた場合はウェイク後の最初のアイドルで実行する。

**【2026-10-12 オーナー決定｜#1066 B】派生メモリ処理（consolidation と期間要約）はローカル時刻の毎時に実行する。** 各時間に閉じた1時間分（前回カーソル〜その時間の頭）を1つの Batch ジョブとして送る。窓 ID は `YYYYMMDD-hHH`（冪等キー）、カーソルは完了済み consolidation の高水位で、寝ていた・オフライン・relay 停止の時間は次の実行が順番通りにまとめて拾い、二重送信しない。新しい素材が無い時間は何も送らない。アイドル要件は課さない（電源条件のみ）。Batch レーン未提供・メモリロック中は毎時サイクル自体を作らない（ローカルレーンは夜間サイクルが担う）。夜間ウィンドウ（02:00〜06:00）は Morning Brief・Warm→Cold 降格・30日パターン分布・取りこぼしの残り分のためだけに残る。送信量（チャンク数）は同じテキストを1回ずつ送るので夜間一括と同等で、relay の日次上限はリクエスト数ではなくチャンク数で数えるため変更不要。

**FR-DC-02（MUST, Std）**: LLM処理は**Select KKキーのAnthropic Batch API**で実行する（CLAUDE.md不変条件5）。BYOKをDream Cycleに使わない。送信するのは処理用チャンク（当日イベントの抽出・整形済み断片）のみで、送信内容はトレーサビリティログに記録する（AR-11）。

**FR-DC-03（MUST, Std）**: Dream Cycleのジョブ内容（実行順）:

| # | ジョブ | 内容 |
|---|---|---|
| 1 | 統合（consolidation） | 当日のevent logからstate tables更新候補を抽出（新規people/projects、commitments、open_loops開閉） |
| 2 | 圧縮（compression） | 当日イベントの日次要約を生成しHot層/検索用に格納。冗長イベントの要約統合 |
| 3 | state更新 | 候補の適用。既存レコードとの名寄せ・矛盾検出 |
| 4 | confidence再計算 | FR-ST-21の入力で全stateレコードを再計算。overdue/staleness更新 |
| 5 | Cold層への降格 | Warm層の30日超過分を要約・int8量子化してCold層パーティションへ移動（FR-MEM-04） |
| 6 | Morning Brief生成 | §6.8 |

**FR-DC-04（MUST, Std）**: 冪等性とクラッシュ耐性: 各ジョブはジョブ実行テーブル（job_runs: ジョブ種別・入力範囲・状態）で管理し、途中失敗時は完了済みジョブをスキップして再開できる。同一入力範囲への二重適用でstateが壊れないこと（upsert設計）。

**FR-DC-05（MUST, Std）**: Batch API失敗時（24h以内に結果が得られない場合を含む）: インジケータをアンバーにし、翌夜のサイクルで未処理分をまとめて処理する。3日連続失敗で赤＋Full UIに詳細表示。**ローカル機能（キャプチャ・検索・Fusion提示）はBatch API失敗の影響を受けない**こと。

**FR-DC-06（MUST, Std）**: Dream Cycleの実行結果サマリ（処理イベント数・state変更数・所要時間・送信チャンク数）をFull UIで閲覧可能にする。

**受け入れ基準（6.7）**: 途中killからの再開テスト。二重実行の冪等性テスト。Batch API全滅時にローカル機能が無影響であることの統合テスト。

### 6.8 Morning Brief

**目的**: 「Macを開いた瞬間、今日が整理されている」体験。

**FR-MB-01（MUST, Std）**: Morning BriefはDream Cycleの最終ジョブとして生成する（Select KKキー・Batch API）。生成内容:

| セクション | 内容 | 出典 |
|---|---|---|
| Today | 今日のカレンダー予定（時刻順）＋各予定へのMeeting Prep導線 | Google Calendar（接続時） |
| Commitments due | 今日期限・期限超過のcommitments | state tables |
| Open loops | staleness上位のopen_loops（最大5件） | state tables |
| What happened | 昨日の要約（3〜5行） | Dream Cycle圧縮ジョブ |
| Suggested actions | 推奨アクション最大3件（各L2/L3ボタン付き） | Fusion/Follow-up Sentinel |

**FR-MB-02（MUST, Std）**: 生成言語はユーザー設定の**出力言語**（FR-SET-04）に従う。UI枠組みは英語（v1）だが、Brief本文は設定言語で生成する。

**FR-MB-03（MUST, Std）**: 提示タイミング: 生成完了後、当日最初の「画面ロック解除またはアプリフォーカス」から60s以内にNotchインジケータをゴールドにし、Hoverプレビューに「Morning Brief ready」を表示する。自動でExpandedをポップアップさせない（割り込まない原則）。既読管理を行い、既読後はインジケータを白へ戻す。

**FR-MB-04（MUST, Std）**: Dream Cycle失敗等でBrief本文が生成できなかった朝は、ローカルデータのみの縮退Brief（カレンダー＋overdue commitments一覧。LLM文章なし）を表示する。空画面・エラー画面を朝一番に見せない。

**FR-MB-05（MUST, Std）**: Brief内の各項目は出典（state/イベント）へのリンクを持ち、低confidence項目（0.5〜0.8）は「possibly」表示で区別する（FR-ST-20）。

**FR-MB-06（SHOULD, Std）**: カレンダー統合の詳細: 当日の予定変更（Brief生成後の追加・削除）はBrief表示時にローカルで差分反映する（再生成はしない。差分は「Updated」マーク付き）。

**受け入れ基準（6.8）**: 生成〜提示のE2Eテスト（モックBatch API）。縮退Briefの表示テスト。出力言語設定（例: 日本語）でBrief本文が該当言語になるテスト。

### 6.9 第1層連携（公式リモートMCP直結）

**目的**: 外部サービスの文脈をワールドモデルに統合し、承認済みの書き込みを実行する。

#### 6.9.1 共通要件

**FR-INT-01（MUST, Std）**: 第1層連携は各サービスの**公式リモートMCPサーバー**へ、shogun-mcp（Rust MCP SDKクライアント）から直接接続する。中間サーバー（Select KK運営サーバー含む）を経由しない。

**FR-INT-02（MUST, Std）**: OAuthは**ユーザー→サービス直接**（MCP仕様のOAuth 2.1 Authorization Code + PKCEフロー、システムブラウザで認可）。アクセストークン・リフレッシュトークンは**Keychainのみ**に保存する（CLAUDE.md不変条件7）。トークンをDB・設定ファイル・ログ・webviewに書き出さない。

**FR-INT-03（MUST, Std）**: 段階導入（Wave）: Wave 1 = Gmail + Google Calendar → Wave 2 = Slack → Wave 3 = Notion + GitHub + Linear。各Waveは前Waveの安定（接続成功率95%以上・クラッシュ増加なしを2週間）を確認してから既定で有効化する。未リリースWaveのサービスは設定画面に「Coming soon」として表示してよい。

**FR-INT-04（MUST, Std）**: 同期方式: 各サービスの読み取りは15分間隔のポーリング（サービス側のレート制限に応じて延長）＋フォーカス文脈に応じたオンデマンド取得（例: Gmailスレッド表示中の該当スレッド取得）。取得データはevent log（`source` = サービス名）へ正規化して格納し、キャプチャ由来イベントと同一の検索・Fusion経路に乗せる。

**FR-INT-05（MUST, Std）**: 読み取りスコープは各サービスで**必要最小限を要求**し、下表の「読み取り範囲」を超えるスコープを要求しない。書き込み操作は下表のレベル列に従う（外部送信系は必ずL3。CLAUDE.md不変条件4）。

**FR-INT-06（MUST, Std）**: 接続失敗・トークン失効時: インジケータをアンバーにし、該当サービスのみ再認証導線を出す。他サービス・ローカル機能に影響を波及させない。再認証まで該当サービスのデータは「最終同期時点」として扱い、鮮度をUIに表示する。

**FR-INT-07（MUST, Std）**: 各サービスはユーザーが個別に接続・切断できる。切断時、Keychainのトークンを削除し、以後の同期を停止する。切断時に既存の取り込み済みイベントを削除するかはユーザーに選択させる（既定: 保持）。

#### 6.9.2 サービス別許可範囲表

以下の表が各サービスの実装範囲の正である。表にない操作を実装しない。

**Gmail（Wave 1）**

| 操作 | 範囲 | レベル |
|---|---|---|
| 読み取り | 受信・送信済みメールのメタデータ＋本文、スレッド構造、ラベル | 同期=バックグラウンド / オンデマンド取得=L2（初回接続時に一括許可可） |
| ドラフト作成・更新 | ユーザーのDraftsフォルダへの下書き | L2 |
| ラベル付与 | ユーザー操作起点、または**ラベル単位の常設承認**（Auto Archive / Sync label to Gmail のトグルON = L2の事前承認。実行ごとに実行履歴＋7日undo。`docs/specs/issue-319-mail-triage-spec.md` FR-TG-21）【2026-09-04 改版 #319】 | L2 |
| 既読化 | v1では実装しない（読んだのはSHOGUNでありユーザーではない。#319） | — |
| **送信** | **第1層では実装しない**。送信はComposio（第2層、§6.10）のみ | —（Composio側でL3） |
| アーカイブ（INBOXラベル除去。可逆） | 常設承認済みラベルの自動実行、またはユーザー操作起点。安全則（既知の相手・同一ドメイン・自分が発言したスレッド・commitment紐付きは対象外）と条件は #319 FR-TG-14/22 に従う【2026-09-04 改版 #319】 | L2 |
| 削除・ゴミ箱・スパム報告 | v1では実装しない | — |

**Google Calendar（Wave 1）**

| 操作 | 範囲 | レベル |
|---|---|---|
| 読み取り | ユーザーのカレンダー一覧・予定（参加者・時刻・場所・説明） | 同期=バックグラウンド |
| 空き時間照会 | free/busy | L2 |
| イベント作成 | 参加者への招待送信を伴うため不可逆 | **L3** |
| イベント更新・削除 | 同上 | **L3** |

**Slack（Wave 2）**

| 操作 | 範囲 | レベル |
|---|---|---|
| 読み取り | 参加チャンネル・DMのメッセージ、メンション、スレッド | 同期=バックグラウンド |
| ドラフト生成（ローカル） | SHOGUN内での下書き作成（Slackへは未送信） | L2 |
| メッセージ投稿・返信 | チャンネル・DMへの投稿 | **L3** |
| リアクション | 外部可視のため | **L3** |

**FR-INT-30（MUST, Std）**: Slackフォールバック: ワークスペース管理者承認が得られず公式リモートMCPに接続できない場合、Slack向けアクションは「ドラフト生成→クリップボードコピー」へフォールバックする（CLAUDE.md連携実装ルール）。フォールバック時のUIは投稿ボタンを出さず「Copy to clipboard」のみを出し、コピー実行はL2とする（デバイス外送信を伴わないため）。Slack公式リモートMCPの提供状況はWave 2着手時に再確認する（§9）。

**Notion（Wave 3）**

| 操作 | 範囲 | レベル |
|---|---|---|
| 読み取り | ユーザーが接続時に許可したページ・データベース | 同期=バックグラウンド |
| ページ/DB行の作成 | 許可済みスペース内 | **L3** |
| ページ更新（追記含む） | 同上 | **L3** |
| 削除 | v1では実装しない | — |

**GitHub（Wave 3）**

| 操作 | 範囲 | レベル |
|---|---|---|
| 読み取り | ユーザーがアクセス可能なIssue / PR / 通知 / コミットメタデータ（コード本文の一括取り込みはしない。参照時のみ） | 同期=バックグラウンド |
| Issue / PRコメントのドラフト | SHOGUN内下書き | L2 |
| Issue作成・コメント投稿 | — | **L3** |
| PRマージ・クローズ・ブランチ操作 | v1では実装しない | — |

**Linear（Wave 3）**

| 操作 | 範囲 | レベル |
|---|---|---|
| 読み取り | ユーザーのチームのIssue・プロジェクト・自分へのアサイン | 同期=バックグラウンド |
| Issueドラフト（ローカル） | SHOGUN内下書き | L2 |
| Issue作成・更新・コメント | — | **L3** |
| ステータス変更 | 外部チームに可視のため | **L3** |

**受け入れ基準（6.9）**: 各サービスにつき「表にない操作のMCPツール呼び出しが実行エンジンの許可テーブルで拒否される」テスト。トークンがKeychain以外（DB・ファイル・ログ）に現れないことの検査（結合テストでのファイルシステム/ログのgrep検査）。Slackフォールバックの動作テスト。

### 6.10 第2層連携（Composio）

**目的**: 公式MCPが提供しない操作（v1ではGmail送信のみ）を、明示的なオプトインの下で提供する。

**FR-C2-01（MUST, Pro）**: 第2層はComposio経由とし、v1の対象操作は**Gmail送信のみ**。他サービス・他操作を第2層に追加する場合は本書の改版を要する。

**FR-C2-02（MUST, Pro）**: 完全オプトイン: 既定は無効。有効化時に専用の同意画面を表示し、次を明示する: (a) 送信がComposio（第三者）のインフラを経由すること、(b) 経由するデータの種類（送信先・件名・本文）、(c) いつでも無効化できること。同意なしに接続フローを開始しない。

**FR-C2-03（MUST, Pro）**: **ドラフト止まりモード**: Composio有効時にも「送信は行わずGmailドラフト作成まで」に制限する設定を必ず用意する（既定: ON）。ONの間、送信系アクションはUIに現れない（グレーアウトではなく非表示。誤操作余地を残さない）。

**FR-C2-04（MUST, Pro）**: Composio経由の送信は**L3**（プレビュー: 宛先・件名・本文全文）。トレーサビリティ画面の該当エントリに「**第三者経由（via Composio）**」バッジを必ず表示する（CLAUDE.md連携実装ルール）。

**FR-C2-05（MUST, Pro）**: Composioの認証情報もKeychainのみに保存。Composio障害時は送信アクションを自動的にドラフト作成（第1層）へフォールバックし、その旨を実行結果に明示する（サイレントに経路を変えて送信はしない — 送信自体は必ず失敗として扱い、ドラフト保存＋通知）。

**受け入れ基準（6.10）**: オプトインなしで送信経路のコードに到達しないテスト。ドラフト止まりモードON時に送信UIが非表示になるテスト。トレーサビリティの「第三者経由」表示テスト。

### 6.11 Memory API（MCPサーバー / CLI / REST）

**目的**: SHOGUNのメモリとワールドモデルを、ユーザーが使う他のAIから利用可能にする。**人間UIとAI APIは完全対称**（CLAUDE.md不変条件6）。

**FR-API-01（MUST, Pro）**: 提供面は3つ:
- **MCPサーバー**（stdio + Streamable HTTP/localhost）: 外部AIエージェント向け
- **CLI**（`shogun` コマンド、shogun-cli）: スクリプト・ターミナル向け
- **REST**（`http://127.0.0.1:7464`、localhostバインドのみ。既定ポート7464は設定で変更可能、使用中の場合は起動時に自動フォールバックし実ポートをCLI `shogun api status` で取得可能にする）: 任意クライアント向け

3面は同一のRust内部API（shogun-core経由）の薄いラッパであり、機能差を作らない。

**FR-API-02（MUST, Pro）**: v1の公開ツール/エンドポイント（最小セット。UIの対応機能と対称）:

| ツール | 内容 | レベル |
|---|---|---|
| `memory.search` | ハイブリッド検索（FR-MEM-20と同一実装） | 読み取り |
| `memory.get_context` | 現在のcontext cache（screen_ctx除外オプション付き） | 読み取り |
| `state.people.list/get` | people照会（confidence付き） | 読み取り |
| `state.projects.list/get` | projects照会 | 読み取り |
| `state.commitments.list/get` | commitments照会 | 読み取り |
| `state.open_loops.list/get` | open_loops照会 | 読み取り |
| `memory.append_note` | ユーザーメモとしてevent logへ追記 | **L1**（ローカル・可逆） |
| `state.propose_update` | state変更の提案（commitment追加等） | **L2**（Notchで承認） |
| `actions.execute` | プリセットエージェントの起動 | 対象操作のレベルに従う（L3操作はFR-AG-04の承認フロー） |

**FR-API-03（MUST, Pro）**: 認証: クライアントごとのAPIトークンを発行（Full UIで発行・失効管理）。トークンはKeychainに保存し、REST/HTTPはlocalhostのみにバインドする。トークンなしの呼び出しは読み取り含め全拒否。

**FR-API-04（MUST, Pro）**: AI経由の操作にも同一のL1/L2/L3を適用する（FR-AG-04）。APIからの承認待ち操作は呼び出し元にpending状態と承認要求IDを返し、承認/拒否/タイムアウト（10分）を照会可能にする。

**FR-API-05（MUST, Pro）**: 対称性の維持プロセス: 新機能のFRは「UIからの操作」と「APIからの操作」の両方を受け入れ基準に含めなければならない。片方のみの新機能を追加する場合は判断記録を必須とする。

**FR-API-06（MUST, Pro）**: API経由の読み取りにもconfidence規則を適用する（レスポンスにconfidence値と `possibly` フラグを含め、0.5未満は既定で除外。明示パラメータ指定時のみ低確度を含めて返す）。

**受け入れ基準（6.11）**: UI検索とAPI検索の結果一致テスト。API経由L3操作がUI承認なしに完了しないE2Eテスト。CLI/REST/MCPの3面で同一操作のスモークテスト。

### 6.12 課金・ライセンス

**目的**: Freeプランなし・トライアル起点のシンプルな課金。オフラインでもユーザーの仕事を止めない。

#### 6.12.1 プランと価格

**FR-BIL-01（MUST）**: プラン構成と価格（USD）:

| プラン | 年払い | 月払い | 内容 |
|---|---|---|---|
| **Trial** | 無料・7日間 | — | Pro相当の全機能 |
| **Standard** | **$49/月**（年一括請求） | **$62/月** | キャプチャ、3層メモリ、state tables、ローカル検索、Notch UI、第1層連携（読み取りコンテキスト統合）、Dream Cycle、Morning Brief |
| **Pro** | **$99/月**（年一括請求） | **$124/月** | Standardの全て＋Memory API（MCP/CLI/REST）＋Composio第2層（送信の解放）＋優先サポート。**2026-09-24 改定**: エージェント実行エンジン（L1/L2/L3すべて: コンテキストアクション実行、チャット、ドラフト生成、Scribe）は Standard に移した |

**FR-BIL-02（MUST）**: キー構成とプランの整合（CLAUDE.md不変条件5と完全整合）:
- **Standard** の観測系 Batch は会社キー専用。live lane も gateway の会社キーを既定とし、BYOK を必須にしない（#1101）。
- **全有償プラン**のエージェント推論・チャット・ドラフトは gateway 既定、BYOK / 委譲は任意（FR-AG-06）。会社の live / batch キーは分離。会社負担分の上限はサーバーで強制し、具体額は #1101 D9 のオーナー回答（2026-10-21 承認: trial $1/日、Standard $3/日/$40/月、Pro $6/日/$80/月。トライアルはアカウント、有償プランはライセンス単位で計量）に従う。
- この対応により「プラン境界＝キー境界」となり、逆転（Standardでユーザー資格情報を要求、ProでSelect KKキーによる推論）を実装しない

**FR-BIL-03（MUST）**: Standardにおける第1層連携は読み取りコンテキスト統合（同期・検索・Fusion・Morning Briefへの反映）に加え、**2026-09-24 改定**でエージェント実行エンジンの書き込み系操作（ドラフト作成・L3送信の承認キュー投入等）も含む。Pro 限定に残るのは Memory API と Composio の実送信解放（draft-stop OFF）のみ。ロック表示はトライアル失効・未課金のときだけ（FR-CF-05の上限規則に従う）。

#### 6.12.2 トライアル

**FR-BIL-04（MUST）【#1101 D1】**: 初回トライアルは7日間、Pro相当の全機能。live AI は gateway 会社キーを既定とし、ユーザー資格情報未設定だけで機能をロックしない。BYOK / サブスク委譲は任意。trial 用のサーバー上限を適用し、D9 のオーナー承認値 $1/日を適用する。
- **【2026-10-13 オーナー決定｜#148 / #1031】** トライアルはアカウントと端末（ハードウェア由来のデバイス id）にそれぞれ1回まで。開始と終了はサーバーが決め（署名済みの `trial_ends_at`）、ローカルの `trial_started_at` スタンプは判定に使わない。新規ユーザーはトライアル開始にアカウント（FR-ACC）が必須。`StandardInTrial` は署名済みトークンの主張だけで決める。設計: `docs/account-passkey-design.md` §7.3

**FR-BIL-05（MUST）**: トライアル満了時（有効な有償プランなし。ライセンスの既存の猶予期間中は有効扱い）: **【2026-10-05 改定｜#940 オーナー判断】**
- **新規キャプチャを停止する。** AX・OCR / Visual recall・会議（検知・録音・ノート保存）・AI セッション取り込みのいずれも新しく記録しない。録音中の会議は停止する。
- **メモリへのアクセスをロックする。** ローカルのキーワード検索と AI 検索、閲覧・読み取り専用ビュー（Today / ダッシュボード、タイムライン、Visual recall の閲覧、会議履歴、メモリに対するチャット、コンテキストアクション、Morning Brief、Dream）、エクスポート（FR-SET-08）。ロックの裏にキーワード検索のフォールバックを置かない。
- エージェント / Memory API / Composio / 同期の停止は従来どおり。
- **到達可能なまま残すもの**: プラン購入とライセンス入力、設定とプライバシー操作、明示的なデータ削除（期間指定削除・全削除）、キャプチャ除外、オンボーディング。
- **ローカルデータは削除しない。** 購読開始・再開まで保持し、決済完了から60s以内に（再起動なしで）アクセスを戻す。
- 強制は Rust コアの単一判定点（`shogun_agents::entitlement` の `local_capture` / `memory_access`）で行い、デスクトップのコマンド・MCP・REST・CLI のすべての入口で、プロバイダ呼び出しや結果の返却より前に安定コード `plan_required` で拒否する。UI はロックされた画面に「Get a plan to continue」と「View plans」（既存の購入フロー）を出し、空の結果・機能の故障・「AI キーを追加」の表示で代用しない。
- 旧版の「閲覧・検索・エクスポートは満了後も30日間可能」は廃止した。

**FR-BIL-06（MUST）**: トライアル開始時のクレジットカード要否。**【2026-09-17 決定・オーナー判断／OPEN-01 解消】既定は「カード必須開始」**とする。オンボーディングのプランステップで価格を提示し、そこから Stripe Checkout へ送り出す。

- **【2026-10-23 オーナー再確認】カード必須・今日の請求額 $0・7日間 Pro。** 無料 CTA は署名済みアカウント付き `POST /api/stripe/checkout` に `trial_only: true`（継続プラン Standard／Pro、月払い／年払い。既定は月払い。claim nonce は送らない）を送り、終了後に表示済みの継続価格で課金されることと、事前に解約できることを示す。サーバーは使用済みアカウント／端末・端末数上限・既存の有効なプランを Stripe 作成前に拒否し、有料購入へ自動フォールバックしない。`STRIPE_TRIAL_DAYS=0` でも専用経路は必ず7日・カード登録あり。選択されたプラン・請求間隔の Price 1件だけを使い、金額・USD・毎月／毎年・licensed/per_unit をカタログと照合する。Stripe が返した `amount_total` が0と確認できた場合だけ URL と `{trial_only:true, trial_days:7, trial_plan:"pro", amount_due_now:0, plan:<選択プラン>, interval:<選択間隔>}` を返す。不明・0以外ならセッションを失効させる処理を試み、URL を返さない。アカウントだけを鍵にした Stripe idempotency key で同じアカウントの Reopen を同一セッションにまとめる。source は app に固定し、メール・source・nonce の入力はセッションに持ち込まない。初回 Checkout 作成後のプラン・請求間隔・端末の変更は同じ鍵の不一致となり trial_selection_locked で拒否する。同じ選択の Reopen だけを許可する。現在のセッションを再取得し、完了済み・期限切れなら URL を返さない。Stripe の24時間の idempotency 保持期間を超える webhook 障害は未解決の運用リスクで、永続的な予約テーブルは追加していない。Standard の継続購入でも Stripe の実際の trial_end までは plan:pro の署名済みトークンを発行し、exp を終了日まで・grace_days を0に制限する。解約状態は実際の値のまま。期限切れ／終了日不明の trialing 状態は更新 webhook を待つ間も新しい Pro トークンを発行しない。
- **【2026-10-23 アプリ内解約】** `POST /api/account/billing/cancel` は Bearer 認証で呼び、対象の契約・顧客をサーバーで選ぶ。期間末の解約を予約し、残りのトライアル／購入済み期間の利用権と過去の領収書を残す。同じ Stripe 顧客に別の未終了契約があれば共有カードは削除しない。なければ既定カード参照を消し、保存済みカード（旧 source も含む）を取り外して確認する。応答は `{ok:true, cancel_at_period_end:true, current_period_end, card_removed, reason?}`。カード削除だけ失敗した場合も解約成功は維持し、`card_removed:false` と固定理由 `other_subscription_active`／`card_removal_failed` を返す。期間末を過ぎて契約が canceled になってもカード削除を再試行できる。終了済み契約自体と過去のカード参照は変更せず、顧客側の既定参照・保存カードと別契約の有無を確認する。終了日の取得ができない終了済み契約は current_period_end:null を返す。IP・アカウント双方のレート制限は fail closed。返金・顧客削除はしない。
- 実装構造は**従来どおり両対応を維持する**（フラグで「カードなし開始」へ戻せること）。決定したのは**既定値**であって構造ではない。CVR が想定を下回った場合にコード変更なしで戻せることがこの MUST の本体
- カード入力の場所は変わらない。**FR-BIL-07 のとおりアプリ内にカード UI を作らず、システムブラウザで Stripe ホストの Checkout を開く。** オンボから送り出すだけで、webview 内にカードフォームを置かない（PCI スコープを広げない）
- トライアル中の全機能提供（FR-BIL-04）と満了時の扱い（FR-BIL-05）は変更しない。変わるのは**トライアル開始の条件**だけ
- 既存のコピーで「カード不要」を明言している箇所（オンボの `obPlanBody`「No card needed.」と ja/de/es の対訳）は、この決定と同時に差し替える。【2026-10-20 オーナー指示で実施】4 言語とも「カード不要」の一文を削除済み（LP と同じく、カード必須の注記も新たに足さない）
- **【2026-10-20 実装｜オーナー指示】フラグはサーバー側の `TRIAL_REQUIRES_CARD`（未設定＝ON）。** ON のとき `/api/license/trial` は新しい無料トライアルを始めず `entitled: false, reason: "checkout_required"` を返し（既に始まっている端末・アカウントのトライアルはそのまま走る）、Stripe Checkout は `trial_period_days: 7` で開く（`STRIPE_TRIAL_DAYS` が設定されていればそちらが勝つ。アカウントが既にトライアルを使っていれば 0 日＝即時課金）。webhook は Checkout で始まったトライアルを `accounts.trial_started_at` に記録し、1 アカウント 1 回を守る。`trialing` の契約はこれまでどおりフルのライセンストークンになり、Standard でも Stripe のトライアル終了（`trial_until`）まで Pro 相当。OFF（`0`）にすると 2026-10-20 以前の「アカウント経由・カード無し」のトライアルに戻る。アプリ側は判定を持たない: Plan ステップはサインイン後に `entitlement_status` が `trial_not_started` なら価格（年払い／月払い）と「Continue to checkout」を出し、`billing_open_checkout` でブラウザの Checkout へ送る。Continue は止めない（Checkout 完了まではロック画面が「Get a plan to continue」を出す）
- **実装上の注意（2026-10-20 訂正）**: オンボーディングの対話ウィンドウ（Plan ステップを含む）は `always_on_top(false)`・レベル 0 で作られており（`onboarding_windows.rs` の `WindowLevelPolicy::Normal`）、ブラウザの Checkout を覆わない。`OVERLAY_LEVEL` にいるのはイントロのオーバーレイだけ。以前ここに書いていた「Checkout の間だけレベルを退避する」作業は不要
- **実装上の注意**: オンボーディング窓は `always_on_top(true)` かつ NSWindow レベル `OVERLAY_LEVEL`(= mainMenu+3 = 27) で全 Space に浮いている（`onboarding.rs`。フルスクリーンアプリの裏に隠れる実機バグの対策）。ブラウザの通常ウィンドウはレベル 0 なので、**そのままでは Checkout のカード入力フォームをオンボ窓が覆い隠す**。Checkout を開いている間だけレベルを退避し、復帰時に戻すこと

#### 6.12.3 Stripeとライセンス検証

**FR-BIL-07（MUST）**: 決済はStripe（Checkout + Billing）。アプリ内にカード情報を扱うUIを作らず、システムブラウザでCheckoutを開く。webhook→ライセンスAPI→アプリの反映で60s以内。

**FR-BIL-08（MUST）**: ライセンス検証: アプリはライセンスAPIに対し24時間ごと＋起動時に検証を行い、署名付きライセンストークン（プラン・有効期限を含む）をKeychainに保存する。検証リクエストにキャプチャ内容・メモリ内容を一切含めない（送るのはライセンスID・アプリバージョン・匿名デバイスIDのみ）。
- **【2026-10-13 追記｜#148】** 検証の認証はライセンスキー、またはアカウントのセッション（短命のアクセストークン）のどちらでもよい。プランの判定は引き続き署名付きライセンストークンで行い、`shogun_mcp::plan_source`・オフライン猶予（FR-BIL-09）・CLI / MCP / REST の判定は変えない

**FR-BIL-09（MUST）**: オフライン猶予: ライセンスAPIに到達できない場合、最後に取得した有効トークンから**14日間**は全機能を維持する（インジケータは7日目からアンバー）。14日超過でトライアル満了と同じ制限状態（FR-BIL-05: 新規キャプチャ停止・メモリのロック。データは削除しない）に移行し、オンライン復帰時の検証成功で即時復元する。決済失敗（Stripe側）の場合はStripeのリトライ設定に従い、最終失敗から7日間の猶予後に制限状態へ移行する。

**受け入れ基準（6.12）**: プラン×機能のアクセス制御マトリクスの網羅テスト。オフライン14日境界のテスト（時計モック）。トライアル満了→課金→復元のE2Eテスト（Stripeテストモード）。

#### 6.12.4 アカウントとパスキー（FR-ACC）【2026-10-13 オーナー決定｜#148】

設計: `docs/account-passkey-design.md`（英語版 `docs/account-passkey-design.en.md`）。実装は設計書 §11 の依存順（#472 → アカウント認証 → メール → ドメインと署名済みビルドでの確認 → Mac の組み込み）。

**FR-ACC-01（MUST, All）**: アカウントは課金・プラン・トライアル・デバイスと内容を含まない使用量・上限・ブロック管理用。メール、パスキー公開鍵、Google identity（provider / subject）、匿名 device id、表示名、セッション/認証コードのハッシュを扱う。メモリ・キャプチャ・state のクラウド保存や同期は含めない。

**FR-ACC-02（MUST, All）**: ログイン手段は**ブラウザでの Google と同期型パスキー**（#1101 D7）。パスワード・Apple サインインは今回の対象外。RP ID は `shogunaios.com`（変更不可）。アカウント作成はオンボーディングの Plan ステップの先頭で行い、ブラウザの Sign in から Google またはパスキーで作成する。以下の仮アカウントと後日のメール確認規則はパスキー登録に適用する。Google の新規作成は `email_verified` を必須とする。Google identity は `sub` で識別し、既存アカウントへの連携にはそのアカウントのパスキー証明を同一フロー内で求める。メール一致だけでは連携しない。メールの確認は後から行えばよく、使い始めの条件にしない（仮アカウント）。**未確認のメールアドレスは予約せず**（持ち主・課金・復旧の権限を確定させない）、最初に確認を完了したアカウントが原子的に得る。確認リンクは要求したアカウントに紐付け、完了にはそのアカウントのパスキーでのセッションが要る（1回限り・期限付き）。アドレスを外しても、外された側のパスキー・購入・デバイス・記憶は移さない。確認済みのアドレスでの登録もアカウントの有無を応答で漏らさない。メールアドレスの一致だけで既存の購入を紐付けない。

**FR-ACC-03（MUST, All）【#1101 D7】**: アプリは単一の Sign in からシステムブラウザの `https://shogunaios.com/app/signin` を開く。Rust が PKCE（S256）・state・nonce を生成し `127.0.0.1:<random port>/callback` で待つ。コードはハッシュのみ保存・60秒・単回使用・challenge と redirect URI に束縛。Rust が state を確認し `/api/account/desktop/exchange` に `{code, code_verifier, device_id}` を送る。サーバーは検証と消費を原子的に行う。ブラウザ既ログイン時もデバイス名を示して明示確認を求める。loopback 以外の redirect は拒否する。Tauri webview で認証しない。正しいプランがあれば Plan を飛ばす。実装の詳細・拒否ケースは account 設計 §8。

**FR-ACC-04（MUST, All）**: アカウントのセッション（リフレッシュトークン）は Keychain の `account-session` にだけ保存する（不変条件7）。リフレッシュトークンは使うたびに原子的に交換する。クライアントは同時に1つしか交換しない。応答を失った再試行（直前のトークン、同じ端末 id、30 秒以内に1回）には同じ新しいトークンを返し、それ以外の再提示は盗難としてその端末のセッションを全部失効させる。

**FR-ACC-05（MUST, All）**: Settings › Account で、サインイン中のアカウント・プラン・ログイン方法（Manage でサイトを開く）・デバイス（名前の変更 / サインアウト）・サインアウト・アカウント削除を扱う。サインアウトはローカルのデータを消さないが、`account-session`・キャッシュしたライセンス / トライアルのトークン・アカウントに紐付いたライセンスキーを消し、バックグラウンドの検証で黙って戻さない。全 API 面がその場でサインアウト（ロック）状態になる（FR-BIL-05）。他の端末から外した場合: 対象がオンラインなら、セッションの系列を失効させ、アクセストークンとライセンストークンの更新を止め、席を原子的に空ける（アカウント経由でもライセンスキー経由でも同じ規則。戻すには正しいサインインが要る）。対象がオフラインなら、発行済みのトークンの 14 日の猶予（FR-BIL-09）が残る間は動く（即時の失効は約束しない）。席数の上限（`LICENSE_SEAT_LIMIT`、既定3）に当たったら、サインイン直後にその場でデバイスを外せるようにする。

**FR-ACC-06（MUST, All）**: 復旧は、確認済みのメールアドレスへのワンタイムコード（6桁、10分、5回まで）のあと、その場で新しいパスキーを作らせる。コードだけではサインイン状態にしない。新しいパスキーの登録・古いパスキーの全失効・古いセッションの全失効を1つの操作として確定し、メールで通知する。コードは1回限りで、送信と試行はアカウントと IP の両方で制限し、再送で上限は戻らない。資格情報の変更には直前のパスキー確認が要り、最後に使えるログイン方法は削除できない。Google-only アカウントでの資格情報変更・復旧の詳細は account 設計の未決事項を解決してから実装する。

**FR-ACC-07（MUST, All）**: アカウント作成・ログイン方法の追加・サインインは本人の対話と確認を要するため UI のみ。MCP / REST / CLI はサインイン状態・プラン・デバイス一覧・使用量・上限を**読む**ことだけできる（不変条件6の扱いは #963 と同じ）。

**FR-ACC-08（SHOULD, All）**: アカウント無しで購入済みの既存ユーザーには、Settings でアカウント作成を勧める（その Mac のライセンスキーで本人確認して紐付ける）。強制はせず、ライセンスキーだけの運用も当面残す。

**実装状況（2026-10-14｜P2）**: Mac 側（ブラウザのサインイン・Keychain のセッション・アカウント経由の検証とトライアル・サインアウト・Settings › Account・オンボーディング・`device.account.get`）を P1 と同じブランチで実装した（未デプロイ）。FR-ACC-06 の直前確認は、パスキーのあるアカウントはブラウザの `/app/reauth`（`POST /api/account/reauth/passkey` が要求したセッションに束縛した challenge を返す）、Google だけのアカウントは Google の再認証で行う。アカウント無しの `/api/license/trial` は新しいトライアルを始めず `account_required` を返す（D7c）。詳細は account 設計の「P2 の状況」。

**受け入れ基準（6.12.4）**: 登録・ログイン・セッション交換・再利用検出・復旧のサーバーテスト。他人のアドレスの先取り（持ち主の登録と確認・確認の競合・古いリンクの再利用・セッション無しのリンク・購入とのメール一致）、同時のリフレッシュ・応答の喪失・交換後の再起動、盗まれた古いセッション・コードの再利用・最後のパスキーの削除、ローカルのサインアウトでの全 API 面のロック、遠隔削除（オンライン / オフライン、古いアクセストークン・リフレッシュトークン・ライセンスキーでの更新の試み、バックグラウンドの再試行、正しいサインインでの復帰、空いた席の再利用）、トライアル付与と席の確保の同時実行のテスト。#1031 の3つの抜け道（スタンプ書き換え・ファイル削除・Standard + 書き換え）でトライアルが延びないテスト。席数の上限とデバイスのサインアウトのテスト。Developer ID ビルドでのパスキーシートの実機確認。

### 6.13 オンボーディング

**目的**: 初回起動から「最初の価値」までを最短にする。権限拒否でも壊れない。

**FR-OB-01（MUST, All）**: 初回起動フロー（この順序。各ステップはスキップ可能で、スキップ時の状態を明示する）:

| # | ステップ | 内容 |
|---|---|---|
| 1 | Welcome | 一言定義の提示。返ってきたユーザー向けに「Already have an account? Sign in」（ブラウザの Google / パスキー、FR-ACC-03）。**【2026-10-13 改定｜#148】アカウント作成はPlanステップの先頭で行う**（ブラウザの Google / パスキー、FR-ACC-02。クレカ要否はFR-BIL-06に従う） |
| 2 | Accessibility権限 | 目的説明（テキストのみ・既定でスクショなし・ローカル保存・Visual recall は任意でOffが既定）→システム設定へ誘導→付与検出で次へ |
| 3 | キャプチャ開始 | 付与確認後、キャプチャを開始しNotchデモ（Hover→Expandedのガイド）を実施 |
| 4 | 連携接続 | Wave 1（Gmail / Google Calendar）の接続を提案。スキップ可 |
| 5 | AIの接続（任意） | 既定は hosted AI（#1101、全有償プラン/トライアル）。BYOK / サブスク委譲は設定でレーンごとに明示選択する。委譲には opt-in 同意必須。ユーザーキー未入力だけでエージェント機能をロックしない |
| 6 | 会議ノート（オプトイン） | 会議を検知して聞き、**録音は保存せず文字起こしテキストのみ残す**こと、bot は会議に参加しないこと、いつでも1タップで止められることを説明し、**ON/OFFを選ばせる**。既定はOFF（FR-MT-01）。スキップ時はOFFのまま進む |
| 7 | 完了 | Morning Briefの予告（「明日の朝、ここに現れる」）と一時停止ホットキーの案内 |

**FR-OB-02（MUST, All）**: ステップ2の説明画面には次を明記する: キャプチャはAccessibility API経由のテキストのみ / 既定ではスクリーンショット・画像を保存しない（Visual recall を自分で On にした場合のみ、作業中のディスプレイのフレーム（除外対象は写さない）を選択した保持期間（既定3日、1〜7日＋上限付きカスタム）ローカル保持し、期限で自動削除することを併記する） / 生データはデバイス外に出ない / 除外リストと一時停止がいつでも使える。

**FR-OB-06（MUST, All）**: ステップ6（会議ノート）でONを選んだ場合のみ、マイク／音声取得のTCC権限を要求する。**OFFのまま権限を要求しない。** 説明にはFR-MT-03の開示テキストを用い、「ShogunAIはZoom / Google Meetにbot参加しません」「承認した会議だけを聞き、音声は保存しません」を明記する。

**FR-OB-03（MUST, All）**: 権限付与の検出は2s間隔でポーリングし、付与後に自動でステップ3へ進む（再起動を要求する場合は再起動後にフローの続きから再開する）。

**FR-OB-04（MUST, All）**: 権限拒否時の体験: ステップ2で「Skip for now」を選べる。スキップ時はFR-CAP-08の縮退状態（連携統合とMorning Briefは可能）でオンボーディングを続行し、完了画面とNotch Hoverに「Enable capture」導線を常設する。拒否を理由にアプリを終了・機能全ロックしない。
- **【2026-10-03 オーナー決定｜Issue #778 オプションC】** 権限画面の副次アクション「Continue without screen reading」でスキップする（Allowが主導線のまま）。スキップは `onboarding.json` の `accessibility_skipped` に永続化し、Rustコアはこの明示的選択がある場合に限り Plan/Ready 以降と完了を Accessibility なしで受け付ける（選択も付与もない書き込みは従来どおり拒否）。付与後にトラストを失った場合（再署名アップデート等）の修復画面からも同じ選択で縮退状態へ進める。完了後は FR-CAP-08 の縮退状態に入り、「Enable capture」導線は展開パネルの常設バナー（システム設定の該当ペインを開くボタン付き）で提供する。後から付与されたら再起動なしで縮退状態を終え、スキップフラグを消す（以後のトラスト喪失では修復画面が再び開く）。完了が拒否された場合はオンボーディング画面にエラーを表示し、無反応にしない

**FR-OB-05（SHOULD, All）**: オンボーディング完了直後の初回価値: 接続済み連携がある場合、初回同期完了時点（目標: 接続から5分以内）で「最初のstate候補」（検出したpeople/open_loops）をNotchから確認できるようにする。

**受け入れ基準（6.13）**: 全ステップスキップで完走できるテスト。権限拒否→後日付与→自動キャプチャ開始の統合テスト。

### 6.14 トレーサビリティ

**目的**: 「デバイスの外に何が出たか」を100%記録し、ユーザーが検証できるようにする（CLAUDE.md不変条件3）。

**FR-TR-01（MUST, All）**: **全外部送信箇所**にトレーサビリティログを実装する。対象はAR-11の全経路（Anthropic Batch/Messages、第1層MCP書き込み、Composio。第1層の読み取り要求も要求パラメータを記録対象とする）。記録項目:

| 項目 | 内容 |
|---|---|
| `ts` | 送信時刻 |
| `destination` | 送信先（API種別＋エンドポイント種別） |
| `route` | `direct` / `via_composio`（第三者経由） |
| `purpose` | `dream_cycle` / `morning_brief` / `indexing` / `agent` / `chat` / `integration_write` / `integration_read` 等の列挙 |
| `key_kind` | `select_kk` / `byok` / `oauth_user` / `composio` |
| `approval` | `l1` / `l2` / `l3` / `background`（同期等の非アクション送信） |
| `payload_digest` | 送信内容の要約（先頭抜粋＋サイズ＋チャンク数）。**全文はローカルに7日間保持**し、以後digestのみ保持 |
| `status` | 成功 / 失敗 |

**FR-TR-02（MUST, All）**: トレーサビリティ閲覧画面（Full UI）: 時系列一覧、送信先/purpose/route/キー種別でのフィルタ、各エントリの詳細（7日以内なら送信全文）。Composio経由エントリには「第三者経由」バッジを表示する（FR-C2-04）。

**FR-TR-03（MUST, All）**: 外部送信を行う関数はトレーサビリティ記録を強制する共通クライアント層（shogun-core内の単一HTTP出口）を必ず経由する。個別コンポーネントが素のHTTPクライアントで外部送信するコードをレビューで拒否できるよう、CIで依存検査（許可リスト外のHTTPクライアント利用検出）を行う。

**FR-TR-04（MUST, All）**: トレーサビリティログ自体はデバイス外に送信しない。ログのローカル保持期間: digest行は無期限（Cold層同様のパーティション）、全文は7日。

**受け入れ基準（6.14）**: 全送信経路（モック）でログ行が生成されるテスト。共通出口を経由しないHTTP呼び出しがCIで検出されるテスト。

### 6.15 設定画面

**FR-SET-01（MUST, All）**: 設定画面（Full UI内）は以下のセクション・項目を持つ:

| セクション | 項目 |
|---|---|
| **General** | ログイン時起動（既定ON）/ 表示ディスプレイ（自動追従・内蔵固定）/ グローバルホットキー2種（展開・一時停止）/ アップデートチャネルと「Check for updates」 |
| **Capture** | キャプチャON/OFF / 除外アプリリスト（既定項目表示・追加/削除、FR-CAP-06の削除不可項目はグレーアウト）/ 除外ドメイン・タイトルパターン / 一時停止の時限オプション既定値 |
| **Meeting notes** | 機能ON/OFF（**既定OFF**、FR-MT-01）/ 会議アプリ別の除外リスト（FR-MT-02(b)）/ 常に除外する予定（繰り返し1on1等）/ 開示テキストの常設表示（FR-MT-03）/ 直近セッション一覧とRecapへの導線 |
| **Memory** | Warm層期間（既定30日、14〜90日で変更可。短縮時はDream Cycleで降格）/ ストレージ使用量表示（層別）/ データ削除（期間指定・アプリ指定・全削除。全削除は確認2段階） |
| **Language** | 生成物の出力言語（既定: システム言語。Morning Brief・ドラフト・チャット応答に適用）。UI言語はv1では英語固定（表示のみ） |
| **Integrations** | 第1層6サービスの接続/切断・同期状態・最終同期時刻 / Composio（オプトイン、同意画面へ）/ ドラフト止まりモード（既定ON）/ **Gmail › Triage**（ラベル一覧と有効化、ラベル単位の Auto Archive・Sync label to Gmail の常設承認トグル（Pro）、カスタムラベル、送信者ルール、Re-run。#319 FR-TG-27【2026-09-04】） |
| **Agents**（Pro） | プリセットの個別有効/無効 / L2→L3への引き上げ設定（FR-SET-05）/ L1自動実行の通知粒度 |
| **API**（Pro） | BYOKキーの設定・検証・削除（Keychain保存。表示は末尾4桁のみ）/ Memory APIのON/OFF / クライアントトークンの発行・失効一覧 |
| **Dream Cycle** | 実行ウィンドウ（既定02:00〜06:00）/ 直近の実行結果サマリ / 手動実行（縮退版） |
| **Account** | プラン表示・変更（Stripeポータルへ）/ ライセンス状態 / トライアル残日数 |
| **Privacy** | トレーサビリティ画面への導線 / テレメトリのON/OFF（既定OFF、§7.7）/ データエクスポート |
| **Advanced** | 診断ログ（キャプチャ内容を含まない）/ SLOメトリクス表示 / DBのintegrity check実行 |

**FR-SET-04（MUST, Std）**: 出力言語設定はMorning Brief（FR-MB-02）・ドラフト生成・チャット応答の生成プロンプトに適用される。UI文言はi18n-ready構造（文言カタログをコードから分離。v1カタログは英語のみ）とする。

**FR-SET-05（MUST, Pro）**: ユーザーは任意の操作種別をL2→L3へ引き上げられる（より慎重側のみ）。L3→L2、L2→L1への引き下げUIは提供しない。

**FR-SET-07（MUST, All）**: データ削除: 期間・アプリ単位の削除はevent log・派生embedding・FTS索引・関連state provenanceを整合的に削除する（provenanceが空になったstateレコードは「根拠喪失」フラグを立て、次回Dream Cycleで再評価または削除）。

**FR-SET-08（MUST, All）**: データエクスポート: event log・state tables・トレーサビリティdigestをJSON Lines形式でローカルにエクスポートできる。エクスポートはL1（ローカル完結）。トライアル満了後（有償プランなし）はロックする（FR-BIL-05、2026-10-05 改定）。

**受け入れ基準（6.15）**: 全設定項目の永続化・再起動後復元テスト。削除操作のリレーショナル整合性テスト。BYOKキーが設定UI経由でもログ・DBに残らないことの検査。

### 6.16 会議ノート（Meeting Notes）

**目的**: 会議を検知して開始し、終わるまで聞き続け、終わったら**そのまま仕事になる**ノートを返す。記録を残すことが目的ではなく、会議で生まれた**決定と約束をstate tablesに接地させる**ことが目的。設計の正本は `docs/meeting-notes-ui-design.md`、区間の器の設計は `docs/meeting-context-and-dashboard-design.md`。

> **本節を貫く原則**: 「録音を残す」のではなく「**聞いて、テキストだけ残す**」。音声はディスクに書かない（不変条件2、NFR-PRV-01）。

**プラン**: FR-MT-01〜21は**全プラン（トライアル含む）**。会議ノートは「使ってみて価値が分かる」タイプの機能で、トライアル中に体験できなければ課金判断の材料にならない。Memory API経由の参照（FR-MT-22）のみPro（API面自体がPro機能のため）。トライアル満了後（有償プランなし）は新規の会議記録と会議履歴の閲覧・エクスポートをロックする（FR-BIL-05、2026-10-05 改定）。保存済みの会議は削除しない。

#### 6.16.1 有効化・同意・オフの設計

**【2026-10-20 オーナー決定で改定】FR-MT-01 の既定は ON（マイクだけの検知 `allow_mic_only_detect` も ON）。保存済みの OFF はアップデート後も OFF のまま、読めない設定ファイルは OFF として扱う。以下の旧文は履歴。**
**FR-MT-01（MUST, All）**: 会議ノート機能の既定は**OFF**とする。オンボーディング（FR-OB群）で一度だけ説明し、ユーザーが明示的にONにした場合のみ有効化する。「黙って有効になっていた」を構造的に作らない。**既定ONへの変更・アップデートによる自動有効化を行わない。**

**FR-MT-02（MUST, All）**: オフは常に以下**3つの粒度**で到達可能であること。

| 段 | 到達経路 | 効果 |
|---|---|---|
| (a) 全体オフ | 設定 → Meeting notes → Off | 検知もASRも走らない。**マイク・音声デバイスに一切アクセスしない** |
| (b) このアプリでは録らない | Offeredの副操作 / 設定の除外リスト | 指定した会議アプリでは以後Offeredを出さない |
| (c) この会議は録らない | Offeredの「Not now」/ Recordingの「Stop」 | 今回のセッションのみ。設定を変更しない |

全体オフ中はNotchにピル（FR-MT-09）を一切表示しない（「オフなのに何か出ている」を作らない）。除外は会議アプリ単位に加え、**特定の予定を常に除外**（繰り返し1on1等）を設定できること。

**FR-MT-03（MUST, All）**: 開示テキストをRecap画面と設定画面に常設する: 「この会議の音声はこのMacの中だけで処理され、録音は保存されません。残るのは文字起こしテキストです」。**事実と実装が一致していること**が受け入れ条件（実装がこの文言を裏切っていないことをテストで担保）。法域別の分岐は持たない（既定OFF＋明示オプトイン＋常時可視のピルで一律運用）。参加者への告知を**プロダクトが代行しない**（会議へ自動でメッセージ・チャット投稿をしない。CLAUDE.md不変条件4／Issue #7 Non Goal）。

#### 6.16.2 検知とセッション（区間）

**FR-MT-04（MUST, All）**: 会議検知は**単一信号に頼らず**、以下の信号の組み合わせで行い、結果をconfidence付きで扱う（断定しない）。

| 信号 | 取得元 | 単独の強さ |
|---|---|---|
| ① 予定がある | `calendar_occurrences`（FR-MT-06） | 強（ただし予定=出席ではない） |
| ② 会議アプリが前面 / マイクが使用中 | NSWorkspace + bundle id表 + マイク使用中フラグ | 中 |
| ③ 会議UIの痕跡 | AXに参加者リスト・Leave/Mute等のコントロールが見える | 中 |

②③が立った時点でセッション候補とし、①と一致すればconfidenceを上げて予定に紐付ける。**①のみ（会議アプリが前面に来ない）では区間を開かない**（「予定はあったが出席の証拠なし」）。
検知対象アプリはbundle id表で管理し、v1はGoogle Meet（ブラウザのURL）とZoomを必須対象とする。
**マイク使用中の検知は「使用中か」の真偽のみを読み、音声ストリームには触れない。** この境界は実装コード上にコメントで明示すること。

**FR-MT-05（MUST, All）**: 点イベントの`event_log`とは別に、**区間**を表す`sessions`を第一級の概念として持つ。

```
sessions   区間: [started_at, ended_at)  kind = meeting | call | focus
  ├─ calendar_occurrence_id?   予定に紐づくか（飛び込み会議はNULL）
  ├─ participants              people.id[]（identity.rsで名寄せ済み）
  ├─ thread_key?               関連する会話スレッド
  ├─ summary / decisions       Recapが書く
  └─ confidence + provenance   検知も推定である以上、断定しない（state tablesと同じ規律）

event_log.session_id  ← additive。区間中のキャプチャ・チャット・メールが自動的にぶら下がる
```

スキーマ変更は**additive**であること（後方互換を破らない。CLAUDE.md）。`kind=focus`により会議以外の区間にも同じ器を使えること。

**FR-MT-06（MUST, All）**: 未来の予定は`event_log`（append-only の過去ログ）ではなく、**state**として`calendar_occurrences`テーブルに保持する。RFC3339の開始/終了時刻を**epoch msとして解釈**し（現行の正規化はts=0に潰すため修正必須）、`title` / `location`（会議URLを含む。第三者に出さない）/ `attendees`（JSON）を構造化フィールドとして持つ。`attendees`はidentity.rs（名寄せ）の供給元となる。**【2026-09-23 実装】** `calendar_occurrences`（V43）として実装した。RFC3339 は epoch ms として読む（`shogun-integrations/result.rs` の `rfc3339_to_ms`。予定の `ts` は `updated` から取り、0 に潰さない）。キーは `(source, external_id)` で、同期ごとに上書き更新し、キャンセルは `status = 'cancelled'` にする。`location` と `attendees` はデバイス外に出さない。検知信号①（FR-MT-04 の `occurrence_now`）への配線は未実装で、別途行う。

#### 6.16.3 ライフサイクルとUI

**FR-MT-07（MUST, All）**: 会議ノートは以下の状態機械に従う。図中の遷移以外を実装しない。

```
  Idle
   │  会議検知（FR-MT-04）
   ▼
  Offered ──"Not now"───────────────────────────────────┐
   │  10秒のグレース（何もしなければ開始）                 │
   ▼                                                    │
  Recording ──"Stop"──→ Wrapping ──→ Recap ──確定──→ state tables
   │                                                    │
   └─ 会議アプリ終了 / 予定終了+10分 / 無音15分 → Wrapping │
                                                        ▼
                                                      Idle
```

**FR-MT-08（MUST, All）**: Offered状態では**10秒のカウントダウン**を提示し、無操作なら録音を開始する。「Not now」は**この会議のみ**に作用し設定を変更しない（FR-MT-02(c)）。長押し/副操作で「このアプリでは今後録らない」（同(b)）に到達できること。**自動で始まるが、始まる前に必ず一度姿を見せる**ことがこの要件の趣旨であり、Offeredを省略して直接Recordingへ遷移する実装を認めない。

**FR-MT-09（MUST, All）**: Recording中はNotch折りたたみ時にも**ピルを常時可視**とし、`● Notes · 12:04  Weekly sync  ■ Stop` の形で **(a) ライブドット (b) 経過時間 (c) セッション名 (d) 1タップのStop** を示す。
- **経過時間を必ず表示する**（「まだ動いているのか」が状態のトグルだけでは分からない）
- Stopは**確認ダイアログなしで即時停止**（止めることに摩擦を作らない）
- 録画を想起させる赤の録画ランプを使わず、ライブドットで表す（録画ではないため）
- **FR-NU-08（フルスクリーン時Hidden）の唯一の例外**がこのピルであり、フルスクリーン中も表示を維持する（常時可視の要請が優先）

**FR-MT-10（MUST, All）**: 会議中の展開パネルは**ユーザー自身がノートを打つ場所**とし、~~**ライブ文字起こしを表示しない**。代わりに `Listening · N participants` の静かな1行のみを出す~~。理由: (1)流れる字を目で追うと会議に集中できない (2)ASRの誤りがその場で見えると訂正したくなる＝仕事を増やす (3)誤りは文脈込みのRecapで直せる。
**【2026-08-24 改訂｜issue #93 で実装済みの現実に追随】** ライブ文字起こし（CC）パネルは**任意表示**として提供する（既定は静かな1行、CCピルでON/OFF、翻訳の one-way / two-way 字幕を含む）。「表示しない」から「押した人にだけ見せる」への変更であり、ノート欄が主役である点・打たなくてもRecapが組める点は不変。

**【2026-10-13 #1101 D1 により #782 改定】** ライブ翻訳・ライブ要約（AI Canvas）・Quick Translate は gateway の会社キーを既定とする。プライバシー設定・`agent_execution` ゲートは維持する。任意の BYOK / 委譲の既存経路は gateway を迂回し、未対応の上書きを黙って会社キーへ切り替えない。字幕単位の委譲性能は未検証であり、既存の制約解除は測定後とする。Recap は Batch のまま。
ユーザーのノートは`session_notes`に保存する。**ノートを打つことを前提にしない**（打たなければ文字起こしだけでRecapを組み立てる）。

**FR-MT-11（MUST, All）**: 会議は、ユーザーが終えるか、実際のシグナルで終わる。**予定の終了時刻は終了条件にしない**（【2026-10-17 オーナー決定｜audit meet-051..053】）。「止め忘れて延々と録り続けていた」を作らない。
- 次の2つは、まず「Still in the meeting?」（Keep recording / End meeting、キーボードのみで操作可、Enter/Esc = Keep recording）を出す。60秒答えがなければ終了する。Keep recording はタイマーを戻し、通話終了の再通知は新しいシグナルまで抑える
  - 通話の終了（会議アプリの終了、対象タブ/ウィンドウの消失、通話アプリがマイクを手放した）
  - 文字起こし音声もノート編集も10分ない（手打ちのノートは活動として数える。音声が一度も始まらない手動/対面の会議も同じ）
- Mac のスリープは、スリープ時刻で終了する（確認は出せない）。復帰後に新しい会議を黙って始めない
- 手動の Stop はいつでも使える

#### 6.16.4 音声パイプライン（RAMの内側と外側）

**FR-MT-12（MUST, All）**: 音声の経路は以下に限る。**リングバッファより先に音声を出さない。**

```
 マイク ────┐
            ├─→ [RAMリングバッファ 30s] ─→ オンデバイスASR ─→ transcript_segments（テキスト）
 システム音声┘        ↑ ここから先に音声は行かない          └→ sessionに紐づく
 (Core Audio tap)     └─ 破棄（ディスクにもクラウドにも書かない）
```

- リングバッファは**30秒固定上限**。ASRが追いつかない場合は**古い音声から捨てる**（貯めれば実質的な録音になるため、上限は設計上の防壁として固定値で持ちユーザー設定に露出しない）
- 音声の取得は**Recording状態の間のみ**。Idle/Offered/Wrapping中は音声デバイスにアクセスしない
- **ファイル入力を要求するASR実装を選ばない**（「一時ファイルなら良い」を許さない＝ストリーミングASRを選ぶ技術的根拠）
- Issue #7で言及された「押す前のN分を遡って含めるリングバッファ」は、上記30秒上限を超える保持となるため**採用しない**（FR-MT-08のOfferedグレースで代替する）

**FR-MT-13（MUST, All）【2026-10-13 #1101 D2】**: Deepgram Nova-3 Multilingual の cloud live STT を既定とする。会社キーをサーバーだけに保持し `/v1/asr/grant` で TTL 60秒の JWT を発行する。Mac から Deepgram に音声を直接送信（`mip_opt_out=true`）、gateway に音声を通さず、波形をディスク・一時ファイルへ書かない。月次音声上限・同時2セッション未満・デバイスごと20秒に1回の mint 上限で admission。BYOK は任意。Settings に送信先・非保存を開示する。接続済み stream の TTL 超過と最大継続時間は実測し、クライアント申告を provider usage と日次照合する。Whisper は offline/dev fallback のみ。macOS 14.4 未満の Core Audio tap 縮退判断は従来どおり §9。

**FR-MT-14（MUST, All）**: 文字起こしは`transcript_segments`（`session_id` / `ts` / `speaker` / `text` / `origin` / `confidence`）としてWarm層に保存し、既存の検索・抽出・Fusionがそのまま効くこと。`origin`は `asr`（音声由来）/ `caption`（画面のライブキャプション由来）を区別して持つ。**キャプション由来か音声由来かで説明責任が異なる**ため、provenanceに必ず残す。

**FR-MT-15（MUST, All）**: 話者分離の基底は「**自分 / それ以外**」（マイク入力かシステム出力かで判別）、不明な場合は`speaker = NULL`とする。**推測で埋めない。**
**【2026-08-24 拡張｜次段を実装】** (a) システム出力レーンは Deepgram diarization で**話者スロット**（`other:N`）に分割して保存する。スロットは「区別」であって「同定」ではない。 (b) スロット→実名の割り当ては `session_speakers` テーブルに、**ユーザーの明示確定のみ**で書く（Recapの話者チップ／`meeting.speakers.set`。候補は提示するが自動適用しない）。候補ソースは精度順に ①**会議アプリ画面のAXテキストから採掘した参加者名**（セッションに紐づくevent_logの会議アプリ行のみ・2キャプチャ以上に出現する名前形スパンのみ。カレンダー不要） ②`people`（最近対話した人）。未確定スロットのUI表示は中立な "Speaker N"。推測で実名を埋めない原則は不変。③`calendar_occurrences.attendees` 突合は calendar レーン実装後に追加する（§9、Issue #263）。**【2026-09-23 実装｜FR-MT-06 / Issue #263】** calendar レーンを実装した: 同期した予定を `calendar_occurrences`（V43）に state として保持し、会議セッション開始時に重なる予定（開始10分前から終了まで・終日予定とキャンセル済みは除く）を `sessions.calendar_occurrence_id` に紐付ける。候補の精度順は **カレンダー出席者（本人を除き、`people` のメール一致で表示名を正規化） ＞ AX由来参加者 ＞ `people`** になった。出席者の名前で確定したときは `session_speakers.source = 'calendar'` を記録する。提示のみで自動割り当てしない原則は不変。

#### 6.16.5 Recap（会議後）

**FR-MT-16（MUST, All）**: セッション終了後、Recapとして **(1)要約3〜5行 (2)決定事項 (3)「誰が何を負ったか」の候補**（commitments / open_loops）を提示する。**候補は候補として提示する** — 低確度は`possibly:`帯のまま出し（FR-ST群のconfidenceゲートに従う）、断定しない。
**【2026-08-24 追加｜会議テンプレート】** Recapは**テンプレート**（`general` / `one_on_one` / `sales_call` / `standup` / `interview` / `customer_research`）で追加セクションを整形できる。テンプレートはセクション見出しの形を変えるだけで、要約・決定事項・next actionsの不変トリオと候補規律は常に維持する。既定は`general`（＝従来と同一出力）。選択は`sessions.template`にセッション単位で保存し、Recap上の変更は再生成を伴う（L1）。レジストリはコード（`shogun-core meeting::templates`）が正で、UI/API両面が同一物を読む（不変条件6）。

**FR-MT-17（MUST, All）**: Recapの候補は`[Track]`の**1タップで確定**し、その瞬間のみ **confidence 1.0 + provenance=ユーザー明示編集** としてstate tablesに入る（§6.4「ユーザー明示編集」経路）。これが唯一の正直な昇格経路であり、**ユーザー確定なしにHigh帯へ昇格させる実装を認めない**。
**【2026-09-23 実装｜Issue #253 第2段】** Recap の next action ごとに `[Track]` を置いた。押下は `event_log`（`source='meeting'`・`kind='track'`、セッションに紐付け）に記録し、その行を根拠に commitment を confidence 1.0 で書く（文字起こし行も weight 0.5 で併記し、Why? から発言へ降りられる）。source を meeting のままにするので Batch lane には出ない（A-2 決定）。追跡できるのは**そのセッションの Recap 候補のみ**（任意文の 1.0 書き込みは拒否）、同じ候補の再押下は冪等。同じ会議からローカル抽出が既に推定していた同一約束（confidence<1.0）があれば、重複を作らずそれを 1.0 に確定する。owner は `people` の表示名・別名と突合し、なし／一人称なら mine、名前ありなら theirs（一致した人を counterparty、一致しなければ counterparty なし＝推測で埋めない）。呼び出し側は direction / counterparty を明示上書きできる（これもユーザー明示編集）。既存の同一約束と owner が食い違う場合は上書きせず `state_disputes`（`field='owner'`）に残す（期日の食い違い＝`due_at` と同じ扱い）。API: `meeting.recap.track`（L1。REST `POST /v1/meeting/recap/track`、CLI `shogun meeting track`）。

**FR-MT-18（MUST, All）**: Recapから`Why?`で**根拠セグメントに降りられる**こと（provenanceの可視化。FR-TR群と同型）。

**FR-MT-19（MUST, All）**: Recapの初回表示は**会議終了から60秒以内**。間に合わない場合は「文字起こし・ノート・予定のみで構成した**縮退Recap**」を先に提示し、**空の画面を出さない**。

**FR-MT-20（MUST, All）**: Recapの要約生成は**Select KKキー（Batch API）**で行う（不変条件5の鍵レーン規律。live lane と会社キー・プロジェクトを分離し、BYOK/委譲を Batch に使わない）。**Recap レーンでは音声を送らず**（ASR は FR-MT-13）、送信するのは文字起こしから作った処理用チャンクのみで、全送信にトレーサビリティログを伴う（不変条件3、FR-TR-01）。議事録の外部共有・送信は**v1スコープ外**（L3扱い、§3.2）。**【2026-08-24 明確化】** 共有**可能な形にする**こと（RecapのMarkdownエクスポート: クリップボードコピー／`meeting.recap.export`）はローカル生成のみでL1。**送信**（メール・Slack投稿等）は引き続きL3かつv1スコープ外。

#### 6.16.6 API対称性・リソース

**FR-MT-21（MUST, All）**: 会議中のリソース上限は**アイドル時SLO（CPU 5%、NFR-SLO-06）とは別枠**とし、ASR稼働中のCPUは**15%を上限**として別途計測する（混ぜるとアイドル5%が意味を失う）。リングバッファ＋ASRモデルのメモリは3層メモリのHot上限（200MB）とは別勘定で計上する。会議中のバッテリー消費を計測しContext Healthに出すこと。

**FR-MT-22（MUST, Pro）**: 不変条件6（人間UIとAI APIの完全対称）に従い、セッション一覧・Recap・文字起こしの参照はMemory API（MCP / CLI / REST）からも同一のL1/L2/L3規律で到達できること。**webview側に会議データの集計ロジックを置かない**（不変条件1）。**【2026-09-16 明確化】** 参照面は `meeting.list`（一覧）、`meeting.recap.get`（構造化 Recap: summary / decisions / next_actions / sections / speakers / note）、`meeting.transcript.get`（ts・話者ラベル付きの文字起こし行）、`meeting.recap.export`（Markdown）の4読み取りで構成する。いずれも読み取りのみ。next_actions は提案であり API 経由でも実行されない（不変条件4）。

**受け入れ基準（6.16）**:
1. **音声がディスクに書かれないことの検査**: Recording中の全ファイル書き込みを監視するテストで、音声データを含むファイルが生成されないことを検証する（FR-MT-12、NFR-PRV-01）。
2. 状態機械（FR-MT-07）の全遷移がテストで検証されている。Offeredを経ずにRecordingへ入る経路が存在しないことを含む。
3. 三段オフ（FR-MT-02）それぞれの効果と、全体オフ時に音声デバイスへアクセスしないことが検証されている。
4. 自動Wrapping条件3種（FR-MT-11）がテストされている。
5. `[Track]`未押下の候補がstate tablesに入らないこと（FR-MT-17）が検証されている。
6. 会議中CPU（FR-MT-21）の計測コードが同梱され、アイドルSLOと別系統で記録される。
7. 既定OFF（FR-MT-01）であること、およびアップデートで既定が変わらないことのテスト。

---

### 6.17 Evening Wrap

**目的**: Morning Briefが1日を開くのに対し、Evening Wrapは1日を閉じる。構造上の違いは1点で、**Wrapは何も生成しない**。夜のWrapで見た状態が、翌朝のBriefでDream Cycleに整理されて返る — この往復で1日のループを作る。

**FR-EB-01（MUST, Std）**: Evening Wrapは**ローカル時刻の設定時（既定17:30）以降**、ユーザーが実際にMacを操作している最初のタイミングで生成・提示する。時刻は設定で変更でき、朝夕それぞれ個別に無効化できる。既読は日付一致で管理し、時計が巻き戻っても二重配達しない。セクションは4つ:

| セクション | 内容 | 上限 |
|---|---|---|
| Today's outcome | 今日の可算成果（完了したcommitments / 閉じたopen_loops / 決定したアクション提案数とその採択数） | — |
| Still open | 今日動きがあった未了項目。優先順位順 | 5件 |
| Tomorrow first | 明日のカレンダー先頭（時刻順）＋明日期限のcommitments | カレンダー3件 |
| Loose ends | 今日開いてまだ開いたままのopen_loops | 5件 |

「今日は何か動いたか」に数で答える設計であり、Today's outcomeは内容を再掲しない。

**FR-EB-02（MUST, Std）**: **【2026-10-17 オーナー決定で改定】** 各セクションはローカルデータの決定的集計で構成し、オフラインでも動作すること。その上に、Morning Brief と同じ Batch lane（会社キー・`evening_wrap` purpose）で今日の窓から最大3行の文を足す。各行は引用元イベントの文字列と完全一致する根拠を持ち、根拠が消えたら行も消える。リクエストは Wrap 時刻の60分前に1回だけ送り、Batch が無い・未完了のときはローカル集計だけを出す（旧: LLM生成・egressを伴わない）。優先順位付けは既存の指標のみを使う（overdue優先、次にstaleness／期限）。confidenceゲートはMorning Briefと同一（FR-ST-20: Lowは除外、Mediumは `possibly` として弱く提示）。**Briefと同じ項目型を再利用**し、「提示してよい事実」の定義が2つのサーフェス間でずれないようにする。

**FR-EB-03（MUST, Std）**: 提示は割り込まない。インジケータの色変化のみで知らせ、自動でポップアップしない。既読管理はMorning Briefと同一。配信判定はユーザーが実際に操作している瞬間を捉えて行う（深夜や17:30ちょうどに無人の画面へ鳴らさない）。

**FR-EB-04（MUST, Std）**: 日境界の計算は**Morning Brief／既存の日次処理と同一の実装を共有する**。固定UTCオフセットから組み立てた第2の日境界規則を持たない（年2回、夏時間の切替で最初の規則と食い違うため）。

**FR-EB-05（MUST, Std）**: Evening WrapはMemory APIからも同一の集計として取得できる（不変条件6）。webview側は集計を一切計算しない（不変条件1）。

**受け入れ基準（6.17）**: 各セクションの上限が守られること。LLM呼び出しとegressが発生しないことをテストで担保。confidenceゲートがBriefと同一結果を返すこと。夏時間の切替日を含む日境界テスト。UIとMemory APIで同一内容が返ること。

---

### 6.18 参照解決

**目的**: 「あの件どうなった」「さっきのやつ」に、聞き返さずに答えられる場合は答え、**確信がない場合は推測せず聞き返す**。間違ったスレッドについて自信を持って答えることは、1回聞き返すコストとは比較にならない信頼の毀損になる。

**FR-REF-01（MUST, Std）**: 指示的な問いに対し、threads（FR-MEM-41）のsalienceランキングで参照先候補を順位付けし、判定を3値で返す:

| 判定 | 条件 | 挙動 |
|---|---|---|
| Resolved | 1位が2位に対して十分な差をつけている | そのスレッドから答える |
| Ambiguous | 複数が拮抗している | **推測せず**、上位候補を提示して聞き返す |
| None | 指し示すに値するものがない | 参照解決を行わない |

判定は**1位の絶対スコアではなく上位2件の差（マージン）**で行う。加えて、対抗馬がなくても弱すぎる候補は採用しない下限を設ける。

**FR-REF-02（MUST, Std）**: 現在画面の文脈を任意入力として受け取り、ランキングに反映できる。画面で見ているスレッドについて「これどうなってる」と聞いたときに、それが選ばれること。

**FR-REF-03（MUST, Std）**: 判定ロジックは純関数としてテスト可能に保つ（スコア列→判定）。ランキングの語句一致項は、短すぎて信号にならない語を除外する。

**FR-REF-04（MUST, Std）**: 参照解決を経た回答には必ず根拠（provenance）を出典として表示する。低confidenceの状態はFR-ST-20の規則で「〜の可能性」として弱く渡す。参照解決そのものはローカルのランキングでありLLMを伴わないが、**それを使って文章を生成するサーフェスは各々のプラン境界に従う**（ドラフト生成はPro）。

**受け入れ基準（6.18）**: 拮抗する2スレッドでAmbiguousになること。空のメモリでNoneを返すこと。弱い単独候補が採用されないこと。現在画面の指定で選択が変わること。判定のp95を計測する。

---

### 6.19 AIセッション取り込み

**目的**: AIツールとのやりとりをワールドモデルに統合し、モデル間のコンテキスト断絶を埋める（取り込み側。配布側は§6.11 Memory API）。

**FR-AIS-01（MUST, Std）**: 新ソース `ai_session` として取り込む。v1の対象は**ローカルにセッションログを持つツール**（Claude Code等のJSONLログ）。ログから復元するのは会話ターン（role / 時刻 / セッションID）であり、AXによる画面上の会話セグメンタはv1では実装しない。

**FR-AIS-02（MUST, Std）**: **既定OFFのオプトイン。** セッションログはユーザーの仕事の記録そのものであり、中には本人の一存で渡せないものも含まれる。ONにしない限り一切読まない。取り込みはローカル処理のみでegressを伴わない。取り込み状況は設定画面に表示する。

**FR-AIS-03（MUST, Std）**: `ai_session` イベントは `thread_key`=セッションIDを持ち、既存の検索・抽出・Fusion・state推定にそのまま乗る。**専用経路を作らない**（ソース対称の原則）。

**FR-AIS-04（MUST, Std）**: state候補のprovenanceに `source=ai_session` を保持し、根拠表示で「AIツールでのやりとり由来」を区別できるようにする。

**FR-AIS-05（MUST, Std）**: 取り込みは**堅牢に失敗する**こと。セッションログは追記中のファイルであり、読み取り時点で行が書きかけであることがある。壊れた1行が他ファイルの取り込みを止めてはならず、アプリを落としてはならない。再取り込みは冪等とし、1回のパスで読む量に上限を設ける（長時間セッションは数十MBに達しうる。残りは次のパスで拾う）。

**受け入れ基準（6.19）**: 既定OFFの検証。壊れた行を含むログで他の行が取り込まれること。同一ログの再取り込みで重複が生まれないこと。取り込んだターンが既存の検索で引けること。

---

### 6.20 Patterns記録

**目的**: 採否の履歴を貯める。v1は**記録のみ**で、そこからの学習・適用は行わない（スコープ表 §3.2）。

**FR-PAT-01（MUST, Std）**: アクション提案の採択・修正・却下、Recap候補の `[Track]`／破棄を、フィードバックイベントとしてローカルに記録する。編集の場合は蒸留の入力となる変更前後のテキストを併せて保持する。**この記録はデバイス外に出さない。**

**FR-PAT-02（MUST, Std）**: 提案そのものの文脈（提示されたサーフェス・スロット位置・その時のアプリ・判断までの所要時間）をメタデータとして併記できる。**メタデータのみで、アクションの内容やキャプチャしたテキストは含めない。** 語彙は閉じた集合として制約し、集計が意味を持つ状態を保つ。カラムは全てNULL許容とし、「記録なし」を0と取り違えない。

~~**FR-PAT-03（v1.5・予告）**: 記録からのFusion優先度の個人化・ドラフトの口調学習・定型のL2提案化は、v1.5要件定義で設計する。v1では実装しない。~~
**【2026-08-24 確定｜Issue #242・オーナー決定】** 予告プレースホルダを以下の確定要件（FR-PAT-03〜07、いずれも v1.5, Std）に置換する。詳細設計は `docs/continual-learning-v2-design.md`。

**FR-PAT-03（MUST, Std, v1.5）**: FR-PAT-01/02 の記録と既存イベント（event_log / sessions / threads / projects）から、**Work Patterns**（行動プロファイル）を夜間にローカル蒸留する。学習対象は4種: `activity`（何をよくやるか）/ `focus`（何に取り組んでいるか。projects活動量・threads・ウィンドウタイトルの反復固有名詞×dwell加重の3ソース）/ `rhythm`（時間帯バンド×活動。バンドは early/morning/afternoon/evening/night の閉じた語彙、バンド偏り≥50%のみ）/ `avoid`（同一 action_kind×context_app で提示5回連続・採択0）。全行に confidence（corroboration曲線流用・**上限0.75** = offered never asserted）と再計算型decay（accumulateしない）。**新しいキャプチャは追加しない**——既存記録の集計のみ。

**FR-PAT-04（MUST, Std, v1.5）**: 提案の**提示**をメタデータのみで記録する（`offer_impressions`: ts / action_kind / surface / rank / context_app）。「無視された」は提示−決定でのみ導出する。内容・キャプチャテキストは載せない（FR-PAT-02と同じ規律）。保持は30日で自動削除。

**FR-PAT-05（MUST, Std, v1.5）**: Work Patterns の消費は**提案の選択と順位・生成内容への注入のみ**とする。(a) FR-CF-03 の優先度スコアに採択率prior（Laplace平滑）・habit prior・時間帯一致priorを加える。(b) `avoid` 一致はハードペナルティで抑制するが、**クールダウン14日後に低ランクで1回だけ再提示**し、採択で即復帰する。(c) FR-CF-04 の汎用アクションは抑制対象外（空パネルを構造的に作らない）。(d) **権限判定（L1/L2/L3）には一切影響しない**ことをテストで固定する。(e) 明示 directive（Shougun.md）は常に学習に勝つ。(f) パターンの自動実行はしない——先回りは提示まで。

**FR-PAT-06（MUST, Std, v1.5）**: 学習は**見せる**。(a) Patterns UI（Personalization、Learnedと同格）に全行を表示し（key・回数・trend・時間帯・ON/OFF・削除）、ブラックボックスを作らない。(b) 新パターンが初めて閾値を超えたとき、**気づきモーメント**を1回だけ提示する（Brief + Notch。気づき1件=提示1回、必ずその場に Undo/OFF、必ず実際の挙動変更と対、文言はテンプレでパターンのキーと回数のみ）。(c) MCP/CLI/REST に `patterns.list` / `patterns.set_active` を対称公開する（不変条件6）。

**FR-PAT-07（MUST, Std, v1.5）**: 効果を計測する: kind別 offer CTR（提示→採択。FR-PAT-04で初めて分母が立つ）/ dismiss・無視率 / 抑制発動・復帰数 / pattern lessons 件数を `shogun metrics` に追加する。プロファイル・回数を含め、**いかなる集計値もテレメトリに送らない**（ローカル表示のみ）。

**受け入れ基準（6.20）**: 記録がegress経路に到達しないこと、ログに出力されないことをテストで担保。閉じた語彙に反する値が入らないこと。既存行がNULLのまま有効であること。

---

### 6.21 視覚キャプチャ（Visual recall）

**目的**: AXテキストで拾えない画面内容（画像内の文字、AXを実装しないアプリ）を、記録メディアを作らずに拾う。

> **本節の前提**: 本節はCLAUDE.md不変条件2に対する**明示的・記録済みの例外**（2026-08-02 オーナー決定、Issue #106/#107）。例外の範囲は「Visual recall がONのとき、OCR用に取得した画面を圧縮JPEGとして暗号化済みDBに最大72時間だけ保持する」ことに限られる。**クラウド送信なし・音声は対象外・永続タイムラインではない。** OFFのときは画像を一切保存しない。

**FR-VIS-01（MUST, All）**: 視覚キャプチャは**イベント駆動のキーフレーム取得のみ**とし、連続録画（動画）を行わない。動画ファイル・スクリーンショットファイルを生成するコードを書かない。

**FR-VIS-02（MUST, All）**: **既定OFF。** OFFの間は画面ピクセルを一切読まず、OCRも走らせない。

**FR-VIS-03（MUST, All）**: OCRは**オンデバイス**（Apple Vision）で行う。画面画像をクラウドのVLM/OCRへ送らない。

**FR-VIS-04（MUST, All）**: 抽出したテキストとprovenanceはevent log（`source = screen_ocr`）に入り、既存の検索・抽出・Fusionにそのまま乗る。**専用経路を作らない**（ソース対称の原則）。

**FR-VIS-05（MUST, All）**: 保持するJPEGフレームは専用テーブルに置き、**ピクセルのみ**を保持する（テキストはevent log側）。保持は**最大72時間**で、期限切れは自動削除する。保持テーブルは作成時刻でインデックスし、削除が保持期間の走査で完結すること。

**FR-VIS-06（MUST, All）**: **無駄なOCRを走らせない。** 対象領域が直前にOCRした内容とピクセル的に同一であればVisionパスをスキップする。判定はアプリ単位で持つ。

**FR-VIS-07（MUST, All）〔2026-08-17時点 未実装〕**: 画面収録権限が拒否された場合は**AXテキストのみへ縮退**し、機能全体を落とさない。現状は権限のプリフライトも拒否時の縮退経路も実装がなく、開示文言（設定・オンボーディング）が「画面収録が必要」と述べているのみ。**本項は既存実装の追認ではなく、埋めるべき差分として記載している。**

**FR-VIS-08（MUST, All）**: 設定・状態の変更はMemory API / MCP / CLIからも対称に行える（不変条件6）。設定はディスク上の設定ファイルを正とし、キャプチャループが書き込みを観測できること。設定の保存はテンポラリ書き込み＋rename（電源断で設定を壊さない）。

**FR-VIS-09（MUST, All）**: オンボーディングと設定画面で次を明記する: 連続録画はしないこと / 画像はデバイス外に出ないこと / 保持は72時間で自動削除されること / いつでも削除・OFFにできること。

**FR-VIS-10（MUST, All）〔2026-09-23 追記｜Issue #283〕**: 保存画面には、同じ瞬間・同じフォーカスの **Accessibility テキスト（通常キャプチャの `event_log` 行）への参照**を添える（`screen_frames.accessibility_event_id`、V24）。テキストのコピーは作らず、フォーカス同一性が一致しないときはリンクしない。検索・表示はテキスト片ごとの provenance（`accessibility` / `ocr`）を保ち、両者を 1 本のテキストに結合しない。保存画面の検索はフレームにつき 1 件に畳み、同点なら Accessibility を優先する。**削除の意味**: 保存画面の削除は JPEG と OCR イベントを消し、Accessibility テキストは通常の記憶として残す（時間範囲削除・全削除で消える）。Off 切替は受動フレームを全削除し、Accessibility テキストには触れない。除外・一時停止・秘匿欄（`AXSecureTextField`）のスキップは AX と OCR の両方に同じく効く。Accessibility 権限が無いときは OCR へ倒さず、キャプチャ全体を止める（除外判定ができないため）。別の同意・トグルは設けない（Accessibility テキストの収集はオンボーディング step 2 で開示済みの通常キャプチャそのもの）。設定画面の開示文に上記の削除の意味を明記する。詳細は `docs/specs/issue-283-visual-recall-accessibility-text.md`。

**FR-VIS-11（MUST, All）〔2026-10-06 追記｜Issue #969〕**: 保存する画像（プレビュー）は**フォーカス中ウィンドウがあるディスプレイ全体**とし、出力はディスプレイのポイント解像度の 3/4、JPEG 品質 50（2026-10-09 オーナー判断: 1x・品質65 では最悪ケース〈1日8時間・10秒ごと〉で 2GiB 上限に約3.6日で届き、保持期間プリセット最大の7日に足りないため画質を下げる）。取得は ScreenCaptureKit の `SCContentFilter(display:excludingWindows:)` で、キャプチャ除外ポリシーに当たるウィンドウを**取得時点で**取り除く: ユーザーが除外したアプリ、既定の除外（パスワード管理・SecurityAgent。ターミナルは 2026-10-19 から既定では除外しない）、SHOGUN 自身、タイトルでプライベートと判定したブラウザウィンドウ、サイト除外があるときに URL を確認できない背面ブラウザウィンドウ（タイトルの無いブラウザウィンドウも同様）、通知バナー（除外アプリの内容を Notification Center 名義で表示するため常に除く）。除外リストは AX と共通の1本で、プレビュー専用のリストを作らない。前面のアプリ・ウィンドウ自体が除外対象ならフレームを撮らない。撮影後にウィンドウ一覧・フォーカス・除外ポリシーを読み直し、除外すべきウィンドウが増えた・フォーカスが移った・ウィンドウが動いたときは破棄する（#300 と同じ考え方）。プレビューは AX テキストの量に関係なく、フォーカス切替時と同一フォーカス10秒ごとに保存し、フォーカス中ウィンドウの画素が前回保存と同一ならスキップする。ストア上限到達中は画素を読まない。**OCR はフォーカス中ウィンドウだけ**を対象とし（ウィンドウ単体の取得。AX が薄いときのみ、FR-VIS-06）、プレビューの保存が OCR や画像モデル推論を起こすことはない。保存フレームはフォーカス中ウィンドウの矩形（`focus_*`、V75）を持ち、「テキストを再読」と自動再スキャンはその矩形だけを読む。OCR イベントの無いプレビューの再読は、保存時刻のイベントを作ってリンクする。`display_id` は実際の CGDirectDisplayID を保存する。【2026-10-11 オーナー決定】ライブ OCR は保存する各フレームで必ず行い、読むのは保存する JPEG ではなく、同じ撮影（ディスプレイのネイティブ解像度）からメモリ上で切り出したフォーカス中ウィンドウの無劣化画像とする（3/4 縮小・品質50 の JPEG では暗い背景の文字の認識率が基準を下回ったため。切り出しは保存しない）。「テキストを再読」は保存済み JPEG を読む。既知の制約: 撮影時にフォーカス中ウィンドウの上に重なっていた他アプリのウィンドウは、再読の矩形に含まれる（ウィンドウ一覧では常時前面の透明オーバーレイと区別できないため。ライブ OCR はウィンドウ単体の取得なので影響しない）。

**受け入れ基準（6.21）**: 既定OFFの検証、およびアップデートで既定が変わらないこと。OFF時に画面ピクセルの読み取りが発生しないこと。72時間超過フレームが自動削除されること。同一画面の連続でOCRがスキップされること。画面収録権限の拒否でAXのみに縮退すること。画像が外部送信経路に到達しないこと（不変条件2/3の検査に含める）。

---

## 7. 非機能要件（NFR群）

### 7.1 SLO（計測定義付き）

CLAUDE.mdのSLO表を計測定義付きで詳細化する。**各SLOの計測コードを製品コードに同梱する**（NFR-SLO-00）。レイテンシに影響する変更はp50/p95を計測してからマージし、結果をPR本文に貼る。

**NFR-SLO-00（MUST）**: 計測基盤: shogun-core内に軽量メトリクス収集（ヒストグラム）を実装し、`shogun metrics`（CLI）とFull UIのAdvanced設定から現在値を確認できる。計測値はローカル保存のみ（テレメトリ送信はオプトイン時もSLO統計値のみ。§7.7）。

| ID | 項目 | 計測点（開始→終了） | p50目標 | p95上限（受け入れ基準） |
|---|---|---|---|---|
| NFR-SLO-01 | Notch展開 | Reactの展開処理開始 → 最初の開いたフレーム（次のrequestAnimationFrame）。Rust状態機械→Reactの遅延と、その後の150ms CSSモーフは含まない | 50ms | **150ms** |
| NFR-SLO-02 | コンテキストアクション提示 | Expanded描画開始 → アクションボタン4件の描画完了 | 80ms | **150ms** |
| NFR-SLO-03 | アクション実行→初トークン | 承認操作のイベント受信 → ストリーミング初トークンのUI描画（**最初に表示される1文字**。定義は下記 2026-09-25 注記） | 600ms | **1s**（ストリーミング必須） |
| NFR-SLO-04 | ローカル検索 | クエリ確定（入力デバウンス後）→ 結果リスト描画完了 | 200ms | **500ms**（10万イベント時） |
| NFR-SLO-05 | context cache更新 | `focus.changed` 受信 → cache差し替え完了 | 150ms | **300ms** |
| NFR-SLO-06 | アイドル時CPU | SHOGUN全プロセス合算、1分平均（ユーザー入力なし・同期なしの状態） | 2% | **5%** |

**NFR-SLO-03 gateway 追加予算（#1101）**: provider 直結に対する追加時間を JP と US の Mac で計測し p50 ≤15ms / p95 ≤40ms を満たす。上表の初可視トークン ≤1s は緩めない。warm / cold admission と接続確立を区別し、サンプル数・失敗・p50/p95 を各実装 PR に記録する。現行 Workers の共有 DB 往復を含めて測定し、未測定を合格としない。

**NFR-SLO-03 計測定義【2026-09-25 オーナー決定｜#388 / #485】**:
- **主指標**: 最初に**画面に見える1文字**が出た時点で止める。後で捨てられる前置き（preamble。例: 本文マーカー前の前置き、ツール呼び出し前の一言）も含む。#533 の実装（使用量台帳の `first_token_ms` = 最初の delta を呼び出し側へ渡した時刻）と同じ定義
- **副指標**: **最終回答の最初の1文字**までの時間も併せて記録する（前置きを捨てた後、ユーザーが実際に使う本文の出だし）。受け入れ基準は主指標のみで、副指標は改善の目安として報告する
  - 記録（#750）: SLO 台帳の別行 `NFR-SLO-03-final`（`Slo::FirstAnswerToken`）。主指標 `NFR-SLO-03` は変えない。ツールを使うターンでは Commit されたモデルターンの最初の1文字、ツールなしのチャットでは主指標と同じ時刻（計時は `shogun_core::metrics::FirstTokenClock`）。`/v1/metrics`・`shogun metrics` では `"acceptance":false`（受け入れ基準ではない）で出し、Advanced の SLO 表では "First answer" 行。目安線は主指標と同じ 1s
- **対象外**: **Codex 委譲**（サブスク委譲の `codex` 経路）は SLO-03 の計測・判定から除外する。`codex exec --json` は回答を一括で返しストリーミングできないため、初トークン＝完了となり SLO-03 の意味を持たない（FR-AG-07 の「非ストリーミングの一括応答UIを作らない」は、この経路で途中表示を偽装する UI を作らないことを意味する）

**NFR-SLO-07（MUST）**: SLO超過の扱い: p95超過を検出したら該当操作をメトリクスに記録し、Advanced画面で確認可能にする。超過してもユーザー操作をブロック・キャンセルしない（提示は遅れても行う）。

### 7.2 セキュリティ

**NFR-SEC-01（MUST）**: secrets（OAuthトークン・BYOKキー・Composio認証情報・ライセンストークン・APIクライアントトークン）は**macOS Keychain（security-framework）以外に保存しない**。平文ファイル・DB・環境変数・ログ・クラッシュレポートへの書き出しを禁止する（CLAUDE.md不変条件7）。

**#1101 サーバー secrets の境界**: 会社 provider キーと OAuth client secret は hosting の secrets store のみ。Mac の Keychain 規則と区別する。gateway は token / code / 本文 / IP をログ・DB に保存しない。account サーバーのセッションと単回コードはハッシュのみ保存する。

**NFR-SEC-02（MUST）**: secretsをwebview（JSコンテキスト）に渡さない。BYOK入力はTauri command経由で即Keychainへ書き込み、UIへの読み戻しは末尾4桁のみ。

**NFR-SEC-03（MUST）**: ローカルREST/MCPサーバーは127.0.0.1バインドのみ。全呼び出しにトークン認証（FR-API-03）。CORSは無効（ブラウザからの呼び出しを想定しない）。

**NFR-SEC-04（MUST）**: 外部通信は全てTLS。証明書検証の無効化オプションを作らない。

**NFR-SEC-05（SHOULD）**: SQLiteのDBファイル・モデルファイルはmacOSのユーザーホーム配下（`~/Library/Application Support/com.selectkk.shogun/`）に0600相当の権限で置く。DB全体の暗号化（SQLCipher等）はv1では採用しない（FileVault前提。判断は付録Bに付記）。

### 7.3 プライバシー原則

**NFR-PRV-01（MUST）**: スクリーンショット・画像・**音声ファイルを保存**するコードを書かない（不変条件2）。**【2026-08-02 明示的例外｜Visual recall】** Visual recall が On のときにかぎり、フォーカス中ウィンドウがあるディスプレイの圧縮 JPEG（除外対象のウィンドウは取得時点で除く。2026-10-06 #969 更新、FR-VIS-11）を暗号化済みメモリ DB（`screen_frames`）にユーザーが選択した有限期間（プリセット1〜7日＋上限3,650日のカスタム、既定・フォールバック3日）だけ保持し、期限による自動削除は常時有効（無効化不可）。バイト予算での早期削除はせず、ストア上限（2GiB）到達時は既存フレームを保持したまま新規取得を停止する。保持期間中はローカルで閲覧・削除できる。既定は Off、クラウド送信なし、音声は対象外（CLAUDE.md 不変条件2の例外、2026-08-20 更新）。音声ファイルについては例外なし。会議音声の取得は会議セッション中に限り、波形はRAM内リングバッファ（上限30秒）でのみ処理し、ディスク・一時ファイル・クラウドのいずれにも書かない。永続化するのは文字起こしテキストとそのprovenance（`origin`: asr / caption）のみ。詳細はFR-MT群（`docs/meeting-notes-ui-design.md`）。

**NFR-PRV-02（MUST）**: 生データ（event logの行・キャプチャ全文）はデバイス外に出さない。クラウドに出るのは目的別の処理用チャンクのみで、全てトレーサビリティログを伴う（不変条件3、FR-TR-01）。

**NFR-PRV-03（MUST）**: テレメトリ・診断ログ・クラッシュレポートにキャプチャ内容（ユーザーのテキスト）を含めない。例外なし（デバッグビルドも同様）。

**NFR-PRV-04（MUST）**: サーバー側（ライセンスAPI・Stripe）にメモリ内容・state内容を保存しない。サーバーが持つのはアカウント・課金・ライセンス状態、および #1101 の内容を含まない使用量・上限・監査情報。gateway を通る処理用 live text は保存しない。

**NFR-PRV-05（MUST）【2026-10-14 #1101 D11】**: リクエスト・レスポンス本文はどこにも保存させない。gateway・relay は本文・音声・文字起こしをディスク・DB・ログ・メトリクス・エラー報告に書かず、ストリームはバッファしない。provider 側は OpenAI（org ZDR、全呼び出しで `store:false`、prompt cache は in-memory のみ）、Azure OpenAI（modified abuse monitoring の免除）、Groq（ZDR）、Deepgram（`mip_opt_out=true` と非保存の書面確認）の承認・確認をローンチ前にそろえる。Azure は免除の承認前に実ユーザーの通信を受けない。Batch は仕組み上 provider が入力・結果ファイルを保持するため、Mac が結果を読み終えた時点で両方を削除し、読まれないものは期限切れの掃除で消す。Morning Brief の Anthropic Batch も同じく読了後に batch を削除する（2026-10-15 オーナー決定。Anthropic のゼロ保持の申請は後回しで、未読分は Anthropic の既定の29日で消える）。各 provider の保持設定は公開の privacy 文書に載せ、月1回の運用確認で有効なままかを確かめる。

### 7.4 クラッシュ耐性・データ完全性

**NFR-REL-01（MUST）**: SQLiteはWALモード。書き込みは全てトランザクション。電源断・強制終了でDBが破損しないこと（`PRAGMA synchronous=NORMAL` 以上）。起動時にintegrity check（quick_check）を行い、破損検出時は直近バックアップ（NFR-REL-03）からの復旧フローを提示する。

**NFR-REL-02（MUST）**: キャプチャ・DB書き込み経路で `unwrap()` 禁止（テストコード以外）。パニックはスレッド単位で隔離・再起動し、アプリ全体を落とさない。クラッシュ（プロセス異常終了）はセッションあたり0を目標とし、発生時は次回起動時に診断（キャプチャ内容を含まない）を記録する。

**NFR-REL-03（MUST）**: ローカルバックアップ: DBのスナップショット（SQLite backup API）を日次（Dream Cycle前）に取得し、最新の検証済み1世代と、保持期間内の最新アップグレード前コピーを保持する。バックアップもデバイス内のみ。
  - **【2026-09-25 オーナー決定・実装済み｜#647 削除済みデータのバックアップ残存】** (1) 「全削除（Delete everything）」は DB 削除のコミット後に `backups/` のスナップショットもすべて消す。失敗は黙殺せずユーザーへ表示する。(2) スナップショットに**保持日数の上限**を設ける: 1〜30日、**既定 7 日**（`backup.json`。UI と MCP/REST/CLI の `backup.status` / `backup.set_retention` で対称）。通常は1世代（2026-09-27 オーナー決定）。最新アップグレード前のコピーは保持期間内に追加で保護し、失敗したアップグレードの再試行でも元のコピーを再利用する。検証が不確かな新しいコピーも追加で残し、自動再検証を最大1日間隔まで遅らせるため、期限切れ削除も遅れ得る。期間を過ぎたスナップショットは起動時・通常30分ごと・公開時・設定変更時に削除する。ただしクラッシュ耐性を優先し、より新しい検証済みスナップショットが無い間は最新の検証済みスナップショットを期間超過でも残す。時間範囲削除などの個別削除はスナップショットを書き換えないため、削除前の行は保持期間のあいだスナップショット側に残り得る（公開ドキュメント `apps/docs/privacy/mechanism.mdx` §3 に明記）

**NFR-REL-04（MUST）**: エラーはユーザーの作業を中断させない形で処理する（Notchインジケータ色、FR-NU-06/07）。モーダルダイアログを出してよいのはL3確認・削除確認・復旧フローのみ。

### 7.5 リソース上限

| ID | 項目 | 上限 |
|---|---|---|
| NFR-RES-01 | アイドル時CPU | 5%（1分平均。NFR-SLO-06と同一） |
| NFR-RES-02 | 常駐メモリ（RSS、アプリ合算） | 通常時900MB以下、うちHot層200MB以下（FR-MEM-02）。ONNXモデルは常駐に含む |
| NFR-RES-03 | ディスク | Warm+Cold+索引の合計使用量を設定画面に表示。20GB到達で通知（アンバー）し、Cold層の圧縮・削除導線を提示 |
| NFR-RES-04 | バッテリー | Dream CycleはFR-DC-01の電源条件に従う。embedding生成等のバックグラウンドジョブはLow Power Mode中は間引く（周期2倍） |
| NFR-RES-05 | GPU/ANE | ONNX推論はCoreML/CPU実行。推論1件あたり50ms以内（e5-small級・512トークン・M1基準）を選定ベンチの基準にする |

### 7.6 配布・更新

**NFR-DST-01（MUST）**: 配布はDeveloper ID署名＋notarization済みDMG。App Storeは使わない。arm64（Apple Silicon）単一バイナリ、macOS 14 Sonoma以上（付録A ADR-005）。

**NFR-DST-02（MUST）**: 更新はTauri updater（署名検証付き）。更新チェックは日次＋手動。更新適用はユーザー確認後（作業中の強制再起動をしない）。updaterへのリクエストにユーザーデータを含めない（バージョン・アーキテクチャのみ）。

**NFR-DST-03（MUST）**: マイグレーション（refinery）はアプリ更新後の初回起動時に自動適用し、適用前にバックアップ（NFR-REL-03）を取得する。

### 7.7 テレメトリ方針

**NFR-TEL-01（MUST）**: デスクトップの匿名機能分析は **PostHog、既定ON（オプトアウト）**（2026-08-08 オーナー決定、CLAUDE.md）。オンボーディングと設定は同じ `analytics.json` の `opt_out` を使う。ウェブサイトの分析は別設定で、明示同意までOFF。ONの場合も送信してよいのは: 匿名デバイスID、アプリバージョン、機能イベント名とカウント（例: `action_executed{level=l2}`）、SLO統計値（p50/p95）、クラッシュ有無。**キャプチャ内容・state内容・検索クエリ・生成物・送信先情報は送らない**（NFR-PRV-03）。

**NFR-TEL-02（MUST）**: テレメトリ送信もトレーサビリティログの対象とする（purpose=`telemetry`）。送信項目の一覧を設定画面から閲覧できる。

### 7.8 アクセシビリティ・UI品質

**NFR-UI-01（SHOULD）**: Full UIはmacOSのキーボード操作・VoiceOverの基本操作に対応する。Notchパネルはホットキーで全機能に到達可能とする（マウス必須にしない）。

**NFR-UI-02（MUST）**: UI文言はSHOGUNブランドルール準拠: 競合プロダクト名を出さない、技術スタック名をUI文言に出さない、絵文字は⚔のみ許可、"AI-powered" / "revolutionary" / "second brain" を使わない。

---

## 8. 技術スタック

CLAUDE.mdの確定表を転記し、選定理由を付す。**このスタックを勝手に変更しない。**

| 領域 | 技術 | 選定理由（1行） |
|---|---|---|
| アプリ枠 | Tauri v2 / Rust backend / React + TS frontend | データ重心をRustに置きつつ、UI開発速度と既存JSモノレポ資産を活かすため |
| Notchパネル | NSPanel（objc2）、`.nonactivatingPanel` + `.canJoinAllSpaces` + `.fullScreenAuxiliary` | フォーカスを奪わず全Space・フルスクリーン補助表示に出せる唯一の組み合わせのため |
| DB | SQLite（rusqlite）+ sqlite-vec、WAL、FTS5 trigram | 単一プロセス埋め込み・実績・多言語FTS・ベクトル検索を1ファイルで満たすため（詳細は付録B） |
| マイグレーション | refinery | Rustネイティブのバージョン管理マイグレーションで後方互換規律を機械化するため |
| macOSネイティブ | AXUIElement / NSWorkspace / NSEventグローバルモニタ / security-framework | テキストのみキャプチャ（不変条件2）とKeychain限定secrets（不変条件7）の実現手段 |
| embedding | ローカルONNX多言語モデル（同梱） | オフライン動作・追加限界費用ゼロ・プライバシー境界維持のため（ADR-001） |
| MCP | Rust MCP SDK（クライアント＋サーバー） | 第1層直結（クライアント）とMemory API（サーバー）を同一SDKで実装するため |
| LLM | Anthropic（Batch API=Select KKキー / Messages API=BYOK） | キー分離（不変条件5）。プロバイダはtrait抽象、v1実装はAnthropicのみ（ADR-002） |
| 第2層連携 | Composio（オプトイン） | 公式MCP未提供の操作（Gmail送信）を第三者経由と明示した上で提供するため |
| 課金 | Stripe | Checkout/Billing/ポータルで課金UIを自前実装しないため |
| 配布 | Developer ID + notarization（App Store不使用） | Accessibility常駐アプリの審査リスク回避と更新速度のため |
| 更新 | Tauri updater | 署名検証付き自動更新の標準手段のため |

---

## 9. リスクと未決事項

### 9.1 未決事項（実装前に決定が必要なもの・時期）

| ID | 未決事項 | 決定時期 | 現時点の扱い |
|---|---|---|---|
| ~~OPEN-01~~ | ~~トライアル開始時のクレジットカード要否~~ | **解消 2026-09-17** | **カード必須開始**（オーナー判断）。既定値の決定であり、両対応の実装構造は FR-BIL-06 のとおり維持する |
| OPEN-02 | embeddingモデルの最終選定（候補: multilingual-e5-small等） | Phase 1着手時のベンチ | 要件はFR-MEM-21で固定（ローカル・多言語・オフライン・限界費用ゼロ・sqlite-vec格納・NFR-RES-05の推論速度） |
| OPEN-03 | Slack公式リモートMCPの提供状況・WS管理者承認フローの実態確認 | Wave 2着手前 | 不可の場合はFR-INT-30のフォールバックが恒常運用になる |
| OPEN-04 | チームプラン（複数シート・組織請求） | v1リリース後の需要検証 | v1は個人のみ（§3.2） |
| OPEN-05 | 価格ローカライズ（JPY等の現地通貨表示・課税） | 課金実装着手前 | v1はUSD建て。Stripeの通貨対応範囲で判断 |
| OPEN-06 | Intel Mac対応 | v1リリース後（要望数と保守コストで判断） | v1はApple Siliconのみ（ADR-005）。要望の記録: 2026-10-23 に 1 件（Intel Mac で arm64 ビルドが開けない。known-issues に明記 #1383） |
| OPEN-07 | オンデバイスASRモデルの選定（オンデバイス・ストリーミング・多言語） | MT3着手前のベンチ | 要件はFR-MT-13で固定。モデルサイズと会議中CPU（FR-MT-21の15%枠）のトレードオフで判断。**ファイル入力を要求する実装は選定対象外**（FR-MT-12） |
| OPEN-08 | システム音声の取得方式とmacOS 14.0〜14.3での縮退挙動 | MT3着手前 | Core Audio tap（`CATapDescription`、macOS 14.4+）を前提とし、非対応環境で「マイクのみに縮退」か「機能を出さない」かを決める（FR-MT-13） |
| OPEN-09 | 話者分離をどこまでやるか | MT3完了後 | v1は「自分 / それ以外」の2値まで（FR-MT-15）。参加者名への割り当ては`calendar_occurrences.attendees`との突合として次段 |
| OPEN-10 | Computer Use（Phase 3）に伴う不変条件2/3/4の改訂を、いつ本文へ適用するか | CU1着手時（M4出荷後） | 改訂案は `docs/computer-use-phase3-design.md` §3 に確定済み（ピクセル知覚のprocess-only送信＝不変条件2/3の例外、上限付き事前承認＝不変条件4の例外）。**v1のあいだは適用しない**——現行不変条件を前提にしたアーキテクチャテストを、実装より先に緩めないため |

### 9.2 主要リスク

| リスク | 影響 | 緩和策 |
|---|---|---|
| Phase 0 No-Go（ノッチUI不成立） | メインサーフェス転換 | CLAUDE.md既定: メニューバー＋パレット方式へ転換。本書のFR-NU群を改版（状態機械・SLOは概ね移植可能） |
| Accessibility APIで十分なテキストが取れないアプリ（Electron・独自描画） | キャプチャ空白地帯 | 取得可否のアプリ別実測をPhase 1早期に実施。取れないアプリは連携（第1層）データで補完。常時キャプチャをスクショOCRへ転換することは**しない**（不変条件2）。テキストが取れないウィンドウに限っては、Off が既定の Visual recall（2026-08-02 例外、選択した保持期間で自動削除・既定3日）が任意の補完手段になる |
| 公式リモートMCPの仕様変更・提供中止 | 第1層連携の停止 | サービスごとに接続層を分離し、個別に無効化・フォールバック可能にする（FR-INT-06） |
| sqlite-vecの性能限界（Warm層行数増大） | 検索SLO超過 | Warm層期間短縮設定（FR-SET-01 Memory）、量子化前倒し。v2でANNインデックス再評価 |
| Batch APIのコスト増大（Select KKキー負担） | 原価圧迫 | チャンク量の上限設計・Dream Cycle処理量のメトリクス監視。プラン原価モデルの月次見直し |
| BYOKハードルによるPro転換率低下 | 収益 | オンボーディングでのキー取得ガイド充実。OPEN-01と合わせてファネル計測 |
| ノッチ常駐アプリというUIパラダイムの誤発火・鬱陶しさ | 解約 | Phase 0の誤発火計測、FR-MB-03の「割り込まない」原則、全自動表示の抑制 |

---

## 10. 付録A: 主要判断記録（ADR）

各ADRは「状況 / 決定 / 理由 / 帰結」の形式。変更する場合は新ADRで上書きし、旧ADRは残す。

### ADR-001: embeddingはローカルONNXモデルを同梱し、クラウドembedding APIを使わない

- **状況**: Warm層のベクトル検索にembeddingが必要。クラウドAPI（高精度・容易）とローカル推論（プライバシー・費用）の選択。
- **決定**: 多言語ローカルONNXモデル（候補: multilingual-e5-small等、最終選定はPhase 1ベンチ = OPEN-02）を同梱する。クラウドembedding APIは使わない。
- **理由**: (1) キャプチャ全文をembedding目的でクラウドに送ると「生データはデバイス外に出さない」（不変条件3）の実質的な骨抜きになる。(2) キャプチャは高頻度でありAPI課金は限界費用が利用時間に比例して増える。ローカルなら限界費用ゼロ。(3) オフラインでも検索が完全動作する。(4) e5-small級はarm64のCPU/CoreML推論で実用速度（NFR-RES-05）。
- **帰結**: 精度は最新クラウドモデルに劣る可能性がある。FTS5とのハイブリッド（FR-MEM-20）で補い、モデル差し替え可能な形（モデルファイル＋次元数をメタデータ管理）で実装する。

### ADR-002: BYOKはv1でAnthropicのみ。プロバイダ抽象化層（trait）は設けるが実装は1つ

- **状況**: エージェント推論のBYOKを複数プロバイダ対応にするかどうか。
- **決定**: v1はAnthropicのみ。`LlmProvider` trait（FR-AG-08）で抽象化のみ行い、実装は1つ。
- **理由**: (1) ツール呼び出し・ストリーミング・プロンプトキャッシュの挙動差を吸収するコストが v1の価値に寄与しない。(2) Select KKキー側（Batch API）がAnthropicであり、プロンプト資産・評価基盤を1系統に集中できる。(3) trait境界だけ正しく切っておけば追加は後から可能。
- **帰結**: 他プロバイダ希望ユーザーを一部逃す。要望はOPEN項目として計測する。trait境界にAnthropic固有型を漏らさない規律をレビューで維持する。

### ADR-003: プラン差別化は「機能差」で行い、キー境界と一致させる

- **状況**: Standard/Proの差を使用量制限（クォータ）にするか機能差にするかの選択。
- **決定**: 機能差で差別化する。Standard = 観測系（キャプチャ・メモリ・検索・Notch UI・第1層読み取り統合・Dream Cycle・Morning Brief）、Pro = 実行系（エージェント実行・チャット・ドラフト・Memory API・Composio）。StandardはSelect KKキーのみで動作しBYOK不要、Proの推論はBYOK必須。
- **【2026-09-24 改定】** エージェント実行（チャット・コンテキストアクション・ドラフト・Scribe）を Standard に移す。実機で Standard 購入直後に全部ロックされる体験が有償プランとして成立しないため（オーナー判断）。推論はユーザー資格情報で動くので会社側コストは変わらない。Pro の差分は Memory API・Composio 実送信・優先サポート。
- **理由**: (1) プラン境界とキー境界（不変条件5）が完全一致し、実装・原価・説明が単純になる。(2) クォータ制は「使うほど罰される」体験になりプロダクト原則（実行 > 提案）と矛盾する。(3) BYOK必須層をProに限定することで、Standardのオンボーディングからキー取得の摩擦を排除できる。
- **帰結**: Standardユーザーは実行系に触れない。FR-CF-05のロック表示（最大1件）でアップグレード動機を作るが、広告的体験にはしない。

**2026-10-13 #1101 による ADR-003 の追補**: 上記の「推論は BYOK 必須」「会社側コストは増えない」「キー境界とプラン境界が一致」は旧判断。現行の目標は gateway 会社キーを既定とし、BYOK / 委譲を任意にする。Standard のエージェント権限は維持し、会社負担の live 利用はサーバー側 caps で制御する。D9 は 2026-10-21 にオーナー承認済み（gateway 設計 §2）。


### ADR-004: ノッチ非搭載環境には「擬似ノッチ」で同型UIを提供する

- **状況**: ノッチはMacBook（2021以降）内蔵ディスプレイにしかない。外部ディスプレイ・ノッチ非搭載Macをどうするか。
- **決定**: メニューバー中央に同型・同状態機械のフローティングパネル（擬似ノッチ）を出す（FR-NU-04）。
- **理由**: (1) 「画面上端中央の固定アンカー」という空間的習慣が製品体験の核であり、環境で挙動が変わると習慣が形成されない。(2) 実装はNSPanelの配置座標の違いのみで、状態機械・SLO・描画を完全共有できる。(3) クラムシェル運用（外部ディスプレイのみ）のユーザーを切り捨てない。
- **帰結**: メニューバー中央のアイコン過密環境では他常駐アイコンと視覚干渉しうる。位置の微調整オプションは要望を見てから検討する。

### ADR-005: 公式サポートはApple Silicon（arm64）のみ、macOS 14以上

- **状況**: Intel Mac対応の要否。
- **決定**: v1はApple Siliconのみ公式サポート。macOS 14 Sonoma以上。Intel対応は将来判断（OPEN-06）。
- **理由**: (1) ノッチ搭載機は全てApple Siliconであり、コア体験の対象機はASに集中している。(2) ローカルONNX推論（embedding）の性能・電力効率がAS前提で設計できる（NFR-RES-05の基準もM1）。(3) 単一アーキテクチャのビルド・署名・QAで配布が簡素になり、v1の検証マトリクスを半減できる。
- **帰結**: Intel機ユーザー（減少中）を逃す。擬似ノッチ（ADR-004）はAS製Mac mini / Studio等のノッチ非搭載機のために引き続き必要。

### ADR-006: 既存pnpmモノレポにCargo workspaceを統合する（リポジトリ分離しない）

- **状況**: Rustコアを別リポジトリにするか、既存JSモノレポ（apps/website等）に統合するか。
- **決定**: 既存モノレポのルートにCargo workspaceを追加し、crates/ + apps/desktop を同居させる（§5.1）。
- **理由**: (1) apps/desktop（Tauri）はRustとReact/TSの両方に跨り、分離するとバージョン同期・CI連携のコストが恒常化する。(2) LP（website）・ドキュメント・アプリの文言/ブランド資産を単一リポジトリで共有できる。(3) 個人〜少人数開発ではリポジトリ分割の権限分離メリットがない。
- **帰結**: CIはJS/Rustのパスフィルタで分割実行する（AR-02の独立ビルド要件）。リポジトリサイズ増大はONNXモデル等のバイナリをGit LFSまたはビルド時取得にして緩和する。

### ADR-007: macOS下限は14を維持し、Apple Foundation Modelsは「任意の高速化」以上にしない

- **状況**: Apple Foundation Models framework（オンデバイスLLM）はmacOS 26を要求する。下限を26へ引き上げてオンデバイス実行を解禁するかの判断（Issue #289、Sentient OS競合レビュー 5/8。決定は2026-08-25のIssueコメント）。
- **決定**: 下限はmacOS 14 Sonomaのまま維持する（ADR-005有効）。Foundation Modelsは採用する場合も任意の高速化経路に限定し、いかなる機能の必須条件にもしない。クラウド経路はmacOS 14〜25・バッテリー駆動・Low Power Mode・サーマル制限下を含め常に完全な経路であり続ける。
- **理由**: (1) macOS 26普及率~86%（2026-07）でも残る利用者を、v1立ち上げ期に自ら切る理由がない。(2) 下限を上げてもクラウド経路の完全性は恒久維持が必要で、オンデバイス化は置き換えでなく2系統目の恒久保守になる。(3) オンデバイス重量処理はwake helper（rootデーモン・deadmanタイマー・管理者プロンプト・恒久サポート面）を連鎖的に要求する。実行場所を動かさないことがこのコストを断つ最安手である。(4) 下限は後から上げられるが、下げるのは実質不可能。
- **帰結**: wake helperは作らない（夜間処理は「蓋閉じ時に投げ、開け時に取る」方式を維持）。競合レビューのオンデバイス評価は現時点で対象外としてクローズし、原価低減は前処理フィルタ・ルーティング側で行う。将来FMを載せる場合の受け入れ条件（機能パリティ・無言フォールバック・AC/サーマル門番・レーン別計測等）は `docs/specs/issue-289-macos-floor-decision-record.md` のDR-289群に従う。

---

## 11. 付録B: ストレージにPGLiteを使わない理由

**決定**: SHOGUNのローカルストレージはSQLite（rusqlite）+ sqlite-vec + FTS5（trigram）を採用し、PGLite（WASM版PostgreSQL + pgvector）は採用しない。

**背景**: 初期構想では「Postgres系の機能（pgvector等）をローカルで使う」選択肢としてPGLiteが検討対象だった。以下の理由で不採用とする。

1. **プロセスモデルの不一致**: PGLiteはWASMランタイム上で動くシングルコネクションのPostgresであり、Rustデーモン（shogun-core）から使うにはWASMランタイム（またはNode系ホスト）の同梱が必要になる。データの重心をRustに置く原則（不変条件1）に対し、データ層だけ異質なランタイムを常駐させるのは構成が倒錯する。SQLite+rusqliteは同一プロセス内の関数呼び出しであり、追加ランタイムがゼロである。

2. **常駐デーモンのメモリフットプリント**: SHOGUNは常駐アプリでありNFR-RES-02（RSS 900MB以下）を守る必要がある。WASM上のPostgresはヒープ・ページキャッシュを二重に抱え、アイドル時フットプリントがSQLite（数MB＋ページキャッシュ）より恒常的に大きい。アイドルCPU 5%制約（NFR-SLO-06）にもWASM層のオーバーヘッドは不利に働く。

3. **クラッシュ耐性の実績**: SQLiteのWAL＋トランザクションは電源断耐性の実績が桁違いに長い（NFR-REL-01の前提）。「メモリは年単位で生きる」データを、ブラウザ由来で比較的新しいPGLiteの永続化層に預けるリスクを取る理由がない。

4. **全文検索**: FTS5 trigramはSQLite組み込みで、日本語を含む多言語の部分一致全文検索が索引付きで動く（FR-MEM-20）。PGLiteでPostgresのFTS（tsvector）を使う場合、CJKの分かち書き問題に別途対処が必要で、拡張（pg_trgm等）のWASMビルド保守も自前になる。

5. **ベクトル検索の要件水準**: 本製品のベクトル検索は「Warm層（30日分）のみを対象にした総当たり」で足りる設計であり（FR-MEM-03）、pgvectorのANNインデックス（HNSW等）が必要になる規模をv1では扱わない。sqlite-vecで要件を満たせる以上、pgvectorのためにPostgresを持ち込む理由がない。

6. **Rustエコシステムの成熟度**: rusqlite・refinery・sqlite-vecはRustからの利用実績が厚く、静的リンクで単一バイナリに収まる。PGLiteの主戦場はTypeScript/ブラウザであり、Rustファーストの本構成ではバインディング・運用知見ともに薄い。

7. **配布の単純さ**: SQLiteは追加バイナリ・拡張のnotarization対象物を増やさない（sqlite-vecは静的リンク）。WASMランタイム同梱は署名・更新・サイズの全てで配布コストを増やす。

**帰結・再評価条件**: 将来、(a) マルチデバイス同期でサーバー側Postgresとのスキーマ共有が決定的に有利になる場合、または (b) Warm層を超えるANN検索が恒常要件になる場合（v2ナレッジグラフ）、ストレージ選定を再評価する。その場合もローカルはSQLiteを維持し、同期層で変換する案を第一候補とする。

---

*本書はCLAUDE.md（運用ルール）の下位文書である。矛盾を発見した場合は実装を止め、CLAUDE.mdの絶対不変条件を優先した上で本書の改版を提案すること。*
