//! Golden search evaluation (docs/search-eval.md).
//!
//! The canonical keyword search — `Db::search_request`, exactly as the search box, the Memory API
//! (MCP / REST / CLI) and the AI keyword phase call it — over one synthetic corpus that looks like
//! a real memory store: captured windows with their UI chrome, OCR text, mail, calendar, meeting
//! recaps and transcripts, derived people / projects / commitments / open loops, near-duplicates,
//! and a flood of more than 512 rows sharing a common phrase. Most questions are Japanese, because
//! that is where search was weakest: two-character words (会議, 請求, 予算), surnames (田中, 佐藤),
//! particles (田中さんの件), okurigana variants (打合せ / 打ち合わせ), katakana, mixed JA + EN,
//! and short ASCII tokens (Q3, PR, CI, UX, v2).
//!
//! Every name, company and figure here is invented. Nothing is model-backed: the run is offline,
//! deterministic, and part of `cargo test -p shogun-core --features db` (CI runs it under
//! `--features daemon-server`). Print the table with
//! `cargo test -p shogun-core --features db --test search_golden_eval -- --nocapture`.
//!
//! Metrics, per category and overall:
//! - recall@k: fraction of a case's relevant results in the top k, averaged over cases;
//! - MRR: mean of 1 / rank of the first relevant result (0 when none);
//! - zero-result rate: positive cases (something relevant exists) that returned nothing at all;
//! - answerable rate: positive cases the search judged answerable;
//! - false-answerable rate: negative cases (nothing relevant exists) judged answerable.
//!
//! The floor asserted at the end is the measured result minus a small margin, so a change that
//! makes search worse fails here. Raise it when search gets better.
#![cfg(feature = "db")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;

use shogun_core::daemon::{Db, SearchRequestV1};
use shogun_memory::event_log::NewEvent;
use shogun_memory::search::{CanonicalEvidenceId, FederatedHit, FederatedSourceId};
use shogun_memory::search_documents::SourceRef;
use shogun_memory::state::{
    CommitmentDirection, CommitmentStatus, NewCommitment, NewOpenLoop, NewPerson, NewProject,
    OpenLoopKind, ProjectStatus, Provenance,
};

const DAY: i64 = 86_400_000;
const HOUR: i64 = 3_600_000;
/// Wednesday 2026-10-14 12:00 UTC. Linux resolves local days in UTC, so relative dates are exact.
const NOW: i64 = 20_740 * DAY + 12 * HOUR;

fn at(days_ago: i64, hour: i64) -> i64 {
    // Days are counted back from today's midnight, so "1 day ago" is yesterday.
    (NOW / DAY - days_ago) * DAY + hour * HOUR
}

// UI chrome, the noise every real capture carries

const MAIL_CHROME: &str = "Mailboxes\nInbox 24\nDrafts 3\nSent\nJunk\nTrash\nArchive\nFlagged\nReply  Reply All  Forward  Archive  Move to Junk\n";
const SLACK_CHROME: &str = "Home  DMs  Activity  Later  More\nThreads\nDrafts & sent\nChannels\n# general\n# random\n# product\nDirect messages\n";
const BROWSER_CHROME: &str = "File Edit View History Bookmarks Window Help\nBack Forward Reload\nNew Tab  Search or type a URL\n";
const NOTION_CHROME: &str = "Search\nUpdates\nSettings & members\nNew page\nTemplates\nImport\nTrash\nShare  Comments  Favorites\n";
const JA_MAIL_CHROME: &str = "メールボックス\n受信 (12)\n下書き\n送信済み\n迷惑メール\nゴミ箱\nアーカイブ\n返信  全員に返信  転送\n";
const JA_SLACK_CHROME: &str = "ホーム  DM  アクティビティ  後で  その他\nスレッド\n下書きと送信済み\nチャンネル\n# 全体\n# 雑談\n";
const JA_CAL_CHROME: &str = "カレンダー\n日  週  月  年\n今日\n";
const OCR_CHROME: &str = "◀ ▶  100%  ⌘  Wed 14 Oct  ●●●  Share  Export\n";

enum Item {
    Event {
        label: &'static str,
        days_ago: i64,
        source: &'static str,
        app: &'static str,
        title: &'static str,
        chrome: &'static str,
        content: &'static str,
    },
    Meeting {
        label: &'static str,
        days_ago: i64,
        title: &'static str,
        lines: &'static [&'static str],
        summary: &'static str,
        decisions: &'static [&'static str],
        next_actions: &'static [&'static str],
    },
    Person {
        label: &'static str,
        name: &'static str,
        summary: &'static str,
    },
    Project {
        label: &'static str,
        name: &'static str,
        summary: &'static str,
    },
    Commitment {
        label: &'static str,
        text: &'static str,
    },
    OpenLoop {
        label: &'static str,
        text: &'static str,
    },
}

#[allow(clippy::too_many_arguments)]
fn ev(
    label: &'static str,
    days_ago: i64,
    source: &'static str,
    app: &'static str,
    title: &'static str,
    chrome: &'static str,
    content: &'static str,
) -> Item {
    Item::Event { label, days_ago, source, app, title, chrome, content }
}

