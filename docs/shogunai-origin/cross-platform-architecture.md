# SHOGUN クロスプラットフォーム・アーキテクチャ方針（少人数運用版）

| 項目 | 内容 |
|---|---|
| 文書ID | cross-platform-architecture |
| 対象 | SHOGUN Platform 全体（`crates/` + `apps/desktop` + 将来の Windows / iOS / Android） |
| ステータス | Draft（オーナー決裁待ち。§3 の D1〜D10 が決裁対象） |
| 最終更新 | 2026-10-19（冒頭に「2026-10-19 更新」を追加。Windows は方針決定、モバイルは方針案を別文書に分けた）。2026-08-25（§5を優先度・依存関係のみに改訂。スケジュールは本書の管掌外） |
| 上位文書 | `/CLAUDE.md`（絶対不変条件。本書と矛盾する場合はCLAUDE.mdが優先） |
| 関連文書 | `docs/requirements-v1.0.md`、`docs/phase1-implementation-plan.md`、`docs/mcp/01-architecture.md`、`docs/privacy-security.md` |
| 記法 | **MUST** = 必須 / **SHOULD** = 強い推奨（逸脱には判断記録） / **MAY** = 任意 |

---

## 2026-10-19 更新（本書より新しい判断。§1 以降は 2026-08-25 時点の記述のまま）

- **Windows: 技術スタックは決定（オーナー、2026-10-19）。** Tauri v2 + Rust Core + React UI を流用し、OS アダプタだけを作る。それ以外は方針案。詳細は [`windows-architecture.md`](windows-architecture.md)。
  - **D9 の改定を提案（決裁待ち）。** コマンドが 188 → 460 に増えた。そのため、「全コマンドを REST/MCP と対称にしてから Windows に着手」ではなく、**機能領域ごとに、移植する分だけ対称化する**。
- **モバイル: 方針案。** オーナー決定済みなのは、同じリポジトリ・OS ごとのネイティブ（Swift / Kotlin）・iOS を基準に Android で同等の体験・セキュリティ最優先。詳細は [`mobile-architecture.md`](mobile-architecture.md)。
  - **D2・D4・D5 の置き換えを提案している。** 提案の中身: スマホに Rust のデータ層（UniFFI）を置き、端末ごとに記憶を持つ。
  - **クラウドに記憶を置かない（オーナー決定 2026-10-19）。** Mac とスマホは、同じネットワーク内で直接 E2E でやり取りする（Mac の派生記憶の写しと、スマホの共有分）。外出先からの問い合わせは任意で、中継は保存しない。D5 の (a)〜(c) への答えは「クラウド同期はしない」。
  - **§8 の「UniFFI で Core をモバイルへ載せない」は、データ層に限って改める提案。** 不変条件3の改定などオーナー判断は同書 §11。
- **数字の更新（2026-10-19 実測）:**
  - `#[tauri::command]` 188 → **460**。`apps/desktop/src-tauri` 約2.7万 → **約8.6万行**。`shogun-core` 約3.4万 → **約15.5万行**。
  - マイグレーションは V24 → **V89**（次は V90）。`crates/` で `target_os` を使うファイルは **28**（出現は約100か所）、シェルを含めると出現は約470か所。
  - XP-01（境界ガード）・XP-02（`event_log.uid`）・XP-03（`packages/protocol`）は**いずれも未着手**。`event_log` は今も `INTEGER PRIMARY KEY` で、`state_provenance.event_id` が参照している。
  - 穴C（可逆な外部書き込み）は `Action::ServiceState` で解消済み。穴D（権限の付与軸）は未解消。
  - §2.3 の「Gmail を Composio へ移管」は古い（Gmail は Google REST 直結、#713）。

---

## 0. 本書の位置付けと読み方

本書は「Mac版を完成させてからWindows版を作る」という進め方を**明示的に否定**し、代わりに **着手順序を「不可逆なものから」に固定する**ための方針書である。

本書が扱うのは次の2点のみ:

1. **今決めないと後で取り返しがつかないもの**を特定し、決める（§3）
2. **何を先にやるか（優先度と依存関係）**を確定する（§5）

**本書はスケジュールを定めない。** 各作業にかける期間・投入量・着手時期はオーナーが決める。本書が規定するのは「どれが先か」「何が何の前提か」だけである。工数見積もりや納期を本書に書き足さない。

本書が扱わないもの: Windows/iOS/Android の実装仕様、Sync のプロトコル仕様、UIデザイン。これらは §5 の該当優先度に到達した時点で別文書に書く。

