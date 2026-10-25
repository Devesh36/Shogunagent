# AI ゲートウェイ運用手順（apps/api / issue #334）

- **対象**: `apps/api` を `api.shogunaios.com` として運用する手順。デプロイ、シークレット、DNS/TLS、ロールバック、事故対応
- **関連**: CLAUDE.md 不変条件 3・5・7 / `docs/batch-relay-design.md` / `apps/api/DEPLOY.md`（オーナー向けの実際のデプロイ手順。wrangler のコマンドはそちらが正） / `docs/unit-economics.md` §4・§4.1（Workers 化の判断理由） / `apps/api/README-asr-proxy.md` / #334 / #286 / #97
- **ホスティング（2026-10-12 オーナー決定、PR #1022）**: Cloudflare Workers（worker 名 `shogunai-relay`、ホスト名は従来どおり `api.shogunaios.com`）＋ 台帳は Supabase の Postgres（東京）を Hyperdrive binding 経由で使う。2026-09-30 の Fly.io（東京 1 台＋永続ボリューム＋SQLite 台帳）は本番に出る前に撤回し、`fly.toml` / Dockerfile / `deploy-gateway.yml` は削除した。本書で「Fly」「ボリューム」「machine」と書いてあった箇所はすべてこの構成に置き換えてある


## #1101 の変更境界（先行仕様、未実装）

[hosted-ai-gateway-design.md](hosted-ai-gateway-design.md) が会社キーによる live lanes、Deepgram grant、browser sign-in、OAuth broker の目標仕様を定義する。旧「ASR は BYOK のみ」は現在の実装状態を説明するもので、#1101 D2 の目標ではない。

**Issue の Fly 手順を本書に上書きしない。** main は Workers + Hyperdrive + Postgres に移行済みで、Issue にある `deploy-gateway.yml` / Fly volume / local SQLite は存在しない。今回のオーナー回答で **Workers + Supabase を維持**することが確定した。Fly へ戻さず現行デプロイ設定を維持し、Workers の複数 isolate / 拠点と共有 Postgres を含めて latency・global admission・block invalidation を検証する。D9 は 2026-10-21 のオーナー回答で承認済み（gateway 設計 §2。計量はトライアルがアカウント、有償プランがライセンス単位）。

会社の provider / connector secrets は選択した hosting の secrets store のみ。live/batch と環境ごとに分離する。新規キーの設定・rotation・漏洩時の失効、connector secret、Supabase JWT signing key の rotation、runaway user の停止手順を各実装 phase と同時に追加し、ローンチ前に一度実演する。旧 mint を復元しただけで運用完了にしない。

ローンチ前に手で行う作業（Phase 0 のデプロイ、Azure OpenAI の準備（D10）、provider ごとのゼロ保持の申請（D11））は [ai-gateway-launch-checklist.md](ai-gateway-launch-checklist.md)。

---

## 0. このプロセスが何を持っているか

**製品全体で `ANTHROPIC_API_KEY` を持つのはここだけ。** デスクトップバイナリにも、共有 Keychain にも、ユーザーの端末のどこにも存在しない（不変条件5）。

裏返すと、ここが漏れると全顧客分の会社支出が漏れる。この文書のルールはほぼ全部そこから来ている。

顧客が持つのは**ライセンストークン**だけ（FR-BIL-08、`v1.<payload>.<sig>`）。ゲートウェイは `LICENSE_PUBKEY_B64` でローカル検証する — ライセンス API への往復はしない。

---

## 1. ホスティング — Cloudflare Workers ＋ Supabase Postgres（Hyperdrive）

**2026-10-12 オーナー決定（PR #1022）。** 中継は Cloudflare Workers（`shogunai-relay`）で動かし、台帳・レート制限・金額ブレーカーは
**共有ストア（Postgres、`apps/api/src/pg-store.ts`、§9-4）だけ**を使う。Postgres は Supabase（東京）で、Worker からは Hyperdrive
binding 経由で接続する。**relay 用の Hyperdrive 設定はクエリキャッシュを無効（`caching.disabled = true`）にして作る** —
所有権（IDOR 判定）と冪等性の `SELECT` が 60 秒古い結果を返すと、監査で正しいと判定した所有権チェックが壊れる（`docs/unit-economics.md` §4.1 要対応 1）。
website 用の Hyperdrive 設定とは別に作る。

載せ替えの理由（同 §4・§4.1）: 原価は動かない（relay のホスティングは原価の 1% 未満。Workers Paid は website と共用で増分 $0、Supabase Pro はライセンス DB のために既に必要）。
運用面が 1 つ減る（パッチ対象のサーバー・永続ボリューム・インスタンスロック・machine-id ピンが無くなる）。IP 単位のレート制限と DDoS 吸収がエッジで手に入る（監査 Low「1 台の接続枯渇」の解消）。

**以前の「Workers ではない理由」は無くなった。** 2026-09 までは台帳が SQLite（ローカルボリューム）で、レート制限バケットがプロセスメモリだったため 1 インスタンス前提だった。
共有ストア（issue #29、2026-09-29）が入った時点でその前提は不要になり、Workers では**共有ストア以外の構成は存在しない**（SQLite ワーカーと `instance-lock.ts` は 2026-10-12 に削除した。Node 起動は in-memory（開発専用・非永続）か Postgres）。
何 isolate で動いても同じ上限を引く。

Hono の `app.fetch` はそのまま、Node の `boot.ts` に相当する入口は Worker の `export default { fetch, scheduled }`。精算スイープ（§9-6）は `setInterval` ではなく **Cron Trigger（15 分ごと）**。
実際のバインディング名・wrangler のコマンド・手順は `apps/api/DEPLOY.md`。

---

## 2. 初回セットアップ

### 2-1. シークレット

Worker の **secret binding**（暗号化保存・コードからのみ参照・ダッシュボードにも値は出ない）に置く。設定コマンドは `apps/api/DEPLOY.md`。

| secret | 中身 |
|---|---|
| `OPENAI_API_KEY` | 会社の OpenAI キー（Batch 専用プロジェクト）。既定の `BATCH_PROVIDER=openai` の Batch・Morning Brief・毎時の `/v1/review`。本番必須 — 無いと起動しない |
| `ANTHROPIC_API_KEY` | 会社の Anthropic キー（§0）。`BATCH_PROVIDER=anthropic` に戻すとき用。起動時に必須 |
| `DEEPGRAM_API_KEY` | 会社の音声認識キー（#1101 D2。Member 権限・プロジェクトは1つだけ）。無いと `/v1/asr/*` は 503 `speech_unavailable` |
| `METRICS_TOKEN` | 任意。`GET /metrics` の bearer。無いと 404 |

次の値は **secret ではなく** `apps/api/wrangler.jsonc` の `vars` に置く（公開鍵と上限値で、秘密ではない。2026-10-18 に表を訂正）:

| var | 中身 |
|---|---|
| `LICENSE_PUBKEY_B64` | `packages/config/license-public-key.json` の `ed25519`（既存リリース鍵） |
| `DAILY_SPEND_CAP_USD` | 全体の日次金額ブレーカー（USD）。本番必須 — 無いと起動しない（§9-6）。目安は **有効ライセンス数 × $0.50 × 1.5**（`docs/unit-economics.md` §3.3。100 人なら $75/日）。月初に見直す |
| `LICENSE_PUBKEY_NEXT_B64` | ローテーション中だけ: 同ファイルの `ed25519_next`（§2-6） |
| `TYPESAFE_API_KEY` | 会社の TypeSafe キー。Jev フォーム判断（`POST /v1/ai/form_decide`、#1144 / #1101 D12）専用。任意 — 無ければその経路だけ 503（起動ログ `form_decide_lane closed`）。ローテーションとアラートは §9-9 |
| `ASR_ENABLED` / `ASR_ALLOWED_SUBJECTS` | 会社の音声認識の有効化と、絞り込むときに許可するライセンス id または `account:<acc>`（空のリストは誰も通さない）。**【2026-10-22 オーナー決定】変数が無いと有償ライセンスとトライアルの全員を通す**（2026-10-20 監査 H3 のトライアル拒否を撤回。トライアルはカード必須になり、予約は全体ブレーカーの `TRIAL_SPEND_SHARE` まで）。本番では変数を消して全員に開放した。緊急時に絞るときだけ置く。`ASR_ALLOWED_SUBJECTS` はライセンス id をコミットしないためダッシュボードにだけ置き、`wrangler.jsonc` の `keep_vars: true` で `wrangler deploy` に消されないようにしている（`apps/api/DEPLOY.md`） |

台帳の接続（Postgres）は secret ではなく **Hyperdrive binding** で渡す（§2-2）。

**`LICENSE_PUBKEY_B64` は公開鍵。** 秘密鍵はライセンス署名側にあり、ここには**絶対に置かない**（`docs/license-signing-key-runbook.md`）。

**`DAILY_SPEND_CAP_USD` は必須（2026-10-10 監査 H1）。** 未設定の中継は `LICENSE_PUBKEY_B64` 欠落と同じく
`missing required env: DAILY_SPEND_CAP_USD` を出して起動しない。外せるのは開発・テストで `RELAY_ALLOW_NO_SPEND_CAP=1`
を**明示的に**置いたときだけ（本番の Worker に置かない）。

