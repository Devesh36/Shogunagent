# CLAUDE.md — ShogunAI

SHOGUNのモノレポ。本ファイルはmacOSアプリ本体(`crates/` + `apps/desktop`)と Batch relay(`apps/api`)の実装時に常に守る運用ルールを記す。要件定義の全文は `docs/requirements-v1.0.md`、Phase 1 の作業指示は `docs/phase1-implementation-plan.md`、進捗と実機持ち越しは `docs/phase1-findings.md`、公開リリースまでの残作業は `docs/public-release-remaining-work.md` を参照。詳細で迷ったら docs/ を読むこと。マーケサイト(`apps/website`)と公開ドキュメント(`apps/docs`)は本ファイルの対象外(各配下のREADME/規約に従う)。

## プロダクト一言定義

ユーザーの仕事のワールドモデルを構築し、Macのノッチから「ボタンを押して仕事が終わる」体験を提供するローカルファーストAI OS。記録ツールではない。**状態の推定と実行**のプロダクト。

> **2026-10-13 #1101 の承認済み設計を反映。** 以下の会社キー既定・ブラウザ認証・OAuth broker は目標仕様であり、実装・本番稼働済みを意味しない。現行 relay は Workers + Supabase（今回のオーナー回答で維持を確定）。Issue の Fly 前提との相違と残る未決事項は `docs/hosted-ai-gateway-design.md` を参照。

## 絶対不変条件（違反するコードは書かない）

1. **データの重心はRustコアに置く。** DB・キャプチャ・context cache・SLO責務はRustプロセスが単独所有。webview側にデータ層のロジックを置かない
2. **画像・音声データを一切保存しない。** 画面キャプチャはAccessibility API経由のテキストのみ。会議の音声は原則オンデバイス処理とし、**SHOGUN 自身は波形をディスク・一時ファイルに書かない**。永続化するのは文字起こしテキストとそのprovenanceのみ。スクリーンショット・録画・音声ファイルを生成するコードを書かない  
   **【2026-08-02 明示的例外｜2026-08-20 更新】Visual recall が On のとき、OCR 用に取得した画面は圧縮 JPEG として暗号化済みメモリ DB（`screen_frames`）に、ユーザーが選択した有限期間だけ保持する。プリセット1〜7日＋上限付きカスタム（最大3,650日）、既定・フォールバックは3日。期限による自動削除は常時有効で、無効化する設定を作らない。バイト予算による早期削除もしない: 暗号化ストアが上限（2GiB）に達したら既存フレームを消さずに新規取得を停止する。保持期間中のフレームはローカルで閲覧・削除できる。クラウド送信なし・音声対象外。Visual recall **Off では受動取得を行わない**（Off 切替時に受動フレームを即時全削除）。保持設定は desktop / MCP / REST / CLI で対称。【2026-10-20 監査 M6】`visual_recall.json`・`exclusions.json`・`capture_control.json`・`memory_api.json` は、書き込むたびにメモリ DB キーから導いた鍵の HMAC 封印（`.<file>.seal`）を添え、読むときに照合する（`shogun_core::settings_seal`）。アプリの外で書き換えられたファイルは、プライバシーを狭める変更（Off・短い保持・除外の追加・一時停止・Memory API Off）だけを適用し、広げる変更は無視して記録する。API 面からの拡大は従来どおり L3 承認を経てアプリが封印付きで書く。更新後の初回起動では既存ファイルをそのまま封印する。【2026-10-20 追記】`web_fetch.json`（Web fetch の同意）・`export_folders.json`（書き出し先フォルダ）・`llm.json`（Agent lane の資格情報の選択・サブスク委譲の同意・OpenAI の応答保存）・`gmail-policy.json`（draft-only）も同じ封印に含める。アプリの外での変更から適用するのは、同意の取り消し・フォルダの削除・既定の資格情報（既定プロバイダ＋推奨モデル）への戻し・サブスク委譲の同意と応答保存の Off・draft-only の On だけで、それ以外のプロバイダ・モデルの変更は無視する。API 面が狭める既存の経路（フォルダの削除・draft-only の On・`agent.source.set` の直接適用）は従来どおり封印付きで書く。【2026-09-25 更新】手動保存（`user_screenshot`）の記述を削除（未実装・オーナー判断 #648）。【2026-10-06 更新｜#969】保存する画像はフォーカス中ウィンドウがあるディスプレイ全体（ポイント解像度の 3/4、JPEG 品質 50。2026-10-09 更新: 2GiB 上限を7日もたせるため画質を下げた）。キャプチャ除外ポリシー（AX と共通の1本）に当たるウィンドウは ScreenCaptureKit で取得時点で取り除き、撮影後にウィンドウ一覧・フォーカス・ポリシーを読み直して変化があれば破棄する。AX テキストの量に関係なく保存する。OCR はフォーカス中ウィンドウの範囲だけ。【2026-10-11 オーナー決定】Visual recall On の間は保存する各フレームについてフォーカス中ウィンドウ範囲の OCR を必ず行う（【2026-10-11 オーナー決定・訂正】OCR が読むのは保存する JPEG ではなく、同じ撮影のネイティブ解像度の画像からメモリ上で切り出したフォーカス中ウィンドウ。保存用の縮小・JPEG 圧縮で暗い背景の文字や小さい文字が読めなくなったため。切り出しは保存しない）。AX テキストがウィンドウの文字を覆っていない（空、または OCR 語の 50% 未満）ときだけ OCR テキストを `screen_text` として通常メモリに残し、フレーム期限では消さず Off で消す。**  
   **【2026-10-13 改定｜#1101 D2】** 既定 ASR は Deepgram Nova-3 Multilingual（クラウド live STT）。2026-09-01 の BYOK 必須を撤回し、会社キーはサーバーの secrets store のみに置く。`POST /v1/asr/grant` が TTL 60 秒の短命 JWT を発行し、Mac は音声を Deepgram へ直接送る（`mip_opt_out=true`、process-only）。音声は gateway を通さず、波形をディスク・一時ファイルに書かない。BYOK は任意の上書き。Settings に送信先・非保存を開示し、Record meeting プロンプトには繰り返さない（#978）。旧 mint 実装の復元・検証は Phase 2。
3. **生データはデバイス外に出さない。** クラウドへ送るのは処理に必要なチャンクのみ。**【#1101 D1 明示的例外】** #1101 の live lane は gateway を経由するが、gateway は本文を保存・ログ出力しない。音声は Deepgram 直結、connector API は各 provider 直結。全送信に本文を含まないトレーサビリティを残し、gateway と下流 provider を開示する。記憶のクラウド同期は対象外。
   **【2026-10-14 #1101 D11｜ゼロ保持】** リクエスト・レスポンス本文は gateway・relay・provider のどこにも保存させない。gateway / relay は本文をディスク・DB・ログ・メトリクス・エラー報告に書かず、ストリームはバッファせず中継する。provider 側のゼロ保持（OpenAI ZDR + `store:false` + in-memory の prompt cache、Azure の modified abuse monitoring 免除、Groq ZDR、Deepgram の書面確認）はローンチ前に全部そろえる。Azure は免除が承認されるまで実ユーザーの通信を流さない。Batch の入力・結果ファイルは Mac が読み終えた時点で provider から削除し、読まれなかったものは期限切れで掃除する（Morning Brief の Anthropic Batch も読了後に削除。Anthropic のゼロ保持の申請は 2026-10-15 のオーナー判断で後回し）。手順は `docs/ai-gateway-launch-checklist.md`
