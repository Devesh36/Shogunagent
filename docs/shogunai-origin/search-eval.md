# Search accuracy: golden eval and the 2026-10-14 lexical rework

Owner request (2026-10-14): 「ユーザー体験を劇的に良くしたい。精度です。検索の精度…今全然ダメ」.
This page records how keyword search is measured, what was wrong, what changed, and the numbers.

## The golden eval

`crates/shogun-core/tests/search_golden_eval.rs` runs the canonical search —
`Db::search_request`, exactly what the search box, MCP, REST, CLI and the AI keyword phase call —
over one synthetic store, offline and deterministic:

- captured windows **with their UI chrome** (Mail, Slack, browser, Notion, Japanese mail /
  calendar chrome), OCR text, Gmail and Google Calendar items, meeting recaps and transcripts,
  derived people / projects / commitments / open loops;
- near-duplicates (a draft and a final, an old and a new price, an old and a new deadline);
- a **flood**: 600 newer rows sharing 「予算の」 with the one row that answers 「予算の承認」;
- 132 questions, most of them Japanese: two-character words (会議, 請求, 予算), surnames (田中,
  佐藤), particles (田中さんの件), okurigana variants (打合せ / 打ち合わせ / 打ち合せ, 申込 /
  申し込み), katakana (バグ, half-width ｷｬﾝﾍﾟｰﾝ), compounds, mixed Japanese + English (Q3の売上,
  CI が不安定), short ASCII (Q3, PR, CI, UX, v2), English, relative dates (昨日の会議, 先週の定例会議,
  yesterday meeting), and six questions about things that are not in memory.

Every name, company and figure is invented. Metrics: recall@5 / @10 (fraction of a case's
relevant results in the top k), MRR, zero-result rate (positive cases that returned nothing),
answerable rate (positive cases the search judged answerable) and false-answerable rate (negative
cases judged answerable). The test asserts floors a little under the measured result, so a
regression fails CI (`cargo test -p shogun-core --features daemon-server`).

```bash
cargo test -p shogun-core --features db --test search_golden_eval -- --nocapture
SHOGUN_SEARCH_EVAL_VERBOSE=1 cargo test -p shogun-core --features db --test search_golden_eval -- --nocapture
```

## Results

Before = `origin/main` 6251f54a; after = this change. Same eval file for both.

| category | cases | R@5 before → after | MRR before → after | zero-result before → after | answerable before → after |
|---|---:|---:|---:|---:|---:|
| ja_2char | 20 | 0.050 → 0.975 | 0.050 → 1.000 | 0.950 → 0.000 | 0.000 → 1.000 |
| ja_person | 10 | 0.333 → 0.950 | 0.350 → 0.850 | 0.600 → 0.000 | 0.100 → 1.000 |
| ja_particle | 12 | 0.861 → 1.000 | 0.875 → 0.958 | 0.000 → 0.000 | 0.000 → 1.000 |
| ja_okurigana | 9 | 0.500 → 0.944 | 0.667 → 1.000 | 0.333 → 0.000 | 0.000 → 1.000 |
| ja_katakana | 10 | 0.800 → 1.000 | 0.800 → 1.000 | 0.200 → 0.000 | 0.000 → 1.000 |
| ja_compound | 13 | 1.000 → 1.000 | 0.885 → 0.923 | 0.000 → 0.000 | 0.000 → 1.000 |
| mixed | 12 | 0.875 → 1.000 | 0.812 → 0.896 | 0.083 → 0.000 | 0.167 → 0.750 |
| short_ascii | 10 | 0.750 → 1.000 | 0.733 → 0.950 | 0.200 → 0.000 | 0.200 → 1.000 |
| english | 14 | 0.976 → 0.976 | 0.964 → 0.952 | 0.000 → 0.000 | 0.071 → 0.214 |
| flood | 4 | 0.500 → 1.000 | 0.375 → 1.000 | 0.000 → 0.000 | 0.000 → 1.000 |
| relative_date | 8 | 0.562 → 1.000 | 0.625 → 1.000 | 0.250 → 0.000 | 0.000 → 0.750 |
| near_dup | 4 | 1.000 → 1.000 | 0.583 → 0.708 | 0.000 → 0.000 | 0.000 → 1.000 |
| negative | 6 | — | — | — | false-answerable 0 → 0 |
| **all** | 132 | **0.653 → 0.985** | **0.634 → 0.948** | **0.278 → 0.000** | **0.048 → 0.873** |

recall@10: 0.653 → 0.989. English MRR moved 0.964 → 0.952 (one case now ranks its answer second behind a meeting recap with a title match). English lowercase questions stay mostly "not answerable" on purpose:
answerability still requires an anchor (a named word, an identifier, a Japanese compound, a quoted
phrase, or a rare long word), and common lowercase English words are none of those.

## What was wrong (audit, 2026-10-14)

- Query terms under three characters were dropped (trigram index), so 会議, 請求, 田中, Q3, PR
  returned nothing; long CJK runs became OR'd sliding trigrams, so 田中さんの件 matched
  佐藤さんの資料 through 「さんの」.
