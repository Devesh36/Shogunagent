# ユニットエコノミクス — 会社負担レーンの原価、原価率、スケール時の損益

**作成:** 2026-10-12。計算は `scripts/unit-economics.py`（標準ライブラリのみ、パラメータは先頭に集約）。この文書の表はそのスクリプトの出力をそのまま貼っている。数字を変えたいときはスクリプトを直して再実行する。
**前提の出典:** `docs/select-kk-cost-zero-base-plan.md`（2026-09-22 のレーン別試算）、`apps/api/src/models.ts`（モデル選択）、`apps/website/src/lib/pricing.ts`（価格）、`crates/shogun-core/src/dreamcycle/jobs.rs` の注記（約 1,650 行/夜）、Claude API の料金表（2026-10-12 確認）。
**未計測であること:** 1 ユーザーの夜間 Batch 量は**実測前**。`llm_usage` 台帳（V30）と `shogun dream | jq .last_send` が実数を出すので、最初の実機ユーザー数名分で本書の「typical」を置き換えるのが第一の作業。

---

## 0. 結論

1. **今のコード（Haiku 4.5、1 行 1 リクエスト）だと Standard の原価率は 100% 超。** 典型ユーザーで LLM だけ月 $68（$49 の 139%）、重いユーザーで $173。ユーザーが増えても赤字が比例して増えるだけで、スケールで解消しない。
2. **一番安くて速いレバーは Haiku 5.5 への切り替え（`models.ts` の 4 行）。** Batch 単価が $0.50/$2.50 → $0.05/$0.25 で**10 分の 1**。トークナイザが約 30% 増え、低 effort でも思考トークンが少し乗る（1 リクエスト 150 tok と仮定）のを織り込んでも、典型 $68 → **$9（19%）**、重いユーザーでも $23（47%）。指示文は 512 トークンからキャッシュに乗るようになり、夜 1 回の書き込み以外は 0.1 倍で読まれる。
3. **セッション化（ゼロベース計画の L1）を足すと典型 $2.08（4%）。** 2 つやれば原価率は 1 桁%になり、Haiku の品質差や相場変動を気にしなくてよくなる。
4. **relay のライセンス単位の日次予算 $3 は、価格に対して大きすぎる。** $3/日 = $90/月 = Standard 年払い月額の 184%。Haiku 5.5 切替後は **$0.50/日（月 $15 ≒ 30%）**、トライアル **$0.25/日** が妥当。全体ブレーカー `DAILY_SPEND_CAP_USD` はユーザー数 × $0.50 × 1.5 程度。
5. **固定費は月 $60〜85 程度**で、ユーザー 2〜3 人で回収できる。問題は固定費ではなく変動費（LLM）の単価。
6. **Fly は必須ではない。** 中継（relay）は「会社の Anthropic キーを端末に渡さずに Batch を叩く場所」であって、Fly 自体に依存はない。Postgres ストア（既存の `pg-store.ts`）を使えば、既に動いている Cloudflare Workers（website）と Supabase に載せ替えられる。追加費用ほぼ $0、ただし移植作業が数日分ある（§4）。

---

## 1. 収益側

| 項目 | 値 | 出典 |
|---|---|---|
| Standard | 年払い $588（月額換算 **$49**）／月払い $62 | `pricing.ts` |
| Pro | 年払い $1,188（月額換算 **$99**）／月払い $124 | 同 |
| トライアル | 7 日、Pro 相当、無課金 | CLAUDE.md |
| Stripe 手数料 | 3.6%（日本発行カード想定。米国なら 2.9% + $0.30） | 要確認 |
| ARPU（想定ミックス: Pro 20%、月払い 30%） | **$63.62 / ユーザー・月** | スクリプト |

注意: 2026-10-10 のセキュリティ監査レポート §1 H1 で「年 $49」と書いたのは誤りで、正しくは**年 $588（月額換算 $49）**。原価率の分母は $49/月で見るのが正しい（本書で統一）。

## 2. 原価側（会社が払うもの）

### 2.1 変動費: Select KK キーで動く Batch レーン

ユーザーの資格情報で動くもの（チャット・ドラフト・Scribe・会議ライブ要約・翻訳・Quick Translate・Form mode の Jev・Deepgram ASR）は**会社の原価に乗らない**。会社が払うのは夜間 Batch と Brief だけ。