起動時に公開鍵を `packages/config/license-public-key.json` と照合する。不一致なら起動しない。
デスクトップの埋め込み鍵も同じファイルとの一致をテストする。設定修復のために鍵ペアを再生成しない。

シークレットを `.env`、`wrangler.toml`（`[vars]` は平文でリポジトリに入る）、リポジトリ、CI ログのいずれにも書かない。
値は secret binding にだけ置き、設定コマンドはコマンド履歴に残るので、履歴を残さないシェルか stdin 経由で。

### 2-6. 署名鍵のローテーション（`kid` / 次の鍵。2026-10-10 監査 M6）

中継は署名鍵を**現行＋次**の 2 本まで信頼する。トークンは任意で `kid`（= SHA-256(raw 32 バイト公開鍵) の先頭 16 hex）を
持ち、`kid` があればその鍵だけ、無ければ現行 → 次の順で検証する。`kid` が信頼する鍵のどれでもなければ 401。
加えて `exp - iat > 48h` または `iat > now + 300s` のトークンは 401（実 TTL は 26h。署名鍵が漏れても長寿命トークンは
鋳造できない）。`iat` の無いトークンも 401（ライセンス API は全トークンに `iat` を入れる。省けば天井を抜けるので必須）。

手順（デスクトップの `crates/shogun-license` と website の署名側が同じ契約を実装している）:

1. 新しい鍵ペアを作り、公開鍵を `packages/config/license-public-key.json` の `ed25519_next` に入れる
2. 中継: `apps/api/wrangler.jsonc` の `vars` に `LICENSE_PUBKEY_NEXT_B64`（`ed25519_next`）を足す → デプロイ。**JSON に `ed25519_next` があるのに
   この変数が無い、または変数があるのに JSON に無い中継は起動しない**（片方だけ新鍵を信頼する状態を作らない）
3. デスクトップを配布（2 本を信頼するビルド）。端末が行き渡るまで website は現行鍵で署名し続ける
4. website の `LICENSE_SIGNING_KEY` を新鍵へ切り替える（トークンは `kid` を持つ）。中継は `kid` で次の鍵を選ぶ
5. 旧鍵のトークンが全部失効したら（26h）、JSON の `ed25519` を新鍵に、`ed25519_next` を削除し、中継の
   `LICENSE_PUBKEY_B64` を新鍵にして `LICENSE_PUBKEY_NEXT_B64` を外す（この順。外し忘れは起動拒否で分かる）

### 2-2. 台帳（Supabase Postgres ＋ Hyperdrive）

台帳は Postgres の共有ストア（§9-4。スキーマ `apps/api/sql/shared-store.sql`、`relay_` 接頭辞の 6 テーブル）。Worker はファイルシステムも
永続的なプロセスメモリも持たないので、**Postgres に届かない中継は起動しない／投入を 503 で閉じる**（§6-3、§9-2）。「とりあえず空の台帳で動かす」経路は無い。

手順（コマンドと binding 名は `apps/api/DEPLOY.md`）:

1. Supabase（東京）に relay 用の DB かスキーマを用意し、`shared-store.sql` を管理ロールで流す（冪等。何度流しても同じ）。
   **relay 専用のロール**を作って 6 テーブルへの `SELECT, INSERT, UPDATE, DELETE` だけを与える — ライセンス DB の表に触れないこと（`docs/unit-economics.md` §4.1 要対応 5）
2. relay 用の **Hyperdrive 設定**を作る。**クエリキャッシュは無効（`caching.disabled = true`）**、接続文字列は TLS 必須（`sslmode=require`、監査 Low F8）。
   website 用の Hyperdrive 設定を流用しない（そちらはキャッシュが有効でよいが relay では所有権判定が壊れる）
3. `wrangler.toml` の Hyperdrive binding でその設定を Worker に結び付ける。接続文字列そのものを secret や `[vars]` に置かない

**台帳の行を消して作り直すことで直さない**（§6-3）。台帳は前にしか進まないカウンタと永続 tombstone で、消すのは全ライセンスの日次カウンタをゼロにするのと同じ
— 支出上限が実質無くなる、事故のコストに天井が無い唯一の失敗モード。Supabase 側のバックアップ（PITR）は Supabase の機能で取り、relay 側に別経路のバックアップは作らない。

### 2-3. デプロイ

**GitHub Actions から**（`.github/workflows/deploy-relay.yml`、`workflow_dispatch` のみ、ubuntu ランナーで `wrangler deploy`）。
Actions → `deploy-relay.yml` のワークフロー → Run workflow。push では走らない — 中継は会社キーの唯一の置き場所で、デプロイは意図的な行為にしてある
（Linux ランナーなので CI 分数の心配は無い。CLAUDE.md の CI 規則は macOS にだけ掛かる）。
ワークフローが要るリポジトリシークレットは Cloudflare のデプロイ用 API トークンだけで、その名前と作り方は `apps/api/DEPLOY.md`。

**プロバイダキーは GitHub には置かない。** `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` / `DEEPGRAM_API_KEY` は Worker の
secret binding のみ（§2-1。`LICENSE_PUBKEY_B64` / `DAILY_SPEND_CAP_USD` は秘密ではないので `wrangler.jsonc` の `vars`）。`wrangler deploy` は既存の secret を触らないので、デプロイのたびに入れ直す必要は無い。CI を経由させると、
鍵を漏らしうる場所がもう1箇所増えるだけで、得るものが無い。

手元から出す場合も同じ `wrangler deploy`（`apps/api/DEPLOY.md`）。Docker イメージは無くなった（Dockerfile・`docker-entrypoint.sh`・`fly.toml` は削除済み）。

**初回デプロイの順序（#343 / #711）。** ゲートウェイはまだ一度も本番に出ていない。#711 以降の
ゲートウェイは `client_request_id` を必須にし、それを送るのは #711 を含むデスクトップ版だけである。
初回デプロイは、#711 を含む main から出すデスクトップ版（0.2.7 以降）と同時に行う。それ以前の
0.2.x は Batch 経路が 400 になる（ローカルのキャプチャと検索は動く）。

### 2-4. DNS / TLS

`api.shogunaios.com` は Worker の **Custom Domain** として `shogunaios.com` のゾーン（Cloudflare）に付ける。DNS レコードと証明書は
Cloudflare が自分で作って管理する — 手で A/AAAA を置かない、「DNS only」にもしない（旧 Fly 手順の逆。TLS 終端は Cloudflare のエッジ）。
手順は `apps/api/DEPLOY.md`。

ホスト名は変わらないので、**端末側の変更は無い**（release ビルドは `https://api.shogunaios.com` にだけ話す。`crates/shogun-core/src/llm/relay.rs`）。
2026-10-12 時点でこのホスト名はまだ NXDOMAIN（`docs/public-release-remaining-work.md` P0-F2）。初回デプロイで初めて解決するようになる。

WAF のレート制限ルールを **`/v1/` で始まるパス全体**（`/v1/batch*`・`/v1/review`・`/v1/asr/*`）に 1 本置く（IP 単位の洪水よけ。2026-10-20 監査 L1: 以前は `/v1/batch` だけだった。既存ルールが `/v1/batch` だけなら `/v1/` に広げる）。主制限は引き続き支払い主体 + device + route（§9-1）。

### 2-5. プロバイダ側の最終防壁

**ローンチ前の必須手順（2026-10-20 監査 M3）。** provider ごとのコンソールで、下の全部を実ユーザーの通信を流す前に設定し、
日付付きの記録を `docs/ai-gateway-launch-checklist.md` §1-12 に残す。ゲートウェイの上限にバグがあった場合や、
中継の外で使われる資格情報（Deepgram の grant JWT、漏れたキー）は、ここが最後の砦になる。ゲートウェイを信用して外さない。
具体的な画面と値の目安は `apps/api/DEPLOY.md` の「Provider-side hard limits」。月の予算の目安は `DAILY_SPEND_CAP_USD × 31`（＋バグで失ってよい額）。

| provider | 設定 |
|---|---|
| OpenAI | **レーンごとに別プロジェクト・別キー**（今は Batch レーン = バックグラウンドの各クラスと毎時の `/v1/review`。#1101 の live レーンは出す時にそれぞれ別プロジェクト）。プロジェクトごとに**月の予算（上限で止まる設定 = ハード上限）**、使うモデルの**低いレート制限**（使わないモデルは最低か不許可）、**請求アラート 50 / 80 / 100%**。前払いクレジットなら自動チャージは切るか上限付きに |
| Groq | 同じ（dictation cleanup レーンを出す前に。中継はまだ Groq を呼ばない）: 専用プロジェクト・キー、止まる月の利用上限、低いレート制限、アラート、ZDR |
| Anthropic | 中継の `ANTHROPIC_API_KEY` 専用の workspace、**workspace の月の利用上限**と低いレート制限、組織の通知 50 / 80 / 100% |
| Deepgram | 会社の音声認識専用の**プロジェクト**（キーから見えるプロジェクトが1つでないと中継は発行しない）。**止まる月の予算**: 前払いクレジットを月の予算分だけ入れて自動チャージを切る。残高・利用アラート。キーは Member でそのプロジェクトだけ。2026-10-22 から `ASR_ALLOWED_SUBJECTS` を外して全員に開放したので、この前払いの上限が最後の壁になる（grant JWT はプロジェクトが許す全 API を呼べ、音声のソケットを中継経由にするまで中継はタグも上限も JWT に縛れない） |
| TypeSafe | Jev フォーム判断（`TYPESAFE_API_KEY`、#1144 / D12）専用のキー。コンソールで**ハード上限と請求アラート**（ローテーションと手順は §9-9） |