fn corpus() -> Vec<Item> {
    use Item::*;
    vec![
        // People and the 田中 cluster (surname, particles, honorifics).
        ev("tanaka_quote_mail", 3, "gmail", "com.google.mail", "見積もりの件（北斗商事 田中）", "",
           "From: 田中 健一 <k.tanaka@hokuto.example>\n先日の見積もりについて、値引き率を5%から8%に変更できないかご相談です。来週火曜までにご回答いただけますか。北斗商事 営業部 田中"),
        ev("sato_wireframe", 6, "capture", "notion.id", "新規会員アプリ / ワイヤーフレーム", NOTION_CHROME,
           "佐藤さんの資料: 新規会員アプリのワイヤーフレーム v3。オンボーディング画面を3枚から2枚に削減しました。"),
        ev("suzuki_expense", 7, "capture", "com.tinyspeck.slackmacgap", "# 経理", JA_SLACK_CHROME,
           "鈴木さんの件、経費精算の締め日を毎月25日に変更する方向で調整中です。"),
        ev("uchiawase_warehouse", 4, "capture", "com.apple.Notes", "メモ", "",
           "明日の打ち合わせは田中さんと北斗商事の新倉庫について。場所は本社5階の会議室B。"),
        Person { label: "person_tanaka", name: "田中 健一", summary: "北斗商事の営業部長。見積もりと新倉庫の窓口。" },
        Person { label: "person_sato", name: "佐藤 美咲", summary: "デザイナー。新規会員アプリのUIを担当。" },
        Person { label: "person_priya", name: "Priya Nair", summary: "Product manager at Nimbus Labs; owns the integration sandbox." },
        Commitment { label: "commit_tanaka_quote", text: "田中さんに修正見積もりを来週火曜までに送る" },
        OpenLoop { label: "loop_nda", text: "高橋さんからNDA第3条のレビュー結果の返答待ち" },
        Project { label: "project_aoba", name: "青葉プロジェクト", summary: "社内ポータル刷新。11月末リリース予定。担当は山本と佐藤。" },
        Project { label: "project_orchid", name: "Orchid launch", summary: "Consumer launch of Orchid; owner Ren Ogawa; QA regression is the critical path." },

        // Meetings: weekly syncs this week and last week, and yesterday's management meeting.
        Meeting {
            label: "m_teirei_this", days_ago: 2, title: "週次定例会議",
            lines: &["今週の定例では予算の見直しについて話しました", "広告費は15%削減で合意です", "採用計画は来週再確認します"],
            summary: "週次定例会議：Q4予算の見直しと採用計画の確認。広告費を15%削減することで合意。",
            decisions: &["広告費を15%削減"], next_actions: &["鈴木さんが修正予算を金曜までに共有"],
        },
        Meeting {
            label: "m_teirei_last", days_ago: 9, title: "週次定例会議",
            lines: &["先週の定例ではオフィス移転の候補地を比較しました", "渋谷と品川の2案に絞りました"],
            summary: "週次定例会議：オフィス移転の候補地を渋谷と品川に絞り込み。",
            decisions: &["候補地を渋谷と品川の2案に絞る"], next_actions: &["山本さんが内見を手配"],
        },
        Meeting {
            label: "m_keiei", days_ago: 1, title: "経営会議",
            lines: &["下期の価格改定について議論しました", "スタンダードプランを月額4,900円に改定する案を承認"],
            summary: "経営会議：価格改定案を承認。スタンダードプランは12月1日から月額4,900円。",
            decisions: &["価格改定を承認"], next_actions: &["高橋さんが利用規約の改定文案を作成"],
        },
        Meeting {
            label: "m_orchid_retro", days_ago: 5, title: "Orchid launch retro",
            lines: &["We missed the QA window because the regression suite took three days", "Ren suggested running the suite in parallel shards"],
            summary: "Retro: the QA regression suite is the bottleneck; we decided to run it in parallel shards.",
            decisions: &["Run the regression suite in parallel shards"], next_actions: &["Ren to split the suite by Friday"],
        },
        Meeting {
            label: "m_1on1_sato", days_ago: 11, title: "1on1 佐藤",
            lines: &["新規会員アプリのデザインレビューの進め方を相談", "レビューは隔週で行う"],
            summary: "佐藤さんとの1on1：デザインレビューは隔週で実施。",
            decisions: &["デザインレビューを隔週にする"], next_actions: &[],
        },

        // Two-character words: 会議 / 請求 / 予算 / 契約 / 採用 / 出張 / 障害 / 決済.
        ev("kaigi_room", 1, "gcal", "com.google.calendar", "会議室A 予約", "",
           "会議室A 予約: 10/15 14:00-15:00 デザインレビュー（佐藤・山本）"),
        ev("teirei_agenda", 10, "capture", "notion.id", "定例会議 アジェンダ", NOTION_CHROME,
           "週次定例会議 アジェンダ テンプレート: 1. 先週の振り返り 2. 数字の確認 3. 共有事項"),
        ev("seikyu_oct", 5, "gmail", "com.google.mail", "【請求書】10月分のご請求について", "",
           "Kestrel Foods 経理ご担当者様\n10月分の請求書（INV-2026-1043）を送付します。お支払期限は11月30日です。"),
        ev("seikyu_aug", 70, "gmail", "com.google.mail", "【請求書】8月分のご請求について", "",
           "Kestrel Foods 経理ご担当者様\n8月分の請求書（INV-2026-0871）を送付します。お支払期限は9月30日です。"),
        ev("yosan_shonin", 25, "capture", "com.apple.Notes", "メモ", "",
           "予算の承認: 2026年度下期のマーケティング予算 1,200万円が経営会議で承認されました。"),
        ev("keiyaku_nda", 8, "gmail", "com.google.mail", "NDA レビュー依頼", "",
           "契約書レビュー依頼：NDA 第3条（秘密保持期間）を3年から5年に変更したい、と高橋さんから連絡がありました。"),
        ev("saiyo_mensetsu", 3, "capture", "com.apple.mail", "採用面接の日程", JA_MAIL_CHROME,
           "採用面接の日程：バックエンドエンジニア候補2名、10/20と10/22。人事 伊藤"),
        ev("shutcho_osaka", 2, "capture", "com.apple.Notes", "メモ", "",
           "出張メモ: 来週の出張は大阪。新幹線は10:03発のぞみ。ホテルは梅田。"),
        ev("shogai_kessai", 5, "capture", "com.tinyspeck.slackmacgap", "# incident", JA_SLACK_CHROME,
           "障害報告：10/9 決済APIが40分停止しました。原因はTLS証明書の期限切れ。再発防止として自動更新を設定。"),
        ev("kakaku_notice", 1, "capture", "com.google.Chrome", "価格改定のお知らせ（下書き）", BROWSER_CHROME,
           "価格改定のお知らせ：スタンダードプランを12月1日から月額4,900円に改定します。"),
        ev("shiharai_terms", 12, "capture", "com.microsoft.Excel", "支払条件.xlsx", "",
           "支払い条件の確認：Kestrel Foodsへの支払いは月末締め翌月末払い。"),
        ev("office_move", 9, "capture", "notion.id", "オフィス移転", NOTION_CHROME,
           "オフィス移転プロジェクト：候補地は渋谷と品川。来月中に内見。"),
        ev("kintai", 6, "capture", "com.google.Chrome", "勤怠システム", BROWSER_CHROME,
           "勤怠の締めは毎月末日。残業申請は前日までに上長へ。"),

        // Okurigana and spelling variants.
        ev("uchiawase_aoba", 6, "gcal", "com.google.calendar", "打合せ：青葉プロジェクト進捗", "",
           "青葉プロジェクト進捗打合せ 参加者: 佐藤, 山本 / 会議室B"),
        ev("moshikomi", 13, "capture", "com.google.Chrome", "セミナー申込", BROWSER_CHROME,
           "セミナーの申し込みを受け付けました。開催日は10/28、オンライン開催です。"),
        ev("torikeshi", 4, "gmail", "com.google.mail", "注文取消のご連絡", "",
           "ご注文（#55102）の取り消しを承りました。返金は5営業日以内に行われます。"),

        // Katakana.
        ev("release_note", 3, "capture", "com.apple.TextEdit", "リリースノート", "",
           "リリースノート v3.2: ダークモードに対応しました。ログイン画面のバグを修正しました。"),
        ev("memo_cho", 7, "capture", "com.apple.Notes", "メモ", "",
           "メモ: サーバーのディスク使用率が85%を超えたらアラートを出す。"),
        ev("design_review_ja", 11, "capture", "com.figma.Desktop", "デザインレビュー", "",
           "デザインレビューのフィードバック：ボタンの色をもう少し落ち着いたトーンに。"),
        ev("campaign", 15, "capture", "com.microsoft.Powerpoint", "キャンペーン企画", "",
           "年末キャンペーン企画：クーポン配布とSNS広告の組み合わせ。予算は300万円。"),

        // Mixed Japanese + English.
        ev("api_v2_ja", 12, "capture", "notion.id", "API v2 仕様", NOTION_CHROME,
           "API v2 仕様ドラフト: ページネーションはカーソル方式に変更し、offset パラメータは非推奨にする。"),
        ev("slack_連携", 8, "capture", "com.tinyspeck.slackmacgap", "# dev", JA_SLACK_CHROME,
           "Slack連携のOAuthスコープを見直し、chat:write だけに絞りました。"),
        ev("ci_flaky_ja", 2, "capture", "com.github.GitHubClient", "CI", "",
           "CI が不安定: search テストが時々タイムアウト。PR #1187 で修正済み。"),

        // Short ASCII tokens.
        ev("q3_revenue", 8, "capture", "com.microsoft.Excel", "Q3 review.xlsx", "",
           "Q3 revenue review (final): revenue up 12% QoQ, churn 2.1% against a 2.0% target. Q4 plan due Oct 30."),
        ev("q3_revenue_draft", 13, "capture", "com.microsoft.Excel", "Q3 review draft.xlsx", "",
           "Q3 revenue review (draft): revenue up 10% QoQ, churn 2.3%. Numbers not final."),
        ev("q2_revenue", 60, "capture", "com.microsoft.Excel", "Q2 review.xlsx", "",
           "Q2 revenue review: revenue up 4% QoQ, churn 2.6%."),
        ev("pr_merge", 2, "capture", "com.github.GitHubClient", "Pull request #1187", "",
           "PR #1187 merged: fix the CI flake in the search tests. Reviewed by Ren."),
        ev("ux_research", 9, "capture", "notion.id", "UX research synthesis", NOTION_CHROME,
           "UX research synthesis: five interviews; the top issue is the onboarding checklist."),
        ev("v2_spec_en", 14, "capture", "com.google.Chrome", "API v2 spec", BROWSER_CHROME,
           "API v2 spec draft: pagination via cursor, deprecate the offset param."),
        ev("ocr_q3_dash", 4, "screen_ocr", "com.google.Chrome", "Revenue dashboard", OCR_CHROME,
           "Q3 Revenue Dashboard ▲12% Churn 2.1% ARR 4.2M Filters Last 90 days"),

        // English.
        ev("contract_draft", 6, "gmail", "com.google.mail", "Draft contract: Kestrel Foods", "",
           "Draft contract for Kestrel Foods: payment terms net 45, auto-renewal clause removed. Please review before Friday."),
        ev("launch_delay", 7, "capture", "com.tinyspeck.slackmacgap", "# orchid", SLACK_CHROME,
           "Agreed: push the Orchid launch to Nov 12 so QA can finish the regression suite."),
        ev("launch_party", 3, "gmail", "com.google.mail", "Orchid launch party", "",
           "Orchid launch party catering confirmed for Nov 14."),
        ev("priya_sandbox", 2, "gmail", "com.google.mail", "Sandbox credentials expire Friday", "",
           "From Priya Nair (Nimbus Labs): the integration sandbox credentials expire Friday; please rotate them."),
        ev("incident_payments", 5, "capture", "com.tinyspeck.slackmacgap", "# incident", SLACK_CHROME,
           "Incident: checkout API returned 502 for 40 minutes; root cause was an expired TLS cert."),
        ev("hiring_plan", 10, "capture", "com.apple.Notes", "Notes", "",
           "Hiring plan for Q4: two backend roles and one designer."),
        ev("vendor_renewal", 16, "gmail", "com.google.mail", "Vendor renewal", "",
           "The analytics vendor renewal was approved at the same price for another year."),
        ev("dana_start", 18, "gmail", "com.google.mail", "Offer accepted", "",
           "Offer accepted by Dana Kim; she starts in the designer role on Nov 3."),
        ev("standup_notes", 1, "capture", "com.apple.Notes", "Standup", "",
           "Standup: Ren is blocked on the staging database credentials; Priya will rotate them today."),
        ev("ops_ticket", 4, "capture", "com.atlassian.jira", "OPS-1182", "",
           "OPS-1182: rotate the staging database credentials by Friday."),
        ev("roadmap_q4", 20, "capture", "notion.id", "Roadmap", NOTION_CHROME,
           "Q4 roadmap: search quality, Japanese tokenization, and faster startup."),
        ev("expense_policy", 22, "capture", "com.google.Chrome", "Expense policy", BROWSER_CHROME,
           "Expense policy: meals over 5,000 yen need a receipt and a business purpose."),

        // Near-duplicates (stale vs current).
        ev("price_old", 30, "capture", "notion.id", "料金", NOTION_CHROME,
           "料金メモ（旧）: スタンダードプランは月額3,900円。"),
        ev("deadline_old", 15, "capture", "com.apple.Notes", "メモ", "",
           "青葉プロジェクトのリリースは11月15日の予定。"),
        ev("deadline_new", 3, "capture", "com.apple.Notes", "メモ", "",
           "青葉プロジェクトのリリースは11月末に延期。理由はSSO対応の追加。"),
    ]
}

