# state 抽出 eval（SynthDay ゴールデン + 決定論スコアラー）

Issue #317 の **モデルを呼ばない部分**の実装記録と運用手順。要件は `docs/requirements-harness-v1.md` の FR-EV-02 / FR-EV-03 / FR-EV-10（ドラフト）。LLM で SynthDay を生成する部分・10 ペルソナへの拡張・judge は予算とキーが揃うまで保留（§5）。

## 1. 構成と実行

| 場所 | 中身 |
|---|---|
| `crates/extract-eval/src/golden.rs` | SynthDay ゴールデン形式 `synthday/v1`（ペルソナ・時刻付き合成イベント・正解 state 行）と同梱ゴールデンの読み込み |
| `crates/extract-eval/src/templates.rs` | 固定の合成テンプレートと架空の名前プール。**ゴールデンに入りうる文字列はここにあるものだけ** |
| `crates/extract-eval/src/generate.rs` | シード付き決定論ジェネレータ（SplitMix64、`src/rng.rs`）と `GOLDEN_SPECS` |
| `crates/extract-eval/src/score.rs` | スコアラー: フィールド別 P/R/F1（hallucination / omission 分離）、provenance 妥当性、確信度較正 |
| `crates/extract-eval/src/runners.rs` | `Candidate` → スコアラー入力のアダプタ、`extract_untrusted` ランナー、Batch リプレイ fixture の読み込み |
| `crates/extract-eval/src/lint.rs` | 「実データなし」lint（メールは `*.example`、URL も同様、7 桁以上の数字列禁止、人名はプール内のみ） |
| `crates/extract-eval/goldens/*.json` | 生成済みゴールデン 4 日分（**手で編集しない**） |
| `crates/extract-eval/replay/*.json` | Batch 分類結果の合成リプレイ fixture（`batch-replay/v2`: 2026-10-12 のセッション化以降、**1 セッション 1 結果**。`events` に根拠イベントの golden id、各項目は逐語引用ではなく `anchor`/`anchor_len`） |

リプレイは **Mail triage OFF 相当のセッション方針**（Gmail 行が消化レーンに残る）で回す。2026-10-12 の L0(b) 以降、本番では Mail triage ON のとき Gmail 行は消化レーンから外れ、約束・未決は Stage B（`mail_batch.rs`）が同じ anchor 契約で抽出する。ハーネスの `the_mail_source_leaves_the_replay_only_under_the_triage_on_policy` が、ON 方針では Gmail セッションだけが消えることを確かめる。
| `crates/extract-eval/tests/gate.rs` | CI ゲート（ゴールデン再生成一致・lint・オラクル整合・`extract_untrusted` の床） |
| `crates/shogun-core/tests/extract_eval.rs` | CI ゲート（`LocalRuleClassifier` と Batch リプレイ。`db` feature 必須） |

```bash
cargo test -p extract-eval                                            # ゲート（cargo test --workspace に含まれる）
cargo test -p shogun-core --features db --test extract_eval -- --nocapture   # Dream Cycle ランナーのゲートとレポート
cargo run  -p extract-eval                                            # extract_untrusted のレポート（Markdown）
cargo run  -p extract-eval -- regen                                   # goldens/*.json をジェネレータから書き直す
```

### CI への載り方（ワークフロー変更なし）