画面の名前やプランの機能は変わる。止める設定が無くアラートしか無いプランでは、自動チャージを切った前払いクレジットを壁にし、その旨を記録する。アラートだけでは完了にしない。

---

## 3. 設定値

| 変数 | 既定 | 意味 |
|---|---|---|
| `DAILY_CHUNK_CAP` | 2000 | ライセンスごとの日次チャンク数上限（Batch レーン） |
| `TRIAL_DAILY_CHUNK_CAP` | 500 | トライアル端末（`plan: "trial"`、issue #763）ごとの日次チャンク数上限。`DAILY_CHUNK_CAP` を超えない。**`DAILY_SPEND_CAP_USD` が未設定ならトライアルのトークンは全部 402**（§9-8） |
| `TRIAL_SPEND_SHARE` | 0.5 | トライアルの投入が使える日次金額上限の割合（0〜1）。当日の合計支出がここに達するとトライアルだけ `spend_cap` 429。残りは有償ユーザーの分（§9-8） |
| `RATE_BURST` | 12 | 連続リクエスト許容数。**支払い主体（ライセンス、アカウント経由のトライアルはアカウント）+ device + route ごと** |
| `RATE_PER_MIN` | 12 | 同バケットの持続レート |
| ~~`USAGE_FILE`~~ | — | **廃止（2026-10-12）。** Node 起動の SQLite 台帳と共に削除。Node 起動は `RELAY_DATABASE_URL` が無ければ in-memory ストア（開発専用・再起動で消える。起動ログに明示） |
| 台帳の接続（Postgres） | Hyperdrive binding | 共有ストア（§9-4）。台帳・レート制限・金額ブレーカーの置き場所。Worker では **Hyperdrive binding**（クエリキャッシュ無効、§2-2）で渡し、接続文字列を変数に書かない。Node 起動では従来どおり `RELAY_DATABASE_URL`（シークレット扱い） |
| `SETTLE_MAX_PER_TICK` / `SETTLE_RECHECK_SECONDS` | 500 / 900 | 精算スイープ 1 回の問い合わせ上限（1〜2000）と、処理中だった束を再問い合わせしない時間（秒）。`src/boot.ts` に上限の計算（500 × 96 回/日 = 48,000 件/日） |
| `DAILY_SPEND_CAP_USD` | **必須**（既定なし） | 全体の日次金額上限（USD、UTC 日、全ライセンス合計）。未設定なら**起動しない**（§9-6、2026-10-10 監査 H1）。目安は有効ライセンス数 × $0.50 × 1.5（`docs/unit-economics.md` §3.3）。`0` は Batch 全停止の緊急手段 |
| `RELAY_ALLOW_NO_SPEND_CAP` | 未設定 | `1` のときだけ `DAILY_SPEND_CAP_USD` 無しで起動できる。**開発・テスト専用。本番に置かない**（トライアルレーンも閉じる） |
| `LICENSE_DAILY_SPEND_CAP_USD` | 0.50 | ライセンス単位の日次金額予算（USD）。1 ライセンスが 1 日に会社キーへ予約できる上限。超えると `429 reason: "license_spend_cap"`、何も転送しない（§9-6）。**2026-10-12 に $3.00 → $0.50**（Haiku 5.5 で typical $0.24/夜の 2 倍。`docs/unit-economics.md` §3.3） |
| `TRIAL_DAILY_SPEND_CAP_USD` | 0.25 | トライアルの日次金額予算（USD）。**アカウント経由のトライアルはアカウント単位**（そのアカウントの全端末で 1 つ。2026-10-20 監査 H1）、端末だけの経路は端末単位。`LICENSE_DAILY_SPEND_CAP_USD` を超えない（§9-8）。**2026-10-12 に $1.00 → $0.25**（7 日で最大 $1.75） |
| `SETTLE_INTERVAL_SECONDS` | （Workers では Cron Trigger） | 精算スイープ。未精算の束縛済みバッチをプロバイダに問い合わせ、終了していれば実使用量を記帳する（§9-6、監査 M9）。**本番は `wrangler.toml` の Cron Trigger（15 分ごと）が `scheduled` を呼ぶ**ので変数は見ない。Node 起動では周期（秒、既定 900、0 で止める） |
| `LICENSE_PUBKEY_NEXT_B64` | 未設定 | 署名鍵ローテーション中だけ: 次の公開鍵（`license-public-key.json` の `ed25519_next` と一致必須。JSON に無いときは設定不可。§2-6） |
| `SPEND_PRICE_TABLE_JSON` | 未設定 | 単価表の**行の上書き**（`apps/api/src/spend.ts` の `DEFAULT_BATCH_PRICES` と同形）。**既定の表に重ねる**（書いた行だけ置き換わり、他のモデルは同梱の単価のまま。2026-10-20 監査 L2。以前は表ごと置き換えていた）。どちらにも無いモデルは最も高い行の単価で、固定の下限 `UNKNOWN_MODEL_FLOOR`（Sonnet 級の Batch 単価）を下回らない |
| `RELAY_DENY_SUBJECTS` | 未設定 | **緊急用**（2026-10-20 監査 L3）。カンマ区切りのライセンス id / `account:<acc>`。全ルート（Batch・review・音声）で 401。ダッシュボードにだけ置く（§6-2） |
| `METRICS_TOKEN` | 未設定（`/metrics` は 404） | `GET /metrics` の bearer（§9-5）。シークレット扱い |
| `METRICS_LOG_INTERVAL_SEC` | 60 | `metrics {…}` 構造化ログの周期。0 で止める（Node 起動。Worker 版は §9-5 / `apps/api/DEPLOY.md`） |
| ~~`RELAY_EXPECTED_MACHINE_ID`~~ | — | **廃止（2026-10-12）。** 単一 machine 構成の machine-id ピン（#787）は、Workers ＋ 共有ストアでは守る対象が無い（§7） |

**数値の設定値は整数（10進の数字のみ）。** `DAILY_CHUNK_CAP` / `TRIAL_DAILY_CHUNK_CAP` / `RATE_BURST` /
`RATE_PER_MIN` / `PORT` / `METRICS_LOG_INTERVAL_SEC` に `12/min`・`1e3`・負数などを入れると、変数名を
出して**起動しない**（issue #787。以前は `NaN` になって上限が黙って無効になっていた）。未設定・空文字は既定値。
`RATE_BURST` / `RATE_PER_MIN` / `PORT` は 1 以上。`DAILY_CHUNK_CAP` / `TRIAL_DAILY_CHUNK_CAP` /
`METRICS_LOG_INTERVAL_SEC` / `SETTLE_INTERVAL_SECONDS` は 0 を許す（`DAILY_CHUNK_CAP=0` は §6-1 の Batch 停止手順）。
金額（`DAILY_SPEND_CAP_USD` / `LICENSE_DAILY_SPEND_CAP_USD` / `TRIAL_DAILY_SPEND_CAP_USD`）は 10 進の小数（`3`、`3.00`、`0.5`）で、
負数・`$`・単位・指数表記は変数名を出して起動拒否。

保守的な値から始めて、内容を含まない利用テレメトリを見て緩める。**逆方向（緩い値から始めて絞る）にしない** — 緩すぎた分は既に使われている。

---

## 4. ヘルスチェック

`GET /healthz` → `{"ok":true}`（`InstanceLock` の `degraded` フラグは 2026-10-12 にロックごと削除した。本文は常にこれだけ）

- 認証なし（デプロイ後の確認に使う側が使えないと意味がない）
- **プロバイダを叩かない**（毎プローブで会社キーを消費し、プロバイダ障害が再試行ループになる）
- 設定値を返さない。設定・鍵・上限は一切出さない
- **設定の整合（鍵の照合、必須 env、台帳のスキーマ）は投入を受ける前に確かめる（issue #787）。** Node 起動（Postgres）では
  スキーマを確かめてから listen する。Worker 版でも同じ検査を通らない限り投入を受けない（`DAILY_SPEND_CAP_USD` 欠落のまま動き続ける状態を作らない）。
  デプロイ後の確認（`deploy-relay.yml` / `apps/api/DEPLOY.md`）はこれを見ている
- プローブのたびに台帳を読む readiness エンドポイントは**作らない**。認証なしで Postgres（Hyperdrive の接続枠）を叩ける入口を
  置くと、本物の投入が `metering unavailable`（503）になる。Postgres に届かなくなった場合は投入が 503（§6-3）になり、
  `metrics` の `rate_5xx` と Workers Logs で見える

---

## 5. ロールバック

Workers は直近のバージョンを保持しているので、コードは **前のバージョンへの即時ロールバック**で戻す（`wrangler` のコマンドは `apps/api/DEPLOY.md`。
Actions の `deploy-relay.yml` は前のコミットを指定して再デプロイしてもよい）。secret binding と Hyperdrive binding はバージョンを跨いで残る。

