# AI ゲートウェイ ローンチ前チェックリスト（#1101 Phase 0 / D10 / D11）

- 対象: オーナーと運用担当が手で行う作業。コードの変更は含まない（Azure 経路・削除処理の実装は §4）
- 関連: [hosted-ai-gateway-design.md](hosted-ai-gateway-design.md)（設計）/ [ai-gateway-runbook.md](ai-gateway-runbook.md)（運用）/ `apps/api/DEPLOY.md`（コマンドの正）/ [public-release-remaining-work.md](public-release-remaining-work.md) P0-F2 / CLAUDE.md 不変条件 3・5 / 要件 FR-AG-06f・NFR-PRV-05
- 作成時点の状態（2026-10-14）: `api.shogunaios.com` は **NXDOMAIN のまま**（Cloudflare DoH が Status 3 を返す）。`apps/api/wrangler.jsonc` の Hyperdrive id は `REPLACE_WITH_RELAY_HYPERDRIVE_ID` のまま。relay に Azure の経路は無い
- **2026-10-18 更新:** relay は稼働中（`/healthz` 200、起動時に必須の設定はそろっている）。Hyperdrive id は `wrangler.jsonc` に入り、Worker は配信中（1-2〜1-5・1-9 は結果から済みと読める。1-1・1-6〜1-8 は外から確かめられない）。残りは 1-10〜1-14 の確認と、`deploy-relay.yml` が `CLOUDFLARE_API_TOKEN` の Hyperdrive 読み取り権限不足で失敗している件。全体の棚卸しは #1193

**#1101 本文の Phase 0 は古い。** Fly の app・`gateway_data` volume・`FLY_API_TOKEN`・`deploy-gateway.yml` は 2026-10-12（PR #1022）に Cloudflare Workers + Supabase（Hyperdrive）へ置き換わっており、main に存在しない。以下は現行構成の手順。

---

## 1. Phase 0 — 既存の Batch relay を本番に出す（P0-F2。ほかの全部より先）

出さない限り、配布済みのビルドは毎晩 Batch に失敗し、Morning Brief は抽出のみの版に落ちる。2026-10-13 の上限処理（Batch で確認されていない約束・未解決事項は 0.4 に抑える）が入ったので、relay が無いと Today の約束・未解決事項はほぼ空になる。