/// Captured windows whose only matching text is UI chrome or incidental mentions.
fn chrome_noise() -> Vec<(i64, &'static str, &'static str, &'static str, String)> {
    let mut out = Vec::new();
    for i in 0..24 {
        out.push((
            i % 12,
            "com.apple.mail",
            "Inbox",
            "capture",
            format!("{MAIL_CHROME}Newsletter #{i}: weekly product digest, nothing to action."),
        ));
        out.push((
            i % 10,
            "com.tinyspeck.slackmacgap",
            "# random",
            "capture",
            format!("{SLACK_CHROME}lunch thread {i}: ramen or curry?"),
        ));
        out.push((
            i % 9,
            "com.apple.mail",
            "受信",
            "capture",
            format!("{JA_MAIL_CHROME}お知らせ {i}: 社内報が更新されました。"),
        ));
        out.push((
            i % 8,
            "com.google.calendar",
            "カレンダー",
            "capture",
            format!("{JA_CAL_CHROME}10:00 朝会\n13:00 会議室の清掃 {i}\n15:00 来客"),
        ));
        out.push((
            i % 7,
            "com.google.Chrome",
            "ニュース",
            "screen_ocr",
            format!("{OCR_CHROME}ニュース {i}: 各社の決算発表が相次ぐ。円相場は小動き。"),
        ));
    }
    out
}

