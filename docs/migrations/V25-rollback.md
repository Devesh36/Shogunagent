# V25__session_speakers.sql — ロールバック手順

対象: `session_speakers` テーブルの新設（FR-MT-15 次段: 話者スロット→実名のユーザー確定割り当て）。

## 影響範囲

additive のみ。新規テーブル1つ。`sessions(id)` を参照する（UNIQUE (session_id, speaker_key)）。
旧ビルドはこのテーブルを読まない。V25 が無い環境では話者は中立ラベル（Me / Other / Speaker N）で
表示されるだけで、文字起こし・Recap 本体は影響を受けない。

## Rollback

V25 が refinery 履歴に残ったまま V24 ビルドを起動しないこと。ユーザーが確定した話者名は
このテーブルにしか存在しないため、必要なら実行前に暗号化 DB を丸ごとバックアップする。

1. SHOGUN のデスクトップ / API / MCP プロセスをすべて停止する。
2. 暗号化 DB ファイルを複製して保管する（確定済み話者名の唯一の復旧経路）。
3. V25 対応ツールが入っている間に、以下のトランザクションを実行する。
4. `refinery_schema_history` に version 21 の行が無いこと、`PRAGMA foreign_key_check` が
   空であること、`session_speakers` が存在しないことを確認する。
5. その後に V24 ビルドを導入・起動する。

```sql
BEGIN;

DROP TABLE session_speakers;

DELETE FROM refinery_schema_history WHERE version = 25;

COMMIT;
```

> **訂正（issue #780）**: この手順は当初 `version = 21` を消す指示になっていた。それでは V21 が未適用扱いで残り、refinery の `verify_migrations` が MissingVersion で失敗してメモリ DB が開かなくなる（新旧どちらのバイナリでも）。さらに V25 が適用済みのまま残るので、再アップグレードしても V25 が再実行されない。**消すのは `version = 25`。**


## データ損失

ユーザーが確定した話者名（speaker_key → display_name の割り当てと出所）がすべて失われる。
`transcript_segments` の話者キー（`me` / `other` / `other:N`）自体は V9 の列であり影響を受けない。
