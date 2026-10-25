# 継続学習2/最適化 — Work Patterns 設計（Issue #242）

**Status**: **Confirmed / P0–P3 実装済み**（2026-08-24 オーナー決定→同日実装。§9参照。FR-PAT-03〜07 として requirements-v1.0.md に要件化済み。P0: 提示ログ・Patterns UI・CTR計測・API対称。P1: `PatternDistillation`（Dream Cycle 第8ジョブ）・ranking prior 3項・avoid抑制＋14日クールダウン。P2: activityパターンの lessons(kind='pattern') 昇格・Morning Brief の pattern_line・気づきモーメント（V28 `noticed_at`、Brief刻印＋notchバナー36h・Undo=OFF）。P3: `Summarizer::pattern_phrase` シーム——Batch lane があるビルドだけがテンプレ文を自然文に1回だけ置換（送るのはパターンのラベルのみ）。マージは `scope_ref='pattern:<key>'` の構造キーで、置換後も夜次昇格が同一lessonに合流する）
**Owner intent（#242 原文の要約）**: オプションキーで送れる、ではなく——自分の入力の癖・やっているプロダクトを理解して、先回りする・提案する・**的外れなことをしない**。例:「翻訳めっちゃしてるな」「ShogunAIの話ばっかだな」。

---

## 0. 一言定義

継続学習1（L5 lessons, Plan D）は「**直された内容**から学ぶ」——承認前編集の差分を style/preference の1文に蒸留する仕組み。
継続学習2は「**行動そのもの**から学ぶ」——何をよくやるか（activity）・何に取り組んでいるか（focus）・何を無視し続けているか（negative）を数値プロファイルとして持ち、**提案の選択と順位**に効かせる。

言い換えると: L5-v1 は生成の質を上げる。本設計（L5-v2 / Work Patterns）は**提案の当たり率**を上げる。「的外れをしない」はここでしか解けない——文言をいくら学んでも、要らない提案は要らない。

## 1. 既に持っているもの（ゼロから作らない）

| 資産 | 状態 | 本設計での役割 |
|---|---|---|
| `feedback_events`（V16）+ offer context（V19: surface/rank/context_app/latency_ms） | **実装済み**（FR-PAT-01/02） | 負のシグナルの主力。採択/却下/編集が文脈付きで既に溜まっている |
| `lessons::acceptance_by_kind`（kind別の decided/adopted カウント） | **実装済み**（FR-CF-03供給と明記） | ranking prior の分子分母。**まだ誰も消費していない** |
| FR-CF-03 の優先度スコア（純関数 `score.rs`） | 実装済み（採択率項は未接続） | prior/penalty の注入点 |
| `lessons`（kind='pattern' が CHECK に**最初から存在**） | 実装済み・patternは未使用 | 言語化されたパターンの置き場。注入/可視化/ON-OFF/decay を丸ごと再利用 |
| `event_log`（app/dwell/セッション紐付き）、`sessions`（focus/meeting）、`threads`/`projects` | 実装済み | activity/focus の原料 |
| Dream Cycle ジョブ枠（LessonDistillation が第7ジョブ） | 実装済み | 第8ジョブとして蒸留を追加 |
| FR-PAT-03（v1.5予告: 「記録からのFusion優先度の個人化」） | **未設計のプレースホルダ** | 本設計がその実体。v1.5要件に昇格させる |

新しいキャプチャは**一切増やさない**。既に記録しているものの集計と消費だけ。

## 2. 学習対象は4つ

### ① activity — 何をよくやるか（「翻訳めっちゃしてるな」）

シグナル: 実行されたアクションの kind（`feedback_events.action_kind` の採択行）、scribe/voice/翻訳モード/検索/会議ノートの利用イベント（すべて既存記録。内容ではなく**回数**）。

例: `translate_ja_en: 31回/7日, trend ↑` / `draft_reply: 2回/7日, trend →`

### ② focus — 何に取り組んでいるか（「ShogunAIの話ばっかだな」）

シグナル（精度順）:
1. `projects` state の活動量（既に活動ベースで生きている）
2. `threads` の活動量上位
3. ウィンドウタイトルの反復固有名詞 × dwell_ms 加重 — **session_speakers::mine_participant_names と同じ「反復するラベルだけ拾う」手法**（2キャプチャ以上・UI語彙除外・タイトル由来）。実績のある抽出器の再利用