| レーン | モデル（現状） | 1 ユーザーあたりの量（典型） | 備考 |
|---|---|---|---|
| A. Dream Cycle 消化 | Haiku 4.5 Batch | **1,650 行/夜**、行ごとに指示 700 tok（非キャッシュ）＋本文 1,200 tok、出力 150 tok | `jobs.rs` 注記。前処理フィルタ（#286）はあるが削減率は未計測 |
| B. Mail triage Stage B | Haiku 4.5 Batch | 50 スレッド/日（上限 300）、1 件あたり指示 ≈1,000 tok（PR #1022 までは毎チャンクに同梱・非キャッシュ）＋スレッド ≈500 tok | PR #1022 で指示を共有 `system` に移しキャッシュ対象に |
| C. Morning Brief | Sonnet 5.5 Batch | 1 通/日、入力 6k・出力 1.5k tok | |
| E. 会議 Recap | Haiku 4.5 Batch | 20 回/月、入力 9k・出力 1.5k | |
| 日次サマリ／プロフィール | Haiku 4.5 Batch | 1 回/日／週 1 回 | 小さい |

Batch 単価（list の 50%、USD/1M tok）: Haiku 4.5 **$0.50 / $2.50**、Haiku 5.5 **$0.05 / $0.25**、Sonnet 5.5 **$1.00 / $5.00**。

### 2.2 固定費（月、概算。請求書で確定させること）

| 項目 | 月額 | 備考 |
|---|---|---|
| Fly.io relay（shared-cpu-1x 512MB ＋ 1GB volume、nrt） | ≈ $3.5 | §4 のとおり無くせる |
| Cloudflare Workers Paid（website ＋ ライセンス API） | $5 | |
| Supabase Postgres | $0 → $25 | 無料枠 500MB。ユーザー 300 人目安で Pro |
| Apple Developer Program | $8.25（$99/年） | |
| GitHub Actions（macOS 枠） | $30〜50 | `docs/actions-budget.md` |
| ドメイン・DNS | ≈ $1 | |
| PostHog | $0 | 100 万イベント/月まで無料 |
| Mintlify（公開ドキュメント） | **未確認** | 無料枠か有料かを確認 |
| **合計** | **≈ $58〜83** | |

ユーザー数に比例しないので、原価率への影響はユーザー 100 人を超えると 1% 未満。

## 3. 結果

### 3.1 LLM 原価（1 ユーザー・月、USD。括弧は $49 に対する比率）

| モデル選択 | light（EN、OCR off） | typical（1,650 行/夜） | heavy（JA、OCR on） |
|---|---|---|---|
| A. 現行コード（Haiku 4.5、1 行 1 リクエスト） | $25.24（52%） | **$67.94（139%）** | $172.82（353%） |
| B. Haiku 5.5 に切替（`models.ts` の行変更。+30% トークン・低 effort の思考・指示文キャッシュ込み） | $3.58（7%） | **$9.08（19%）** | $23.05（47%） |
| C. セッション化（L1、約 60 セッション/夜）、Haiku 4.5 | $11.77（24%） | $12.89（26%） | $16.85（34%） |
| D. セッション化 ＋ Haiku 5.5 | $1.90（4%） | **$2.08（4%）** | $3.08（6%） |

レーン別（typical）: 現行は消化レーン A が $65.59 で全体の 97%。他は合計 $2.35。**直すべき場所は 1 つ。** B の内訳は消化 $8.36・メール $0.28・Brief $0.41・他 $0.03。

### 3.2 スケール時の損益（typical、月、USD。原価率 ＝（Stripe ＋ LLM ＋ 固定）／売上）

**A. 現行コード** — LLM $67.94/人・月。**損益分岐なし**（貢献利益が負）

| ユーザー | 売上 | Stripe | LLM | 固定 | 粗利 | 原価率 |
|---|---|---|---|---|---|---|
| 10 | $636 | $23 | $679 | $58 | **−$124** | 119% |
| 100 | $6,362 | $229 | $6,794 | $58 | **−$719** | 111% |
| 1,000 | $63,620 | $2,290 | $67,944 | $83 | **−$6,697** | 111% |
| 10,000 | $636,200 | $22,903 | $679,435 | $83 | **−$66,221** | 110% |

**B. Haiku 5.5 切替** — LLM $9.08/人・月。損益分岐 1 人

| ユーザー | 売上 | Stripe | LLM | 固定 | 粗利 | 原価率 |
|---|---|---|---|---|---|---|
| 10 | $636 | $23 | $91 | $58 | $465 | 27% |
| 100 | $6,362 | $229 | $908 | $58 | $5,168 | 19% |
| 1,000 | $63,620 | $2,290 | $9,075 | $83 | $52,172 | 18% |
| 10,000 | $636,200 | $22,903 | $90,751 | $83 | $522,463 | 18% |