4. **L1（自動実行）に外部送信系アクションを絶対に含めない。** 送信・投稿・カレンダー作成は必ずL3（明示確認）
5. **キーの分離【2026-10-13 改定｜#1101 D1】**: Batch（インデックス・分類・Dream Cycle・Morning Brief）は会社キー専用。Live（chat / agent / draft / Scribe / rewrite / dictation cleanup / live summary・translate / Quick Translate）も gateway 上の会社キーを既定とする。会社の live / batch キー・プロジェクトを分離し、Mac・バイナリ・共有 Keychain に会社キーを配らない。BYOK とサブスク委譲はレーンごとの任意の上書きで、選択時は gateway を経由しない。**BYOK・サブスク委譲を Batch に使わない。** **【2026-10-15 オーナー決定｜#1101 D12 / #1144】** `form_decide`（Jev フォーム判断）も relay 上の会社 TypeSafe キー（Worker secret `TYPESAFE_API_KEY`）を既定とし、ユーザー自身の TypeSafe キー（Keychain `jev-forms-byok`）は任意の上書き（保存時は relay を経由せず TypeSafe 直結）。
   **【#782 を #1101 D1 で改定】** 会議ライブ要約・翻訳と Quick Translate も gateway 既定へ移す。旧「ユーザー資格情報のみ・会社キーへのフォールバックなし」は移行前の実装。rewrite は OpenAI `gpt-6-luna` / reasoning `low` / `service_tier: "priority"`、dictation cleanup は Groq の会社キー。サーバーが lane ごとに model・effort・出力上限を決め、クライアントに選ばせない。
   **【2026-10-13 オーナー決定】Batch lane のモデルを Anthropic Haiku から OpenAI `gpt-6-luna`（OpenAI Batch API）へ移す。** classify / summarize / about_profile / daily_activity_summary が対象。中継のサーバー側会社キー（`OPENAI_API_KEY`）で動かし、ユーザーの BYOK・サブスク委譲は使わない（プロバイダが変わるだけで不変条件5は同じ）。【2026-10-16 #1157】Morning Brief も luna へ移した。【2026-10-17 オーナー決定】Evening Wrap も Morning Brief と同じ Batch lane（`evening_wrap` purpose、luna）で最大3行の根拠付きの文を足す（FR-EB-02 改定）。`BATCH_PROVIDER=anthropic` で旧 Haiku / Sonnet に戻せる。詳細は docs/batch-relay-design.md §4.4
   **【2026-10-14 #1101 D10｜2026-10-15 オーナー改定｜OpenAI のクレジットを先に使う】** OpenAI モデルを使うレーン（live と Batch の両方）は、まず **OpenAI 直結**（会社の OpenAI クレジット）を第一経路にする。Azure OpenAI（スタートアップ向けクレジット）は、abuse monitoring 免除の承認後に OpenAI 直結の自動フォールバックとして加え、OpenAI のクレジットが尽きたら第一経路を Azure に切り替える。自動切り替えは最初のバイトより前の 429・5xx・タイムアウト・未デプロイのみで、ストリーム開始後は再試行しない。切り替え順はサーバーの lane 設定で、アプリのリリースを要しない。どちらも会社キーで、不変条件5の向き（BYOK・委譲を Batch に使わない）は変わらない。Azure の免除が承認されるまで Azure に実ユーザーの通信を流さない（D11）
6. **人間UIとAI API（MCP/CLI）は完全対称。** 新機能はUIとAPI両方から呼べる形で設計する。AI経由の操作にも同じL1/L2/L3を適用
7. **端末の secrets（OAuthトークン・BYOKキー・アカウントの refresh token）はKeychain以外に保存しない。** 平文ファイル・DB・ログへの書き出し禁止
   **【2026-08-13 明示的例外｜ライセンストークン】** FR-BIL-08 の署名済みライセンストークン（`v1.<payload>.<sig>`）は `billing.json`（app-data、平文）に置く。これは**秘密ではない**: Ed25519署名済みで改竄不能、約24時間で失効する。payload の device id は **この Mac のハードウェアから毎回計算する匿名 id**（`IOPlatformUUID` をアプリ固有の値と合わせて SHA-256 にかけたもの。`shogun_license::hardware_device_id`）と照合するので、ファイルを別の Mac にコピーしても通らない【2026-10-03 #777 で修正。それまでは照合先の id も同じファイルから読んでいたため、コピーで通っていた】。移行のため、この束縛より前の形式のトークンは、ビルド時刻 + 14 日より前（遅くとも 2027-01-01 まで）に発行されたものに限り受け付ける。CLI/MCP/RESTの3面がKeychainに触れずプラン状態を読めることが設計上の要件（`shogun_mcp::plan_source`）。【2026-10-20 監査 M7】ただし平文ファイルの改竄で延長されないよう、3面は Keychain のトライアル開始時刻（`trial-started-at`）と時刻の最高水位（`time-high-water-mark`）をダイアログ無しで読めるときは読み、開始時刻は早い方・最高水位は遅い方を使う（読めなければファイルどおり）。トライアルの終了は署名済みトライアルトークンの `period_end` からだけ延びうる（署名の無い値は早めることにだけ使う）。**ライセンスキー本体**（`shogun-XXXX-…`、APIのbearer）は引き続きKeychainのみ。Batch relay がこのトークンをbearerとして提示する（`billing::relay_bearer` → `shogun_mcp::trial_relay`）。※ASR mint は #1101 D2 で再導入する目標仕様（現行実装では削除済み）
   **【2026-10-01 追記｜#763 トライアルの中継トークン】** 7日トライアル中はライセンスが無いので、ライセンスAPI が端末ごとに発行する `plan: "trial"` のトークンを `trial_relay.json`（app-data、平文）に置き、Batch relay の bearer にする。同じ理由で秘密ではない（署名済み・device バインド・約24時間で失効、`exp` はトライアル終了を超えない）。`shogun_license::verify` は `trial` を拒むのでプランは付与しない（トライアル判定は引き続き `onboarding.json` のローカル時計）。中継への提示以外に使わない  
   **【2026-10-06 決定｜#963】stdio の Memory API（`shogun-mcp`）はトークンを使わず、アプリごとの「接続許可」で通す。** 他アプリの設定ファイルに書くのは `shogun-mcp --client <id>` だけで、秘密の値は無い。許可（アプリ・`read`/`full`・作成日時）はトークン照合用データと同じ Keychain 項目（`memory-api-tokens` の `grants`）に置き、起動時と毎回の呼び出しで確かめる。作成と権限の拡大はアプリ内（Settings の Add to … / Connect）だけで、API 面は一覧のみ。トークンをクリップボード・`launchctl setenv`・シェル設定・Claude Desktop の設定ファイルへ通す手順は廃止した（同じ Mac の他プログラムから読めた上、Mac の外へも漏れ得たため）。同じユーザー権限のプログラムが `--client` を名乗れば許可済みの範囲を使える点は、トークン時代と同等として受け入れ、起動元アプリの署名確認を第2段階で入れる（`shogun_mcp::launcher_check`。確認表は実機で Team ID と起動経路を確かめるまで空）。【2026-10-20 監査 M5】`--client` も有効なトークンも無い stdio の起動は、接続もトークンも一つも無い状態（新規・Delete everything 後）でも拒否する（旧「何も発行されていない間はプロセスを信頼して full」を廃止）。#963 以前のエントリはトークン付きなので引き続き動き、トークンも `--client` も無いエントリは Settings の Add to … か自動接続で書き直す。モデルを呼ぶツール（`translate.text`・`translate.reply_draft`・`memory.search` の AI モード・`skills.run`・`forms.plan`）は接続・トークンごとに毎分30回まで（`shogun_mcp::rate_limit`）。`shogun-mcp` が Keychain から自分でトークンを読む方式は、自己認証になって意味が無いので採らない。REST / CLI のトークンは従来どおり
   **【2026-10-14 オーナー決定｜#963 の改定】アプリが動き出したら、Mac で見つけた AI アプリ（Claude Code・Codex CLI と同じ設定を読む ChatGPT desktop / Codex アプリ・Cursor・Claude Desktop）に stdio の Memory API（`shogun-mcp --client <id>`）をクリック無しで自動で入れる。** 自動で作る接続は `read` のみで、`full` への引き上げは従来どおり Settings でユーザーが選ぶ。#963 の「接続の作成はアプリ内の Add to … クリックだけ」をこの範囲で改める（API 面から接続を作れない・広げられない点は変わらない）。条件は Memory API が On・プランが Memory API を含む（Pro / トライアル）・サーバーが同梱されアプリが Applications にある（開発ビルドのパスは書かない、#744）の全部。既に接続済みのアプリと、ユーザーが Remove / Disconnect したアプリ（`memory_api.json` の `auto_connect_declined`。Delete everything も含む）は自動では二度と追加しない。Settings の「Connect new AI apps automatically」（既定 On）で止められる。判定は `shogun_mcp::client_install::auto_connect_plan`、起動後・30分ごと・Memory API や自動接続を On にしたとき・プラン変更時に走る。**Memory API の既定**: `memory_api.json` が無い Mac（新規インストール、と一度も設定を保存していない既存ユーザー）は初回起動で `enabled: true` の設定ファイルを書く。既に `enabled: false` のファイルがある Mac は Off のまま。**【2026-10-20 オーナー決定｜監査 F4 の緩和】自動で作った接続（クリック無し）は、ユーザーがアプリ内で許可するまで会議メモ（文字起こし・議事録・話者・会議の検索行）と Visual recall（保存画面と、通常メモリに残す OCR の `screen_text`）を読めない。** 自動接続のそれ以外（`read`・クリック無し）は変えない。印は Keychain の `grants` の各接続の `sensitive_sources`（`withheld` / `allowed`）で、自動接続は `withheld`、Add to … / Connect で新しく作る接続は従来どおり `allowed`。印の無い既存の接続は、自動接続を出した 2026-10-14（UTC）以降に作られた `read` を `withheld` と読む（クリックした証拠が残っておらず、`memory_api.json` の `auto_connected` は平文の表示用なので根拠にしない。安全側）。それより前の接続と `full` は `allowed`。許可はアプリ内だけ（Settings の接続ごとの「Include meetings and screen recall」、または `full` への引き上げ）で、MCP/REST/CLI は一覧（`api.tokens.list` の `sensitive_sources`）と取り消し方向（`api.connections.restrict`、L1）だけ。判定は `shogun_mcp::source_policy` の1か所（全ツールの網羅表）で、`withheld` のセッションは会議・画面のツールと、デスクトップのモデルが記憶全体を読むツール（`skills.run` 等）を `source_not_allowed` で拒否し、検索・イベント・セッション・コンテキスト・根拠の抜粋からは返す前にデータ層（`DbBackend` / `Db`）で除く。REST/CLI のトークンは対象外。派生した状態行・期間要約は元の出典まで遡って除かない（`docs/memory-api-mcp.md`）