識別子 **XP-nn** は安定識別子である。統合・削除で欠番が生じても再採番しない。

---

## 1. 背景 — なぜ今クロスプラットフォームを考えるのか

### 1.1 放置した場合に起きること

SHOGUNは現在 macOS 専用として実装されている。このまま Mac版を完成させ、そのあとで Windows版・iPhone版・Android版を個別に作ると、各OSごとに以下を**再実装**することになる。

```text
Memory / Agent / Sync / Auth / Context / Permission / Billing / Traceability
```

結果として運用対象は「1つのプロダクト」ではなく「4つの別プロダクト」になる。少人数チームにとってこれは致命的で、機能を1つ足すたびに4回実装し、4回レビューし、4回リリースすることになる。**人数が少ないほど、この失敗のコストは大きい。**

### 1.2 SHOGUNの製品定義そのものが、単一プラットフォームと矛盾している

SHOGUNの一言定義は「ユーザーの仕事のワールドモデルを構築し、状態を推定して実行する」ことである（`CLAUDE.md`）。ワールドモデルは **ユーザーの仕事全体** を対象にするが、現代の仕事は1台のMacに閉じていない。

- 会議に出るのは Mac、通知を受けるのは iPhone
- 業務PCが Windows の職場は多い
- 移動中に思いついた指示を出すのは Mobile、それを実行するのは Desktop

つまり **「単一デバイスで完結するワールドモデル」は定義上不完全**である。クロスプラットフォームは機能追加ではなく、製品定義の完成条件である。

### 1.3 一方で、目的は「全OS版を出すこと」ではない

少人数で4プラットフォームを同時に育てることはできない。本取り組みの目的はこう定義する。

> **SHOGUN という共通 Platform の上に、Mac / Windows / iOS / Android を後から載せられる状態を作る。**
> Mac版の開発速度を落とさずに、それを達成する。

「Mac版の開発速度を落とさない」は制約ではなく**要件**である。Macが売れている間しか他プラットフォームに投資する原資が生まれないため、Macの前進を止める施策は本書では採らない。

### 1.4 したがって本書の基本戦略

```text
先にやる    : 不可逆なもの（ID・Schema・Permission）と、境界のCI強制
後に回す    : Sync の実装、Mobile の実装、大規模リライト
Windowsの位置: 「アーキテクチャが正しいか」の検証手段であって、製品目標ではない
```

「後に回す」は優先度の宣言であって、期限の宣言ではない。着手時期はオーナーが決める。

---

## 2. 現状の棚卸し（2026-08-25 実測）

方針を立てる前に、実体を測った。**結論は「思っているより既にクロスプラットフォームである」。**

### 2.1 crate 別のOS依存

| crate | LOC | macOS依存 | 評価 |
|---|---:|---|---|
| `shogun-agents` | 2,045 | **なし** | そのままCore |
| `shogun-fusion` | 2,030 | **なし** | そのままCore |
| `shogun-redact` | 880 | **なし** | そのままCore |
| `shogun-mark` | 1,046 | **なし** | そのままCore |
| `shogun-license` | 493 | **なし** | そのままCore |
| `shogun-memory` | 14,998 | `embed_onnx.rs` のみ（開発者専用feature） | 実質Core |
| `shogun-mcp` | 10,245 | `plan_source.rs` のみ | 実質Core |
| `shogun-integrations` | 3,137 | Keychain（`core-foundation`） | Adapter抽出が必要 |
| `shogun-core` | 33,763 | capture / audio / meeting / notch / daemon | **要整理** |
| `apps/desktop/src-tauri` | 27,157 | 全面（`#[tauri::command]` × 188） | Interface層 |

### 2.2 すでに存在する Adapter seam（trait）

「Platform Adapter Interface を作る」は新規作業ではない。既に以下が存在する。

```text
AudioSource / Transcriber / DeepgramAuth   ← 音声
CursorReader / TextInserter                ← 入力・UI Automation
AxNode                                     ← Accessibility ツリー走査
HttpTransport / StreamingTransport         ← ネットワーク
MemoryBackend / Embedder                   ← ストレージ・埋め込み
BatchClient / AgentClient / BatchLane      ← LLM Provider
LocalEffector / ExecutionObserver          ← アクション実行
TraceabilitySink / EntitlementSource       ← 監査・課金
```

### 2.3 すでに存在する Agent Tool Protocol

