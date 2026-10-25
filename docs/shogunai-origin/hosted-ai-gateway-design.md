# Hosted AI gateway — #1101 の設計と移行境界

- 出典: [Issue #1101](https://github.com/Torutesu/ShogunAI-/issues/1101)、2026-10-13 の D1/D2/D3/D4/D6/D7 と browser sign-in の追記、2026-10-14 の D10（Azure OpenAI 優先）/ D11（ゼロ保持）。
- 状態: **承認済みの製品方針を仕様へ反映する先行変更。実装・デプロイ・受け入れ完了ではない。**
- 現行確認: main `234f7e6f`。`apps/api` は Cloudflare Workers + Supabase Postgres（Hyperdrive）。#1101 の Fly/SQLite 前提とは異なる。
- **今回のオーナー回答**: 現行の **Cloudflare Workers + Supabase を維持**する。#1101 の Fly への移転案は採用しない。
- 実装状況: [live relay implementation](hosted-ai-gateway-implementation.md)（段階導入・本番は未有効）。
- 認証の詳細: [account-passkey-design.md](account-passkey-design.md) / [English](account-passkey-design.en.md)。既存運用: [ai-gateway-runbook.md](ai-gateway-runbook.md)。

## 1. 承認済みの方針

| 決定 | 目標 |
|---|---|
| D1 | live AI は会社キーを gateway 経由で使う。BYOK / 委譲はレーンごとの任意 override。Batch に BYOK / 委譲を使わない |
| D2 | 会社 Deepgram キーによる短命 grant を戻す。音声は Mac → Deepgram 直結 |
| D3 | rewrite は OpenAI `gpt-6-luna` / reasoning `low` / priority、dictation cleanup は Groq |
| D4 | アカウントは既存 licence key と併存し、紐付けは任意 |
| D6 | 会社が connector OAuth app を所有し、必要な client secret はサーバーだけに置く。**2026-10-14 例外**: Google のデスクトップ型 client の secret はリリースビルドに埋め込み、connector broker は作らない（CLAUDE.md、`docs/oauth-client-setup.md`） |
| D7 | アプリの単一 Sign in → システムブラウザの Google / パスキー → PKCE loopback |
| D10（2026-10-14、2026-10-15 改定） | OpenAI モデルの lane（live・Batch）は **OpenAI 直結が第一経路**（会社の OpenAI クレジットを先に使う）。Azure OpenAI は免除の承認後にフォールバックとして加え、OpenAI のクレジットが尽きたら第一経路を Azure に切り替える。要件 FR-AG-06f |
| D11（2026-10-14） | ゼロ保持。gateway / relay / provider のどこにも本文を残さない。各 provider の承認前はその provider に実ユーザーの通信を流さない。要件 NFR-PRV-05 |
| D12（2026-10-15、#1144） | Jev フォーム判断（`form_decide`）は relay 上の会社 TypeSafe キー（Worker secret `TYPESAFE_API_KEY`）が既定。ユーザー自身の TypeSafe キーは任意の上書き（TypeSafe 直結）。Jev がすべての欄を判断し、一般チャットモデルで代替しない。TypeSafe の保持・ZDR 条件はローンチ前の確認事項（オーナー判断待ち） |

Apple sign-in とメール/パスワードによる通常ログインは、Issue タイトルだけを根拠に追加しない。メール確認・復旧は #148 の規則を維持する。

## 2. 実装に持ち込まない旧前提と未決事項

| 項目 | 確認した相違 / 次の判断 |
|---|---|
| Hosting / D5 | オーナー回答により Workers + Hyperdrive + Supabase を維持。Fly / local SQLite を復元しない。Supabase は現行の東京を維持し、Workers の実行配置・JP/US の遅延は測定対象。Fly の `nrt` / `iad` 選択は今回の構成には適用しない |
| Hot path | 現行 Workers は共有 Postgres で admission。Issue の「remote DB 0回」「local SQLite sub-ms」は現行に対する事実ではない。Issue 自体も unknown user の synchronous fetch を含むため、warm/cold を分けて定義・計測する |
| Phase 0 コマンド | `fly.toml` / Dockerfile / `deploy-gateway.yml` は main で削除済み。Issue の dispatch は使えない。現行手順は `apps/api/DEPLOY.md`。この仕様変更ではホストを作成・移転しない |
| D8 | Secure Enclave proof は後続の hardening。現在の hardware device id / bearer を盗難耐性のある proof と表現しない |
| D9 | **2026-10-21 オーナー承認**: trial $1/day、Standard $3/day・$40/month、Pro $6/day・$80/month、audio 20h/month を初期値とする。計量の主体は relay の他の予算と同じ `meterSubject`（トライアルはアカウント、有償プランはライセンス。`acc` があっても有償はライセンス）。text relay で金額上限を実装し、audio admission は後続で実装する |
| D10 / D11 の未確認点 | Morning Brief は Anthropic Batch（`ANTHROPIC_API_KEY`）のまま。2026-10-15 オーナー決定（C）: relay は Mac が読み終えた Anthropic batch を削除する（`releaseResults`、実装済み）。Anthropic のゼロ保持の申請は後回し。Azure の Batch でも OpenAI と同じ「読み終えたら削除」を実装する。手順と申請は [ai-gateway-launch-checklist.md](ai-gateway-launch-checklist.md) |
| Account | Google-only の重要操作での再認証、復旧時の identity の扱い、Mac nonce の束縛を Phase 1 で確定する（account 設計 §8）。既存アカウントのメール一致自動連携は禁止 |
| Schema ownership | Drizzle が app tables、別 relay schema を raw SQL が所有する案は Issue 内でも proposed。移行前に現行 schema と照合する |

## 3. 通信・秘密・保存の境界

- Mac の通信は Rust core の既存共通クライアントを使う。webview に provider key、refresh token、ネットワーク責務を渡さない。
- 会社の live / batch provider キーを環境別・用途別に分離し、hosting の secrets store のみに置く。配布バイナリ・共有 Keychain・リポジトリ・DB・ログに入れない。
- `POST /v1/ai/{lane}` は `{input, instructions?, tools?, attachments?}` を lane の schema で検証し、サーバーが upstream body を構築する。model / effort / output cap / tier をクライアントに選ばせない。未知フィールドは除去し不正な既知フィールドを拒否する。
- 初期 lane: `chat` / `agent` / `draft` / `scribe` / `summary_live` / `translate_live` / `quick_translate` は OpenAI。rewrite は D3、dictation cleanup は Groq。`form_decide` は D12 により relay の `POST /v1/ai/form_decide`（会社 TypeSafe キー、ユーザーキーは任意の上書き。実装済み、本番デプロイ前）。Batch の既存経路は維持する。
- hosted tools は lane が許可するものだけ。chat 1MiB / rewrite 64KiB の body cap、lane 別 token cap、画像添付は許可された lane のみ。Visual recall の保存フレームをこの仕様で自動送信することはない。
- OpenAI は `store:false`。`safety_identifier` と lane ごとの `prompt_cache_key` は HMAC。Groq は org ZDR を運用で有効にする。provider retention の条件はローンチ前に公式資料と実設定で再確認する。
- gateway は本文・token・code・メール・IP を永続化もログ出力もしない。upstream error body は返さず status + step に変換する。metrics のラベルは route template と lane、個人識別子を入れない。
- 各呼び出しの端末側トレーサビリティに lane / 下流 provider / gateway 経由 / byte counts を記録し、Settings で開示する。L1 に外部への送信・投稿アクションを追加しない。
- BYOK / 委譲を選んだレーンは gateway を呼ばない。上限到達時も黙って別の課金経路へ切り替えない。

## 4. 認証・admission・使用量

既存の Ed25519 licence / trial token 検証を使い、新しい gateway token 体系は作らない。account access token は account/licence API 用であり、gateway bearer と混同しない。

1. token の署名・有効期限・device claim を検証する。ログイン時は `acc`、アカウント無しは `lic` / trial device で集約する。
2. plan・account/licence の status を確認する。ブロック、Stripe の変更、device revoke は認証された invalidation 経路で伝達する。**2秒以内の block** を実測する。Workers isolate のローカル cache だけで全拠点に伝わるとは扱わない。伝達失敗・古い cache の fail-closed 方針を Workers の複数 isolate / 拠点に対して設計する。
3. 原子的な共有台帳で daily/monthly dollar cap、rate、同時 stream（user 3、device 別）を検査して最大見積額を予約する。ストアの障害時に空台帳や無制限へ fallback しない。
4. 終了時に provider usage で精算する。中断で usage が不明なら予約を保持する。provider ごとの global breaker と provider 側の利用上限/請求アラートを設ける。

| 応答 | UI/動作 |
|---|---|
| 401 | token 不正として再認証を案内 |
| 403 blocked | 利用停止を表示 |
| 402 plan_required | 既存の Get a plan to continue |
| 429 rate_limited / budget_exceeded | 上限と reset time、Retry-After を提示 |
| 503 | breaker / admission 障害。空の成功結果を返さない |

MCP / REST / CLI に使用量と caps の読み取りを追加し、UI と同じ権限で提供する。

DB は account/status/device/licence と内容を含まない usage aggregates / audit を扱う。全テーブル RLS deny-by-default。gateway は専用の最小権限 role（必要な account/device/subscription/licence の SELECT、usage_daily の INSERT/UPDATE）とし service role を渡さない。`usage_daily` は400日、account deletion は device/usage を cascade、admin_actions は匿名化する。account サーバーの refresh token と認証コードはハッシュだけ保存する。スキーマ追加には migration と rollback 手順を付ける。

## 5. Deepgram

- 追加 `634d8bd84`、削除 `8684c32d0` の mint / client 実装を調べて再利用する。旧 `/v1/asr/token` と新 `/v1/asr/grant` の違いを意図的に扱う。
- 会社の専用 project・Member scope のキーで Deepgram `/v1/auth/grant`、`ttl_seconds:60`。
- 月次音声上限内・同時セッション2未満・device ごと20秒に1 mint。Mac は短命 JWT で Deepgram WSS へ直結し `mip_opt_out=true` と HMAC user tag を付ける。
- 接続済み stream が token TTL を超えて継続するか、最大継続時間を provider 側で制限できるかを検証する。mint 制限だけで音声時間の hard cap が成立するとは言わない。
- client の session 時間申告と provider usage を日次照合し、不一致・濫用はブロックと監査の対象。音声の gateway proxy 化は今回の範囲外で、別のプライバシー変更判断を要する。

## 6. Connector broker

`POST /v1/oauth/{service}/token` / `/refresh` はログイン済みアカウント・端末・rate を検証する。service ごとの token endpoint と loopback redirect allowlist を固定し、secret を加えて交換・refresh、レスポンスを Mac へ返す。token/code を保存しない。token は Mac の Keychain、以後の API は Mac → provider 直結。

Slack と GitHub の secret 必要性・MCP 互換性を検証し、必要なサービスだけ broker を使う。Google installed-app loopback + PKCE、Linear public PKCE、Notion 等の dynamic registration の既存直結経路は、実際の provider 条件を確認して維持する。Google のサインイン用 client と Gmail/Calendar 用 client を分ける。

## 7. 実装順と合格条件

1. **この先行仕様変更**: CLAUDE、要件、日英 account 設計、runbook の相違を整合する。#1101 全体を完了扱いにしない。
2. **Phase 0**（手順は [ai-gateway-launch-checklist.md](ai-gateway-launch-checklist.md) §1）: 現行 Workers の手順で既存 relay の本番前提と疎通、認証済み Batch 1件、#573 recover、リリースビルドの `generated=true` Brief を確認する。Issue の NXDOMAIN 記述を現在の実測として流用しない。
3. **Phase 1**: #472 課金/ライセンスの本番検証、account endpoints/session、実メール到達、browser + PKCE、Mac 組込み。新規/既存/サイト既ログイン、Google/パスキー双方、誤 state/verifier・期限切れ/再使用 code・メール一致だけの連携の拒否、licence-only 互換、全 API 面の読み取りを検証する。
4. **Phase 2**: live gateway、端末 lane routing、grant、caps/UI、block、usage reconciliation。全 live lane をユーザーキー無しで検証し、override が gateway を迂回することも確認する。
5. **Phase 3**: connector broker、release build の接続と refresh、`strings` で secret が無いこと、保存しないことを確認する。

gateway 追加遅延は直結比 **p50 ≤15ms / p95 ≤40ms**、JP と US で計測。NFR-SLO-03 の初可視文字 **≤1秒** は維持する。接続の warm/cold、admission cache hit/miss、失敗、サンプル数を報告する。HTTP connection reuse、buffer しない SSE、15秒 heartbeat は実装後に実測で確認する。Issue の6回の priority 試験は p95 達成証拠ではない。

各 phase の code PR はローカル検証・Linux CI、src-tauri 変更時の `run-ci` 1回、公証済み実機を該当境界で確認する。実装前/同時に公開 privacy docs と subprocessor list をWorkers と Supabase の実構成・配置、転送する live text、音声直結、provider retention に合わせて更新する。今の仕様PRで稼働中の製品説明を先行して書き換えない。

## 8. ローンチ前の外部作業

Google connector の restricted scopes / CASA、Slack 配布・管理者承認、Google サインイン用 client、provider ZDR と予算設定、Deepgram 専用 project、secrets rotation の実演、privacy/subprocessor の公開が必要。provider の審査・データ保持条件はコードが通っただけでは確認済みにならない。