- **【2026-10-20 オーナー決定】既定 On に変更:** 会議ノート（`enabled`）とマイクだけの会議検知（`allow_mic_only_detect`）、Visual recall、音声入力と個人辞書の音声認識への共有、Web fetch（`web_fetch.json` が無いときは同意済み扱い）、起動音。FR-MT-01 の「既定 Off」を上書きする。設定を一度保存した Mac（`false` が書かれている）はそのまま Off。封印済みの設定を外から消した・壊した場合は従来どおり狭い側（Off）に倒す。
- **【2026-10-12 オーナー決定｜#1066 B】派生メモリ処理（consolidation・期間要約）は夜間ではなく毎時の Batch で行う。** 経路は引き続き Select KK キー（relay）だけで、サブスク委譲・BYOK を使わない（不変条件5は不変）。Morning Brief・Warm→Cold 降格・30日パターン分布は夜間サイクルに残す。新しい素材が無い時間・Batch 未提供・メモリロック中は何も送らない。マイグレーションは不要（既存の `dream_cycles` 台帳を `YYYYMMDD-hHH` の窓 ID で再利用）。

**【#1101 D1 / D6 サーバー側 secrets】** 会社の provider キーと connector OAuth client secret はサーバーの secrets store のみ。OAuth broker は交換・更新のみ行い、ユーザーの token・code を保存せず、そのまま Mac に返す。アカウント認証サーバーには既存 #148 設計どおりセッション・認証コードのハッシュのみを置く。  
**【2026-10-14 オーナー決定｜D6 の例外】Google の「デスクトップ アプリ」型 OAuth client の secret はリリースビルドに埋め込む（`SHOGUN_GOOGLE_CLIENT_SECRET`、release.yml）。** Google 自身がインストール型アプリの secret は秘密として扱わないと定めており、同意画面と PKCE verifier なしでは何も得られない。broker にすると全ユーザーの Gmail refresh token が会社サーバーを通り（約1時間ごとの更新も含む）、サーバー障害で全員の連携が止まり、制限付きスコープの審査範囲にサーバーが入るため採らない。Slack・GitHub・Linear・Notion はもともと secret を使わない（PKCE / device flow / DCR）ので broker は作らない。トークンは引き続き Mac の Keychain のみ。secret の差し替えはアプリ更新で行う。

## 技術スタック（確定。勝手に変更しない）

- **アプリ**: Tauri v2 / Rust backend / React + TypeScript frontend
- **対応環境**: macOS 14 Sonoma以上 / Apple Silicon (arm64) のみ。ノッチ非搭載Mac・外部ディスプレイはメニューバー中央の擬似ノッチパネルで対応
- **Notchパネル**: NSPanel（objc2）、`.nonactivatingPanel` + `.canJoinAllSpaces` + `.fullScreenAuxiliary`
- **DB**: SQLite（rusqlite）+ sqlite-vec、WALモード、FTS5 trigram。マイグレーションはバージョン管理必須（refinery）
- **埋め込み**: **【2026-08-23 決定 / issue #237】配布ビルドはローカル埋め込みモデルもONNX Runtimeも同梱せず、ダウンロードもしない。** 検索は字面一致（FTS5 trigram）。クラウドembedding APIも使わない（不変条件3）。ローカル埋め込みは開発者専用の明示的オプトイン（`--features local-embedder`）で、出荷ビルドに入れる変更を書かない。既存DBのベクトルは破壊しない（参照されないだけ）。詳細は `docs/embedding-model-setup.md` / `docs/release-signing-and-distribution.md` §4-a
- **macOSネイティブ**: AXUIElement / NSWorkspace / NSEventグローバルモニタ / security-framework（Keychain）
- **MCP**: Rust MCP SDK。クライアント（公式リモートMCPへ直接OAuth）とサーバー（Memory API）の両方
- **連携**: Gmail は Google REST API 直結、他の6サービスは公式リモートMCP。Composio の旧ライブラリと設定は互換・旧資格情報の管理用のみで、製品の接続・送信経路では使わない
- **Agent lane の資格情報**: gateway の会社キーが既定（#1101）。**【2026-10-21 オーナー決定】** SHOGUN（会社キーの hosted lane）をアプリ内に選択 UI の無い固定の既定とする（`LlmSettings::default()` が `shogun`。#1318 でモデルカードは削除済みで戻さない）。送信先と非保存の開示（api.shogunaios.com → OpenAI、本文を保存しない）は Settings の Privacy & Security に置く。保存済みの BYOK・サブスク委譲の選択は上書きしない。CLI/MCP/REST の `agent.source.set`（`shogun` を含む）と `agent.source.get` はそのまま。任意の BYOK / サブスク委譲は既存経路を維持する。委譲はユーザーがログイン済みの公式 CLI を opt-in で起動する方式のみ。**他アプリの資格情報ファイル・Keychain エントリの読み取り、ベンダーのコンシューマ向け OAuth の自前実装は禁止。**
- **BYOK**: v1はAnthropic + OpenAI互換（プロバイダ抽象化層あり）
- **課金**: Stripe / **配布**: Developer ID + notarization（App Store不使用）/ **更新**: Tauri updater
- 判断記録: ストレージにPGLiteを使わない理由は docs/requirements-v1.0.md 付録B