**台帳のスキーマ（`shared-store.sql`）は追加のみ**なので、古いコードが新しいスキーマを読んでも壊れない。逆（新しいコードが要る列を持たない
古いスキーマ）にならないよう、スキーマを先に流してからコードを出す（§9-4 の順）。

（履歴）**After the schema-2 cutover, do not roll back to an earlier JSON or single-blob SQLite image.**
Only deploy an image that understands the normalized schema-2 tables; otherwise disable submissions
while repairing it. The frozen JSON source and pre-cutover database are stale once new jobs are
accepted. Stop every old writer before migration; do not run storage versions concurrently.
See [usage ledger schema-2 rollback](migrations/usage-ledger-v2-rollback.md). — この段落は Node ＋ SQLite 台帳のもので、
その台帳は 2026-10-12 に削除した（本番は Postgres だけ、SQLite 台帳で本番が動いたことは一度も無い（P0-F2）。開発は in-memory か Postgres）。履歴として残す。

**台帳は巻き戻さない。** 台帳は前へ進むだけのカウンタで、巻き戻すと当日分の支出が二重に使える。コードを戻しても Postgres の行はそのままにする（Supabase の PITR を台帳の復旧に使わない）。

**シークレットも巻き戻さない。** 鍵をローテーションした直後のロールバックは、古いコードに新しい鍵を渡すだけで問題ない（鍵の形式は変わらない）。

---

## 6. 事故対応

### 6-1. 会社支出が想定を超えている

1. Workers Logs（ダッシュボード、または `wrangler tail`。`apps/api/DEPLOY.md`）で経路別の件数を見る（本文は出ない。`logging.ts` が method / path / status / 件数しか書かない。
   Workers Logs は `console.*` を全部拾うので、redacting logger 以外から `console.*` を呼ばない）。
   `event=license_spend_cap_reached` が特定の `license=<hash>` に集中していれば、1 ライセンスが予算を使い切っている
   （乱用か、予算が低すぎるか）。`metrics` 行の `plans.<plan>.reservations.license_spend_capped` も同じことを数える
2. 上限を下げる（Worker の secret / 変数の値を変えて反映する。手順は `apps/api/DEPLOY.md`）:
   `LICENSE_DAILY_SPEND_CAP_USD` を下げる（1 ライセンスの日次予算を絞る。他は無影響）、
   `DAILY_SPEND_CAP_USD=0`（全投入を `spend_cap` 429）、`DAILY_CHUNK_CAP=0`（Batch を止める）
3. **ローカルのキャプチャ・検索・メモリは影響を受けない** — 会社負担の仕事だけが止まり、端末は動き続ける

### 6-2. ライセンストークンが漏れた疑い

トークンは発行から26時間で失効し（`TOKEN_TTL_SECONDS`。トライアルのトークンはトライアル終了を超えない）、payload の device id に束縛される。**ただしゲートウェイが device claim を metering キーに使うのは blast radius を絞るためであって、盗んだ側を締め出す機能ではない**（bearer トークンなので、持っている側は claim も持っている）。

**中継のトークンは早期に失効させられない（2026-10-20 監査 L3）。** 中継は署名をローカルで検証するだけでライセンスAPI に問い合わせないので、
ライセンス側で失効させても、発行済みのトークンは `exp` まで（**最長 26 時間**）通る。急ぐときは Worker のダッシュボードで
`RELAY_DENY_SUBJECTS` にライセンス id か `account:<acc>` を足す。次のリクエストから全ルートが 401 になる。ライセンス側の失効から
26 時間たったら外す。それまでに漏れたトークンが使えるのは、その支払い主体の日次予算（`LICENSE_DAILY_SPEND_CAP_USD`）まで。

**署名鍵（`LICENSE_SIGNING_KEY`）が漏れた疑い。** 中継は `exp - iat > 48h` のトークンを拒むので、鋳造できるのは短命のトークンだけで、それも 1 ライセンスあたり日次予算、全体で `DAILY_SPEND_CAP_USD` までしか使えない。復旧は §2-6 のローテーション手順（次の鍵を先に信頼させ、署名側を切り替え、旧鍵を外す）。

### 6-3. 台帳が読めない

`tryReserve` が `unavailable` を返し、ルートは **503 を返してプロバイダを叩かない**（fail closed）。意図した挙動なので、直し方は台帳を読めるようにすること。**「とりあえず空の台帳で起動」は絶対にやらない** — 全ライセンスのカウンタをゼロにするのと同じ。

---

## 7. スケール

**水平スケールは Workers が自動で行い、台帳・レート制限・金額ブレーカーは Postgres（共有ストア、§9-4）にあるので、
isolate が何個になっても同じ上限を引く。** 操作で台数を足す・減らすものは無い。

2026-09 までの「2 台目を足す前に共有ストアへ」「`InstanceLock` は 2 台目の machine を検出できない（#787）」「`RELAY_EXPECTED_MACHINE_ID` で
machine-id をピンする」は、SQLite 台帳＋プロセスメモリのバケット＋1 台の VM という構成の話で、Workers ではその構成が存在しない（§1）。
`instance-lock.ts` と SQLite ワーカーは 2026-10-12 に削除した。Node 起動は in-memory（開発専用）か Postgres。

スケール時に先に詰まるのは中継ではなく（`docs/unit-economics.md` §4.1）:

- **Postgres / Hyperdrive の接続枠**。1 万ユーザーで夜 13 万クエリ・十数 QPS の見込みで、Hyperdrive（有料: 接続上限 60〜100）で足りる。無料 Hyperdrive の 10 万クエリ/日は超える
- **Anthropic 側の組織レート上限**。Batch 作成は 1 ユーザー・夜 1〜2 回。数千ユーザー規模で営業経由の引き上げが要る。セッション化（L1、`docs/select-kk-cost-zero-base-plan.md` §3）が 1 リクエスト内の items を 20 分の 1 にするのでここにも効く
- **台帳の tombstone 行**（監査 Low F5、永続）。Supabase Pro の 8GB に収めるために保持期限を入れる（未実装）

トリガー（spec §4-4）: 中継 p95 > 2 s が 10 分継続。見る順は Hyperdrive の接続待ち → Postgres の遅いクエリ → 上流（Anthropic）の遅延。
Worker の CPU 時間（Workers Paid 既定 30 秒/リクエスト）は Ed25519 検証と 8MiB の JSON 解析で 1 投入 ≈ 100〜200 ms なので、CPU が先に詰まることは想定しない。

---

## 8. 既知の限界（#334 / #286 のフォローアップ）

- **金額ブレーカーは本番必須になった（2026-10-10 監査 H1。以前は「既定で無効」）。** `DAILY_SPEND_CAP_USD` の額そのものはオーナー判断（spec §8-4）のままだが、未設定の中継は起動しない。ライセンス単位の日次予算（既定 $3）も常時有効。プロバイダ側のハード上限（§2-5）は引き続き外さない
- **`/v1/messages` は不要になった（2026-10-03 #782）。** #334 が定義していた同期 Messages 中継の対象3経路（`quick_translate` / `meeting_live_translate` / `meeting_live_summary`）は、ユーザー自身の資格情報（Agent lane: サブスク委譲 / BYOK）で動くよう移した。デスクトップはどの経路でも Select KK キーを読まない（debug ビルドの Batch 直結を除く）
- ~~会議 ASR はゲートウェイの対象外（2026-09-01 BYOK 決定）~~ **2026-10-13 #1101 D2 で撤回。** 中継が `POST /v1/asr/grant` / `/refresh` / `/finish` で会社の Deepgram キーから 60 秒の短命 grant を発行し、音声は Mac から Deepgram へ直接送る（`apps/api/src/asr-app.ts`、`apps/api/DEPLOY.md` の Company-funded speech rollout）。ユーザー自身のキーは任意の上書き
- **device claim は blast radius の限定であって認証ではない**（§6-2）

### #343 cutover and recovery