`crates/shogun-mcp/src/tool_catalog.rs` が既にそれである。モデルには実MCPツール名を見せず、`list_calendar_events` のような **hub operation name** だけを見せる。Gmail を公式MCP から Composio へ全面移管したとき、変更は `toolmap` の編集だけで済んだ。**この設計は実戦で証明済みであり、拡張すればよい。**

### 2.4 すでに存在する「プラットフォーム独立性の番人」

`.github/workflows/ci.yml` の `rust` ジョブは **ubuntu-latest 上で `cargo clippy --workspace --exclude shogun-desktop-spike` と `cargo test --workspace --exclude shogun-desktop-spike` を実行している。**

つまり **`shogun-core` を含む全crateは、既に Linux でコンパイルとテストが通っている。** macOS固有コードは `#[cfg(target_os = "macos")]` と optional feature でゲートされており、除外されているのは Tauri シェル（`shogun-desktop-spike`）だけである。

これは本書における最重要の発見である。**「Coreはプラットフォーム独立」という条項は、すでにCIで守られている。** 少人数チームにとって、これは「ゼロから作る」ではなく「既にあるものを言語化して強化する」作業であることを意味する。

### 2.5 すでに存在する不変条件ガード

`scripts/` に自己テスト付きの検査スクリプトが6本あり、CIで実行されている。

```text
check-http-egress.py        FR-TR-03: 追跡済みegress以外がHTTPクライアントに依存しない
check-secret-exposure.py    不変条件7: Secret::expose() は許可ファイルのみ
check-media-writes.py       不変条件2: 画像・音声バイトがファイルシステムに到達しない
check-log-hygiene.py        ログにキャプチャ内容を含めない
check-migrations.py         マイグレーションは加算的のみ（DROP/RENAME/ALTER COLUMN禁止）
check-batch-source-filter.py 不変条件5: Batch laneのキー分離
```

**本書が新たに必要とするガードは、この確立されたパターンに1本追加するだけで済む**（§5 XP-01）。

### 2.6 実測で見つかった構造的な穴

| # | 穴 | 影響 |
|---|---|---|
| A | `event_log.id` が `INTEGER PRIMARY KEY`（rowid、デバイスローカル、単調増加） | **sync と構造的に非互換。かつ `state_provenance.event_id` が外部キーで参照しており、後から変えると全書き換え** |
| B | event に `device_id` / `schema_version` がない | どのデバイス由来か、どの版のスキーマかが復元できない |
| C | 「第三者サービス上の**可逆な**書き込み」を表す `Action` が型に存在しない | `LocalAction`（ローカル専用）と `SendAction`（不可逆送信）の間が空白。Gmailのdraft/label書き込みが `tool_catalog` から公開できていない（当該ファイルのコメントが自認） |
| D | ユーザーの権限設定（Always/Once/Ask/Never）を表す軸がない | 現行は Level（L1/L2/L3）と Entitlement（プラン）のみ |
| E | `apps/desktop/src-tauri` に `#[tauri::command]` が188個。`shogun-core` の daemon は10,863行（#324 で `daemon/` 配下の13モジュールへ分割済みだが、総量は減っていない） | Windows UI を作るとき188個を書き直す構造。CLAUDE.md 不変条件6（人間UIとAI APIは完全対称）が実効的に守られていない |
| F | `shogun-core` という名前が実体（OS結合あり）と食い違う | 名前が嘘をつくと、半年後に誰も境界を守らなくなる |

---

## 3. 決裁事項 — 今決めるべき10の判断

以下は **オーナー決裁を要する**。決まるまで該当領域の実装に着手しない。

### D1. Core は Rust crates のままとする。TypeScript で書き直さない

**決定**: SHOGUN Core は `crates/` 配下の Rust crate 群である。`packages/` はTS向けの**投影**（生成された型、Design Tokens、APIクライアント）に限定する。

**理由**: Coreは約70k LOCのRustで、Linux CIも通っている。これをTSへ移す作業はCore全体に及び、その間 macOS 側の前進と直列になる。得られるものは「Webでも動く」だけであり、SHOGUNの中核（キャプチャ・常駐・低レイテンシ）はWebで動かない。投入量に対する見返りが最も小さい選択肢である。

**帰結**: 「`packages/core/{memory, context, agent}`」という構成は採らない。採ると `crates/` と二重化するか、リライトを誘発する。