/// More than 512 newer rows sharing 「予算の」 — the flood that pushed search off BM25.
fn flood() -> Vec<String> {
    (0..600)
        .map(|i| format!("予算の確認 チケット #{i}: 経費カテゴリの再分類と予算の配分見直し。"))
        .collect()
}

struct Case {
    category: &'static str,
    query: &'static str,
    /// Empty = nothing relevant exists; the case measures abstention.
    relevant: &'static [&'static str],
}

fn cases() -> Vec<Case> {
    let c = |category, query, relevant| Case { category, query, relevant };
    vec![
        // Two-character words.
        c("ja_2char", "会議", &["m_teirei_this", "m_teirei_last", "m_keiei"]),
        c("ja_2char", "請求", &["seikyu_oct"]),
        c("ja_2char", "予算", &["m_teirei_this", "campaign"]),
        c("ja_2char", "契約", &["keiyaku_nda"]),
        c("ja_2char", "採用", &["saiyo_mensetsu", "m_teirei_this"]),
        c("ja_2char", "出張", &["shutcho_osaka"]),
        c("ja_2char", "障害", &["shogai_kessai"]),
        c("ja_2char", "決済", &["shogai_kessai"]),
        c("ja_2char", "勤怠", &["kintai"]),
        c("ja_2char", "大阪", &["shutcho_osaka"]),
        c("ja_2char", "面接", &["saiyo_mensetsu"]),
        c("ja_2char", "移転", &["office_move", "m_teirei_last"]),
        c("ja_2char", "返金", &["torikeshi"]),
        c("ja_2char", "残業", &["kintai"]),
        c("ja_2char", "会議 予算", &["m_teirei_this"]),
        c("ja_2char", "請求 期限", &["seikyu_oct"]),
        c("ja_2char", "障害 原因", &["shogai_kessai"]),
        c("ja_2char", "採用 面接", &["saiyo_mensetsu"]),
        c("ja_2char", "契約 変更", &["keiyaku_nda"]),
        c("ja_2char", "出張 新幹線", &["shutcho_osaka"]),
        // Surnames and people.
        c("ja_person", "田中", &["tanaka_quote_mail", "person_tanaka", "uchiawase_warehouse"]),
        c("ja_person", "佐藤", &["person_sato", "sato_wireframe"]),
        c("ja_person", "鈴木", &["suzuki_expense"]),
        c("ja_person", "高橋", &["keiyaku_nda", "loop_nda"]),
        c("ja_person", "伊藤", &["saiyo_mensetsu"]),
        c("ja_person", "田中 健一", &["person_tanaka", "tanaka_quote_mail"]),
        c("ja_person", "田中さん", &["tanaka_quote_mail", "person_tanaka", "uchiawase_warehouse"]),
        c("ja_person", "佐藤さんとの1on1", &["m_1on1_sato"]),
        c("ja_person", "田中さんの見積もり", &["tanaka_quote_mail", "commit_tanaka_quote"]),
        c("ja_person", "Priya", &["priya_sandbox", "person_priya"]),
        // Particles and honorifics.
        c("ja_particle", "田中さんの件", &["tanaka_quote_mail", "person_tanaka", "uchiawase_warehouse"]),
        c("ja_particle", "鈴木さんの件", &["suzuki_expense"]),
        c("ja_particle", "予算の承認", &["yosan_shonin"]),
        c("ja_particle", "契約書のレビュー", &["keiyaku_nda", "loop_nda"]),
        c("ja_particle", "経費精算の締め日", &["suzuki_expense"]),
        c("ja_particle", "支払いの条件", &["shiharai_terms"]),
        c("ja_particle", "新倉庫について", &["uchiawase_warehouse", "person_tanaka"]),
        c("ja_particle", "オフィス移転の候補地は？", &["office_move", "m_teirei_last"]),
        c("ja_particle", "広告費は何％削減した？", &["m_teirei_this"]),
        c("ja_particle", "価格改定はいつから？", &["m_keiei", "kakaku_notice"]),
        c("ja_particle", "セミナーの開催日", &["moshikomi"]),
        c("ja_particle", "証明書の期限切れ", &["shogai_kessai"]),
        // Okurigana / spelling variants.
        c("ja_okurigana", "打合せ", &["uchiawase_warehouse", "uchiawase_aoba"]),
        c("ja_okurigana", "打ち合わせ", &["uchiawase_warehouse", "uchiawase_aoba"]),
        c("ja_okurigana", "打ち合せ", &["uchiawase_warehouse", "uchiawase_aoba"]),
        c("ja_okurigana", "申込", &["moshikomi"]),
        c("ja_okurigana", "申し込み", &["moshikomi"]),
        c("ja_okurigana", "取消", &["torikeshi"]),
        c("ja_okurigana", "取り消し", &["torikeshi"]),
        c("ja_okurigana", "見積り", &["tanaka_quote_mail", "commit_tanaka_quote"]),
        c("ja_okurigana", "支払条件", &["shiharai_terms"]),
        // Katakana.
        c("ja_katakana", "リリースノート", &["release_note"]),
        c("ja_katakana", "バグ", &["release_note"]),
        c("ja_katakana", "ダークモード", &["release_note"]),
        c("ja_katakana", "デザインレビュー", &["design_review_ja", "m_1on1_sato", "kaigi_room"]),
        c("ja_katakana", "キャンペーン", &["campaign"]),
        c("ja_katakana", "ワイヤーフレーム", &["sato_wireframe"]),
        c("ja_katakana", "サーバー", &["memo_cho"]),
        c("ja_katakana", "アラート", &["memo_cho"]),
        c("ja_katakana", "ｷｬﾝﾍﾟｰﾝ", &["campaign"]),
        c("ja_katakana", "クーポン", &["campaign"]),
        // Compounds of three or more characters.
        c("ja_compound", "定例会議", &["m_teirei_this", "m_teirei_last", "teirei_agenda"]),
        c("ja_compound", "経営会議", &["m_keiei", "yosan_shonin"]),
        c("ja_compound", "請求書", &["seikyu_oct"]),
        c("ja_compound", "新幹線", &["shutcho_osaka"]),
        c("ja_compound", "秘密保持期間", &["keiyaku_nda"]),
        c("ja_compound", "青葉プロジェクト", &["project_aoba", "deadline_new", "uchiawase_aoba"]),
        c("ja_compound", "青葉プロジェクトのリリース日", &["deadline_new", "project_aoba"]),
        c("ja_compound", "新規会員アプリ", &["sato_wireframe", "person_sato"]),
        c("ja_compound", "経費精算", &["suzuki_expense"]),
        c("ja_compound", "広告費", &["m_teirei_this"]),
        c("ja_compound", "年末キャンペーン", &["campaign"]),
        c("ja_compound", "マーケティング予算", &["yosan_shonin"]),
        c("ja_compound", "スタンダードプランの料金", &["m_keiei", "kakaku_notice"]),
        // Mixed Japanese and English.
        c("mixed", "Q3の売上", &["q3_revenue", "ocr_q3_dash"]),
        c("mixed", "API v2 の仕様", &["api_v2_ja", "v2_spec_en"]),
        c("mixed", "NDA 第3条", &["keiyaku_nda", "loop_nda"]),
        c("mixed", "Slack連携", &["slack_連携"]),
        c("mixed", "OAuth スコープ", &["slack_連携"]),
        c("mixed", "CI が不安定", &["ci_flaky_ja", "pr_merge"]),
        c("mixed", "Kestrel Foods の請求書", &["seikyu_oct"]),
        c("mixed", "TLS証明書", &["shogai_kessai", "incident_payments"]),
        c("mixed", "INV-2026-1043", &["seikyu_oct"]),
        c("mixed", "Orchid のローンチ延期", &["launch_delay"]),
        c("mixed", "SSO対応", &["deadline_new"]),
        c("mixed", "Nimbus Labs の Priya", &["priya_sandbox", "person_priya"]),
        // Short ASCII tokens.
        c("short_ascii", "Q3", &["q3_revenue", "q3_revenue_draft", "ocr_q3_dash"]),
        c("short_ascii", "q3 revenue churn", &["q3_revenue"]),
        c("short_ascii", "PR 1187", &["pr_merge", "ci_flaky_ja"]),
        c("short_ascii", "CI flake", &["pr_merge"]),
        c("short_ascii", "UX interviews", &["ux_research"]),
        c("short_ascii", "v2 pagination", &["v2_spec_en", "api_v2_ja"]),
        c("short_ascii", "Q4 plan", &["q3_revenue", "hiring_plan"]),
        c("short_ascii", "Q2 churn", &["q2_revenue"]),
        c("short_ascii", "OPS-1182", &["ops_ticket"]),
        c("short_ascii", "UX", &["ux_research"]),
        // English.
        c("english", "draft contract", &["contract_draft"]),
        c("english", "draft", &["contract_draft", "v2_spec_en", "q3_revenue_draft"]),
        c("english", "Orchid launch delay", &["launch_delay", "project_orchid"]),
        c("english", "launch party catering", &["launch_party"]),
        c("english", "sandbox credentials", &["priya_sandbox"]),
        c("english", "checkout API 502", &["incident_payments"]),
        c("english", "hiring plan", &["hiring_plan"]),
        c("english", "vendor renewal", &["vendor_renewal"]),
        c("english", "when does Dana start", &["dana_start"]),
        c("english", "staging database credentials", &["ops_ticket", "standup_notes"]),
        c("english", "regression suite parallel", &["m_orchid_retro"]),
        c("english", "QA bottleneck retro", &["m_orchid_retro"]),
        c("english", "expense policy receipt", &["expense_policy"]),
        c("english", "roadmap search quality", &["roadmap_q4"]),
        // Flood: >512 newer rows share 「予算の」.
        c("flood", "予算の承認", &["yosan_shonin"]),
        c("flood", "マーケティング予算の承認", &["yosan_shonin"]),
        c("flood", "予算 承認 経営会議", &["yosan_shonin"]),
        c("flood", "1,200万円", &["yosan_shonin"]),
        // Relative dates.
        c("relative_date", "昨日の会議", &["m_keiei"]),
        c("relative_date", "先週の定例会議", &["m_teirei_last"]),
        c("relative_date", "今週の定例", &["m_teirei_this"]),
        c("relative_date", "昨日の価格改定", &["m_keiei", "kakaku_notice"]),
        c("relative_date", "yesterday meeting", &["m_keiei"]),
        c("relative_date", "standup notes yesterday", &["standup_notes"]),
        c("relative_date", "2日前のメモ", &["shutcho_osaka"]),
        c("relative_date", "先週の打ち合わせ", &["uchiawase_aoba", "uchiawase_warehouse"]),
        // Near-duplicates: the current value should lead.
        c("near_dup", "スタンダードプランの月額", &["kakaku_notice", "m_keiei"]),
        c("near_dup", "青葉プロジェクトのリリース", &["deadline_new"]),
        c("near_dup", "Q3 revenue review", &["q3_revenue"]),
        c("near_dup", "請求書 INV", &["seikyu_oct"]),
        // Nothing relevant exists: search should abstain.
        c("negative", "スキューバダイビングの資格", &[]),
        c("negative", "給与明細の再発行", &[]),
        c("negative", "Mars rover telemetry", &[]),
        c("negative", "vacation photos from Hokkaido", &[]),
        c("negative", "株主総会の議事録", &[]),
        c("negative", "引っ越し業者の見積もり比較", &[]),
    ]
}