## リポジトリ構成（pnpm workspace + Cargo workspace 同居）

```
crates/
  shogun-core/         # デーモン: キャプチャ、DB、context cache、イベントバス、LLM/HTTP の唯一の egress
  shogun-memory/       # スキーマ、マイグレーション（src/migrations/V<n>__*.sql）、3層メモリ、state tables、検索
  shogun-fusion/       # Context Fusion: f(state, screen_ctx, intent) → action
  shogun-agents/       # L1/L2/L3実行エンジン、プリセットエージェント、エンタイトルメント
  shogun-mcp/          # MCPクライアント＋サーバー、REST API、サービス別スコープ表
  shogun-integrations/ # 第1層コネクタアダプタ（公式リモートMCP）
  shogun-cli/          # shogunコマンド
  shogun-license/      # 署名済みライセンストークンの検証（FR-BIL-08/09）
  shogun-redact/       # DB書き込み・ログ経路の秘密マスキング（依存なし）
  shogun-microphone/   # マイク検出・検証（全サーフェス共通）
  shogun-mark/         # ロゴ（Dock/メニューバーのアイコン生成）
  spike-harness/       # SLO計測ハーネス（Phase 0 から継続）
  memory-bench/        # 検索 p95 ベンチ（NFR-SLO-04）
  pii-bench/           # PII/secret 残存ベンチ
  ax-noise-bench/      # AX キャプチャのノイズ・トークン・事実保持ベンチ（#302）
apps/
  desktop/             # Tauriアプリ（Notchパネル、Full UI、設定）。src-tauri（crate 名 shogun-desktop-spike）は macOS/arm64 でしか build できない
  api/                 # Batch relay（Hono）。ライセンストークン認証で Anthropic Batch API を中継。docs/batch-relay-design.md
  website/             # マーケサイト（本ファイルの対象外）
  docs/                # 公開ドキュメント（Mintlify。本ファイルの対象外）
packages/              # TS共有パッケージ（tokens / ui / config など。desktop は @shogun-ai/tokens の build 生成物を import する）
scripts/               # CI と同じ不変条件ガード（check-*.py / check-feature-matrix.sh / check-manual-macos.rb）
docs/                  # 要件・仕様・判断記録。docs/migrations/ にマイグレーションごとのロールバック手順
```

## プラン構成（要件詳細は docs/requirements-v1.0.md §課金）

- Freeプランなし。7日間フルトライアル（Pro相当）→ Standard / Pro。**【2026-10-20 オーナー決定】トライアルはカード必須。** Stripe Checkout でカードを登録して始まり（`trial_period_days: 7`）、終了前に解約しなければ選んだプランへ自動移行する。カード無しの無料トライアルはサーバーの `TRIAL_REQUIRES_CARD=0` でだけ戻せる（FR-BIL-06）。「No card needed」などカード不要の文言をどの面にも書かない
- **Standard**: キャプチャ＋メモリ＋検索＋Notch UI＋第1層連携（読み取り）＋Dream Cycle＋Morning Brief＋エージェント実行（L1/L2/L3: チャット・アクション・ドラフト・Scribe）。#1101 で会社キーを既定とし BYOK 不要へ移す。会社負担の live 利用にもサーバー側の予算・レート・同時実行制限を適用する（**【2026-10-21 オーナー承認｜D9】** trial $1/日・Standard $3/日/$40/月・Pro $6/日/$80/月。計量の主体は relay の他の予算と同じ `meterSubject`（`apps/api/src/auth.ts`）で、トライアルはアカウント（`acc` の無い端末のみのトライアルはそのトライアルライセンス）、有償プランはトークンに `acc` があってもライセンス単位）。
- **Pro**: ＋Memory API（MCP/CLI/REST）＋Gmail実送信の解放＋優先サポート
- **【2026-09-24 決定】エージェント実行（チャット・コンテキストアクション・ドラフト・Scribe）を Pro から Standard へ移す。** 実機で Standard を買った直後に「Chat is part of Pro」で全部ロックされるのは、有償プランとして痛すぎる（オーナー判断）。判定は `crates/shogun-agents/src/entitlement.rs` の 1 か所（`Plan::Standard` → `agent_execution: true`）。Pro に残るのは Memory API と Gmail 実送信解放のみ。要件 §課金 / FR-AG-06 / FR-AG-17 / FR-BIL-03 / 付録 ADR-003 を同日改定
- **【2026-10-05 決定｜#940】トライアル満了後（有効な有償プランなし。ライセンスの既存の猶予期間中は有効扱い）は新規キャプチャを止め、メモリへのアクセスをロックする。** ロック対象はローカルのキーワード検索と AI 検索、閲覧と読み取り専用ビュー（Today・タイムライン・Visual recall・会議履歴・メモリに対するチャット・コンテキストアクション・Morning Brief・Dream）、エクスポート。保存済みのデータは削除せず、プランを買えば再起動なしで戻る。到達可能に残すのはプラン購入とライセンス入力・設定とプライバシー操作・明示的なデータ削除・キャプチャ除外・オンボーディングだけ。判定は `crates/shogun-agents/src/entitlement.rs` の `local_capture` / `memory_access`（プラン判定の1か所）で、デスクトップのコマンド・MCP・REST・CLI の全入口でプロバイダ呼び出しや結果の返却より前に `plan_required` で拒否する。ロックの裏にキーワード検索のフォールバックを置かない。UI は「Get a plan to continue」と「View plans」（既存の購入フロー）を出し、空の結果・故障表示・「AI キーを追加」で代用しない。FR-BIL-05 の「満了後30日は閲覧・検索・エクスポート可」は廃止（要件 FR-BIL-05 を同日改定）。トライアル中と各有償プランの挙動・API アクセス規則は変えない
- **【2026-10-13 #148 を #1101 D7 で改定】** アカウントは課金・プラン・トライアル・デバイス・内容を含まない使用量/制限/ブロック管理用。アプリの **Sign in** はシステムブラウザの `https://shogunai.si/app/signin` を開き、Google または同期型パスキーで認証する。PKCE + state + 60秒・単回使用のハッシュ化コードで loopback に戻る。Google は別 OAuth client（`openid email profile`）、識別子は `sub`、メール一致だけでは連携しない。refresh token は Keychain `account-session`、15分の access token はメモリのみ。既存のライセンスキー運用・14日オフライン猶予は維持し、紐付けは任意。新規トライアルはアカウントと端末に各1回。作成・認証は UI、MCP/REST/CLI は状態・プラン・デバイス・使用量・上限の読み取り。詳細は `docs/account-passkey-design.md`。
- **【2026-10-07 更新】Gmail 読み取りは直接接続で Standard に含める。** 実送信の解放（draft-only OFF）は Pro/trial を保持。旧 entitlement field `composio_send_unlock` はこの権限の互換名であり、Composio 呼び出しを意味しない。provider OAuth は全プラン共通で必要。
- プラン判定はRustコア側で行う。webview側のゲーティングだけに頼らない
- **【2026-10-12 決定】運営用ライセンス（`docs/ops-licenses.md`）は Free プランではない。** 社内・テスター・投資家・取材・提携先に、Stripe を通さず Pro を渡す枠（オーナー判断）。非公開のコード1つ、総数30件・1件365日・1件ずつ取り消し可・1 IP 1時間10件まで。LP・料金ページ・アプリの文言に出さず、公開の無料枠へ広げる変更を書かない
- **会議ノート（§6.16 FR-MT群）はトライアル含む全プランで使える。** #1101 D2 で会社キーから短命 grant を発行する方式へ移し、ユーザーの音声認識キーは任意とする。会議中ノートと手打ちの保存はキー無しでも動く。Memory API 経由の参照（FR-MT-22）は Pro。
- **【解消済み 2026-08-08】** design-system ブランチとの「Free / $0 プラン」分岐は、オーナー判断（**Free廃止・全員課金**、2026-07-26）どおり本ファイルを正としてマージ済み。LP（`apps/website` の pricing）も Standard $49 / Pro $99（年額、7日フルトライアル）へ全ロケール更新済み。Freeプランを再導入する変更を書かない