```text
crates/            ← SHOGUN Core（Rust）。唯一のCore
packages/protocol/ ← Rust型から生成した JSON Schema / TS / Swift / Kotlin（生成物、手書き禁止）
packages/tokens/   ← Design Tokens（唯一の手書き共有物）
native/<os>/       ← OS固有ネイティブコード（必要になったら作る。今は不要）
```

### D2. モバイルは Rust Core を持たない（Companion方式）

**決定**: iOS / Android には Rust Core を載せない。ローカルEvent Storeも持たせない。モバイルは Desktop / Relay の REST surface を叩く**薄いクライアント**とする。

**理由**: UniFFI 経由で Rust Core をモバイルに落とす案は、Coreが真に1つになる利点があるが、ビルド・配布・バイナリサイズ・デバッグの各コストが跳ね、少人数では吸収できない。加えて、モバイルの役割（Capture / Recall / Command / Approve / Notify）はいずれもローカルなワールドモデルを必要としない。

**帰結**: 「すべてのデバイスが Local Event Store を持つ」という一般論は **Desktop にのみ適用する**。本書はこれを明記する。書かないと、モバイル着手時に必ずSQLiteが持ち込まれ、そこから4プロダクト化が始まる。

```text
Desktop (Mac / Windows)          Mobile (iOS / Android)
  Local Event Store あり            Local Event Store なし
  Core あり（Rust）                 Core なし
  Observe / Understand / Execute    Capture / Recall / Command / Approve / Notify
```

### D3. Event ID を ULID 化する（本取り組みで唯一の不可逆作業）

**決定**: `event_log` に `uid TEXT UNIQUE`（ULID、device_id を含む）を **加算的マイグレーション**（`V<次の空き番号>__event_uid.sql`。本書更新時点で main は V24 まで使用済み）で追加し、新規書き込みで必ず埋める。既存行にはバックフィルする。参照は段階的に `uid` へ寄せる。

**理由**: 現行の `INTEGER PRIMARY KEY` はデバイスローカルな連番であり、デバイス間でマージできない。かつ `state_provenance.event_id` が外部キーで参照しているため、ユーザーのDBが育ってから変更すると全テーブル書き換えになる。**今なら追加カラム1本で済む。**

**注**: `scripts/check-migrations.py` が加算的マイグレーションのみを許すため、この方針は既存のガードと整合する。

### D4. state は同期しない。event だけ同期し、state は各デバイスで再生成する

**決定**: 将来 sync を実装する際、同期対象は `event_log` のみとする。`people` / `projects` / `commitments` / `open_loops` は同期せず、各デバイスが自分の event から再生成する。

**理由**: state は既に Dream Cycle が event から生成している。event は append-only なので**構造的に衝突しない**。これにより conflict resolution の設計が事実上不要になる。少人数にとってこの単純化の価値は非常に大きい。

**帰結**: state テーブルのIDはデバイスローカルのままでよい。ULID化は `event_log` に限定してよい。

### D5. Sync / Cloud は最後に回す。ただし「後から入れても壊れない形」だけ先に作る

**決定**: Sync Engine とクラウドDBの実装は §5 の最低優先度（P3）に置く。先に済ませておくのは D3（ULID）と D6（device_id 付与）と §4 の Schema 定義のみで、これらは sync を後から入れても既存データが壊れないための下地である。

**理由（技術）**: sync は単独で最大の投入量を要する作業であり、他のすべてより優先度が低い。

**理由（プロダクト）**: SHOGUNの製品約束の中核は `CLAUDE.md` 不変条件3「生データはデバイス外に出さない」である。**Syncを入れることは技術判断ではなく製品定義の変更**であり、実装着手前に以下のいずれを採るかを別途決裁する必要がある。

- (a) E2EE 前提（サーバは復号鍵を持たない）
- (b) event は出さず、要約済みの state だけを同期する
- (c) クラウドを経由せず、ローカルネットワーク / iCloud 的な経路で同期する

決めずに「Cloud」と書いたまま実装に降ろすと、不可逆な選択が暗黙に確定する。

### D6. Device Identity を正式化する

**決定**: `shogun-license` が既に持っている `device_id` を、ライセンス専用IDから**プラットフォーム全体のDevice Identity**へ昇格させる。すべての event に `device_id` と `platform`（`macos` / `windows` / `ios` / `android` / `web`）を付与する。

**理由**: 既に存在するものを昇格させるだけなので追加コストが小さい。D3 と同じマイグレーションで実施できる。

**Identity 階層**（v1では Organization / Workspace は単一固定でよいが、**スキーマには最初から入れる**）:

