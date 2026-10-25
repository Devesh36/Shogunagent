# V40__writing_voice_offer.sql — ロールバック手順

対象: `writing_voice_settings.offer_state`（'pending' / 'decided'）。ノッチが一度だけ出す
「書き方をリライトに使う？」の同意カードに答えたかどうかを記憶する列。

## 影響範囲

additive のみ（列追加、既定値 'pending'）。既存の `enabled` / `revision` の意味は
変わらない。ただし同じ変更で **収集は `enabled` に依存しなくなった**（例文はローカルに
常時学習し、`enabled` はプロバイダへの同梱だけを制御する）。V40 より前のバイナリは
`enabled=0` のとき収集も止めるので、ダウングレード後は学習が止まるだけで壊れない。

## ロールバック

SQLite は `ALTER TABLE … DROP COLUMN` を V3.35 以降でサポートするが、CHECK 付き列のため
テーブル再構築が要る。列を残したままでも旧バイナリは動く（未知の列は無視される）ので、
通常は履歴行の削除だけで十分:

```sql
BEGIN;
DELETE FROM refinery_schema_history WHERE version = 40;
COMMIT;
```

> **訂正（issue #780）**: この手順は当初 `version = 39` を消す指示になっていた。それでは V39 が未適用扱いで残り、refinery の `verify_migrations` が MissingVersion で失敗してメモリ DB が開かなくなる（新旧どちらのバイナリでも）。さらに V40 が適用済みのまま残るので、再アップグレードしても V40 が再実行されない。**消すのは `version = 40`。**


## データ損失

なし。ロールバック後に V40 を再適用すると `offer_state` は 'pending' に戻り、
既に答えたユーザーにカードがもう一度出る（`enabled=1` の人には出ない）。