- No NFKC, no okurigana handling (打合せ ≠ 打ち合わせ); English-only stopwords and date cues.
- Above 512 FTS matches, archive-wide search fell back to the newest rows **unscored**: a common
  word in the question buried the one row that matched all of it.
- No recency or source-quality prior; Japanese questions never had an answerability anchor, so
  every one of them was "not answerable"; excerpts for CJK fell back to the window head (chrome).

## What changed

- **Terms** (`crates/shogun-memory/src/search/terms.rs`): NFKC + lowercase; split at script
  boundaries; hiragana particles / honorifics split and are dropped; okurigana inside a kanji
  compound is folded (打ち合わせ → key 打合); Japanese relative-date words are not terms; two-letter
  Latin tokens with a letter (Q3, PR, v2) are terms unless English function words. A CJK word of
  three or more characters is one quoted substring for the trigram index.
- **Gram index** (V81 `event_grams_fts`, owner-approved): per event, the distinct kanji bigrams
  (okurigana folded), katakana / hangul bigrams and two-character Latin tokens, `detail=none`.
  Indexed on the write path; a pending queue (filled by triggers) and a newest-first back-fill run
  in the hourly local maintenance (3 s budget per pass). Rollback: `docs/migrations/V81-rollback.md`.
- **Retrieval** (`search/lexical.rs`): per concept, read matching rowids newest-first up to
  50,000 from the trigram and gram indexes (rowids only, no ranking inside SQLite); take rows
  matching **all** concepts that occur at all first (≤ 1,500; a word the archive never contains
  does not veto the AND), then the rows whose matched concepts have the highest summed idf
  (≤ 300); score that bounded set
  in Rust with BM25 over concepts (idf from the postings counts, title ×2, a bonus when a
  multi-word Japanese run appears verbatim, ×1.5 idf for a concept naming a known person or
  project). Replaces both the archive-wide BM25 path and the unscored >512 fallback. Derived
  documents use the same scorer plus a bounded substring scan for two-character terms; meetings
  use it too, and a question that names only a time and "meeting" lists that time's meetings.
- **Ranking** (`search/federated.rs`): bounded priors on every lane score — source quality
  (meetings / derived / notes ×1.10, mail / calendar / chat ×1.05, OCR ×0.85) and recency
  (≤ ×1.10, half-life 14 days). Measured MRR: 0.929 neither, 0.941 recency only, 0.947 both;
  entity boost +0.001. Down-weighting short "chrome" lines was tried and changed nothing, so it is
  not in the scorer.
- **Answerability**: Japanese compounds and Latin identifiers (NDA, Q3, v2, INV-…) count as
  anchors, matched with the same concept matcher (so 打合せ is satisfied by 打ち合わせ).
- **Dates** (`search/filtering.rs` `query_time_window`, signature unchanged): 昨日 / きのう,
  一昨日, 今日 / 本日 / 今朝, N日前 / N days ago (kanji numerals too), 過去N日 / last N days,
  今週 / 先週 / 先々週 (weeks start Monday), 今月 / 先月, 今年 / 去年. A range named in the
  question bounds the raw-event lane as well (`daemon/search.rs`), and may reach before the Warm
  window.
- **Excerpts** (`search/excerpt.rs`): centred on the window covering the most distinct concepts,
  preferring running text over short chrome lines; with no match, start at the first non-chrome
  line instead of the window head.
- **AI expansion prompt**: tells the model what the index can match (Japanese words of two or
  more characters, short codes), to write Japanese as separate words, that okurigana variants
  match each other, and that a compound may be quoted when it must match exactly.

## Size and latency

Gram index size (`gram_index_size_report`, 3,000 synthetic rows each): kanji-dense Japanese
1,172 KiB next to `event_fts` 1,232 KiB and `event_log` 1,496 KiB (≈ +43 % of log + trigram
index — a worst case, real windows carry more kana and Latin chrome); English 68 KiB next to
`event_fts` 2,208 KiB and `event_log` 1,140 KiB (≈ +2 %). Screen-frame JPEGs (up to 2 GiB) are
not affected.

Latency — `memory-bench`, release, 100k events, `--path federated` (the daemon's fusion), on-disk,
Linux container (advisory, not a certifying run):

| workload | p50 before → after | p95 before → after | recall@5 / MRR before → after |
|---|---:|---:|---:|
| clean (500 q) | 16.64 → 9.20 ms | 24.57 → 11.11 ms | 1.000 / 1.000 → 1.000 / 1.000 |
| duplicate (500 q) | 14.63 → 8.90 ms | 25.56 → 14.50 ms | 1.000 / 1.000 → 1.000 / 1.000 |
| temporal (12 q) | 16.09 → 8.91 ms | 18.10 → 13.02 ms | 1.000 / 0.500 → 1.000 / 0.500 |

Write latency (clean): p50 0.50 → 0.52 ms, p95 0.83 → 0.85 ms (inline gram indexing).
The first version capped postings at 2,000 per concept and dropped recall@5 on the clean
workload to 0.21 (a question word absent from the archive vetoed the AND, and old answers fell
outside the truncated postings of common words); the caps and the AND rule above fixed it.
The temporal workload's stale-outranking (a superseded value ranking first) is unchanged: the
bounded recency prior is deliberately too small to flip a lexically stronger stale row; not addressed here.