**D. セッション化 ＋ Haiku 5.5** — LLM $2.08/人・月

| ユーザー | 売上 | Stripe | LLM | 固定 | 粗利 | 原価率 |
|---|---|---|---|---|---|---|
| 10 | $636 | $23 | $21 | $58 | $535 | 16% |
| 100 | $6,362 | $229 | $208 | $58 | $5,867 | 8% |
| 1,000 | $63,620 | $2,290 | $2,082 | $83 | $59,165 | 7% |
| 10,000 | $636,200 | $22,903 | $20,822 | $83 | $592,392 | 7% |

（C の表はスクリプト出力にある。原価率 24〜33%。）

### 3.3 relay の予算の見直し

| 設定 | 現在の既定 | 現行コードでの意味 | Haiku 5.5 切替後の推奨 |
|---|---|---|---|
| `LICENSE_DAILY_SPEND_CAP_USD` | $3.00（月 $90 = 184%） | typical $2.26/夜なので**ほぼ常に通る**＝赤字を止めない。heavy は毎晩 cap で切られ、夜間レビューが欠ける | **$0.50**（月 $15 ≒ 30%。typical $0.30/夜の 1.7 倍の余裕。予約は bytes/2 で過大なので、実測で heavy が切られるなら $0.75 へ） |
| `TRIAL_DAILY_SPEND_CAP_USD` | $1.00 | トライアル 7 日で最大 $7 の会社負担 | **$0.25**（7 日で最大 $1.75） |
| `DAILY_SPEND_CAP_USD`（全体） | 未設定だと起動拒否 | — | **有効ライセンス数 × $0.50 × 1.5**。100 人なら $75/日。月初に見直す |

予約は bytes/2 の過大見積りなので、実使用の 2〜3 倍で cap に当たる点に注意（監査 M9 の意図どおり安全側）。精算スイープが実使用で上書きするので、翌日以降の予算は正しく戻る。

## 4. Fly なしでできるか

**できる。** relay が Fly にある理由は 2026-09-30 のホスティング決定（東京 1 台＋永続ボリューム）だけで、コードは Fly 固有ではない。

| 方式 | 何が要るか | 月額 | 判断 |
|---|---|---|---|
| **今のまま Fly** | `DAILY_SPEND_CAP_USD` を設定してデプロイ。DNS `api.shogunaios.com` を向ける | ≈ $3.5 | 最速。ただし運用面が 1 つ増える（Fly の秘密・ボリューム・インスタンスロック） |
| **Cloudflare Workers に載せ替え** | relay は Hono なので `app.fetch` はそのまま動く。ストアは **Postgres（`pg-store.ts`、既存）** を Hyperdrive 経由で Supabase に。SQLite ワーカーと `instance-lock.ts` は Node 専用なので Workers では使わない（Postgres 経路では不要）。精算スイープ `setInterval` → Cron Trigger。結果ストリーミングは Workers の `fetch` でそのまま通る。本番の Postgres スキーマ `sql/shared-store.sql` を適用 | ≈ $0（Workers Paid $5 は website と共用） | **推奨。** 面が 1 つ減り、Postgres なら将来の複数インスタンスにもそのまま耐える。移植 2〜4 日＋検証。CPU 時間（Workers Paid 既定 30 秒/リクエスト）は Ed25519 検証と 8MiB の JSON 解析で足りる |
| **relay を無くす（Batch も BYOK）** | Standard が「キー無しで動く」前提を捨てる | $0 | **不採用。** 製品の約束（観測系は Select KK キーで動く）を壊す。会社キーを端末に置くのは不変条件違反 |

Workers 版の注意: Supabase 無料枠は接続数が少ないので Hyperdrive 必須（website は既に使っている）。relay の Postgres 書き込みは 1 リクエスト 1 トランザクションで短いので問題ない。

## 5. 「減価率」について

本書で出しているのは**原価率**（売上に対する直接費の比率）と**粗利**。固定費の償却（開発費・Mac・初期の macOS CI など）を月割りで載せたい場合は、`Business.fixed` に `"amortized_dev"` のような行を足せば同じ表に乗る。CAC（獲得コスト）を入れる場合も同じ場所。スクリプトはそのために固定費を辞書で持っている。

## 6. やること（順番）