**2026-10-12 note.** The paragraphs below describe the Node + SQLite ledger (one dedicated worker, `MAX_PENDING_USAGE`, WAL, the
schema-2 import). That ledger was **removed on 2026-10-12** (PR #1022 second pass): production has run on Cloudflare Workers with
the Postgres store since PR #1022 and never ran it, and development uses the in-memory store or Postgres. The SQLite-specific
recovery steps are history and apply to nothing deployed. The request-identity, reservation and reconciliation semantics
(reserve-then-commit, `submission_unknown` / `provider_accepted_unbound`, no age-based retirement) are the same in `pg-store.ts`
and still hold.

Requires Node >=22.13. Updated desktop clients send IDs to the older optional-ID relay, so ship
those clients before enforcing the new required field if availability for old clients matters.
Missing-ID clients receive 400 without provider access; local capture/search keep working.
The Docker stages pin Node 22.23.3; update both together and run the API suite on that version
and the minimum-supported version. No image-build verification is implied by local Node tests.

The SQLite connection runs in one dedicated worker per gateway process. The HTTP thread does
not wait synchronously on its five-second busy timeout. Admission allows eight pending ledger
operations; a six-second deadline includes startup and queue time. This bounds the ledger queue,
not total HTTP/provider concurrency. Ledger overload/failure returns 503 and `Retry-After: 1`.
Clients must retry/recover the same request ID. When a reservation returns unavailable, the
live handler has not called the provider: after the failed worker exits, it makes one compensating
transaction that refunds/reopens only an unbound submitting claim with its exact claim token.
A repeated or stale cancellation refunds nothing. If compensation cannot commit, or the gateway
process dies before compensation, the claim stays unknown; do not infer provider non-acceptance
from age alone. Once createBatch is entered, provider uncertainty retains the armed claim.
If a bind committed, recovery replays its saved handle. Permanently unknown claims still need
operator reconciliation; this change adds no automatic age-based retirement. SIGTERM/SIGINT stops
accepting HTTP, drains active handlers, then stops the worker; abrupt death uses SQLite recovery.

Stop all old JSON and schema-1 SQLite writers, preserve an authoritative SQLite backup and the
JSON import source, then start the schema-2 image on the same volume. Startup atomically migrates
existing SQLite state (preferred) or legacy JSON into indexed request, owner and counter tables.
The old blob is dropped only inside that successful commit. Do not run storage versions concurrently. Back up with SQLite's backup API or stop writers
before copying the database/WAL; never copy only an active database file or restore old counters.
Preserve `USAGE_FILE.initialized` with the database. If initialized SQLite state is missing,
restore the authoritative SQLite backup with writers stopped; never delete the guard to restart
from JSON. A crash before the first import commit may leave only the guard. Keep submissions
stopped until an operator verifies no provider reservations existed and restores/imports the
correct ledger offline. Ordinary recovery `{found:false}` is not proof that a paused worker cannot
submit: changed local mail claims require the relay's atomic `retired:true` response.
Disable core dumps and proxy/APM body capture. No cloud body cache requires TTL cleanup.

Alert on fixed `submission_unknown` / `provider_accepted_unbound` events. Retain the original
request identity indefinitely when provider acceptance cannot be proved or disproved. No clock
expiry authorizes retry. Reconciliation policy and offline/native/provider evidence boundaries:
[Batch design](batch-relay-design.md#process-only-bodies-and-operations-343) /
[#343 audit](issue-343-audit.md). This code change does not deploy or validate any hosting configuration.

---

## 9. 共有サービスの容量（issue #29 / `docs/specs/issue-29-shared-service-capacity.md`）

### 9-1. レート制限のキー（FR-SVC-03）

| 経路 | 主制限（キー） | IP 制限 | 実装 |
|---|---|---|---|
| `POST /api/license/verify` | **licence 指紋 + device id**、30/分 | 300/分（旧 30/分の 10 倍） | `apps/website/src/lib/rate-limit-policy.ts` |
| `POST /api/license/claim` | （認証なし・nonce。nonce で数えると推測ごとに枠が増えるため IP のまま） | 120/分（変更なし） | 同 route |
| `POST /api/support/report` / `…/reply` | （認証なし） | **20/時**（旧 5/時。オーナー判断 §8-3 は提案値を既定に採用。1 定数 `SUPPORT_REPORT_PER_IP_PER_HOUR`） | `rate-limit-policy.ts` |
| 中継 `POST /v1/batch` / `GET /v1/batch/{id}` / `POST /v1/batch/recover` / `POST /v1/review` | **支払い主体 + device + route**（`bucketKey`。支払い主体はライセンス、アカウント経由のトライアルは `account:<acc>`。2026-10-20 監査 H1）、`RATE_BURST` / `RATE_PER_MIN` | WAF で `/v1/` 全体に IP 単位の洪水よけ（§2-4）。不正トークンは署名検証で 401 になり、台帳にも上流にも届かない | `apps/api/src/app.ts` |

IP 制限は CGNAT・オフィス NAT で「同じ会社の 30 人」を巻き添えにするため、主制限は認証済みの識別子に置く。
IP 制限は洪水よけとしてだけ残す。rate_limits テーブルに入るのは licence **指紋**（SHA-256 先頭 12 桁）で、
キーそのものは入らない。

### 9-2. 障害時に開くか閉じるか（fail-open / fail-closed）

「開く」= 依存先が死んでいても要求を通す。「閉じる」= 通さずエラーにする。どちらも**端末側のローカル機能は無影響**（FR-DC-05）。

| 経路 / 部品 | 依存先が落ちたとき | 方針 | 根拠（コード） |
|---|---|---|---|
| website のレート制限（下の1つを除く全 route） | DB エラー・2 秒超 | **fail-open**（通す） | `apps/website/src/lib/rate-limit.ts` の `catch` → `allowed: true` |
| website: 新規トライアル開始の上限（`license-trial-start`） | DB エラー・2 秒超 | **fail-closed**（500 `db_unavailable`、開始させない）。支出を守る上限なので障害で外さない。端末は1時間後に再試行し、それまでローカルレーン | `rate-limit.ts` の `failClosed` / `license/trial/route.ts` |
| 中継: トライアルのトークン | `DAILY_SPEND_CAP_USD` 未設定 | **fail-closed**（402、計上もプロバイダ呼び出しも無し） | `app.ts` の `authorize`、§9-8 |
| `license/verify` の licence / subscription 参照 | DB エラー・3 秒超 | **fail-closed**（500 `db_unavailable`）。端末はオフライン猶予 14 日で吸収 | `verify/route.ts` |
| `license/verify` の `license → plan` キャッシュ | — | 成功だけを 5 分保持。失敗・not_found は保持しない（`license-plan-cache.ts`） | FR-SVC-05 |
| `license/verify` の検証記録（device_count） | DB エラー | **fail-open**（記録を捨ててトークンは返す） | `recordVerification(...).catch` |
| `license/claim` | DB エラー | fail-closed（500）。端末は自分の締切まで再ポーリング | `claim/route.ts` |
| `support/report` 保存 | DB エラー | fail-closed（500）。通知メール失敗は fail-open | `support/report/route.ts` |
| 更新マニフェスト | GitHub Release 読めない | **fail-open 方向**（204 = 「最新です」、`no-store`）。更新は翌日以降に届く | `update/[target]/[current]/route.ts` |
| 中継: 認証（公開鍵） | 公開鍵未設定・不一致 | **fail-closed**（401 / 起動拒否） | `auth.ts` / `index.ts` |
| 中継: 計上台帳（`UsageStore`） | 読めない・書けない・混雑 | **fail-closed**（503 + `Retry-After: 1`、プロバイダを叩かない） | `app.ts` / `usage.ts`、§6-3 |
| 中継: レート制限（`RateLimiter`） | 本番（共有ストア、Postgres）は DB エラー時 **fail-open**（`ratelimit_unavailable (failing open)` を 1 行ログ）。**支出を守るのはその直後の台帳で、こちらは fail-closed**（チャンク上限・ライセンス予算・全体ブレーカーの予約が書けなければ 503、プロバイダを叩かない。音声の grant も発行しない）。DB 障害で通るのはレート制限までで、台帳の先には進まない（2026-10-20 監査 L4 で明文化）。Node 起動の in-memory 版は障害点なし | `pg-store.ts` | §9-4 |
| 中継: 金額ブレーカー | 台帳と同じトランザクション | **fail-closed**（台帳障害として 503） | §9-6 |
| 中継: 上流（Anthropic） | 5xx / タイムアウト | 502。予約は「明確な拒否」以外は残す（安全側） | `app.ts` |

### 9-3. キャッシュ（FR-SVC-05）

- **更新マニフェスト**: 応答（マニフェスト本体と「最新です」の 204）は `Cache-Control: public, max-age=600`。
  リリースが読めなかった 204 だけは `no-store`（読み取り失敗を 10 分固定しない）。定数 `UPDATE_CACHE_CONTROL`
- **`license → plan`**: isolate ごとに 5 分・最大 5,000 件。Stripe webhook で subscription を更新した isolate は
  即時に捨てる。他の isolate は最長 5 分古い plan を返しうる（端末トークン自体が約 24h なので実害の上限は変わらない）。
  **手動で `revoked_at` を立てた場合も最長 5 分は検証が通る** — 急ぐなら website を再デプロイして isolate を入れ替える

### 9-4. 共有ストア（FR-SVC-04）

**本番の唯一のストア（2026-10-12）。** 2026-09-30 の決定（Fly.io 東京 1 台＋永続ボリューム、共有ストアは当面無効）は、本番に出る前に
Workers への載せ替えで置き換えた（§1、`docs/batch-relay-design.md` OPEN-B1）。Workers にはファイルシステムが無いので、SQLite 台帳という
選択肢は本番に存在しない。「有効化のオーナー判断」は Workers 決定と同時に済んだことになる。以下はプロビジョニング手順（コマンドは `apps/api/DEPLOY.md`）。

- 実装: `apps/api/src/pg-store.ts`（`SqlUsageStore` / `SqlRateLimiter`）、`apps/api/src/sql.ts`（`postgres` ドライバ。website と同じ）
- スキーマ: `apps/api/sql/shared-store.sql`（`relay_` 接頭辞の 6 テーブル。冪等・追加のみ。**refinery マイグレーションではない** — 端末の DB ではなく中継の台帳）
- 意味論は in-memory ストアと同じ: 識別子の状態機械（`usage.ts` の純関数）を `SELECT … FOR UPDATE` で読んだ 1 行に適用。
  初回 claim は `INSERT … ON CONFLICT DO NOTHING`、チャンク・金額の予約は条件付き upsert（`… SET n = n + $k WHERE n + $k <= $cap RETURNING`）で
  判定と加算が 1 文。チャンクと金額は同じトランザクション（reserve-then-commit は変わらない）
- 障害時: 台帳は fail-closed（503、プロバイダを叩かない）、レート制限は fail-open（§9-2）
- 検証: `apps/api/test/pg-store.test.ts` が同じ SQL を SQLite ダブルで CI 実行。実 Postgres では
  `RELAY_TEST_DATABASE_URL=postgres://…/<使い捨て DB>` を付けて同じテスト＋2 クライアント同時予約を実行（CI には入れない）

**プロビジョニング**

1. Postgres を用意する（website と同じ Supabase プロジェクト（東京）に**別スキーマか別 DB**、または専用インスタンス）。
   Hyperdrive が接続をプールするので、Hyperdrive に渡す接続文字列は Supabase の**直結**（TLS 必須、`sslmode=require`）。`prepare: false` にしてあるのでプーラー越しでも動く
2. 中継専用のロールを作り、6 テーブルへの `SELECT, INSERT, UPDATE, DELETE` だけを与える（DDL 権限は不要。ライセンス DB の表には触れない）
3. `psql "<管理ロールの接続文字列>" -v ON_ERROR_STOP=1 -f apps/api/sql/shared-store.sql`（管理ロールで。何度流しても同じ）
4. relay 用の Hyperdrive 設定を**クエリキャッシュ無効**で作り、`wrangler.toml` の binding で Worker に結ぶ（§2-2）。Node 起動（ローカル）では
   従来どおり `RELAY_DATABASE_URL`（stdin 経由。§2-1 と同じ扱い）。全テーブルを読めなければ**投入を受けない**

**切り替え（SQLite 台帳 → 共有ストア）** — **本番では不要、コードも無い。** 中継は SQLite 台帳で本番に出たことが無く（P0-F2、初回デプロイが Workers ＋ Postgres）、
SQLite 台帳そのものを 2026-10-12 に削除した。以下は削除前の台帳ファイルが手元に残っていた場合だけの履歴手順。
台帳は前にしか進まないカウンタと永続 tombstone なので、コピーしてから書き手を替える:

1. Batch 投入を止める（`DAILY_CHUNK_CAP=0` → 再起動。GET は動き続ける）
2. 旧インスタンスを止め、SQLite のバックアップ API でスナップショットを取る（§5 / #343 節の手順）
3. 3 テーブルを CSV で移す。CSV では NULL と空文字が同じ空欄になるので、NOT NULL の列（退役 tombstone の `hash` / `date` は空文字）
   だけ `FORCE_NOT_NULL` で空文字に戻し、それ以外の空欄は NULL として読む:
   ```
   for t in usage_days usage_idem usage_owners; do
     sqlite3 -header -csv usage.json.sqlite "SELECT * FROM $t" > $t.csv
   done
   psql "$RELAY_DATABASE_URL" -v ON_ERROR_STOP=1 \
     -c "\copy relay_usage_days FROM 'usage_days.csv' WITH (FORMAT csv, HEADER)" \
     -c "\copy relay_usage_idem FROM 'usage_idem.csv' WITH (FORMAT csv, HEADER, FORCE_NOT_NULL (hash, date))" \
     -c "\copy relay_usage_owners FROM 'usage_owners.csv' WITH (FORMAT csv, HEADER)"
   ```
   列順は SQLite の schema-2 と同じ。
   件数を突き合わせる（`SELECT count(*)` を両側で）。金額ブレーカーの行は移さない（その日の推定値。0 から始まり安全側）
4. Postgres を向けて起動、`DAILY_CHUNK_CAP` を戻す。**SQLite 側の書き手を二度と起動しない**（二重台帳になる）
5. `/healthz` の `degraded` は共有ストア構成では常に false（`InstanceLock` を使わない）

**戻す**: 共有ストア → SQLite への戻し先は本番には無い（Workers にファイルシステムが無い）。Node 起動に戻すなら同じ CSV 手順の逆で、投入を止めてから。

### 9-5. 観測とアラート（FR-SVC-06）

- **`GET /metrics`**（中継）: `METRICS_TOKEN` を設定したときだけ有効。`Authorization: Bearer <METRICS_TOKEN>`。未設定なら 404、違えば 401
- **構造化ログ**: `metrics {JSON}` を 1 行（Node 起動では `METRICS_LOG_INTERVAL_SEC`、既定 60 秒ごと。Worker 版の出し方は `apps/api/DEPLOY.md`
  — isolate は常駐しないので、周期タイマー前提では書かない）。Workers Logs から拾えるので、エンドポイントを外に出さずにアラートを組める。
  Workers Logs の保持期間は短く設定し、redacting logger（`logging.ts`）を唯一の出口として維持する（`docs/unit-economics.md` §4.1 要対応 4）
- 中身（数値とルートテンプレートのみ。**licence / device / batch id・本文・鍵は含まない**）:
  - `routes["/v1/batch" | "/v1/batch/:id" | "/v1/batch/recover" | "/healthz"]`: 直近 5 分の `rps`・`p95_ms`（ヒストグラム境界値）・`rate_429`・`rate_5xx`・件数、起動以来の累計
  - `batch.plans[<plan>]`: 当日（UTC）の予約結果（`ok` / `capped` / `spend_capped` / `unavailable` / `unknown`）、`active_licences`、`chunks_reserved`、
    `cap_consumption` = 予約チャンク合計 ÷（アクティブライセンス数 × `DAILY_CHUNK_CAP`）、`licences_over_80pct`
  - `spend`（ブレーカー有効時）: `spent_micros` / `cap_micros` / `consumption`
- isolate 内の集計。Workers では isolate が複数・短命なので、`/metrics` の値は**その isolate が受けた分の下限**として読み、
  アラートはログ側（Workers Logs の `metrics` 行と `event=…` 行）で組む。全体の予約・支出は台帳（Postgres の `relay_usage_days` /
  金額ブレーカーの日次行）が正

| アラート | 条件 | 最初に見るもの |
|---|---|---|
| 5xx 率 | いずれかのルートで `rate_5xx > 0.01` が 5 分継続 | Workers Logs の `event=metering_unavailable` / 502（上流）/ 台帳（§6-3） |
| 中継 p95 | `/v1/batch` か `/v1/batch/:id` で `p95_ms > 2000` が 10 分継続 | §7 のスケールトリガーと同じ（Hyperdrive の接続待ち → Postgres → 上流） |
| 当日上限消化 | プランごとに `cap_consumption > 0.8` | 端末側のジッタ（FR-SVC-01）が効いているか、`DAILY_CHUNK_CAP` が原価モデルと合っているか |
| 金額ブレーカー | `spend.consumption > 0.8` | その夜の投入量と単価表。到達すると全端末がローカルレーンに落ちる（仕様どおり） |

### 9-6. 金額サーキットブレーカー（FR-SVC-08）

- **本番必須（2026-10-10 監査 H1）。** `DAILY_SPEND_CAP_USD`（USD、UTC 日、全ライセンス合計）が無い中継は起動しない。
  額はオーナー判断（spec §8-4、#286 の原価表待ち）。開発・テストだけ `RELAY_ALLOW_NO_SPEND_CAP=1` で外せる
- **ライセンス単位の日次予算（H1）。** 同じ見積もりを `LICENSE_DAILY_SPEND_CAP_USD`（既定 **$0.50**。trial トークンは
  `TRIAL_DAILY_SPEND_CAP_USD` 既定 **$0.25**、ライセンス予算を超えない。2026-10-12 に $3.00 / $1.00 から引き下げ — Haiku 5.5 で
  typical が $0.24/夜になり、$3.00 では赤字を止めない。`docs/unit-economics.md` §3.3）にも同一トランザクションで予約する（台帳の日次行
  `spend_micros`。`shared-store.sql` の追加のみの ALTER）。
  超えるなら `429 reason: "license_spend_cap"`（`Retry-After` 無し）で何も転送しない。他のライセンスには影響しない。
  Standard 1 本（年 $49）を買って中継を無料の LLM 代理にする経路は、この予算で 1 日数ドルに頭打ちになる
- 投入時: 上限見積もり（入力 = **バイト/2**（監査 M9。バイト/4 は句読点・コード・CJK・base64 で 2〜3 倍過小だった）、
  出力 = クラスの `max_tokens` 全量、キャッシュ breakpoint 付き system は 1h 書き込み単価）を
  チャンク予約と**同じトランザクション**で予約。超えるなら `429 reason: "spend_cap"`（`Retry-After` 無し = 上限到達。§4.5）。
  上流の明確な拒否・投入前の失敗では見積もりを返す（全体・ライセンス両方）
- 実額への置き換えは 2 経路、**先に走った方が 1 回だけ**（`settleSpend` は既精算を無視する）:
  端末が結果ストリームを最後まで読んだとき、または**精算スイープ**（本番は Cron Trigger で 15 分ごとに `scheduled` が走る。Node 起動では
  `SETTLE_INTERVAL_SECONDS`、既定 900 秒。1 回に最大 `SETTLE_MAX_PER_TICK`（既定 500）件、処理中だった束は
  `SETTLE_RECHECK_SECONDS`（既定 900 秒）の間は再問い合わせしない。同じ tick で台帳の保持期限切れ行の削除も行う）。
  スイープは直近 3 日の未精算・束縛済みバッチをプロバイダに問い合わせ（URL は固定、id は台帳の所有者行から）、`ended` なら
  結果を 1 行ずつ流し読みして usage × 単価表を記帳する（どこにも保持しない）。プロバイダが明確な 4xx で答えたバッチは
  見積もりを確定額として閉じる（過大側）。通信・5xx の失敗は次回へ。ログは `settlement_sweep {"checked":…,"settled":…,"abandoned":…,"pending":…,"failed":…}`
  の件数だけ。`abandoned` が続くなら所有者行とプロバイダの食い違い、`failed` が続くならプロバイダ到達性を見る
- **`system` の SHA-256 許可リストは入れない（判断、監査 H1-3）。** 配布ビルドの `system` は consolidation と morning brief の
  2 purpose では固定文字列だが、残る 4 purpose（meeting_recap / mail_triage / about_profile / daily_activity_summary）は
  `system` を持たず、プロンプト全体が `chunk` に乗る（1 ユーザーの内容そのものなので固定できない）。許可リストは
  6 経路のうち 2 つしか塞げず、しかも Rust 側のプロンプト文言と中継の env をロックステップで配布しないと（旧版・新版の
  両ハッシュを並べない限り）アプリ更新ごとに Batch が止まる。汎用プロキシ化を金額で塞ぐのがライセンス単位の予算で、
  許可リストはそれに何も足さない。再検討するなら、全 purpose を `system`（固定）＋ `data` の 2 分割に揃えてからにする
- 単価表: `apps/api/src/spend.ts` の `DEFAULT_BATCH_PRICES`（Batch 単価、2026-10-12 時点: Haiku 5.5 $0.05 / $0.25、
  キャッシュ読み 0.1 倍・1h 書き 2 倍。Haiku 4.5 $0.50 / $2.50 の行は古い端末の精算用に残す。Sonnet 5 / 5.5 $1 / $5）。
  `models.ts` のモデルを変えたらここも変える。表に無いモデルは最も高い行の単価で計上し、固定の下限 `UNKNOWN_MODEL_FLOOR`
  を下回らない（無料・安価扱いにしない。日付付きスナップショット `…-YYYY-MM-DD` は元のモデルの行で計上）。
  `SPEND_PRICE_TABLE_JSON` は既定の表に**重ねて**行を上書きする（2026-10-20 監査 L2）。**Haiku 5.5 は 100K トークン超のプロンプトが 10 倍単価**（$0.50 / $2.50）で、
  192 KiB の CJK 議事録はそこに掛かり得るので、表の行は `longPrompt` に 2 枚目の単価を持ち、予約（バイト/2 の見積もり）と
  精算（`input + cache_read + cache_creation`）の両方でプロンプト長に応じた方を使う（`spend.ts` `rateCardFor`）。Haiku の全レーンは `output_config.effort`（分類・抽出 `low`、Recap `medium`）を中継が付け、
  `classify` の `max_tokens` は thinking 込みで 2,048。約 2.9 KB の共有 instruction は 512 トークン最小を越えるので
  1h の `cache_control` が付く（Haiku 4.5 の 4,096 では付かなかった）
- **毎時の review（`/v1/review`、2026-10-20 監査 C1）。** 2 回分（最初の呼び出しと、空・不正 JSON への 1 回の再試行）を予約し、
  OpenAI が請求した分で閉じる: 全試行の `usage` の合計（打ち切り・フィルタ・拒否・不正 JSON でも請求されるので計上する）。
  請求が分からない試行（タイムアウト・切断・5xx）があれば予約全額を確定額として残し、`review_bill_unknown (reservation kept)` を
  ログに出す。チャンクを返すのはプロバイダに届かなかったときだけ。以前は失敗時に 0 で精算していた
- **プロバイダ側のハード上限（§2-5）は最後の壁として外さない。** ブレーカーは中継の見積もりに依存するので、単価表の誤りを守れない
- 事故時の即効: Worker の `DAILY_SPEND_CAP_USD` を `0` にする（`apps/api/DEPLOY.md`）→ 全投入が `spend_cap` 429（GET とローカル機能は無影響）

### 9-7. 負荷試験（FR-SVC-07）— ローンチ前に 1 回、手動で

CI には入れない（費用と再現性）。`apps/api/test/hardening.test.ts`（上限 10 に 50 同時 → ちょうど 10 受理）は CI に残る。

**手順**

1. ステージングの中継を用意する（本番と同じイメージ・同じストア構成。**上流キーは本番のものを使わない** — 可能なら上流をフェイクにする）
2. 試験用ライセンスでデバイストークンを N 本発行し 1 行 1 本のファイルにする（レート制限は licence + device 単位なので端末ごとに別トークン）
3. 実行（リポジトリのルートで）:
   ```
   pnpm --filter @shogun-ai/api load-test -- --url https://<staging-relay> --tokens-file tokens.txt \
     --devices 3000 --poll-per-hour 30000 --poll-seconds 3600 \
     --verify-url https://<staging-site> --verify-keys-file keys.txt --verify-rps 1000 --verify-seconds 60
   ```
   `keys.txt` は `<license_key> <device_id>` の行（ステージングの試験ライセンスのみ。本番キーを使わない）
4. 手元の予行演習は `--self-host`（`scripts/local-relay.ts` を別プロセスで起動、上流はフェイク、使い捨て鍵ペア）。
   `--store memory|postgres`（SQLite は 2026-10-12 に削除。下表の SQLite 行は履歴）、`--submit-over-sec S` で投入を S 秒に均す（FR-SVC-01 のジッタ相当）

**合格基準（spec §3）**

| 項目 | 基準 |
|---|---|
| `POST /v1/batch` | 受理までの p95 ≤ 1 s。429/503 は端末がローカルレーンに落ちる/再試行するので率そのものは UX に直結しないが、5xx（503 を除く）は 1% 未満 |
| `GET /v1/batch/{id}` 30,000/時 | 5xx 率 < 1%、p95 ≤ 1 s |
| `license/verify` 1,000 rps × 60 s | p95 ≤ 300 ms。失敗は端末の猶予で吸収されるが 5xx 率 < 1% |
| 共有側全断 | 検索・キャプチャ・Fusion・チャット・会議ノートは無影響（端末側で別途確認） |

**手元の予行演習の結果（2026-09-29、Linux コンテナ 1 台、上流フェイク 50 ms、負荷側と中継は別プロセス・同一ホスト）**

| 構成 | 投入 | 初回 202 | 最終 202（`Retry-After` 再試行 ≤3） | 受理 p95 | ポーリング 30,000/時 相当（30 s） |
|---|---|---|---|---|---|
| SQLite（既定） | 300 同時 | 23 / 300 | 245 / 300 | 672 ms | 250 件 5xx 0、p95 60 ms |
| SQLite（既定） | 3,000 同時 | 8 / 3,000 | 106 / 3,000 | 16.4 s | 250 件 5xx 0、p95 67 ms |
| SQLite（既定） | 3,000 を 60 s に均す | 2,920 / 3,000 | 2,988 / 3,000 | 132 ms（初回） | — |
| Postgres 16（共有ストア、プール 10） | 300 同時 | 300 / 300 | 300 / 300 | 2.2 s | 250 件 5xx 0、p95 75 ms |
| Postgres 16（共有ストア、プール 10） | 3,000 同時 | 3,000 / 3,000 | 3,000 / 3,000 | 19.8 s | 250 件 5xx 0、p95 97 ms |
| Postgres 16（共有ストア、プール 10） | 3,000 を 60 s に均す | 3,000 / 3,000 | 3,000 / 3,000 | 237 ms | — |
| in-memory（参考） | 3,000 同時 | 3,000 / 3,000 | 3,000 / 3,000 | 6.5 s | 250 件 5xx 0、p95 64 ms |

読み方:

- **完全同時の 3,000 はどの構成でも p95 1 s を満たさない**（1 プロセスの処理量は SQLite ≈ 50 件/s、Postgres ≈ 125–150 件/s、in-memory ≈ 430 件/s）。
  spec の想定は「3,000 台/時」（≈ 0.8 件/s）で、60 s に均しただけ（想定の 60 倍の密度）で両ストアとも p95 < 250 ms。**端末側のジッタ（FR-SVC-01）が前提**
- **SQLite 台帳は同時到着に弱い**: 待ち行列 8 件（`MAX_PENDING_USAGE`）を超えた分は即 503。1 投入が台帳操作を 3〜4 回（claim → reserve → bind）
  順に取りに行くので、群れの中では後段の操作が新着に枠を奪われ、初回受理がほぼ 0 になる。`Retry-After: 1` の再試行も同期しやすい。
  失敗はすべて fail-closed の 503（プロバイダ未呼び出し）で、二重投入は起きていない
- Postgres の処理量はプール数（既定 10）と往復遅延で決まる。実環境（別ホストの DB）では往復が増える分下がる
- `license/verify` はステージングのサイトと試験ライセンスが要るので、この予行演習では未実施

### 9-8. トライアルの中継トークン（issue #763）

7日トライアル中の端末はライセンスを持たないので、ライセンスAPI（`POST /api/license/trial`）が
端末ごとに `plan: "trial"` のトークンを発行し、中継がそれを bearer として受ける。
**発行に「インストール済みである証明」は無い**（device id は任意の文字列）。守りは次の5つで、
1つでも欠けた状態で有効化しない。

| 守り | どこ | 値 |
|---|---|---|
| 全体の日次金額ブレーカー | 中継 `DAILY_SPEND_CAP_USD` | **必須（中継自体が起動しない）。** 開発用に `RELAY_ALLOW_NO_SPEND_CAP=1` で外した中継ではトライアルのトークンは 402 |
| トライアルが使える予算の割合 | 中継 `TRIAL_SPEND_SHARE` | 既定 0.5。当日の合計支出（有償＋トライアル）がブレーカーのこの割合に達したら、トライアルの投入だけ `spend_cap` 429。**残りは有償ユーザーの分** |
| 支払い主体ごとの日次金額予算 | 中継 `TRIAL_DAILY_SPEND_CAP_USD` | 既定 $0.25（有償 $0.50 の 1/2。2026-10-12 まで $1.00）。超えると `license_spend_cap` 429（§9-6）。**アカウント経由（`acc` 付き）のトライアルはアカウント単位**で、そのアカウントが device id をいくつ使っても予算は 1 つ（2026-10-20 監査 H1）。`acc` の無い端末だけの経路は端末単位のまま |
| 支払い主体ごとの日次チャンク上限 | 中継 `TRIAL_DAILY_CHUNK_CAP` | 既定 500（有償 2000 の 1/4）。同じくアカウント単位 / 端末単位 |
| 会社の音声認識 | 中継 `ASR_ALLOWED_SUBJECTS` | 許可リストが無ければトライアルも使える（2026-10-22 オーナー決定）。予約は全体ブレーカーの `TRIAL_SPEND_SHARE` まで |
| 新規トライアル開始の上限 | website `TRIAL_START_PER_IP_PER_DAY` | IP あたり 1日20件、fail-closed |
| トークンの寿命 | website `buildTrialTokenPayload` | 最長 26 時間、トライアル終了を超えない |

最悪ケースの見積もり: 1 IP が 1日に開始できるのは 20 端末、各端末は 7 日間、1日 500 チャンクまで。
1 IP が毎日上限まで開始し続けると 7 日目に 140 端末 × 500 = 7万チャンク/日。IP をまたぐ分も含めて、
金額は `DAILY_SPEND_CAP_USD × TRIAL_SPEND_SHARE` で頭打ちになり、有償ユーザーの投入は止まらない。
アラート（§9-5）で `plans.trial` の `spend_capped` が増えていたら、乱用か割合の設定が低すぎるかのどちらか。

**有効化の順序（チェックリスト）。** 逆順でも壊れないが、デスクトップを先に出すとトライアル端末が
1時間ごとに `trial_token_failed` を記録してローカルレーンに留まる。

1. [ ] `DAILY_SPEND_CAP_USD` を決めて中継に設定する（オーナー判断 §8-4）
2. [ ] `TRIAL_SPEND_SHARE` と `TRIAL_DAILY_CHUNK_CAP` を確認する（既定 0.5 / 500。保守的な値から始める）
3. [ ] website をデプロイする（`trial_devices` テーブルは `db:migrate` が作る。追加のみ）
4. [ ] 中継をデプロイし、起動ログが `trial_lane open` であることを確認する
5. [ ] 検証用の device id で発行を確認する（その id のトライアルが1件始まる）:
       `curl -s -X POST https://shogunaios.com/api/license/trial -H 'content-type: application/json' -d '{"device_id":"<検証用のid>"}'`
       → `entitled: true` とトークン
6. [ ] ここまで済んでからデスクトップのビルドを配布する

**止め方。** Worker の `TRIAL_DAILY_CHUNK_CAP` を `0` に（トライアルの投入だけ `daily_cap` 429。有償は無影響、
端末はローカルレーンに落ちる）、または `TRIAL_DAILY_SPEND_CAP_USD` を `0` に（手順は `apps/api/DEPLOY.md`）。`DAILY_SPEND_CAP_USD` を外して
止めない: 2026-10-10 以降は中継が起動しなくなる。

### 9-9. Jev フォーム判断（`POST /v1/ai/form_decide`、issue #1144 / #1101 D12）

Scribe の「fill this form」と Memory API のフォーム記入で、ユーザーが自分の TypeSafe キーを保存していない端末は、
この経路で会社の TypeSafe キーを使う（自分のキーがある端末は中継を通らず TypeSafe 直結）。中継は端末の
`{state, questions}` に固定モデル `jev-1.13.0` と `Authorization: Bearer <TYPESAFE_API_KEY>` を付けて
`https://api.typesafe.ai/v1/systemone` を 45 秒タイムアウトで 1 回呼び、TypeSafe の JSON をそのまま返す。
本文（欄のラベル・候補の事実・TypeSafe の答えとエラー本文）は保存もログ出力もしない。

**キーの置き方。** Worker secret だけ（`wrangler secret put TYPESAFE_API_KEY`、手順は `apps/api/DEPLOY.md`）。
リポジトリ・GitHub（Actions secret を含む）・`vars`・`.env` に置かない。無い間は経路が 503 を返し、端末は何も記入しない
（一般のチャットモデルで代替しない）。起動ログの `form_decide_lane open` / `closed` で確認する。

**上限。**

| 上限 | 値 | 超えたとき |
|---|---|---|
| 支払い主体ごと（ライセンス。アカウント経由のトライアルはアカウント — `meterSubject`、2026-10-20 監査 H1） | 30 件 / UTC の 1 時間（共有ストアの固定窓。idle 削除で戻らない） | 429 `rate_limited` + `Retry-After`（窓の終わりまで） |
| 端末ごとの同時実行 | 2（共有ストアの slot。死んだ呼び出しの slot は 60 秒で戻る） | 429 `rate_limited` + `Retry-After: 2` |
| 金額 | 既存の日次ブレーカーと支払い主体別の上限（§9-6、`/v1/review` と同じ `meterSubject`）。予約 = 本文バイト ÷ 4 × $0.042/M（出力は無料）、TypeSafe の `usage.input_tokens` で精算 | 上限到達の 429（`Retry-After` なし） |

フォーム判断は Batch の日次チャンク数を消費しない（呼び出し中だけ 1 を予約し、終わったら戻す）。
レートの全体キー（`RATE_BURST` / `RATE_PER_MIN`）も `form_decide` 用の別バケットで同時にかかる。

**TypeSafe の応答の変換。** 200 はそのまま。401/403（会社キーが拒否された）は 503 にして
`ALERT typesafe_key_rejected status=401` を 1 行出す — 端末に「キーを追加して」とは言わせない。
429 → 429 `rate_limited`（TypeSafe の `Retry-After`、無ければ 30 秒）。529 → 503 `overloaded`。422 → 422。
それ以外と通信失敗・タイムアウトは 502。精算は請求額を下回らない側に倒す（2026-10-20 監査 C1、`/v1/review` と同じ）: 408 以外の 4xx（判断していない）は 0、5xx・529・通信失敗・タイムアウト・`usage` の無い 200 は請求された可能性があるので予約額をそのまま計上する（5xx 等は `form_bill_unknown (reservation kept)` を 1 行出す）。

**アラート（Workers Logs）。**

| アラート | 条件 | 対応 |
|---|---|---|
| 会社キー拒否 | `ALERT typesafe_key_rejected` が 1 件でも | 下のローテーション。TypeSafe のコンソールでキーの状態と残高を確認 |
| TypeSafe 側の混雑 | `event=upstream_rate_limited` / `event=upstream_overloaded` が 5 分で多数 | TypeSafe の利用上限・ステータス。必要なら TypeSafe に上限引き上げを依頼 |
| 経路の停止 | `event=form_decide_unavailable` が出続ける | 起動ログが `form_decide_lane closed` なら secret の設定漏れ |
| 金額 | `spend.consumption > 0.8`（§9-5）と、TypeSafe コンソールの請求アラート | TypeSafe 側のハード上限と請求アラートも必ず設定する（§2-5） |

**キーのローテーション。**

1. [ ] TypeSafe のコンソールで新しいキーを作る（古いキーはまだ消さない）
2. [ ] `wrangler secret put TYPESAFE_API_KEY --config apps/api/wrangler.jsonc`（stdin で渡し、履歴に残さない）。secret の更新で新しい isolate が新しいキーを読む
3. [ ] 認証済みの検証用ライセンスで 1 件フォーム判断を通し、Workers Logs に `POST /v1/ai/form_decide 200 … provider=typesafe` が出ることを確認する
4. [ ] TypeSafe のコンソールで古いキーを無効にする。以後 `ALERT typesafe_key_rejected` が出ないことを確認する

漏えいの疑いがあるときは 1 → 2 → 4 を先に（3 は後で）行う。端末側の作業は不要（会社キーは端末に無い）。

**ローンチ前の確認（オーナー判断待ち）。** TypeSafe の保持・ゼロデータ保持（ZDR）条件を書面で確認する。
確認できるまで、公開文書は「中継は保存しない、TypeSafe が処理する」とだけ書き、TypeSafe 側の保持期間を約束しない
（`docs/ai-gateway-launch-checklist.md`）。
