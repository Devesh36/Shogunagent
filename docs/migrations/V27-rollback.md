# V27__work_patterns.sql — ロールバック手順

対象: `offer_impressions` / `work_patterns` の2テーブル新設（継続学習v2 / Issue #242、FR-PAT-03/04）。

## 影響範囲

additive のみ。FKなし・既存テーブルへの変更なし。旧ビルドは両テーブルを読まない。
V27 が無い環境では提示ログとプロファイルが取れないだけで、提案・学習(L5 lessons)・キャプチャは
従来どおり動く。

## Rollback

V27 が refinery 履歴に残ったまま V26 ビルドを起動しないこと。`work_patterns.active = 0`
（ユーザーが切ったパターン）はこのテーブルにしか無いため、必要なら実行前に暗号化 DB を
バックアップする。`offer_impressions` は30日で自動失効する提示メタデータのみで、失って
困る行は構造的に存在しない。

1. SHOGUN のデスクトップ / API / MCP プロセスをすべて停止する。
2. 暗号化 DB ファイルを複製して保管する。
3. V27 対応ツールが入っている間に、以下のトランザクションを実行する。
4. `refinery_schema_history` に version 23 の行が無いこと、`PRAGMA foreign_key_check` が
   空であること、両テーブルが存在しないことを確認する。
5. その後に V26 ビルドを導入・起動する。

```sql
BEGIN;

DROP TABLE offer_impressions;
DROP TABLE work_patterns;

DELETE FROM refinery_schema_history WHERE version = 27;

COMMIT;
```

> **訂正（issue #780）**: この手順は当初 `version = 23` を消す指示になっていた。それでは V23 が未適用扱いで残り、refinery の `verify_migrations` が MissingVersion で失敗してメモリ DB が開かなくなる（新旧どちらのバイナリでも）。さらに V27 が適用済みのまま残るので、再アップグレードしても V27 が再実行されない。**消すのは `version = 27`。**


## データ損失

提示ログ（30日分のメタデータ）と行動プロファイル（毎晩再計算される導出値＋ユーザーのON/OFF
スイッチ）が失われる。`feedback_events`（V16/V19）は影響を受けないため、プロファイルの
数値は次回蒸留で概ね再構築できるが、**ユーザーが切ったパターンのOFF状態は戻らない**。