```text
Organization
 └ Workspace
     └ User
         └ Device (device_id, platform)
```

### D7. Permission は4軸に分ける。既存の型安全性は緩めない

**決定**: 以下の4軸を混ぜない。

| 軸 | 意味 | 現状 |
|---|---|---|
| **capability** | tool が何をするか（静的）: `read` / `draft` / `write` / `execute` / `external_side_effect` | **未実装** |
| **level** | ユーザー確認の強さ: L1 / L2 / L3 | `shogun_agents::permission::Level` |
| **grant** | ユーザー設定: Always / Once / Ask / Never | **未実装** |
| **entitlement** | プラン上の可否 | `shogun_agents::entitlement::Entitlements` |

**MUST**: `capability → level` を導出する関数を**1本だけ**用意し、すべての判定をそこに通す。

**MUST**: 現行の型レベル保証 —— 承認キューは `SendAction` しか受け取れず、`SendAction` は必ずL3 —— を**緩めない**。capability 軸は既存構造の上に足すのであって、置き換えではない。

**MUST（穴Cの解消）**: 「第三者サービス上の可逆な書き込み」を表す第3のカテゴリを `Action` に追加する。これは capability でいう `write`（≠ `external_side_effect`）にあたる。`tool_catalog.rs` が公開できずにいる Gmail draft / label 書き込みがこれで解ける。**この穴はプラットフォームを広げる前に塞ぐ**（広げてから直すと4面に波及する）。

### D8. `shogun-core` は分割リライトしない。運用定義 + CIで守る

**決定**: 33,763 LOC の `shogun-core` を「純粋Core」と「OS結合daemon」に物理分割するリライトは**行わない**。代わりに次の運用定義を採用する。

> **プラットフォーム独立である = Linux CI（`cargo clippy/test --workspace --exclude shogun-desktop-spike`）が通る。**

**理由**: この定義は既にCIで機械的に検査されており（§2.4）、追加の実装を要さない。一方、物理分割はCore全体を触る作業で、その間 Mac の前進と直列になる。同じ保証を得るのに投入量が桁違いに大きい。少人数における最適解は「既に効いている検査を憲法に昇格させる」ことである。

**ただし SHOULD**: 新規コードは `#[cfg(target_os = ...)]` を crate 全体に散らさず、**adapter モジュールに閉じる**。既存の cfg 使用箇所は13箇所であり、この規律は現実的に維持可能である。

**MUST**: `shogun-core` 内で OS固有APIを呼ぶファイルは、必ず trait の実装として書く（§2.2 のパターンに従う）。trait 定義側にOS依存を書かない。

### D9. Windows は「Adapter実装」ではなく「UI層の薄さの検証」から始める

**決定**: Windows 着手の前提条件を **「`#[tauri::command]` 188個が `shogun-mcp` の REST/MCP surface と対称になっていること」** とする。

**理由**: 現状のまま Windows に進むと、188個のコマンドを Windows 用に書き直すことになる。一方、`CLAUDE.md` 不変条件6「人間UIとAI API（MCP/CLI）は完全対称」を実効化すれば、tauri command は REST surface への**薄いプロキシ**に縮み、Windows UI は同じ surface を叩くだけになる。

これは新ルールの追加ではなく**既存ルールの徹底**であり、Mac版にとっても純粋な改善（AI経由の操作面が広がる）である。少人数において「1つの作業が2つの目的を果たす」ことは最重要の選定基準である。

**帰結**: Windows で最初に検証するのは Screen Capture の移植ではなく、**UI層が本当に薄いか**である。

### D10. 「やんちゃしてよい領域」を明示する

以下は捨てる前提で速く作ってよい。レビュー基準を上げない。

```text
UI / UX / アニメーション / LLM Provider / Embedding モデル / Vector DB
Analytics ツール / 通知ツール / Framework / 内部API / プロトタイプ機能
```

以下は慎重に決める（本書 §3 と §4 が対象）。

```text
Context Schema / Event ID / Device Identity / Workspace Identity
Sync Semantics / Encryption / Permission Model / Audit Model
Protocol Versioning / Public API / データ所有権 / 削除 / 移行戦略
```

---

## 4. Universal Context Schema

### 4.1 設計原則

**MUST**: 現行 `event_log`（`V1__init.sql`）から**機能を後退させない**。現行が保証しているものは以下であり、すべて維持する。