1. **【PR #1022 で実施中】`models.ts` の `classify` / `summarize` / `about_profile` / `daily_activity_summary` を `claude-haiku-5-5` に**（`output_config.effort` は中継が選ぶ）。**【2026-10-13 オーナー決定で上書き】** main 側でこの 4 クラスの既定プロバイダは OpenAI `gpt-6-luna`（OpenAI Batch API、`docs/batch-relay-design.md` §4.4）になった。Haiku 5.5 の行は `BATCH_PROVIDER=anthropic` のときの行として残る。luna の Batch 単価は未確認のため本書の B〜E 行は Anthropic 行の試算であり、luna 既定での原価は単価確認後に別途置き換える（中継の予算は luna を最悪行で予約するので、確認までは早めに止まる方向に外れる）。relay だけの変更（端末は `model_class` しか送らない）。Haiku 5.5 の構造化出力・`max_tokens` 8192 の互換を 1 晩分のリプレイ（`crates/extract-eval`）で確認する。`spend.ts` の価格表に Haiku 5.5 の行を足す。
2. **【PR #1022 で実施中】relay 予算を §3.3 の値に**（`LICENSE_DAILY_SPEND_CAP_USD=0.50`、`TRIAL_DAILY_SPEND_CAP_USD=0.25` が既定、`DAILY_SPEND_CAP_USD` は必須でユーザー数から）。
3. **実測で typical を置き換える**: `llm_usage`（V30）から 1 夜あたりの items・入力トークン・出力トークン、`shogun dream | jq .last_send` で前処理フィルタの削減率。本書の 1,650 行/夜はコード注記であって実測ではない。
4. **【PR #1022 で実施中】セッション化（L1）** を `docs/select-kk-cost-zero-base-plan.md` §3 どおりに実装（L0〜L3 を同 PR で）。Haiku 5.5 だけでも 15% に入るので急がないが、heavy ユーザー（37%）と将来の単価変動の保険になる。
5. **【PR #1022 で実施中・オーナー決定 2026-10-12】Fly → Workers の移植**（§4.1）。worker `shogunai-relay`、Supabase Postgres を Hyperdrive（クエリキャッシュ無効）経由、精算スイープは Cron Trigger 15 分、デプロイは `deploy-relay.yml`（`workflow_dispatch`）。Fly のファイルは削除。手順は `apps/api/DEPLOY.md`、運用は `docs/ai-gateway-runbook.md`。
6. 固定費の未確認項目（Mintlify、Stripe の料率）を請求書で確定し、スクリプトの値を直す。
7. **【PR #1022 で実施済み】Gmail の二重送信を止める（ゼロベース計画 L0(b)）。** Mail triage が ON のあいだ `gmail` 行は消化レーンに乗らず（`SessionPolicy::for_mail_triage`、OFF なら残る）、Stage B が人間の往復（`people` / `team`）を confidence に関係なく読んで `commitments` / `open_loops` を anchor 付きで返し、消化と同じ経路で state tables に書く。同じコミットの再監査（2026-10-12）で直した 3 点: (a) **relay の出力スキーマ**（`apps/api/src/schemas.ts`）が `commitments` / `open_loops` と `line` / `anchor` / `anchor_len` を列挙していなかった——構造化出力はスキーマに無いフィールドを黙って出さないので、消化レーンのセッション化も Stage B の約束抽出も本番では空になっていた。relay を端末より先に配る（`apps/api/DEPLOY.md` §5）。(b) 本文抜粋 1,200 字は**最新メッセージから**切る（`mail_triage::stage_b_excerpt`。スレッド先頭からでは、再送のきっかけになった返信が長いスレッドでは一度も見えない）。(c) Stage B の指示（ラベル定義・lesson・契約 ≈1,000 tok、1 件の入力の約 7 割）を消化レーンと同じ共有 `system` に移し、relay のキャッシュに乗せる。スクリプトの E 行（`gmail_on_stage_b_only`、`mail_instruction_cached`、`mail_share_sent=0.60`、`mail_action_out_tok=120`、`gmail_share_of_rows=0.15`）: typical で消化 $1.37 → $1.16、メール $0.28 → $0.43、合計 $2.08 → **$2.03/月**。**ほぼ中立（わずかに安い）**——このレバーは原価ではなく正しさ（1 スレッド 1 回の読み、メールの約束の取りこぼし無し）のためで、`gmail_share_of_rows` と送信割合は 3 の実測で置き換える。

### 4.1 Workers 載せ替えの評価（2026-10-12 追記）

**原価への影響: ほぼゼロ。** relay のホスティングは原価の 1% 未満で、どちらに置いても原価率は動かない。