| # | 作業 | 誰が | 確認 |
|---|---|---|---|
| 1-1 | Supabase のプロジェクトが一時停止しないプランであることを確かめる | オーナー | ライセンス DB と同じプロジェクト（DEPLOY.md §1 の注記） |
| 1-2 | relay 専用ロール `shogun_relay` と `relay` スキーマを作り、`apps/api/sql/shared-store.sql` をそのロールで流す（DEPLOY.md §1） | オーナー | `\dt relay.*` に `relay_*` の6テーブル、`public` の表は `permission denied` |
| 1-3 | relay 用の Hyperdrive を **キャッシュ無効** で作る（`wrangler hyperdrive create shogunai-relay-store --caching-disabled …`、直結か session pooler、`sslmode=require`） | オーナー | `wrangler hyperdrive get <id>` が `caching: { disabled: true }` |
| 1-4 | Hyperdrive の id を `apps/api/wrangler.jsonc` の `hyperdrive[0].id` に入れる PR を出してマージ（id は秘密ではない） | Claude でも可 | `deploy-relay.yml` の placeholder ガードが通る |
| 1-5 | Worker の secret を入れる: `ANTHROPIC_API_KEY`（Morning Brief）、`OPENAI_API_KEY`（クレジットのある会社 OpenAI org の、Batch 専用プロジェクトのキー。`BATCH_PROVIDER=openai` の既定で必須）、任意で `METRICS_TOKEN` | オーナー | 値は Worker の secret だけ。GitHub・`vars`・チャットに貼らない |
| 1-6 | `vars` の `DAILY_SPEND_CAP_USD` を有効ライセンス数 × $0.50 × 1.5 で見直す（同梱値は 20 ライセンス分の `15.00`） | オーナー | DEPLOY.md §3 |
| 1-7 | リポジトリ secret `CLOUDFLARE_API_TOKEN`（Workers 編集 + Hyperdrive 読み取り）/ `CLOUDFLARE_ACCOUNT_ID` を確かめる（website の `deploy.yml` と共用） | オーナー | — |
| 1-8 | `api.shogunaios.com` に手作りの A / CNAME が残っていれば消す（custom domain の作成と衝突する） | オーナー | DEPLOY.md §4 |
| 1-9 | Actions → **Deploy batch relay (Cloudflare Workers)** を main で `deploy` にチェックして実行 | オーナー（実行の判断） | ワークフローの最後で `/healthz` が 200 |
| 1-10 | 初回デプロイ後の確認（DEPLOY.md §6）: `/healthz` → `{"ok":true}`、cron の `settlement_sweep`、テスト用トークンでのプローブ Batch が 202、別ライセンスからの GET が 404、`RATE_BURST` 超えで 429、ログに本文・トークン・id が出ていない | オーナー（トークン発行）+ Claude（結果の読み取り） | `relay.relay_usage_days` にプローブの1行 |
| 1-11 | WAF のレート制限ルールを **`/v1/` で始まるパス全体**（`/v1/batch*`・`/v1/review`・`/v1/asr/*`）に1本（既存が `/v1/batch` だけなら広げる。2026-10-20 監査 L1）、Workers Logs の保持を最短に | オーナー | DEPLOY.md §4 |
| 1-12 | **必須（2026-10-20 監査 M3）: provider 側のハード上限。** 実ユーザーの通信を流す前に provider ごとに全部: **OpenAI** — レーンごとに別プロジェクト・別キー（今は Batch レーン = Batch と `/v1/review`。live レーンは出す時に別プロジェクト）、月の予算（上限で止まる設定）、使うモデルの低いレート制限、請求アラート 50/80/100%。**Groq** — 同じ（dictation cleanup レーンを出す前に）。**Anthropic** — 中継キー専用 workspace の月の利用上限・低いレート制限・通知。**Deepgram** — 専用プロジェクト、自動チャージを切った前払いクレジットで止まる月の予算、残高アラート、`ASR_ALLOWED_SUBJECTS` は 2026-10-22 のオーナー決定で外し、有償プランとトライアルの全員に開放した（前払いの上限が最後の壁）。アラートだけでは完了にしない | オーナー | runbook §2-5 / DEPLOY.md「Provider-side hard limits」。provider ごとに日付付きのスクリーンショットか書き出しを残す |
| 1-12a | Workers Logs に `ALERT speech_` 行のアラートを置く（音声のプロジェクトで音声以外の利用が出た印。2026-10-20 監査 H3 で全体の自動停止をやめた代わり） | オーナー | DEPLOY.md「Company-funded speech」の Non-speech usage |
| 1-13 | リリースビルド（0.2.7 以降。`client_request_id` を送る #711 を含む版）で一晩回し、`generated=true` の Morning Brief と #573 の recover 経路を確かめる | オーナー（実機） | P0-F2 / P0-D4 を閉じる |
| 1-14 | `public-release-remaining-work.md` の P0-F2 を「完了」に更新 | Claude | — |

注意: デバイス側の Batch の約束（schema）を変えるリリースは、対応する relay を **先に、または同時に** 出す（DEPLOY.md §5）。

---

## 2. Azure OpenAI（D10）

**2026-10-15 オーナー改定: OpenAI のクレジットを先に使う。** 第一経路は OpenAI 直結（relay の現行実装のまま）。Azure は免除の承認後にフォールバックとして加え、OpenAI のクレジットが尽きたら第一経路にする。したがって Azure は Phase 0 とローンチの前提ではない。Azure の経路は relay / gateway にまだ実装されていない（§4）。下の申請・準備はコードと並行して始められる。**免除（2-4）が承認されるまで、実ユーザーの通信を Azure に流さない**（D11）。