例: `ShogunAI: dwell 62%/7日` / `EDINET案件: 8%`

### ③ rhythm — いつ何をやるか（時間帯×活動。2026-08-24 オーナー決定でスコープ入り）

シグナル: activity と同じイベント列に、ローカル時刻の時間帯バンドを付けて集計するだけ（新しい記録は不要。`ts_ms` は全イベントが持っている）。

バンドは5つの閉じた語彙: `early`（5–9時）/ `morning`（9–12）/ `afternoon`（12–17）/ `evening`（17–21）/ `night`（21–5）。

例: `translate_ja_en@morning: 18回/7日` / `meeting_notes@afternoon: 9回/7日` / `deep_work（長dwellのfocusセッション）@early`

消費: (a) ranking prior に時間帯一致項——朝に翻訳をよくやる人には朝、翻訳が上に来る。(b) Brief の1行（「朝は翻訳、午後は会議の週でした」）。(c) 気づきモーメント（§5e）。

### ④ negative — 何が的外れか（無視・却下のパターン）

シグナル: V19 文脈付きの `reject` ＋ **新規: offer_impressions**（提示されたが触られなかった、を導出するための提示ログ）。

例: `(open_app, context=Xcode): 直近5回連続 提示→無反応` → この文脈でこの提案は的外れ。

## 3. データモデル（V27、additive）

```sql
-- 提示ログ。「無視された」は 提示 − 決定 でしか導出できない（FR-PAT-02の思想の続き）。
-- メタデータのみ: 内容・キャプチャテキストは載せない。高頻度なので30日で自動削除（retentionジョブ）。
CREATE TABLE offer_impressions (
    id          INTEGER PRIMARY KEY,
    ts_ms       INTEGER NOT NULL,
    action_kind TEXT    NOT NULL,
    surface     TEXT    NOT NULL CHECK (surface IN ('notch', 'option_key', 'chat', 'recap', 'api')),
    rank        INTEGER,
    context_app TEXT
) STRICT;
CREATE INDEX idx_offer_impressions_kind_ts ON offer_impressions (action_kind, ts_ms);

-- 数値プロファイル。「事実」ではなく「傾向」— state tables と同じ規律（confidence必須・decay・
-- 低confidenceは消費しない）で、値は蒸留ジョブが毎晩再計算する（accumulate しない = recompute.rs と同じ思想）。
CREATE TABLE work_patterns (
    id           INTEGER PRIMARY KEY,
    kind         TEXT    NOT NULL CHECK (kind IN ('activity', 'focus', 'rhythm', 'avoid')),
    key          TEXT    NOT NULL,           -- 'translate_ja_en' / 'ShogunAI' / 'open_app'
    scope        TEXT    NOT NULL CHECK (scope IN ('global', 'app', 'band')),
    scope_ref    TEXT,                        -- avoid は context_app、rhythm は時間帯バンドが入る
    count_7d     INTEGER NOT NULL DEFAULT 0,
    count_30d    INTEGER NOT NULL DEFAULT 0,
    trend        REAL    NOT NULL DEFAULT 0,  -- 7日EWMAの傾き。Briefの「今週は翻訳が2倍」の供給
    confidence   REAL    NOT NULL CHECK (confidence >= 0.0 AND confidence <= 1.0),
    active       INTEGER NOT NULL DEFAULT 1,  -- ユーザーが個別OFF可（Learnedと同じ）
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    UNIQUE (kind, key, scope, scope_ref)
) STRICT;
```

言語化された1文（"You translate JA→EN constantly; keep translation one tap away"）は**新テーブルを作らず** `lessons` に `kind='pattern'` で昇格させる。注入経路（Shougun.md相乗り・Fusion scope一致 top-k）・Learned UI・ON/OFF・corroboration/decay・50件キャップをそのまま使う。

## 4. 蒸留（Dream Cycle 第8ジョブ `PatternDistillation`）

毎晩、ローカルルールのみで（Batch lane不要が既定）:

1. **activity**: `feedback_events`（採択行）+ 利用イベントを kind 別に 7d/30d 集計。`count_7d >= 5` で行を生成/更新。confidence は `recompute::corroborated_confidence`（既存曲線）を evidence=count_7d で流用、**上限0.75**（High禁止 — lessonsと同じ「offered, never asserted」）。
2. **focus**: projects活動量 → threads → タイトル固有名詞（反復ラベル抽出。session_speakers の採掘器と同手法）の順にマージ、dwell比率上位5件。比率 <5% は行にしない。**3ソースすべて P1**（2026-08-24 決定）。
3. **rhythm**: activity と同じ集計を時間帯バンド別に割るだけ（同一ジョブ内の1パス追加）。バンド内 `count_7d >= 5` かつ**そのバンドへの偏り ≥ 50%** で行を生成——「朝もやるし夜もやる」は rhythm ではない。
4. **avoid**: (action_kind × context_app) ごとに直近の決定列を見る。**提示5回連続・採択0**（reject または impression-無反応）→ avoid 行を生成。1採択で即削除。
5. **昇格**: activity/focus/rhythm の上位で `count_7d >= MIN_RULE_OCCURRENCES(3)×5` の安定パターンだけを lessons(kind='pattern') に1文で書く（定型テンプレ文。Batch lane があれば自然な言い回しに置換——invariant 5 どおり Batch の仕事）。
6. **decay**: 全行を再計算で更新するので、消えた行動は自然に消える（half-life不要。7d窓が事実上のdecay）。

## 5. 消費（ここが「先回り」と「的外れ回避」の本体）

### 5a. Notch ranking prior（FR-CF-03 の未接続項を接続する）

`score.rs` の純関数に2項を追加:

```
adoption_prior = (adopted + 1) / (decided + 2)        -- Laplace平滑。acceptance_by_kind が既に返す2値から
habit_prior    = normalize(work_patterns.activity[kind].count_7d)   -- 0..1
score' = score + w_adopt * adoption_prior + w_habit * habit_prior
```

`acceptance_by_kind` が counts を返す設計（薄い証拠に振り回されない）はこのためにある。スナップショットテスト（FR-CF-03受け入れ基準）を更新。

### 5b. 抑制（的外れをしない）

- `avoid` 行に一致する (action_kind × context_app) は候補スコアに**ハードペナルティ**（下位固定）。枠が余っていれば最下位に1枠だけ出す。枠が競っていれば出さない
- **文脈付きが原則**。global 抑制はユーザーの明示OFF（Learned/Patterns UIのスイッチ）のみ
- **FR-CF-04 は常に勝つ**: 汎用アクション（Save note / Search memory / Extract tasks）は抑制対象外。空パネルを構造的に作らない
- **クールダウン14日**: 以後1回だけ低ランクで再オファー。採択されたら avoid 行は即削除（人は変わる）
- **絶対規則（Plan D-5 と同文）**: patterns は提案の**選択と順位**にのみ影響する。権限判定（L1/L2/L3）には一切影響しない。テストで固定

### 5c. 生成への注入

lessons(kind='pattern') が既存経路でドラフト/チャットの standing prompt に入る（scope一致 top-k・confidence gate・Medium は possibly 扱い）。「ShogunAIに取り組んでいる」を生成が前提にできる。**明示 directive（Shougun.md）が常に優先** — 学習は明示設定と矛盾したら負ける。

### 5d. Morning Brief

trend から1行だけ:「Translation is up 2× this week.」。rhythm があれば時間帯の言い方になる（"Mornings are translation, afternoons are meetings this week."）。既存 briefs パイプラインに supply を1本追加。

### 5e. 気づきモーメント（「勝手に学んでる感」を出す — 2026-08-24 オーナー決定）

学習を隠さないだけでなく、**学習したことをプロダクトの演出として見せる**。新しいパターンが初めて閾値を超えた夜、翌朝の Brief と Notch に1回だけ「気づき」を出す:

> ⚔ Noticed: you translate JA→EN a lot in the mornings. Translation is now one tap away before noon. — *Undo*

規律:
- **気づき1件 = 提示1回**。同じパターンで繰り返し出さない（うるさい学習は的外れと同罪）
- 必ず **Undo/OFF をその場に付ける**（Patterns UIまで潜らせない）。UndoはそのパターンのOFFであり、削除ではない（Learnedと同じ）
- 気づきが**実際の挙動変更と必ず対**になる（「気づきました」だけ言って何も変わらないのは不信の素）。昇格・並び替え・既定化のどれかを伴う
- 文言はテンプレ（Batch lane 利用時のみ自然文）。キャプチャ内容は載せない——パターンのキーと回数のみ

## 6. プライバシー・可視化