- `extract-eval` はワークスペースメンバーなので、`ci.yml` の `cargo test --workspace --exclude shogun-desktop-spike` がそのまま `tests/gate.rs` を走らせる。
- Dream Cycle 側のランナーは `shogun_core::dreamcycle::jobs`（`db` feature 限定）を使うため、`crates/shogun-core/tests/extract_eval.rs`（`#![cfg(feature = "db")]`）に置いた。`ci.yml` の `cargo test -p shogun-core --features db` ステップが拾う。
- **`extract-eval` から `shogun-core[db]` に依存させなかった理由**: そうすると resolver 2 の feature 統合で `cargo test --workspace` の全クレートが `shogun-core` を `db` 付きでビルドすることになり、CI から「`db` なしの shogun-core」のビルド・テストが黙って消える。矢印を逆にし、`shogun-core` の dev-dependency に `extract-eval` を置いた（`extract-eval` は `shogun-core` に依存しないので循環しない）。
- `dreamcycle/jobs.rs` は一切変更していない（open PR #723 と衝突させないため）。使っているのは既存の公開 API（`LocalRuleClassifier` / `Classifier` / `parse_batch_classification` / `llm::anthropic::BatchResult`）と `shogun_memory::event_log::BATCH_EXCLUDED_SOURCES` のみ。
- HTTP クライアント依存なし（`scripts/check-http-egress.py`）。ネットワーク・モデル呼び出しなし。

### スコアの定義

- **マッチ**: 予測行は「同じテーブル」「正解行の evidence イベントを少なくとも 1 つ引用」「キー（`description` / `name`）の類似度 ≥ 0.5」のとき正解行と 1 対 1 で対応づく（類似度の高い順に貪欲割当）。`description` はトークン F1（英語は単語、日本語は文字 bigram）、`name` は完全一致 1.0 / 語単位の部分一致 0.6（"Oren" と "Oren Vale"）。
- **フィールド別**（`people.name` / `projects.name` / `commitments.{direction,description,due}` / `open_loops.{kind,description}`）:
  - tp = 対応づいた予測で値が正しい
  - **hallucinated（精度の分母）** = 対応のない予測が述べる全フィールド、対応ありで値が違う、正解に無い値を述べた（例: 期限の無い約束に `due`）
  - **omitted（再現率の分母）** = 対応のない正解行の全フィールド、対応ありで値が違う/欠けている
  - wrong value = 上の両方に数えられる「対応ありで値違い」の内訳（enum の取り違えを純粋な捏造と区別するため）
- `due` はプロダクトと同じく `shogun_memory::due::due_at_with_offset` で記述文から読み出し、ペルソナの固定オフセットで日付にする（ホストのタイムゾーンに依存しない）。
- 同一キーの予測は行としてマージする（プロダクトが 1 行に複数 provenance を持つのと同じ）。
- **provenance 妥当性**（純コード、マージ前の引用ごと）: 引用イベントが存在しない = dangling、存在するが支持しない = unsupported。支持の判定はイベントの**保存テキスト**（`sanitize::persist_body`、`event_log` と同じ）に対して、エンティティは `knowledge::persist` と同じ規則（quote が本文の連続部分列で、name と context を含む）、記述文は正規化後の部分列、または内容トークンの 60% 以上が本文に現れること（モデルの言い換えを許すため）。
- **確信度較正**: バケットは `shogun_fusion::confidence::band` の境界そのもの（Low `[0, 0.5)` / Medium `[0.5, 0.8)` / High `[0.8, 1]`。境界値はコピーせず `band()` から読み出す）。バケットごとに件数・平均確信度・正解率を出し、ECE = Σ (n_b/N)·|正解率_b − 平均確信度_b|。
  - **FR-ST-20 との接続**: High は「事実として使ってよい」、Medium は「`possibly:` として弱く渡す」。よって High の予測は 80% 以上、Medium は 50% 以上当たっていなければ下流の扱いが嘘になる。これを `band_promise_violations()` で検査し、Batch リプレイではゲートにしている。
  - 対応づいた行について、正解行の `expected_band`（1 イベントの明示的言明 = medium、2 イベント以上で裏付け = high。FR-ST-20/21 が High を反復証拠用に予約しているため）と予測バンドを突き合わせ、over / under-confident を数える。

## 2. 失敗タクソノミー（FR-EV-10）

各項目は「対応 evaluator」か「未カバー」を必ず持つ。テンプレートは `src/templates.rs` の `key`、「実測」は §6 の初期スコアでの挙動。