| # | 作業 | 確認・メモ |
|---|---|---|
| 2-1 | Azure と OpenAI 両方のクレジットの種類・額・**期限**を確かめる | Azure のスタートアップ向けは「引き換えから90日 / 確認から180日、延長なし」とされている。**OpenAI を先に使う間に Azure の期限が来ると失効する**。Azure の期限が OpenAI の消化見込みより早いなら、オーナーに順番の見直しを相談する |
| 2-2 | Azure の予算アラートを 50 / 80 / 100 % で設定 | サブスクリプション単位。宛先は監視しているメール |
| 2-3 | Azure OpenAI（Foundry）リソースを US リージョンに作成。デプロイを **live と batch で分ける**: live = Global Standard（US 内処理にこだわるなら Data Zone US）、batch = Global Batch。モデルは `gpt-6-luna` と現行のチャットモデル | live と batch でキー・クォータ・費用を分離（#1101 §4 の breaker） |
| 2-4 | **modified abuse monitoring の免除を申請**（Azure は既定で不正利用審査のためプロンプトを最大30日保存する。免除でそれを止める） | **要確認: 申請資格。** Microsoft が対象顧客を限定している場合があり、クレジットだけのスタートアップ契約で通るかは申請して確かめる。通らなければ Azure は使わない（OpenAI 直結のまま。ゼロ保持を優先） |
| 2-5 | TPM クォータ（Global Standard）と Batch のエンキュー上限を確かめ、足りなければ引き上げ申請 | 既定値は小さいことがある |
| 2-6 | live デプロイに1回だけ実呼び出しして、`gpt-6-luna` + `service_tier: "priority"` が実際に付くか（応答の `service_tier`）を確認 | Azure は付けられないとき黙って default で返す。rewrite の priority 前提を確かめる |
| 2-7 | キーとエンドポイントは Worker の secret / `vars` にだけ置く（Fly ではない） | 変数名は実装 PR で決める。live と batch でキーを分ける |

---

## 3. ゼロ保持（D11）— provider ごとの申請と設定

ローンチ前に全部そろえる。コードが通っただけでは「確認済み」にしない。

| provider | 作業 | 確認 |
|---|---|---|
| OpenAI | org の **Zero Data Retention** を申請。live と batch でプロジェクトを分け、プロジェクト予算とハード上限を設定 | 承認の記録。ZDR は Files / Batch を対象にしないので、Batch ファイルは relay が読了後に削除する（実装済み、`apps/api/src/openai-gateway.ts`） |
| Azure OpenAI | §2-4 の免除 | 承認の記録 |
| Groq | Data Controls で **ZDR を有効化**（セルフサービス）、利用上限を設定 | 設定画面の記録 |
| Deepgram | 専用プロジェクトと Member 権限のキー、止まる月の予算（§1-12）と残高アラート。音声・文字起こしを保存しないことの **書面確認**（またはゼロ保持の追加契約） | 書面 |
| TypeSafe（Jev フォーム判断、#1144 / D12） | 会社キーを Worker secret `TYPESAFE_API_KEY` に置く（`ai-gateway-runbook.md` §9-9）。コンソールでハード上限と請求アラート。フォームのラベル・候補の事実を保存しないこと（保持期間・ZDR）の **書面確認** — 条件の判断はオーナー（未決） | 書面。確認までは公開文書で TypeSafe 側の保持を約束しない |
| Anthropic | **2026-10-15 オーナー決定（C）: 結果の削除だけ先に入れ、ゼロ保持の申請は後回し。** Morning Brief は Anthropic Batch のまま。relay は Mac が結果を読み終えた時点で `DELETE /v1/messages/batches/{id}` を呼び、入力と結果を Anthropic から消す（`FetchAnthropicGateway.releaseResults`）。Mac が一度も読まなかった batch は Anthropic の既定どおり作成から29日で結果が消える | ローンチ前に、Anthropic のゼロ保持を申請するかをもう一度判断する。申請しないなら privacy 文書に「Morning Brief の処理中と未読時は最大29日 Anthropic に残る」と書く |
| 公開文書 | privacy 文書（`apps/docs/privacy/*`）と subprocessor 一覧に、各 provider の保持設定・gateway のホスティング（Cloudflare Workers / Supabase 東京）・通過するもの（live の処理用テキスト、保存しない）・音声は Deepgram 直結を書く | 稼働に合わせて更新。先に書かない |
| 運用 | 月1回、上の設定が有効なままかを確認する | 記録を残す |

---

## 4. この後のコード作業（参考。別 PR）

- relay に Azure provider を追加（OpenAI 互換 v1 エンドポイント、`api-key` ヘッダー）。lane 設定で第一経路 / フォールバックを並べ、最初のバイトより前だけ切り替える。provider ごとのサーキットブレーカー。応答の実 tier を記録
- Azure Batch でも OpenAI と同じ「入力は結果を読んだら削除、結果は Mac が読み終えたら削除、放置分は掃除」を実装
- Anthropic の未読 batch（Mac が読まなかったもの）を早めに消す掃除。OpenAI の `sweepOrphans` と違い batch に印を付けられないので、relay の台帳にある id だけを対象にする
- Phase 2 の live lane・Deepgram grant・上限は [hosted-ai-gateway-design.md](hosted-ai-gateway-design.md) §3〜§5