## データモデルの原則

- **event log と state tables を分離する。** state tables = `people` / `projects` / `commitments` / `open_loops`
- 全stateレコードに必須: 根拠イベントへの参照（provenance）＋ **confidence**。低confidenceの状態をContext Fusionが事実として生成物に混ぜてはならない（「〜の可能性」として弱く渡す）
- **2026-10-02 owner decision:** Local extracted candidates remain below 0.5 and off dashboard/context actions and briefs until scheduled AI Batch review. Unavailable Batch leaves them pending; capture/search still work. Distinct live Batch-reviewed quotes may strengthen/refresh a memory (automatic confidence <0.8); repeated sightings cannot. User corrections remain authoritative. Date/status edits preserve links; wording edits cannot claim old quotes as proof.
- **2026-10-14 owner decision (revised the same day, Goldfish parity):** commitments and open loops are written only by the loop consolidator. The hourly review (`/v1/review`) reports typed signals (commitment / blocker / question / decision / followup, each with its event and exact quote) into the append-only `loop_signals` table (V83); signals never close and are never shown. After a review that produced signals, at most hourly and 8 times a local day, one `loops_consolidate` call sees the open list, the signals it has not read yet (newest first, ≤24 source excerpts: this hour's always go in and an older backlog fills the rest) and the recently closed items, and returns at most 6 items plus `resolved` and `duplicates`. Code stores an item only when its quote is an exact span of an event the call was shown and its confidence is ≥0.84; that confidence is stored and shown as fact (no "possibly"). `resolved` needs a quote; a duplicate folds into its target (`merged_into`, provenance copied, nothing deleted). Rows retire on a timer since last seen — same_day 3 days, short 14 days, project 45 days, unconfirmed rows 14 days (2026-10-19, was 1/3/30/3) — unless due later, snoozed, or pinned/edited by the user; no source is exempt and aged rows are kept (`aged_at`). Lists show open, unmerged, unaged rows, same-day first then most recent. Rows earlier builds wrote straight from the review (base 0.85, no horizon) drop below 0.5 (off the dashboard; never deleted; pinned/edited untouched) in a one-time maintenance step, and those seen in the last 2 days are re-seeded as signals (their stored quote and event) so the consolidator re-lists the real ones in place at ≥0.84; `lift_legacy_reviewed_confidence` is removed. Calibration details: issue #1110.
- **【2026-10-13 オーナー決定】** Batch による引き上げにはモデルの引用（元の文に含まれる quote）が必須。説明文が一致しただけでは引き上げない。2026-10-02 より前の「何度か見た」だけで 0.5 以上になった約束・未解決事項は、ローカル保守で 0.4 に戻す（ピン留め・Batch 確認済みの引用あり・モデルが書いた説明文の行は対象外。削除はしない）。Today の「今後の予定」は Google カレンダーの予定（`calendar_occurrences`）を表示し、文章から拾った期限付きの約束は「期限の約束」にだけ出す
- **【2026-10-14 オーナー決定｜精度】** チャットのモデルには端末内の記憶ツール（search_memory / get_event / list_commitments / list_open_loops / find_person / find_project / calendar_range。読み取り専用・L1・メモリロック #940 とキャプチャ除外に従う）を常に渡し、モデルが自分で記憶を引けるようにする（ユーザー自身のプラン枠の呼び出しが増えることは了承済み）。サブスク委譲では SHOGUN の指示をシステム指示として渡す（Claude Code `--append-system-prompt-file`、Codex `-c developer_instructions`。`--system-prompt` での置き換えは引き続き禁止）。会話履歴・根拠の文字数予算と思考量は変えない。AI 未確認のメール分類の推測から L3 の返信ボタンを出さない。日本語2文字の検索のための追加の索引は可（マイグレーションは追加のみ）
- **【2026-10-19 オーナー決定｜Goldfish 並み】** 1回しか名前の出ない人物・プロジェクト（レビューが作った行で言及 1 セッション）は 60 日残す（2026-10-14 の 14 日を上書き）。削除するのは言及数 < 2 かつページ未作成かつ最終言及が 60 日より前の行だけで、ピン留め・ユーザー編集・約束/未解決事項が指す行は消さない。言及は「1 アプリ × 1 時間」のセッション単位で数える。ターミナルは既定でキャプチャする（既定の除外から外す。秘密情報のマスキングは従来どおり。外したいユーザーはアプリ単位の除外として Settings / API から追加する）。フォーム記入はターミナルを読まず記入しない
- スキーマはspatial-ready: `window_pose` / `gaze_target` / `dwell_ms` / `display_id` / `window_bounds` のカラム余地を最初から確保
- 3層メモリ: Hot（24h、RAM）/ Warm（30日、DB）/ Cold（全履歴、int8量子化＋期間パーティション）。**通常のベクトル検索はWarm層のみを対象にする**（sqlite-vecは総当たりのため）
- 後方互換を破るマイグレーションを書かない。メモリは年単位で生きるデータ

## SLO（実装のアクセプタンス基準。計測コードを同梱する）

| 項目 | 上限 |
|---|---|
| Notch展開（Idle→Expanded） | 150ms |
| コンテキストアクションボタン提示 | 150ms |
| アクション実行→初トークン | 1s（ストリーミング必須）※ |
| ローカル検索 | 500ms |
| context cache更新（フォーカス切替） | 300ms |
| アイドル時CPU（SHOGUN自身） | 5%（1分平均） |

※【2026-09-25 決定｜#388 / #485】初トークンは**最初に見える1文字**で測る（後で捨てる前置きも含む。#533 の実装どおり）。最終回答の最初の1文字までの時間も副指標として記録する。Codex 委譲は `codex exec --json` が一括応答でストリーミングできないため SLO-03 の対象外。定義は docs/requirements-v1.0.md NFR-SLO-03

- レイテンシに影響する変更はp50/p95を計測してからマージする
- 【2026-10-07 オーナー決定｜PR #978】Notch の open モーフは 200ms。#826 の 150ms 指定を更新した。展開 SLO の 150ms 上限は React の展開処理開始から最初の開いたフレームまでを測り、その後の 200ms CSS モーフは含めない。旧 100ms は Phase 0 の履歴値。
- context cacheは「押してから収集」禁止。常時プリアセンブル

## 開発フェーズ

- **Phase 0（完了・2026-07 に Go 判定）**: ノッチUIスパイク。物理ノッチ実機で展開100ms・cache300ms+CPU5%・ホバー誤発火の SLO 合格を確認しノッチ方式で Go（判定の記録は `docs/phase1-implementation-plan.md` §0、実測台帳は `docs/phase0-findings.md`）。仕様 `docs/notch-ui-prototype-spec.md` と指示書 `docs/phase0-dev-instructions.md` は履歴として残す。常駐24hソークのみ M2 ゲートへ持ち越し
- **Phase 1（v1、今ここ）**: Notch UI本実装 → キャプチャ＋メモリ＋state tables → Context Fusion＋L1/L2エージェント → 第1層MCP連携（Wave 1: Gmail+Google Calendar → Wave 2: Slack → Wave 3: Notion+GitHub+Linear）→ 課金＋トライアル。作業指示は `docs/phase1-implementation-plan.md`（M1〜M5 と WP 単位）、進捗と実機持ち越しは `docs/phase1-findings.md`。Linux で閉じる純ロジック層はほぼ実装済みで、残りは macOS 実機・実 secret・UI レビューが入口の作業。公開リリースの P0/P1/P2 は `docs/public-release-remaining-work.md`
- 開発セッションは Linux コンテナで走ることがある。**macOS コード（`apps/desktop/src-tauri`）はローカルでコンパイルできない**前提で、純ロジックは Linux でテストできるクレートに置き、macOS アダプタは `macos.yml`（macos-14）で compile+clippy、実測はオーナーの実機で行い結果を貼ってもらう
- 会議ノート（検知＋オンデバイスASR＋Recap）はv1へ前倒し（Issue #7 / `docs/meeting-notes-ui-design.md`）。ただし**音声ファイル・録音の保存は恒久的に対象外**（不変条件2）。実装順はM1〜M5（同書§7）
- v1に含めない: ナレッジグラフ/同期/メタメモリ（v2）、Computer Use/visionOS（Phase 3）。頼まれてもv1スコープに足さず、docs/requirements-v1.0.md のスコープ表を根拠に確認を取る
- 2026-10-09 のオーナー承認による限定例外: Scribe から明示的に選択したフォームの空のテキスト欄だけを、別ウィンドウで人が確認して AXValue で記入する（`docs/features/form-mode.md`）。送信・クリック・決済・秘密情報・無人実行は含めず、一般の Computer Use は Phase 3 のまま。フォームの判断は TypeSafe Jev Decisions API が行い、一般チャットモデル・サブスク委譲に置き換えない（同日のオーナー訂正）。【2026-10-15 オーナー決定｜#1101 D12 / #1144】Jev は relay（`POST /v1/ai/form_decide`、Batch と同じ licence / trial bearer）の会社 TypeSafe キーで呼ぶのが既定。ユーザー自身の専用 Keychain キーは任意の上書きで、保存されていれば TypeSafe へ直結する。relay はモデル `jev-1.13.0` を固定して付け、本文を保存・ログ出力しない。判断結果の解析としきい値は Mac 側のまま。【2026-10-10 オーナー決定】ネイティブのドロップダウン選択・チェックボックスのチェック・ラジオボタンの選択も対象に加える。操作は確認済みのコントロールと、そのドロップダウン自身のメニュー項目への AXPress だけ。テキストは確認済みの欄にフォーカスを移し、フォーカスが移ったことを確かめてから AXValue で書く（WebKit はフォーカス中の欄に書き込むため）。チェックを外す・既に選ばれた選択を変えることはしない。送信・次へ・その他のボタンやリンク・決済・秘密情報・規約や同意のチェックボックスは含めない。独自実装のドロップダウン（AXPopUpButton でないもの）は対象外。【2026-10-10 オーナー決定・追記】Scribe に「fill this form」等と入力したこと自体をそのフォームへの人の確認とみなし、別ウィンドウを開かずに直接記入する（進行と結果はノッチに表示）。別ウィンドウでの確認は Memory API からの依頼にだけ残す。Jev に届かないとき（relay が使えず自分のキーも無い・プラン外・上限到達）は記入しない（一般チャットモデルの判断に置き換えない）。【2026-10-14 オーナー決定】右 ⌥ の1回押しはフォーカス中の欄への下書きを維持する。空欄にいるだけではフォーム全体の記入を許可しない。フォーム全体は Scribe を開いて明示的に依頼する。【2026-10-14 オーナー訂正】Jev はフォーム記入専用で、通常の Scribe 編集の前に置かない。Scribe のモデルが `fill_form` ツールを持ち、依頼がフォーム全体の記入なら自分で呼ぶ。記入の前に Jev が入力された指示だけ（画面の文字は渡さない）からフォーム記入の依頼かを確かめる。スコープボタンと固定のトリガー句は不要。安全なラベルの取得後、メモリ検索とドロップダウンの読取りを並行する。記入後10秒以内の右 ⌥ は「元に戻す」で、SHOGUN が書いた欄のうち今も SHOGUN の値のままの欄だけを空に戻す（ユーザーが変えた欄・ラジオ・空のプレースホルダへの戻しは触らない）。自分が付けたチェックを外すのはこの元に戻すだけの例外。Esc で記入を途中で止める。明示的な記入が開くのはユーザー自身が足した除外（アプリ・タイトル・サイト）だけで、既定の除外（パスワードマネージャー・macOS の認証ダイアログ・プライベートウィンドウ・ターミナル・SHOGUN 自身）では Scribe からも記入しない。Jev へ送るのは秘密情報を除いた空欄のメタデータと候補値・その根拠の抜粋だけで、送信ごとにトレーサビリティを記録する（既存どおり）。

## 連携実装ルール

- 第1層 = provider 直結（Gmail は REST、他サービスは公式リモートMCP）。OAuthはユーザー→サービス直接、トークンはKeychain
- **【2026-10-07 オーナー判断｜#713】Gmail の読み取り・ドラフト・送信・ラベル操作は Google Gmail REST API に直接接続する。** 2026-07 の全面 Composio 例外を撤回する。Desktop OAuth + PKCE + offline access を使い、access/refresh tokens は `gmail-tokenset` Keychain のみ。`gmail.modify` を明示的に要求し、永久削除・ゴミ箱・一括送信は公開しない。Composio プロジェクトキー・user ID・バックエンドは不要。
  - provider の OAuth 同意が接続の権限。旧 Composio の同意・APIキー・設定変更は直接 Gmail の接続状態や送信権限を変更しない。
  - Gmail 読み取りは Standard と有効な trial。実送信は従来の Pro/trial 権限を保持し、必ず L3 専用確認。`gmail-policy.json` の draft-only は既定 ON。API は ON に締めるだけ、OFF は人間がアプリ Settings で選ぶ。
  - 接続確定前に bounded mailbox read を行う。空の受信箱も成功。request/response 本文はログに残さず、Google への各 HTTP 呼び出しは送信前に fail-closed トレーサビリティを記録する。
  - disconnect・OAuth client 変更・並行 Connect に負けたフローは grant を保存しない。曖昧な send 失敗は自動再送しない。旧 Composio 経路の pending approval は経路変更して実行せず、新しい proposal を要求する。
  - public release は Google OAuth verification（restricted scope の要件に応じた assessment を含む）と、製品側 OAuth client 配布を別途確認する。ローカル OAuth helper と fixture test を公開版の認証証拠にしない。
- Slack: WS管理者承認で接続不可の場合、ドラフト生成→クリップボードコピーにフォールバック
- Composio経由の連携はトレーサビリティ画面に「第三者経由」を明示するUIを伴うこと

## コード規約

- Rust: clippy warnings deny。`unwrap()` はテスト以外禁止（キャプチャデーモンは絶対に落とさない）。ルート `Cargo.toml` の `[workspace.lints]` が `clippy::all` / `unwrap_used` / rustc `unused` を deny にしているので、`-D warnings` を手で足す必要はない。`expect_used` は意図的に warn（テストの `expect` は許容）
- 外部への HTTP/WS 送信は `shogun-core` の共通クライアント層だけが持つ（FR-TR-03）。他クレートに reqwest 等を足さない。`scripts/check-http-egress.py` が `cargo metadata` で検査する
- 秘密値は `Secret` 型で持ち、`.expose(` は許可リストのファイル以外で呼ばない（`scripts/check-secret-exposure.py`）
- マイグレーションは `crates/shogun-memory/src/migrations/V<n>__<name>.sql`（refinery）。**追加のみ**で DROP / RENAME / ALTER COLUMN を書かない（`scripts/check-migrations.py` が落とす）。ロールバック手順は `docs/migrations/V<n>-rollback.md` に置き、SQL 先頭コメントから参照する
- **【2026-09-30 決定 / #800】マイグレーションは main に入った時点で凍結。** コメント1文字の編集・改名・番号の再利用も禁止（refinery がチェックサム不一致で DivergentVersion を返し、アップグレードした全ユーザーのメモリが開かなくなる）。直したいときは新しい `V<max+1>` を足す。未マージの自分のマイグレーションは自由に直してよい。番号が main と衝突したら自分の側を振り直す。検査は `MIGRATION SAFETY — MUST PASS`（`migration-safety.yml`。main とリリースタグを git で比較するだけで台帳は無い）。スキーマの移行を実行するのはデスクトップアプリだけ（移行前スナップショットの後）。MCP/REST/CLI は古いスキーマを見たら移行せずに `memory_update_pending` を返す
- **【2026-10-20 決定】各マイグレーションは自分の履歴行と同じ1トランザクションで適用する**（`shogun_memory::migrate_and_check`。refinery 既定の非グループ実行は SQL と履歴行を別々にコミットし、その間の強制終了で次回以降の起動が毎回失敗していた）。CI の `MIGRATION UPGRADE — every version upgrades`（`migration-safety.yml`）で `upgrade_lineage::every_version_upgrades_atomically_to_latest` が V1〜最新の全バージョンについて、行を入れたストアで次のマイグレーションを途中失敗させ、変化が残らないこと・次回起動で最新まで上がること・行が減らず外部キーが壊れないことを検査する。新しいマイグレーションはこのテストを通らなければマージしない
- クラッシュ耐性優先: DB書き込みはWAL＋トランザクション。電源断でメモリを壊さない
- エラーはユーザーの作業を中断させない形で処理（Notchインジケータの色で通知）
- テレメトリ・ログにキャプチャ内容（ユーザーのテキスト）を含めない。デバッグログも同様。`println!`/`eprintln!` にユーザー由来の識別子を補間しない（`scripts/check-log-hygiene.py` が落とす）。画像・音声バイトを `fs::write` 等でファイルに書かない（`scripts/check-media-writes.py`）
- **匿名プロダクト分析は単一系統（2026-08-08 統合決定）**: PostHog オプトアウト方式（既定ON）。永続状態は `analytics.json` の `opt_out` のみで、オンボーディングのトグルも設定画面の Privacy & Security カードも**同じ状態**を読み書きする（`analytics_get_opt_out` / `analytics_set_opt_out`）。旧 `privacy.json`（opt-in方式, #28 Slice D）は廃止。分析イベントに機能カウント以外（キャプチャ内容・個人データ・キー）を載せない
- UI文言は英語（v1）。文言はコードから分離しi18n-readyに保つ（desktop は `apps/desktop/src/strings.ts` のカタログに集約する）
- UI文言・外部向けコピーを書く場合はSHOGUNブランドルール準拠: 競合名を出さない、技術スタック名を出さない、絵文字は⚔のみ、"AI-powered/revolutionary/second brain"禁止  
  **【2026-09-26 決定】接続先の表示（Memory API の装着先として Claude Code / Codex / Cursor / ChatGPT desktop / Claude Desktop 等の名前を出す）は例外。** 「どのアプリに SHOGUN の記憶を装着するか」を選ばせる UI・その手順を説明する docs に限り、相手アプリ名をそのまま出してよい（名前を伏せると装着先を選べない）。比較・優劣の表現はしない（「〜より良い」「〜の代わりに」等を書かない）。LP・マーケコピーでの競合名禁止は変わらない（Issue #400）  
  **【2026-10-03 決定｜#778】記憶の取り込み元（Import memory）の選択にも同じ例外を適用する。** 取り込み元ごとに書き出し用プロンプトの文面が違うため、相手アプリ名（ChatGPT / Claude / Cursor 等）を伏せると、ユーザーが自分の使っているものを選べない。条件は装着先と同じ（比較・優劣の表現をしない、LP・マーケコピーには広げない）

## CI コスト（private リポジトリ。枠切れは停止であって超過課金ではない）

- **Actions は従量。** このリポジトリは private なので、public リポジトリのような無料枠は無い。含まれる分数は Free 2,000分／Pro・Team 3,000分（月）
- **macOS は Linux の約10倍**（$0.062/分 対 $0.006/分）で、同じ月間枠から引かれる。実質の予算は「macOS 200〜300分/月」だと思うこと。2分の不要な macOS ジョブは枠を約20分食う
- **枠を使い切ると、支払い方法が無い限りジョブは停止する。** 超過課金に流れるのではなく止まる。しかも **macOS だけでなく Linux ジョブも含めて全部**止まる（2026-09-05 15:05 に実際に発生し、CI 全ジョブが3秒で failure になった。Actions API の実測で `macos.yml` 単体が約30時間で 2,407 課金分を使い、月枠3,000分を焼き切っていた。焼いたのは100% macOS で、Linux は巻き添え）。**切れたときの復旧手順（枠切れの見分け方・予算の足し方・金額の目安）は `docs/actions-budget.md`**

**【2026-10-17 オーナー決定】Actions 予算不足時は追加課金せず、同じリポジトリの PR に `run-ci-all` を付けて Mac mini で公開前チェック一式を手動実行する。** `macos.yml` 内だけで実行し、通常の Linux 自動 CI は残す。Rust feature matrix・全ガード・依存監査・frontend/relay/website の検証・使い捨て Postgres の E2E・凍結マイグレーション検査を省略しない。macOS 上の代替検証であることを記録し、未実行の Linux ジョブを成功に書き換えない。新しいリビジョンには新しいラベル操作が必要。`macOS ready to merge` 自体もホスト型枠で起動できない場合、同一リビジョンの Mac mini 本体と追加チェックがすべて成功した実行を公開判定の証拠とする。

**【2026-10-20 オーナー決定】Linux CI（`ci.yml` / `website.yml` / `migration-safety.yml`）はドラフト PR では回さず、Ready for review にした時点で全部回す。`cargo audit` は依存ファイル（`Cargo.lock` / `Cargo.toml` / `.cargo/`）が変わったときと毎日1回（`audit.yml`）だけ回す。**

**【2026-10-21 オーナー決定】リリースは `MIGRATION SAFETY — MUST PASS` と `MIGRATION UPGRADE — every version upgrades` の2つが通ったコミットからしか作らない。** 本番でユーザーのメモリが開けなくなる（DB ロック・移行の失敗）のを防ぐため。`release.yml` は、ビルドするコミットでこの2つのチェックが success であることを GitHub API で確かめ、無い・実行中・失敗なら署名の前に止まる。この2つを PR で赤のまま残さない、スキップさせない、マージ時に `macOS ready to merge` と同じく必ず確認する。

守ること:

1. **【絶対】Linux は自動で回す。macOS は誰かが頼んだときだけ回す。**（2026-09-10 オーナー決定 / PR #447・#460）
   - `ci.yml` / `website.yml` は Linux のみ（下の 2026-10-20 決定で Mac mini の Linux VM。予備の ubuntu-latest は $0.006/分）。**PR で自動的に回す**。ローカルで通してから push する（規則2）のは変わらないが、Linux を止めて浮く額は誤差で、代わりに失う検証の方が高い
   - **【2026-10-20 オーナー決定】Linux CI はオーナーの Mac mini 上の Linux VM（self-hosted、ラベル `shogun-linux`）で自動で回す。** GitHub ホストの Linux が1日約 $10（10月は19日で26,748分 ≈ $160）になったため。対象は `ci.yml` / `website.yml` / `migration-safety.yml` / `macos.yml` のゲート / `ci-autofix.yml` の読み取り専用ジョブ・毎日の `audit.yml`（2026-10-22 追加）で、全ジョブが同じ1つの `runs-on` 式を持つ。Mac mini が止まったらリポジトリ変数 **`CI_LINUX_RUNNER=hosted`** で全部が即座に `ubuntu-latest` に戻る。fork のコードは変数に関係なく常に `ubuntu-latest`（使い捨てでない VM に渡さない。skipped は必須チェックで合格扱いなので止めずに戻す）。VM のジョブは読み取り専用トークン・secrets 無し・Docker と sudo 無し（`sudo` / `Swatinem/rust-cache` / setup-node のキャッシュは `runner.environment == 'github-hosted'` のときだけ）。書き込みトークンを持つジョブ（`ci-autofix` の publish / comment）、`deploy*.yml`、`release.yml` は動かさない。手順・切り戻し・ディスクは `docs/mac-mini-linux-runner.md`
   - 【2026-10-13 オーナー決定】`macos.yml` の本体はオーナーの **Mac mini（self-hosted runner）** で回す（Actions の macOS 分を使わない。手順は `docs/mac-mini-runner.md`）。GitHub の macos-14（$0.062/分）は Mac mini が止まっているときの予備（`run-ci-hosted`）。Mac mini はこのリポジトリ自身のブランチの PR にしか走らせない（フォークのコードを渡さない）。**Mac を確保するジョブは `push` では絶対に走らせない。** 走るのは次の2つだけ:
     - PR に **`run-ci` ラベル**（Mac mini）か **`run-ci-hosted` ラベル**（GitHub の Mac）を付ける。**マージ直前に、そのリビジョンに対して1回**
     - Actions タブから **`workflow_dispatch`** で手動実行（PR もラベルも要らない。`runner` で `mac-mini` / `github` を選ぶ）
   - 仕掛けは `macos.yml` の中で完結している。ワークフロー自体は `pull_request` を受けるが、**Mac のジョブが `if` でラベルイベントを要求**し、同居する ubuntu の `macOS ready to merge` が「このリビジョンは未検証」を赤で報告する。**ワークフローのトリガーではなくジョブの `if` が支出を止めている** ので、macOS ジョブを足すときは必ず同じ条件を付ける
   - ラベルは「そのリビジョンへの1回の依頼」であって永続スイッチではない。push するたびにゲートが赤に戻るので、外して付け直す
   - **この規則は `scripts/check-manual-macos.rb` が機械的に検査する。** macos.yml に `push` が復活していないか、Mac の全ジョブ（Mac mini と予備）が決まったラベル条件を持っているか、Mac mini を macos.yml 以外で使っていないか、ci.yml / website.yml に macOS ランナーが紛れ込んでいないか、Linux が自動で回るままか、Linux の全ジョブが `CI_LINUX_RUNNER` で切り戻せる共通の `runs-on` 式（fork はホスト型）を持つか、self-hosted Linux のジョブが secrets・書き込みトークン・Docker・sudo に頼っていないか、ゲートが skipped を承認と誤らないかを見る。**ワークフローを触ったら必ず走らせること**（Ruby のみ。CI 分数を1分も使わない）
   - 【2026-10-13 オーナー決定】`release`（署名・公証・DMG）も Mac mini で回す。Mac mini のジョブは必ず `MACOSX_DEPLOYMENT_TARGET: "14.0"` を持つ（Mac mini の macOS はアプリの対応下限より新しく、無いと配布物が古い Mac で動かない）。`phase0-smoke` は使い捨て VM 前提（sudo で TCC を書き換える）なので GitHub の macos-14 のまま
   - **【2026-10-20 オーナー決定｜セキュリティ監査 H2】`release` は専用のリリース用ランナー（Mac mini の別の macOS ユーザー、ラベル `shogun-mac-release`）で回し、PR ジョブを動かすランナー（`shogun-mac-mini`）とは絶対に共有しない。** 上の 10-13 の「release も Mac mini（PR と同じランナー）」をこの形に改める。PR ジョブが `~/.cargo/bin` などに残したものを、署名の秘密を持ったリリースが実行しないため。リリース用ランナーはフック（`scripts/mac-release-runner/job-started.sh`）で `release.yml`（v* タグ push か main からの Run workflow）以外を断る。止まっているときはリポジトリ変数 `RELEASE_RUNNER=github` で macos-14。アプリは署名せずにビルドし、署名は `scripts/release-codesign.sh` が一時キーチェーン（鍵は取り出し不可）で行う。タグは main 上のコミットだけ、Run workflow は main からだけ、ジョブは `environment: release`。`check-manual-macos.rb` が「`release.yml` 以外は `shogun-mac-release` を使わない」「`release.yml` は `shogun-mac-mini` を使わない」を含めて検査する。手順は `docs/mac-mini-runner.md` §5、プランの制約とオーナーの作業は `docs/release-signing-and-distribution.md` §1-e
   - `phase0-smoke` は1回13分＝枠約134分。`release` はタグ push。**どちらも手動で回す前にオーナーに確認を取る**
   - 開発者向けの手順は `docs/manual-macos-ci.md`
2. **ローカルで再現できる検証は、push する前に手元で通す。** 下の「ローカル検証」の一式はすべて Linux で走る。CI を「試しに回す」ために使わない
3. **同一 PR への小刻みな push をしない。** push ごとに全ワークフローが回る。修正はまとめてから1回で押す
4. **macOS ジョブを、自動で走るワークフローに置かない。** GitHub は `paths:` をワークフロー単位でしか適用しないので、`ci.yml` に1つ置けば無関係な変更でも macOS ランナーを確保してしまう（これが `macos.yml` を分離した理由）。`macos.yml` に ubuntu のゲートジョブが同居しているのは逆方向——**安いジョブを高いワークフローに置く**のは構わない。禁止なのは高いジョブを安いワークフローに置くこと
5. **YAML アンカーを使わない。** Actions は `&anchor` / `*alias` を解決しない。ローカルで parse できてもランナーで落ちる

## ローカル検証（push 前に通す。`ci.yml` の Linux ジョブと同じ内容）

```bash
pnpm install && pnpm --filter @shogun-ai/tokens build   # 生成トークン CSS（gitignored）。desktop の typecheck/test の前提

# Rust。macOS 専用の shogun-desktop-spike は Linux では除外する（macos.yml が唯一の検証）
cargo clippy --workspace --exclude shogun-desktop-spike --all-targets
cargo test   --workspace --exclude shogun-desktop-spike
scripts/check-feature-matrix.sh        # 既定 feature が空のクレートを net / db / exec / server 付きでも通す

# 不変条件ガード（ci.yml と同じ。各 --self-test → 本検査）
for g in http-egress secret-exposure media-writes log-hygiene migrations batch-source-filter; do
  python3 scripts/check-$g.py --self-test && python3 scripts/check-$g.py
done

# TS（turbo 経由。desktop / api / website / packages）
pnpm typecheck && pnpm test && pnpm lint

ruby scripts/check-manual-macos.rb     # .github/workflows を触ったときだけ
```

- 共有の enum / trait を変えたときは、macOS シェル側の網羅 match が壊れていないか `macos.yml`（`run-ci` ラベル）で 1 回だけ確認する。Linux のどのコマンドもそこには届かない

## コミット・PR

- Conventional Commits（feat: / fix: / perf: / docs:）
- SLO関連の変更は計測結果をPR本文に貼る
- スキーマ変更はマイグレーションファイル＋ロールバック手順（`docs/migrations/V<n>-rollback.md`）を必須添付