| # | 失敗モード | 再現テンプレート / fixture | evaluator | 実測（初期） |
|---|---|---|---|---|
| T1 | 話者の取り違え: 他人の一人称の約束（チャットログ、謙譲語メール）を自分の約束にする | `commit_theirs_chat`, `ja_commit_theirs` | `commitments.direction` の wrong value | ローカル規則で 4/4 件取り違え。リプレイ fixture に 1 件 |
| T2 | 引用された例文・参照ページを約束にする | `trap_quoted_example`, `ja_trap_quoted` | 行の hallucination（by template） | ローカル規則が両方とも誤抽出 |
| T3 | 「Let me know …」（依頼の誘い）を約束にする | `trap_let_me_know` | 行の hallucination | ローカル規則が誤抽出（`let me ` 手がかり）。リプレイにも待ち状態として 1 件 |
| T4 | Web ページの修辞疑問を返信待ちにする | `trap_job_ad` | 行の hallucination | ローカル規則が誤抽出 |
| T5 | ナビゲーション UI のラベル（"Follow up"）を手がかりにする | `trap_chrome` | 行の hallucination | ローカル規則が誤抽出 |
| T6 | ぼかし（"Maybe I could …"）を約束に格上げする（FR-ST-11 違反） | `trap_hedge` | 行の hallucination | ローカル規則は正しく無視。リプレイ fixture に 1 件 |
| T7 | 日本語の緩い手がかり（「お願いします」⊃「します」）で別テーブルに入る | `ja_review` | 行の omission + hallucination | ローカル規則で約束として誤抽出 |
| T8 | 同じ事実を別ソースで言い換え → 行が重複する | `restated_commit` | 行の hallucination（重複） | ローカル規則で 1 件重複 |
| T9 | 不可視文字（ZWSP）が手がかりを隠す | `zwsp_commit` | `tests/gate.rs` が matched を assert | 取りこぼしなし（ingress / 保存時の strip が効いている） |
| T10 | open loop の種別違い | リプレイ fixture | `open_loops.kind` の wrong value | fixture に 1 件 |
| T11 | 別名で人物行が分裂（"Kasimir" と "Kasimir Lode"） | リプレイ fixture | `people` の hallucination（`knowledge.rs` の既知制約） | fixture に 1 件 |
| T12 | 会議トランスクリプトにしか出ない人物・プロジェクトが抽出されない（Batch は `meeting` を送らず、ローカル規則はエンティティを出さない） | 各日の `commit_mine_meeting` / `*_decision` | `projects` / `people` の omission | リプレイで projects 2 件 omission。**設計上の欠落**として記録 |
| T13 | 根拠のない行の捏造 | リプレイ fixture（カレンダーに無い約束） | provenance unsupported + hallucination | リプレイで 2 件検出（カレンダーへの捏造と、T3 の言い換えた待ち状態） |
| T14 | 存在しないイベントの引用 | `score.rs` 単体テスト | provenance dangling | 現行ランナーでは構造上 0（parse が未知 custom_id を拒否） |
| T15 | 期限の誤り・欠落・捏造 | 期限付きテンプレート全般 | `commitments.due` | 対応行では 100%。ローカル規則では T2 の例文と T8 の重複行にも期限が付き、hallucinated として 3 件 |
| T16 | 確信度の較正ずれ（バンドの約束を破る） | 全体 | 較正表・ECE・`band_promise_violations` | 両ランナーとも過小確信（ECE 0.36〜0.46）。約束違反なし |
| T17 | 1 件の壊れた Batch 項目（コードフェンス、欠落、未送信 id）でウィンドウ全体が失われる | `batch_replay_failure_shapes` | `parse_batch_classification` が `Err`（= ウィンドウは再開可能なまま）であることを assert | カバー |
| T18 | 契約外のエンティティ（未知フィールド・未知 kind）・未知の open loop 種別 | リプレイ fixture | エラーにならず黙って捨てられ、hallucination にもならないこと | カバー |
| T19 | 指示文の形をした記述の永続化 | リプレイ fixture（"Ignore previous instructions …"） | parse で落ちること（hallucination 0）。永続化側は `shogun-memory/tests/poison_persist.rs` | カバー |
| N1 | 日をまたぐ状態遷移（done / cancelled / overdue、期限の変更 → dispute） | — | **未カバー**（複数日 SynthDay が必要） | — |
| N2 | 約束の相手（`counterparty_id`）・プロジェクトへの紐付け | — | **未カバー**（フィールド未採点） | — |
| N3 | person / project 以外のエンティティ（topic / tool / organization / employment / education / activity / self） | リプレイ fixture に topic 1 件 | **未カバー**（`unscored entities` として件数のみ） | — |
| N4 | Recap / Morning Brief の要約忠実性 | — | **未カバー**（FR-EV-05、judge が必要） | — |
| N5 | 実モデル固有の振る舞い（言い換えの揺れ、拒否、900 トークン上限での切断） | — | **未カバー**（LLM ランナー保留、§5） | — |
| N6 | Batch 前段プレフィルタ（#286）で分類前にイベントが落ちる | — | **未カバー**（`prefilter_texts` は crate 外非公開。リプレイは cloud 側全イベントを期待） | — |
| N7 | 英日混在の文（"I'll send 資料"） | `mix_commit_mine`, `mix_commit_theirs`, `mix_waiting`, `mix_trap_quoted`（day-04） | by template の行・`commitments.direction` / `due` | ローカル規則は 3 件とも正しく抽出（向き・期限も正しい）。英語の手がかりは日本語が混ざっても常に有効なため。混在の例文（`mix_trap_quoted`）は T2 と同じく誤抽出。リプレイ fixture では 3 件とも正しい |
| N8 | open loop の経過時間（staleness）の誤り | — | **未カバー** | — |