| 項目 | Fly（現行） | Cloudflare Workers ＋ Supabase Postgres |
|---|---|---|
| ホスティング | ≈ $3.5/月（1 台） | Workers Paid $5 は website と共用なので増分 $0。10M リクエスト・30M CPU-ms/月込み |
| 1 ユーザー・夜のリクエスト | — | 送信 1〜2 ＋ ポーリング最大 11（`DREAM_MAX_POLLS`）＋ 結果取得 1 ≒ 13 → 月 400。**1 万ユーザーで月 4M**、込みの 10M 以内 |
| CPU 時間 | — | 8MiB JSON の解析＋Ed25519 検証で 1 送信 ≈ 100〜200 ms。1 万ユーザー × 30 夜 ≈ 45M CPU-ms → 込み分超過は $0.02/M で **月 $0.3** |
| 台帳 DB | Fly volume の SQLite | Supabase。無料枠は **7 日無操作で停止**するので本番は Pro $25/月。ただしライセンス DB が既に Supabase にあり、本番で停止は許されないので **Pro は relay に関係なく必要**＝増分 $0 |
| 1 万ユーザー時の DB 負荷 | — | 夜 13 万クエリ、約 3 時間に散るので **十数 QPS**。Hyperdrive（有料: 接続上限 60〜100）で十分。無料 Hyperdrive の 10 万クエリ/日は超える |
| スケール時に増えるもの | 台数 × $3.5 ＋ volume | 実質なし（10 万ユーザーで月 +$15 程度） |

**セキュリティ: 改善 3 点、要対応 5 点。**

改善:
- **IP 単位レート制限と DDoS 吸収がエッジで手に入る**（監査 Low「Fly 1 台の接続枯渇」の解消）。WAF のレート制限ルールを `/v1/batch` に 1 本置く。
- **パッチ対象のサーバーと永続ボリュームが無くなる。** 台帳は Postgres（RLS、TLS）に一本化、`instance-lock.ts` と SQLite ワーカーは不要になる。
- **水平スケールが自動**で、単一インスタンス前提のロックや machine-id ピンが消える。

要対応（やらないと退行）:
1. **Hyperdrive のクエリキャッシュは既定 ON（60 秒）。** 所有権（IDOR 判定）や冪等性の `SELECT` が古い結果を返し得るので、relay 用の Hyperdrive 設定は **`caching.disabled = true`** で作る（website 用とは別設定）。これは監査で「正しい」と判定した所有権チェックを壊さないための必須条件。
2. **データ経路の開示文を直す。** `apps/docs/privacy/mechanism.mdx` と `what-leaves-your-mac.mdx`、プライバシーポリシーは「Fly.io 東京」と明記している。Workers はユーザーに最も近い PoP で動く（日本のユーザーなら日本国内だが固定ではない）。OPEN-B1 の決定を更新し、文面を「Cloudflare のネットワーク上の relay」に変える。
3. **Ed25519 検証の実装確認。** 今は `node:crypto` の `verify(null, …)`。workerd の `nodejs_compat` で動くかを最初に確かめ、動かなければ WebCrypto `subtle.verify({name:"Ed25519"})` に置き換える（契約は変えない）。
4. **ログ。** Workers Logs は `console.*` を全部拾う。既存の redacting logger を唯一の出口として維持し、保持期間を短く設定する。
5. **Postgres への TLS を明示**（`sslmode=require`、監査 Low F8）と、relay 専用の DB ロール（licence DB の表に触れない）。

変わらないもの: 会社の Anthropic キーは Worker の secret binding（書き込み専用、コードからのみ参照）で、Fly の env と同等のリスク区分。マルチテナントの V8 isolate 上で動く点は Fly の共有 VM と同じ受容範囲。

**スケール: ボトルネックは Workers ではなく Anthropic 側。** Batch 作成は 1 ユーザー・夜 1〜2 回なので 1 万ユーザーで 1〜2 万回/夜。組織のレート上限は数千ユーザー規模で営業経由の引き上げが要る（ゼロベース計画 §の注記どおり）。セッション化（L1）は 1 リクエスト内の items を 20 分の 1 にするので、ここにも効く。Postgres は上記のとおり余裕があるが、台帳の tombstone 行が永続（監査 Low F5）なので、Pro の 8GB に収めるために保持期限を入れる。

**作業量: 2〜4 日。** `app.fetch` はそのまま、`boot.ts` の Node サーバーを Worker の `export default { fetch, scheduled }` に置き換え、env を binding から読み、精算スイープを Cron Trigger に移す。Postgres ストアは既にあるので台帳のコードは書かない。切替は DNS（`api.shogunaios.com`）の向け先変更 1 回で、端末側の変更は無い。