#[derive(Default)]
struct Labels {
    events: HashMap<i64, &'static str>,
    sessions: HashMap<i64, &'static str>,
    derived: HashMap<SourceRef, &'static str>,
}

fn seed() -> (Db, Labels) {
    use shogun_memory::meeting_index::index_session;
    use shogun_memory::session::{close, open, NewSession};
    use shogun_memory::transcript_segments::{append, NewSegment, Speaker};

    let mut conn = shogun_memory::open_in_memory().unwrap();
    let mut labels = Labels::default();
    let mut seq = 0usize;
    let mut insert = |conn: &rusqlite::Connection,
                      ts: i64,
                      source: &str,
                      app: &str,
                      title: &str,
                      content: &str|
     -> i64 {
        seq += 1;
        let hash = format!("golden-{seq}");
        shogun_memory::event_log::insert(
            conn,
            &NewEvent {
                ts,
                source,
                kind: if source == "gmail" { "email" } else { "text" },
                app_bundle_id: Some(app),
                window_title: Some(title),
                content,
                content_hash: &hash,
                dwell_ms: 4_000,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap()
    };

    // Chrome noise and the flood go in first, then the corpus; flood rows are the newest ids
    // in the archive except for corpus rows, as they would be after a busy month.
    for (days_ago, app, title, source, content) in chrome_noise() {
        insert(&conn, at(days_ago, 9), source, app, title, &content);
    }
    let anchor_evidence = insert(
        &conn,
        at(29, 9),
        "capture",
        "com.apple.Notes",
        "Evidence",
        "state evidence row",
    );
    let corpus = corpus();
    for item in &corpus {
        match item {
            Item::Event { label, days_ago, source, app, title, chrome, content } => {
                let body = format!("{chrome}{content}");
                let id = insert(&conn, at(*days_ago, 10), source, app, title, &body);
                labels.events.insert(id, label);
            }
            Item::Meeting { label, days_ago, title, lines, summary, decisions, next_actions } => {
                let started_at = at(*days_ago, 11);
                let session_id = open(
                    &conn,
                    &NewSession {
                        kind: "meeting",
                        started_at,
                        title: Some(title),
                        app_bundle_id: Some("us.zoom.xos"),
                        calendar_occurrence_id: None,
                        confidence: 0.9,
                        provenance: "{}",
                    },
                )
                .unwrap();
                for (i, line) in lines.iter().enumerate() {
                    append(
                        &conn,
                        &NewSegment {
                            session_id,
                            ts: started_at + 60_000 * (i as i64 + 1),
                            speaker: Speaker::Other,
                            text: line,
                            confidence: 0.9,
                        },
                        started_at + 60_000 * (i as i64 + 1),
                    )
                    .unwrap();
                }
                close(&conn, session_id, started_at + HOUR).unwrap();
                index_session(&mut conn, session_id).unwrap();
                shogun_memory::meeting_recaps::save(
                    &conn,
                    session_id,
                    summary,
                    &serde_json::to_string(decisions).unwrap(),
                    &serde_json::to_string(next_actions).unwrap(),
                    None,
                    "golden-eval",
                    started_at + HOUR + 60_000,
                )
                .unwrap();
                labels.sessions.insert(session_id, label);
            }
            Item::Person { label, name, summary } => {
                let id = shogun_memory::state::insert_person(
                    &mut conn,
                    &NewPerson {
                        display_name: name,
                        relationship_summary: Some(summary),
                        confidence: 0.85,
                        now: at(6, 12),
                        ..NewPerson::default()
                    },
                    &[Provenance::new(anchor_evidence)],
                )
                .unwrap();
                labels.derived.insert(SourceRef::Person(id), label);
            }
            Item::Project { label, name, summary } => {
                let id = shogun_memory::state::insert_project(
                    &mut conn,
                    &NewProject {
                        name,
                        status: ProjectStatus::Active,
                        summary: Some(summary),
                        participants_json: None,
                        sources_json: None,
                        confidence: 0.85,
                        now: at(6, 12),
                    },
                    &[Provenance::new(anchor_evidence)],
                )
                .unwrap();
                labels.derived.insert(SourceRef::Project(id), label);
            }
            Item::Commitment { label, text } => {
                let id = shogun_memory::state::insert_commitment(
                    &mut conn,
                    &NewCommitment {
                        direction: CommitmentDirection::Mine,
                        counterparty_id: None,
                        description: text,
                        due_at: Some(at(-6, 18)),
                        status: CommitmentStatus::Open,
                        project_id: None,
                        confidence: 0.85,
                        now: at(3, 12),
                    },
                    &[Provenance::new(anchor_evidence)],
                )
                .unwrap();
                labels.derived.insert(SourceRef::Commitment(id), label);
            }
            Item::OpenLoop { label, text } => {
                let id = shogun_memory::state::insert_open_loop(
                    &mut conn,
                    &NewOpenLoop {
                        kind: OpenLoopKind::WaitingOnThem,
                        description: text,
                        counterparty_id: None,
                        project_id: None,
                        opened_at: at(8, 12),
                        confidence: 0.85,
                        now: at(8, 12),
                    },
                    &[Provenance::new(anchor_evidence)],
                )
                .unwrap();
                labels.derived.insert(SourceRef::OpenLoop(id), label);
            }
        }
    }
    // The flood is newer than the 予算の承認 note (25 days ago): spread over the last 19 days.
    for (i, content) in flood().iter().enumerate() {
        let days_ago = (i % 19) as i64;
        insert(&conn, at(days_ago, 8), "capture", "com.atlassian.jira", "経費チケット", content);
    }

    let db = Db::new(conn, Arc::new(|| NOW));
    let _ = db.rebuild_hot_now();
    (db, labels)
}

fn hit_labels(hit: &FederatedHit, labels: &Labels) -> Vec<&'static str> {
    let mut out = Vec::new();
    for source in &hit.sources {
        let label = match &source.id {
            FederatedSourceId::Event(id) => labels.events.get(id),
            FederatedSourceId::Meeting(id) => labels.sessions.get(id),
            FederatedSourceId::Derived(reference) => labels.derived.get(reference),
            _ => None,
        };
        out.extend(label.copied());
    }
    for evidence in &hit.evidence_ids {
        if let CanonicalEvidenceId::Session(id) = evidence {
            out.extend(labels.sessions.get(id).copied());
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

#[derive(Default, Clone, Copy)]
struct Tally {
    cases: usize,
    positives: usize,
    negatives: usize,
    recall_5: f64,
    recall_10: f64,
    mrr: f64,
    zero: usize,
    answerable: usize,
    false_answerable: usize,
}

impl Tally {
    fn add(&mut self, relevant: &[&str], ranked: &[Vec<&'static str>], answerable: bool) {
        self.cases += 1;
        if relevant.is_empty() {
            self.negatives += 1;
            self.false_answerable += usize::from(answerable);
            return;
        }
        self.positives += 1;
        if ranked.is_empty() {
            self.zero += 1;
        }
        self.answerable += usize::from(answerable);
        let found_in = |k: usize| {
            relevant
                .iter()
                .filter(|label| ranked.iter().take(k).any(|hit| hit.contains(label)))
                .count() as f64
                / relevant.len() as f64
        };
        self.recall_5 += found_in(5);
        self.recall_10 += found_in(10);
        self.mrr += ranked
            .iter()
            .position(|hit| relevant.iter().any(|label| hit.contains(label)))
            .map_or(0.0, |index| 1.0 / (index as f64 + 1.0));
    }

    fn mean(&self, value: f64) -> f64 {
        if self.positives == 0 {
            0.0
        } else {
            value / self.positives as f64
        }
    }

    fn row(&self, label: &str) -> String {
        let rate = |n: usize, d: usize| if d == 0 { 0.0 } else { n as f64 / d as f64 };
        format!(
            "| {label} | {} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} |\n",
            self.cases,
            self.mean(self.recall_5),
            self.mean(self.recall_10),
            self.mean(self.mrr),
            rate(self.zero, self.positives),
            rate(self.answerable, self.positives),
            rate(self.false_answerable, self.negatives),
        )
    }
}

struct Report {
    overall: Tally,
    table: String,
    p50_ms: f64,
    p95_ms: f64,
}

fn run() -> Report {
    let (db, labels) = seed();
    let mut by_category: Vec<(&'static str, Tally)> = Vec::new();
    let mut overall = Tally::default();
    let mut latencies = Vec::new();
    let verbose = std::env::var_os("SHOGUN_SEARCH_EVAL_VERBOSE").is_some();
    for case in cases() {
        let started = std::time::Instant::now();
        let response = db
            .search_request(&SearchRequestV1 {
                query: case.query.to_string(),
                limit: 10,
                ..SearchRequestV1::default()
            })
            .unwrap();
        latencies.push(started.elapsed().as_secs_f64() * 1000.0);
        let ranked: Vec<Vec<&'static str>> = response
            .federated_hits
            .iter()
            .map(|hit| hit_labels(hit, &labels))
            .collect();
        let answerable = response.answerability.answerable;
        if verbose {
            // log-hygiene-ok: invented eval fixture text, printed only when the developer opts in
            println!(
                "[{}] {:?} answerable={answerable} ({:?}) relevant={:?}\n    got={:?}",
                case.category, case.query, response.answerability.reason, case.relevant, ranked
            );
        }
        overall.add(case.relevant, &ranked, answerable);
        match by_category.iter_mut().find(|(name, _)| *name == case.category) {
            Some((_, tally)) => tally.add(case.relevant, &ranked, answerable),
            None => {
                let mut tally = Tally::default();
                tally.add(case.relevant, &ranked, answerable);
                by_category.push((case.category, tally));
            }
        }
    }
    latencies.sort_by(f64::total_cmp);
    let pick = |q: f64| latencies[((latencies.len() as f64 - 1.0) * q).round() as usize];
    let mut table = String::from(
        "| category | cases | R@5 | R@10 | MRR | zero-result | answerable | false-answerable |\n\
         |---|---:|---:|---:|---:|---:|---:|---:|\n",
    );
    for (name, tally) in &by_category {
        table.push_str(&tally.row(name));
    }
    table.push_str(&overall.row("**all**"));
    Report { overall, table, p50_ms: pick(0.5), p95_ms: pick(0.95) }
}

#[test]
fn golden_search_eval() {
    let report = run();
    println!("\n{}", report.table);
    println!(
        "search_request latency over the eval: p50 {:.1} ms, p95 {:.1} ms (debug build)",
        report.p50_ms, report.p95_ms
    );
    let o = report.overall;
    assert!(o.cases >= 120, "the golden set must stay broad");
    // Floors: the measured result minus a small margin (docs/search-eval.md).
    let recall_5 = o.mean(o.recall_5);
    let mrr = o.mean(o.mrr);
    let zero_rate = o.zero as f64 / o.positives as f64;
    let answerable = o.answerable as f64 / o.positives as f64;
    let false_answerable = o.false_answerable as f64 / o.negatives as f64;
    println!(
        "overall: R@5 {recall_5:.3} MRR {mrr:.3} zero {zero_rate:.3} answerable {answerable:.3} false-answerable {false_answerable:.3}"
    );
    assert!(recall_5 >= FLOOR_RECALL_5, "recall@5 regressed: {recall_5:.3}");
    assert!(mrr >= FLOOR_MRR, "MRR regressed: {mrr:.3}");
    assert!(zero_rate <= CEIL_ZERO_RATE, "zero-result rate regressed: {zero_rate:.3}");
    assert!(answerable >= FLOOR_ANSWERABLE, "answerable rate regressed: {answerable:.3}");
    assert!(
        false_answerable <= CEIL_FALSE_ANSWERABLE,
        "search claimed an answer for absent memory: {false_answerable:.3}"
    );
}

// Measured 2026-10-14 (docs/search-eval.md): R@5 0.985, MRR 0.948, zero-result 0.000,
// answerable 0.873, false-answerable 0.000.
const FLOOR_RECALL_5: f64 = 0.95;
const FLOOR_MRR: f64 = 0.91;
const CEIL_ZERO_RATE: f64 = 0.03;
const FLOOR_ANSWERABLE: f64 = 0.83;
const CEIL_FALSE_ANSWERABLE: f64 = 0.0;

/// Pages (bytes) used by the tables whose names start with `prefix`, from SQLite's `dbstat`.
fn bytes_of(conn: &rusqlite::Connection, prefix: &str) -> i64 {
    conn.query_row(
        "SELECT COALESCE(sum(pgsize), 0) FROM dbstat WHERE name = ?1 OR name LIKE ?1 || '\\_%' ESCAPE '\\'",
        [prefix],
        |row| row.get(0),
    )
    .unwrap()
}

/// Deterministic text from a vocabulary: `words` per row, LCG-chosen.
fn synthetic_rows(vocabulary: &[&str], rows: usize, words: usize, joiner: &str) -> Vec<String> {
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    (0..rows)
        .map(|_| {
            (0..words)
                .map(|_| {
                    state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                    vocabulary[(state >> 33) as usize % vocabulary.len()]
                })
                .collect::<Vec<_>>()
                .join(joiner)
        })
        .collect()
}

fn index_sizes(rows: &[String]) -> (i64, i64, i64) {
    let conn = shogun_memory::open_in_memory().unwrap();
    for (i, content) in rows.iter().enumerate() {
        shogun_memory::event_log::insert(
            &conn,
            &NewEvent {
                ts: NOW - i as i64,
                source: "capture",
                kind: "text",
                app_bundle_id: Some("com.apple.Notes"),
                window_title: Some("Notes"),
                content,
                content_hash: &format!("size-{i}"),
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap();
    }
    (
        bytes_of(&conn, "event_log"),
        bytes_of(&conn, "event_fts"),
        bytes_of(&conn, "event_grams_fts"),
    )
}

/// Size of the V81 gram index next to the trigram index it complements, for Japanese-heavy and
/// English-only text (docs/search-eval.md quotes these numbers).
#[test]
fn gram_index_size_report() {
    const JA: &[&str] = &[
        "今日の", "定例会議では", "予算の見直しについて", "田中さんと", "打ち合わせを行い", "請求書を",
        "確認しました。", "新規会員アプリの", "リリースは", "来週に延期", "デザインレビューの", "フィードバック",
        "経費精算の", "締め日を", "変更する方向で", "調整中です。", "障害報告：", "決済APIが", "停止しました。",
        "原因は", "証明書の", "期限切れ", "採用面接の", "日程を", "共有します", "オフィス移転", "候補地は",
        "渋谷と品川", "キャンペーン", "企画書を", "作成", "メール", "送信済み", "下書き", "受信",
    ];
    const EN: &[&str] = &[
        "the", "quarterly", "revenue", "review", "meeting", "notes", "pricing", "vendor", "renewal",
        "approved", "launch", "delayed", "regression", "suite", "parallel", "credentials", "rotate",
        "staging", "database", "Q3", "PR", "merged", "CI", "flake", "design", "feedback", "hiring",
        "plan", "backend", "roles", "inbox", "drafts", "sent", "archive", "is", "in", "of", "to",
    ];
    for (label, rows) in [
        ("japanese", synthetic_rows(JA, 3_000, 24, "")),
        ("english", synthetic_rows(EN, 3_000, 40, " ")),
    ] {
        let (log, trigram, grams) = index_sizes(&rows);
        println!(
            "{label}: event_log {} KiB, event_fts {} KiB, event_grams_fts {} KiB (+{:.0}% of event_fts, +{:.0}% of log+fts)",
            log / 1024,
            trigram / 1024,
            grams / 1024,
            100.0 * grams as f64 / trigram.max(1) as f64,
            100.0 * grams as f64 / (log + trigram).max(1) as f64,
        );
        assert!(grams > 0 && grams < trigram, "{label}: the gram index stays smaller than the trigram index");
    }
}