## 3. Batch リプレイ fixture

`replay/<day>.json` は、ゴールデン日の **cloud 側イベント（`meeting` 以外）ごとに 1 件**、統合（consolidation）指示が要求する JSON 契約どおりの結果を持つ。**モデル出力ではない**。手で形を作った合成データで、測っているのはモデル品質ではなく「parse 経路 + スコアラー」。`meeting` イベントはプロダクトと同じくローカル規則で分類してから合流させる。

意図的に入れてある誤り（スコアラーと parse の各経路を通すため）:

| 日 | 誤り | 通る経路 |
|---|---|---|
| day-01 | `loop_waiting_chat` の種別を `follow_up` に / 短い名前の person を追加 | T10 / T11 |
| day-01 | カレンダーに無い約束を捏造 | T13（unsupported） |
| day-01 | `trap_hedge` を約束に、`trap_let_me_know` を待ち状態に | T6 / T3 |
| day-01 | エンティティに契約外フィールド `confidence` | T18（`deny_unknown_fields` で破棄） |
| day-01 | `topic` エンティティ | N3（unscored） |
| day-01 | Oren Vale のメールの約束の `line` を 1 行ずらす（anchor は正しい） | anchor が一意なので `line_mismatch` として数えつつ正しいイベントに解決（2026-10-12 セッション化） |
| day-01 | `anchor_len` を文脈より短く指定 | span の拡張 |
| day-02 | 解決できない anchor（どの行にも無い） | 項目を落として `unresolved` に数える |
| day-02 | `loop_review_pr` を丸ごと取りこぼし | omission |
| day-02 | 指示文の形の open loop、未知の種別 `urgent` | T19 / T18 |
| day-03 | 謙譲語の約束 1 件の向きを `mine` に | T1 |
| day-04 | 名前だけで呼ばれる人物（"Idris"）の person エンティティを出さない（姓が本文に無いので、正解の "Idris Solano" には対応づけられない） | people の omission |

ゴールデンを再生成して日のイベント構成が変わった場合、その日の fixture も作り直す（`crates/shogun-core/tests/extract_eval.rs` が golden イベントを製品と同じ経路でセッション化し、「セッションごとにちょうど 1 件・`meeting` を含まない・anchor が解決する・lint 通過」を検査するので、ずれは CI で分かる。セッション id はキー `(app, 正規化タイトル, 時間バケット)` の xxhash64 なので、イベントの時刻やウィンドウを変えると fixture の id も変わる）。fixture の人名はペルソナのプールからのみ取る。