- 全テーブルはローカルのみ。テレメトリ・分析イベントに**回数すら**載せない（コード規約どおり）
- `delete_all` / `delete_since` / `export_json` に両テーブルを追加（E-01/E-14 チェックリスト化済みの教訓）
- **Patterns UI**: Personalization の Learned の隣に「Patterns」カード——「SHOGUNはあなたをこう見ている」を全件表示（key・回数・trend・時間帯・ON/OFF・削除）。ブラックボックス化しないことが信頼の要件（Plan D-4 と同文）。**方針は「見せる学習」**（2026-08-24 決定）: 設定の奥に隠さず、気づきモーメント（§5e）で能動的にも見せる。透明性は同意の代替ではなく前提——見せた上で、全部その場で切れる
- MCP/CLI/REST 対称（不変条件6）: `patterns.list` / `patterns.set_active`。実装パターンは meeting.speakers 系で確立済み（memory_api → db_backend → rest_routing → CLI）

## 7. 効果測定（`shogun metrics` に追加）

| 指標 | 意味 | 期待する向き |
|---|---|---|
| offer CTR（kind別・7d） | 提示→採択率 | 上がる（先回りが当たっている） |
| dismiss/ignore 率 | 的外れ率 | 下がる（本Issueの直接KPI） |
| 抑制発動数 / 復帰数 | negative学習の健全性 | 発動が飽和せず、復帰が発生する |
| pattern lessons 件数・hit rate | 言語化の寄与 | L5-v1 の指標と同居 |

impressions を取り始めるので CTR が**初めて分母付きで**測れる。この数値がそのまま「使うほど賢くなる」のエビデンス第2弾。

## 8. 実装計画

| 段階 | 内容 | 工数 | 出せる価値 |
|---|---|---|---|
| **P0** | `offer_impressions` 記録（notch_actions/option_key に1行フック）＋ V27 ＋ Patterns UI（読み取りのみ: 集計SQLを直接表示）＋ metrics | 2–3日 | 「SHOGUNはあなたをこう見ている」が見える。CTRが測れる |
| **P1** | `PatternDistillation` ジョブ（activity / focus 3ソース / rhythm / avoid）＋ ranking prior 3項（採択率・habit・時間帯一致）＋ 抑制/クールダウン | 4–5日 | 提案の並びが人と時間帯に合い始める。的外れが引っ込む（**本Issueのコア**） |
| **P2** | lessons(pattern) 昇格 ＋ Fusion注入 ＋ Brief 1行 ＋ **気づきモーメント（§5e）** | 3日 | 生成が「あなたの文脈」を前提にし、学習が体験として見える |
| **P3**（任意） | Batch lane での言い回し生成 | 1日 | 磨き |

P0→P1 の順序が重要: **抑制（信頼）が先、先回り（魔法）は後**。的外れが1回続くだけで「わかってないやつ」認定になり、以後の先回りが全部ノイズ扱いされる。気づきモーメントを P2 に置くのも同じ理由——「気づいた」と言うのは、並びが実際に当たり始めてから。

## 9. オーナー決定（2026-08-24 確定）

1. **rhythm（時間帯×活動）→ スコープに含める。** P1の蒸留ジョブに同居（§2③・§4-3）、消費は時間帯prior＋Brief＋気づきモーメント
2. **Patterns UI → 「見せる学習」。** Learnedと同カード群で全件表示に加え、気づきモーメント（§5e）で能動的に見せる。学習していることをプロダクト価値として演出する——ただし気づき1件=提示1回・必ずUndo付き・必ず実挙動の変更と対
3. **要件文を本件で確定。** FR-PAT-03〜07 として requirements-v1.0.md に記載（v1.5予告プレースホルダを置換）
4. **focus のタイトル固有名詞抽出 → P1 に含める。** 誤検出の見え方リスクは「見せる学習」方針とセットで受容——Patterns UIで即OFF/削除できることが担保

## 10. やらないこと（スコープ外を明記）

- キャプチャの追加・新しい常駐監視（既存記録の集計のみ）
- 権限モデルへの学習の影響（絶対規則）
- パターンの自動実行（「翻訳をよくやるから勝手に翻訳する」はやらない。先回りは**提示**まで。実行は常にユーザーの1タップ以降）
- クラウドへのプロファイル送信（ローカルのみ。Batch lane を使う場合も送るのは言い回し生成の入力チャンクのみ＋トレーサビリティ）