- `content TEXT NOT NULL` — テキストであることが保証される（FTS5 trigram索引が張れる）
- `content_hash` — dedup（FR-CAP-03）
- spatial-ready 列 — `display_id` / `window_bounds` / `window_pose` / `gaze_target`
- `state_provenance` による event ↔ state の多対多 provenance

**MUST NOT**: `content: unknown` のような型なしフィールドを採用しない。理由は3つあり、いずれも致命的である。

1. FTS索引が張れず、ローカル検索SLO（500ms）が満たせない
2. 保持期限・削除・redaction の対象判定ができず、**不変条件2/3の実装が成立しない**
3. マイグレーションが書けない

**MUST**: `type` による discriminated union とする（type ごとに content の型が確定する）。

### 4.2 Event の必須フィールド

```text
uid            ULID。グローバル一意。device_id を含む            ← D3
schema_version スキーマ版。最初から入れる                        ← 欠けていると移行不能
user_id        所有者                                           ← D6
workspace_id   所属ワークスペース                                ← D6
device_id      由来デバイス                                     ← D6
platform       macos | windows | ios | android | web            ← D6
app            アプリ識別子（任意）
type           screen | message | document | meeting | action | decision | task | ...
ts             発生時刻（unix ms）
content        type により型が確定する discriminated union
content_hash   dedup / sync冪等性                               ← 現行から継承
provenance     根拠への参照                                     ← SHOGUNの中核
relationships  関連 event の uid（任意）
privacy        { scope, retention, redacted }                   ← §4.3
spatial        { display_id, window_bounds, window_pose, gaze_target } ← 現行から継承
```

### 4.3 `privacy` は2値では足りない

`scope: "local" | "cloud"` だけでは SHOGUN の実際のデータ分類を表現できない。最低限3フィールドが要る。

```text
scope     local | synced | cloud
retention Hot(24h) | Warm(30d) | Cold(全履歴) | Ephemeral(N日、Visual recall の1〜3650日)
redacted  shogun-redact を通過済みか
```

**理由**: Visual recall のフレームは既に「ユーザー指定の有限期間（既定3日）」で自動削除される（`CLAUDE.md` 不変条件2の明示的例外）。この保持ポリシーが event に載っていないと、同期・エクスポート・削除の各処理が正しい判断をできない。

### 4.4 単一ソースとコード生成

**MUST**: Schema は **Rust型を単一ソース**とし、そこから JSON Schema / TypeScript / Swift / Kotlin を**生成**する。`packages/protocol/` は生成物の置き場であり、手書きファイルを置かない。

**理由**: Monorepo にすれば Schema のズレが防げるというのは誤りである。Monorepo は「同じコミットに入る」ことしか保証しない。**ズレを実際に防ぐのはコード生成だけである。** 少人数では手動同期は必ず破綻する。

**MUST**: `schema_version` を持ち、リーダーは未知バージョンのイベントを**捨てずに保持**する（前方互換）。

---

## 5. 優先度 — 何を先にやるか

### 5.0 本章はスケジュールではない

**MUST**: 本章に期間・工数・着手時期・納期を書き足さない。それはオーナーが決める。本章が規定するのは次の3つだけである。

1. **優先度**（P0〜P3）— どれを先に片付けるか
2. **依存関係** — 何が何の前提か（前提を飛ばした着手は手戻りになる）
3. **判断根拠** — なぜその優先度なのか

### 5.1 優先度の決定基準

優先度は「速さ」でも「大きさ」でもなく、次の3つで決めている。

| 基準 | 意味 | 高いほど優先 |
|---|---|---|
| **不可逆性** | 後回しにすると移行コストが増え続ける / 元に戻せない | ◎ |
| **波及係数** | 先に直さないと、後続の全プラットフォームに同じ歪みが複製される | ◎ |
| **前提性** | 他の作業の着手条件になっている | ○ |

| 優先度 | 定義 |
|---|---|
| **P0** | 不可逆、または以降すべての作業の受け皿。他に先行して着手する |
| **P1** | 波及係数が高く、P2以降の前提。P0完了後の主戦場 |
| **P2** | P1が揃って初めて着手が成立する。前倒しすると手戻りが確定する |
| **P3** | 未決裁事項でブロックされている。決裁が下りるまで着手しない |

### 5.2 優先度一覧