## 4. バグ報告 → 合成再現ケース → ゴールデン追加（標準フロー）

1. **形だけを抜き出す。** 報告から「ソース（メール / チャット / 画面 / 会議）」「言語」「手がかりの形（どの語で誤発火 / 取りこぼしたか）」「期待と実際（テーブル・向き・種別・期限）」だけを書き出す。**報告の本文・人名・メールアドレス・ウィンドウタイトル・社名は写さない**（ユーザーデータであり、不変条件 3 とログ規約の対象）。
2. **テンプレートを足す。** `crates/extract-eval/src/templates.rs` の `TEMPLATES` に、プールのスロット（`{person}` `{project}` `{artifact}` …）で書き直した再現を追加する。何も抽出されるべきでないなら `key` を `trap_`（日本語は `ja_trap_`）で始める。`exercises` に何を試すかと Issue 番号を書く。`gold` には**正しい答え**を書く（今の規則が出す答えではない）。
3. **ゴールデン日に載せる。** `crates/extract-eval/src/generate.rs` の `GOLDEN_SPECS` の該当日の `required` に `key` を足す（または新しい `DaySpec` を別の `seed` / `id_base` で追加し、`golden.rs` の `GOLDEN_FILES` と `runners.rs` の `REPLAY_FILES` にも登録する）。
4. **再生成する。** `cargo run -p extract-eval -- regen` → `goldens/*.json` をコミット。既存テンプレート・プール・ジェネレータの挙動を変えた場合は `TEMPLATE_SET` を上げる。日の構成が変わったら §3 に従いその日のリプレイ fixture を作り直す。
5. **確認する。** `cargo test -p extract-eval` と `cargo test -p shogun-core --features db --test extract_eval -- --nocapture`。レポートの「By template」表に再現テンプレートが現れる。修正 PR では、その行が直ったことを `zwsp_commit` と同じ形の assert で固定する。
6. **タクソノミーに行を足す**（§2）。evaluator が無ければ「未カバー」と書く。

`tests/gate.rs` がゴールデンとジェネレータ出力のバイト一致と lint を検査するので、手で書いた文字列がゴールデンに直接入ることはない。

## 5. 保留したもの（とその理由）

| 項目 | 理由 |
|---|---|
| LLM による SynthDay 生成（ペルソナ → 時系列イベントグラフ → マルチソースの一日、Select-KK Batch レーン） | オーナー判断: まずモデルを呼ばない部分から。予算とキーが揃ってから。LLM 生成が入るときは、生成物に対する同等の「実データなし」ガード（プール外の固有名詞検出など）を別途用意する |
| ペルソナ 10 体・能力あたり 20〜50 タスク（FR-EV-02 の初期規模） | LLM 生成が前提の規模。現状は 3 ペルソナ・4 日・48 イベント・正解 59 行 |
| judge（LLM 採点） | 予算とキー待ち。FR-EV-05 の judge 規律（別モデルファミリー・両順序・手ラベル較正）を満たす準備も含めて後続。現スコアラーは純コード（トークン重なり）でマッチするため、言い換えの妥当性判定は粗い |
| QA ペア（FR-EV-04 の検索 eval 用） | 抽出 eval の範囲外。SynthDay 形式に後から足せるよう、`format` タグでバージョンを分けている |
| Layer 0 ローカルトレースビュー・週次オープンコーディング | 本スライスの範囲外。タクソノミー表（§2）は先に置き、トレース由来の失敗はここに行として足す |
| eval 結果とプロンプト版（`prompt_id`、#313）の紐付け | 現行の consolidation 指示は 1 つで、リプレイ fixture は契約（JSON 形）にのみ依存する。プロンプトを変える PR の before/after（FR-EV-01）を取るときに、fixture とレポートに prompt_id を持たせる |
| 実 Batch 結果の記録・再生 | 実データ（ユーザーの画面テキスト由来の応答）をリポジトリに入れられないため、合成イベントに対する実モデル応答を記録する運用は LLM 生成と同時に設計する |

