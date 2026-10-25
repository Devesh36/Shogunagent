# ローカル埋め込みモデル（開発者向けオプトイン）

**【2026-08-23 決定 / issue #237】配布ビルドはローカル埋め込みモデルも ONNX Runtime も同梱しない。**
リリースの検索は**字面一致（FTS5 trigram）のみ**であり、モデルを探しにも行かない。この文書は、
意味検索の挙動を手元で確認したい**開発者だけ**のための手順。

| | 配布ビルド | 開発者オプトイン |
|---|---|---|
| モデル重み（約470MB） | **同梱しない・ダウンロードしない** | 各自 `scripts/fetch-embedding-model.sh` |
| ONNX Runtime | **同梱しない** | `brew install onnxruntime` |
| Cargo feature | なし（既定） | `--features local-embedder` |
| 検索 | 字面一致のみ | ハイブリッド（字面＋意味、RRF融合） |

既存ユーザーのDBに古いビルドが書いたベクトルが残っていても壊れない。クエリ側のベクトルが無い
ので**参照されないだけ**で、削除もマイグレーションもしない。

---

## 1. モデルの取得（開発機のみ）

```bash
./scripts/fetch-embedding-model.sh
```

`models/multilingual-e5-small/` に `model.onnx` と `tokenizer.json` を置く（`models/` は
gitignore 済み）。既にあるファイルはスキップするので再実行は安全。**このディレクトリを
`.app` に入れる経路は存在しない**（`tauri.conf.json` の `bundle.resources` から削除済み）。

## 2. ONNX Runtime

`ort` は実行時ロード（`load-dynamic`）。ビルド時にダウンロードしないので、`libonnxruntime.dylib`
が実行時に必要。

```bash
brew install onnxruntime
```

> **バージョンは選べない。** `ort 2.0.0-rc.10` は `ORT_API_VERSION = 22` でコンパイルされており、
> ONNX Runtime **1.22.x** を指す。ヘッダより古いランタイムは `OrtApi` に null を返し、セッション
> 構築が失敗する。

探索順（`OnnxEmbedder::load` 内、`resolve_runtime`）:

1. `ORT_DYLIB_PATH`（明示指定が最優先。指定先が存在しなければエラー）
2. `/opt/homebrew/lib/`（Apple Silicon の Homebrew）
3. `/usr/local/lib/` → `/opt/local/lib/` → `/usr/lib/`

> Apple Silicon の注意: `dlopen` はライブラリ名だけ渡されると `/usr/local/lib` と `/usr/lib` しか
> 探さず、Homebrew の `/opt/homebrew/lib` は見に行かない。`brew install onnxruntime` しただけでは
> 見つからないため、上記の探索を入れてある。
>
> さらに `ort` は**ライブラリが見つからないとpanicする**（Resultを返さない）。そのままだと
> 「モデルが無ければ字面検索に落ちる」という設計が成立しないので、ort に触る前に存在確認して
> `Err` を返すようにしてある。

## 3. 開発中の起動

```bash
export SHOGUN_EMBED_MODEL=$PWD/models/multilingual-e5-small/model.onnx
export SHOGUN_EMBED_TOKENIZER=$PWD/models/multilingual-e5-small/tokenizer.json
cd apps/desktop && cargo tauri dev --features local-embedder
```

feature を付けなければ埋め込みコードはコンパイルすらされない（＝配布ビルドと同じ構成）。
環境変数だけ設定しても何も起きない。

起動ログで確認:

| ログ | 意味 |
|---|---|
| `[embed] local model loaded — hybrid search enabled (developer build)` | 意味検索が有効 |
| `[embed] SHOGUN_EMBED_MODEL / SHOGUN_EMBED_TOKENIZER unset — search stays lexical` | 環境変数未設定（正常） |
| `[embed] model present but failed to load (…)` | **異常**。ファイルはあるが使えない |
| `[embed] embedded N event(s)` | バックログを埋め込み中 |

## 4. モデルが正しく動いているかの確認

```bash
SHOGUN_EMBED_MODEL=$PWD/models/multilingual-e5-small/model.onnx \
SHOGUN_EMBED_TOKENIZER=$PWD/models/multilingual-e5-small/tokenizer.json \
cargo test -p shogun-memory --features onnx -- --ignored --nocapture
```

「質問に対して、答えを含む文が無関係な文より高い類似度になる」ことを検証する。ここが通らなければ
モデルかトークナイザの不整合。

## 5. 設計上の注意（静かに精度が落ちる箇所）

実装で押さえている3点。いずれも**エラーにならず精度だけ落ちる**ので、変更時は注意:

1. **e5のロール接頭辞** — `query:` / `passage:` を付けないと精度が明確に落ちる
2. **attention maskを使った平均プーリング** — パディングを平均に混ぜると短文がパディング
   ベクトルに引きずられる
3. **L2正規化** — コサイン類似度とベクトルストアの距離が単位ベクトルを前提にしている

## 6. 埋め込みの動作（feature ON のときだけ）

- 書き込み経路では**やらない**（FR-MEM-22）。バックグラウンドジョブが少しずつ処理する
- 遅れても問題ない — 未埋め込みのイベントも**FTSでは見つかる**。言い換えで引けないだけ
- 推論スレッドは1本に固定（アイドルCPU 5%のSLOがあるため、キャプチャやUIとコアを奪い合わない）
- feature OFF ではジョブ自体を起動しない（存在しないモデルを待つスレッドを作らない）