| 優先度 | # | 作業 | 前提 | 判断根拠 |
|---|---|---|---|---|
| **P0** | **XP-01** | `scripts/check-platform-boundary.py` を追加しCIに組み込む。依存方向違反（core→OS固有）と、trait定義側のOS依存を検出。既存6本と同じ自己テスト付きパターン | なし | **受け皿。** これが先に無いと、以降のすべての成果が次の変更で崩れる。単独で完結し、他を一切ブロックしない |
| **P0** | **XP-02** | `V<次の空き番号>__event_uid.sql`: `event_log` に `uid` / `device_id` / `platform` / `schema_version` を加算追加。書き込み経路で必ず埋める。既存行はバックフィル | D3・D6 決裁 | **唯一の不可逆作業。** ユーザーのDBが育つほど移行コストが増え続ける。遅らせることで得られるものが何もなく、失うものだけがある |
| **P1** | **XP-03** | Context Schema を Rust型として1箇所に定義し、`packages/protocol/` へ生成するパイプラインを作る | XP-02, D1 | XP-02 で確定した形を他言語から見える形に固定する。ここを飛ばすと各プラットフォームが独自にSchemaを手書きし、本書が最も避けたい「4プロダクト化」がSchemaから始まる |
| **P1** | **XP-04** | Permission 4軸整理 + 「可逆な外部書き込み」カテゴリ追加（穴C・D） | D7 決裁 | **既存の穴であり、広げる前に塞ぐ種類のもの。** 塞がずにプラットフォームを増やすと、同じ穴を4面で埋めることになる。加えて Gmail draft/label が `tool_catalog` から公開できるようになり、macOS版の機能追加を兼ねる |
| **P1** | **XP-05** | `#[tauri::command]` 188個を `shogun-mcp` の REST/MCP surface と対称化。tauri command は薄いプロキシへ。機能領域ごとに分割して段階投入する | XP-01 | **Windows着手の前提**であり、同時に `CLAUDE.md` 不変条件6の実効化＝macOS版のAI操作面拡張。1つの作業が2つの目的を果たすため、P1内では最も費用対効果が高い |
| **P2** | **XP-06** | Windows: REST surface 越しの最小UI（検索 + Recall + 承認）。キャプチャは**まだ実装しない** | XP-05 の主要領域完了 | アーキテクチャの検証点。ここでUIが薄く作れなければ XP-05 が不十分だったという明確なシグナルになる。XP-05 前に着手すると、この信号が読めない |
| **P2** | **XP-07** | Windows: Graphics Capture / UI Automation / Global Shortcut / Tray を Adapter として実装 | XP-06 | 検証が通ってから本体に着手する。XP-06 を飛ばすと、188コマンドの二重実装が確定する |
| **P3** | — | **Sync**（Local Event Store → Sync Engine → Cloud） | **D5 の製品決裁** | 単独で最大の投入量を要し、かつ `CLAUDE.md` 不変条件3に正面から当たる。決裁前の着手を禁止する（§3 D5） |
| **P3** | — | **Mobile Companion**（iOS → Android） | Sync の形が確定 | Approve / Remote Command はデバイス間疎通の形が決まらないと設計できない。Sync より先に着手すると必ず作り直しになる |

### 5.3 依存グラフ

前提を飛ばした着手は手戻りになる。順序として拘束力があるのはこの矢印だけで、それ以外は並べ替えてよい。

```text
XP-01 ──────────────► XP-05 ──► XP-06 ──► XP-07
  (受け皿)              (対称化)   (Win検証)  (Winアダプタ)

XP-02 ──► XP-03
  (uid)     (Schema生成)

XP-04                              [D7決裁を待つ]
  (Permission)

                    [D5決裁] ──► Sync ──► Mobile
```

- **XP-01 / XP-02 / XP-04 は互いに独立**である。3本のうちどれから着手してもよい
- **XP-03 は XP-02 の後**でなければ意味がない（固定すべき形が未確定のため）
- **XP-06 を XP-05 より前に着手しない**。検証の信号が読めなくなる
- **Sync と Mobile は決裁がブロッカー**であり、技術的な準備の問題ではない

### 5.4 完了定義 — 「Platform v1 の種が蒔けた」状態

到達時期は定めない。到達したかどうかだけを判定する。

```text
[ ] Linux CI が Core 全体で緑（= プラットフォーム独立が機械的に保証されている）
[ ] check-platform-boundary.py がCIで動いている                          … XP-01
[ ] すべての新規 event が uid / device_id / platform / schema_version を持つ … XP-02
[ ] Context Schema が Rust 単一ソースから他言語へ生成されている            … XP-03
[ ] Permission が4軸に分かれ、capability→level の導出が1本の関数に集約     … XP-04
[ ] 「可逆な外部書き込み」が Action 型で表現できる                        … XP-04
[ ] tauri command の主要領域が REST surface と対称                       … XP-05
[ ] Windows で最小UIが REST surface 越しに動く                           … XP-06
[ ] 上記のいずれによっても macOS 版の出荷が阻害されていない
```