## 6. 初期スコア（CI の床の根拠）

3 日・44 イベント・正解 53 行（people 14 / projects 8 / commitments 16 / open_loops 15）。

| ランナー | 行 P / R / F1 | フィールド F1（micro） | provenance | 較正 |
|---|---|---|---|---|
| `extract_untrusted`（取り込み時ローカル規則） | 78.9% / 56.6% / 65.9% | 73.8% | 100%（40/40） | 全件 Low、正解率 78.9%、ECE 0.455（過小確信） |
| `LocalRuleClassifier`（Dream Cycle、Batch なし） | 78.9% / 56.6% / 65.9% | 73.8% | 100%（40/40） | 同上 |
| Batch リプレイ（parse + 会議はローカル規則） | 92.6% / 94.3% / 93.5% | 92.6% | 97.1%（66/68、unsupported 2） | Medium 48 件・正解率 91.7%（約束 ≥50% を満たす）、ECE 0.356 |

- ローカル規則の再現率が低いのは people / projects を一切出さないため（仕様）。commitments / open_loops に限れば再現率 100% / 93.3%。精度を落としているのは T1〜T5・T7・T8。
- **day-04（英日混在、N7）を足した後**（4 日・48 イベント・正解 59 行）: ローカル規則は行 P / R / F1 = 82.5% / 55.9% / 66.7%、フィールド F1（micro）75.1%、provenance 100%（41/41）、ECE 0.491。Batch リプレイは 93.2% / 93.2% / 93.2%、フィールド F1 92.9%、provenance 97.3%（72/74）、Medium 53 件・正解率 92.5%、ECE 0.360。上の表は 3 日時点の初期値として残す。`mix_` テンプレートは `required_only`（シード付きの抽選に入らない）なので、day-01〜03 のゴールデンはバイト単位で変わっていない。
- 2 つのローカル規則ランナーが一致するのは想定どおり: `event_log` は保存時に不可視文字を落とすので、`LocalRuleClassifier` が読む保存テキストと `extract_untrusted` の入力は同じ抽出結果になる。T9 が崩れればどちらかが落ちる。
- Batch リプレイの数字は **fixture の出来**であってモデル品質ではない（§3）。parse 経路の回帰（エンティティが落ちる、確信度が変わる、捨てるべきものが残る）を検知するための基準線。

床（`tests/gate.rs` / `crates/shogun-core/tests/extract_eval.rs`）は `retrieval_eval.rs` と同じく緩く置いた: ローカル規則は commitments 再現率 ≥ 0.70・精度 ≥ 0.55、open_loops 再現率 ≥ 0.70・精度 ≥ 0.60、provenance 100%、Low より上に出ないこと。リプレイは行 P/R ≥ 0.70・フィールド F1 ≥ 0.70・provenance ≥ 0.90・unsupported の検出・バンド約束違反なし。締めるのは測定に基づいて行う。

## 7. `requirements-harness-v1.md`（ドラフト）との差分

- FR-EV-02: 生成は Batch レーンではなく固定テンプレートの決定論ジェネレータ。規模も初期基準（10 ペルソナ）未満。QA ペアなし。→ §5。
- FR-EV-03: 「確信度較正曲線」は 10 等分ビンではなく Fusion のバンド境界 3 バケットで出す。問うべきは「各バンドが下流での扱い（FR-ST-20）に見合う正解率か」なので、バンド境界で切るのが意味論的に正しいと判断した。ECE も同じバケットで計算。
- FR-EV-03 受け入れ基準「CI 閾値ゲート」: 緩い床で実装済み。FR-EV-01（PR 本文への before/after 添付の CLAUDE.md 規約化）は本 PR の範囲外。
- FR-EV-10: タクソノミーと evaluator の対応表は本書 §2。週次オープンコーディングの運用（トレースビュー起点）は未着手。