最後の1行は他の8行と同格の完了条件である。満たさずに他を満たしても達成とみなさない。

## 6. チーム編成 — 少人数における方針

**決定**: OS別チーム（Mac / Windows / iOS / Android）を作らない。**Core / Desktop / Mobile というチーム分割も、現在の人数では作らない。**

**理由**: Core / Desktop / Mobile の3チーム構成は、各チームが独立に走れるだけの人数がある前提の議論である。現在の実体（少人数＋AI）でチームを切ると、コミュニケーションコストだけが増えて境界は守られない。境界を守るのは組織図ではなくCIである（Constitution 13条）。

**代替**: 組織構造ではなく**変更対象による規律**を採る。

| 変更対象 | 規律 |
|---|---|
| §3 D10 の「やんちゃ領域」 | 通常レビュー。速度優先 |
| `crates/`（Core） | **CIガード全通過が必須**。Schema / Permission / ID に触る変更は本書の更新（`docs:` コミット）を伴う |
| マイグレーション | `check-migrations.py` 通過 + ロールバック手順の添付（`CLAUDE.md` 既定） |

人が増えたときに初めて、Core / Desktop / Mobile の分割を検討する。その際も「OS別に切らない」原則は維持する。

---

## 7. Architecture Constitution（少人数版）

以下は原則として破らない。

```text
 1. Core is platform-independent — 運用定義: Linux CI が通ること。
 2. Every OS capability goes through an adapter (a trait). Trait 定義側に OS 依存を書かない。
 3. Context is represented by universal events.
 4. Every event carries a globally unique id, a schema version, a device id, and its provenance.
 5. Every device belongs to one unified user identity.
 6. Security and permissions are explicit, never implicit.
 7. Agents never directly depend on OS-specific APIs.
 8. Providers are replaceable dependencies, not architecture.
 9. Native implementations are allowed when they produce a better product.
10. Mobile and Desktop can have different interfaces while sharing the same SHOGUN.
11. Data leaving the device is a product decision, not an implementation detail.
12. Move fast on reversible decisions. Be deliberate on irreversible decisions.
13. Boundaries are enforced by CI, not by review discipline.
14. Schemas are generated from a single source, never hand-synced across languages.
```

条項4・11・13・14 は少人数運用のために追加した。特に **13 は本書全体の背骨**である —— レビューで守る約束は、人が少ないほど早く風化する。

---

## 8. 意図的に採らなかった選択肢（判断記録）

| 選択肢 | 採らない理由 |
|---|---|
| Core を TypeScript へ移す | 投入量が最大級である一方、Coreの本質（常駐・低レイテンシ・キャプチャ）がWebで動かないため見返りが最小（D1） |
| React Native / Flutter で全OS統一 | SHOGUN は OS Integration が深く、統一で失うものが得るものを上回る。共通化するのは Logic / Schema / Tokens であって UI ではない |
| UniFFI で Rust Core をモバイルへ | 少人数では吸収できないビルド・配布コスト。モバイルの役割にローカルCoreは不要（D2） |
| `shogun-core` の物理分割リライト | Linux CI という等価な保証が既に存在し、同じものを得るのに投入量だけが増える（D8） |
| Repo を OS ごとに分割 | API / Schema / Tool Protocol / Design System / Auth / Sync が各所でズレる。分割は人が増えてから再検討 |
| Sync を先に実装 | 単独で最大の投入量を要し、かつ製品定義の変更（不変条件3）を伴うため決裁が先（D5・P3） |
| Windows を Screen Capture から着手 | UI層が薄いことの検証が先。逆順だと188コマンドの二重実装が確定する（D9） |

---

## 9. 改訂ルール

- 本書の §3（決裁事項）と §4（Schema）の変更は `docs:` コミットでバージョン管理し、変更理由を §8 に追記する
- `CLAUDE.md` の絶対不変条件と矛盾する変更は行わない。矛盾が生じた場合は `CLAUDE.md` が優先する
- XP-nn は安定識別子とし、再採番しない
- **本書に期間・工数・着手時期・納期を書き足さない。** 本書が規定するのは優先度と依存関係のみで、スケジュールはオーナーが別途決める（§5.0）
