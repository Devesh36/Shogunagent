//! The fixed synthetic material every golden day is built from. **Nothing else can reach a golden
//! file**: the generator reads only these constants and a seed, and `tests/gate.rs` fails if a
//! checked-in day differs from what the generator produces.
//!
//! Rules for editing this file (docs/extract-eval.md §4):
//! - Names, organisations and projects are invented. Do not paste a real person, customer,
//!   colleague or company — not even "as an example". Email domains are `*.example` (RFC 2606).
//! - A bug report becomes a template here, rewritten with pool names: keep the *shape* that broke
//!   (the cue, the structure, the language), never the reporter's words.
//! - `gold` is what a correct extractor should produce, not what today's rules happen to produce.
//!   A template whose text the rules get wrong is the point, not a mistake.
//!
//! Slots (filled by the generator): `{person}` full name, `{first}` how the text addresses them
//! (English first name / Japanese surname), `{email}`, `{handle}` their chat handle (the email's
//! local part, written `@{handle}`), `{person2}` a second person, `{me}` the
//! persona's short name, `{project}`, `{artifact}`, `{channel}`, `{pr}`, `{weekday}` a weekday
//! later in the same week and `{due}` its date, `{due_tomorrow}` the next day's date.

/// Bump when a template, pool or the generator changes what a seed produces. Written into every
/// golden file (`generator.template_set`).
pub const TEMPLATE_SET: &str = "v1";

/// One person in a persona's world. `short` is how text addresses them.
#[derive(Debug, Clone, Copy)]
pub struct PoolPerson {
    pub name: &'static str,
    pub short: &'static str,
    pub email: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    Ja,
}

impl Lang {
    pub fn as_str(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Ja => "ja",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PersonaDef {
    pub id: &'static str,
    pub name: &'static str,
    pub short: &'static str,
    pub role: &'static str,
    pub org: &'static str,
    pub lang: Lang,
    pub utc_offset_secs: i32,
    pub people: &'static [PoolPerson],
    pub projects: &'static [&'static str],
    pub artifacts: &'static [&'static str],
    pub channels: &'static [&'static str],
}

pub const PERSONAS: &[PersonaDef] = &[
    PersonaDef {
        id: "p-eng-lead",
        name: "Mara Quint",
        short: "Mara",
        role: "Engineering lead",
        org: "Larkspur Freight",
        lang: Lang::En,
        utc_offset_secs: 0,
        people: &[
            PoolPerson { name: "Oren Vale", short: "Oren", email: "oren.vale@larkspur.example" },
            PoolPerson { name: "Tamsin Ekberg", short: "Tamsin", email: "tamsin.ekberg@larkspur.example" },
            PoolPerson { name: "Idris Solano", short: "Idris", email: "idris.solano@larkspur.example" },
            PoolPerson { name: "Juno Farrant", short: "Juno", email: "juno.farrant@larkspur.example" },
            PoolPerson { name: "Kasimir Lode", short: "Kasimir", email: "kasimir.lode@larkspur.example" },
        ],
        projects: &["Heron Rollout", "Atlas Ledger", "Kestrel Onboarding"],
        artifacts: &["rollout plan", "cost estimate", "migration checklist", "design doc", "test report"],
        channels: &["eng-platform", "heron-launch", "ops-sync"],
    },
    PersonaDef {
        id: "p-ops-manager",
        name: "Tobias Wren",
        short: "Tobias",
        role: "Customer operations manager",
        org: "Halcyon Tiles",
        lang: Lang::En,
        utc_offset_secs: -5 * 3600,
        people: &[
            PoolPerson { name: "Beatrix Onwu", short: "Beatrix", email: "beatrix.onwu@halcyon.example" },
            PoolPerson { name: "Caspian Holt", short: "Caspian", email: "caspian.holt@halcyon.example" },
            PoolPerson { name: "Nadia Ferro", short: "Nadia", email: "nadia.ferro@halcyon.example" },
            PoolPerson { name: "Lucan Pryce", short: "Lucan", email: "lucan.pryce@halcyon.example" },
            PoolPerson { name: "Ysolde Marr", short: "Ysolde", email: "ysolde.marr@halcyon.example" },
        ],
        projects: &["Quarry Dashboard", "Bluebird Renewal", "Mosaic Pricing"],
        artifacts: &["renewal quote", "support rota", "vendor shortlist", "onboarding guide", "usage report"],
        channels: &["cs-team", "renewals", "ops-sync"],
    },
    PersonaDef {
        id: "p-ja-pm",
        name: "真野湊",
        short: "真野",
        role: "プロダクトマネージャー",
        org: "株式会社コヨミ",
        lang: Lang::Ja,
        utc_offset_secs: 9 * 3600,
        people: &[
            PoolPerson { name: "佐伯灯", short: "佐伯", email: "saeki@koyomi.example" },
            PoolPerson { name: "久住蒼", short: "久住", email: "kusumi@koyomi.example" },
            PoolPerson { name: "早瀬律", short: "早瀬", email: "hayase@koyomi.example" },
            PoolPerson { name: "氷見奏", short: "氷見", email: "himi@koyomi.example" },
        ],
        projects: &["ミナト刷新", "コヨミ移行", "灯台ダッシュボード"],
        artifacts: &["見積書", "設計書", "移行計画", "議事録"],
        channels: &["pm-team", "minato", "release"],
    },
];

/// One event a template emits.
#[derive(Debug, Clone, Copy)]
pub struct EventTpl {
    pub source: &'static str,
    pub app: &'static str,
    pub window: &'static str,
    pub text: &'static str,
}

/// A gold row a template emits, evidenced by all of the template's events. People and projects
/// are not listed here: the generator derives them from which pool names the day's text mentions.
#[derive(Debug, Clone, Copy)]
pub enum GoldTpl {
    Commitment {
        direction: &'static str,
        description: &'static str,
        /// Slot for the deadline the sentence states, if any.
        due: Option<&'static str>,
    },
    OpenLoop {
        kind: &'static str,
        description: &'static str,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct Template {
    pub key: &'static str,
    pub lang: Lang,
    /// What the template exercises — shown in the failure taxonomy (docs/extract-eval.md §2).
    pub exercises: &'static str,
    pub events: &'static [EventTpl],
    pub gold: &'static [GoldTpl],
}

impl Template {
    /// A trap produces text that looks actionable but must yield no commitment / open loop.
    pub fn is_trap(&self) -> bool {
        self.key.starts_with("trap_")
            || self.key.starts_with("ja_trap_")
            || self.key.starts_with("mix_trap_")
            || self.key.starts_with("dash_trap_")
    }

    /// Mixed English/Japanese templates (`mix_`, taxonomy N7) and the dashboard-noise repros
    /// (`dash_`) appear only where a day requires them. Seeded picks never draw them, so adding one
    /// leaves every existing day byte-identical.
    pub fn required_only(&self) -> bool {
        self.key.starts_with("mix_") || self.key.starts_with("dash_")
    }
}

pub const TEMPLATES: &[Template] = &[
    Template {
        key: "commit_mine_email",
        lang: Lang::En,
        exercises: "explicit first-person promise with a weekday deadline",
        events: &[EventTpl {
            source: "gmail",
            app: "Mail",
            window: "Re: {project}",
            text: "To: {person} <{email}>\nHi {first},\nThanks for the notes on {project}. I'll send the {artifact} by {weekday}.\nBest, {me}",
        }],
        gold: &[GoldTpl::Commitment {
            direction: "mine",
            description: "I'll send the {artifact} by {weekday}",
            due: Some("{due}"),
        }],
    },
    Template {
        key: "commit_theirs_email",
        lang: Lang::En,
        exercises: "a promise made to the user, relative deadline",
        events: &[EventTpl {
            source: "gmail",
            app: "Mail",
            window: "{project}: {artifact}",
            text: "From: {person} <{email}>\nHi {me},\nWe will send you the signed {artifact} tomorrow.\n{first}",
        }],
        gold: &[GoldTpl::Commitment {
            direction: "theirs",
            description: "We will send you the signed {artifact} tomorrow",
            due: Some("{due_tomorrow}"),
        }],
    },
    Template {
        key: "commit_theirs_chat",
        lang: Lang::En,
        exercises: "speaker attribution: someone else's first-person promise in a chat log",
        events: &[EventTpl {
            source: "slack",
            app: "Slack",
            window: "#{channel}",
            text: "{person}: I'll get the {artifact} to you by {weekday}.\n{me}: Perfect, thanks!",
        }],
        gold: &[GoldTpl::Commitment {
            direction: "theirs",
            description: "I'll get the {artifact} to you by {weekday}",
            due: Some("{due}"),
        }],
    },
    Template {
        key: "commit_mine_meeting",
        lang: Lang::En,
        exercises: "a promise in a meeting transcript (Batch-excluded source: local rules only)",
        events: &[EventTpl {
            source: "meeting",
            app: "Meeting notes",
            window: "{project} sync",
            text: "{person}: Can we get the {artifact} before the review?\n{me}: Yes, I'll draft the {artifact} by {weekday}.",
        }],
        gold: &[GoldTpl::Commitment {
            direction: "mine",
            description: "I'll draft the {artifact} by {weekday}",
            due: Some("{due}"),
        }],
    },
    Template {
        key: "restated_commit",
        lang: Lang::En,
        exercises: "one promise evidenced twice across sources (dedup, High band)",
        events: &[
            EventTpl {
                source: "gmail",
                app: "Mail",
                window: "Re: {artifact}",
                text: "To: {person} <{email}>\nI'll share the {artifact} with the team tomorrow.\n{me}",
            },
            EventTpl {
                source: "slack",
                app: "Slack",
                window: "#{channel}",
                text: "{me}: Heads up, I'll share the {artifact} with the team tomorrow.",
            },
        ],
        gold: &[GoldTpl::Commitment {
            direction: "mine",
            description: "I'll share the {artifact} with the team tomorrow",
            due: Some("{due_tomorrow}"),
        }],
    },
    Template {
        key: "zwsp_commit",
        lang: Lang::En,
        exercises: "a hidden format character inside the promise cue",
        events: &[EventTpl {
            source: "capture",
            app: "Notes",
            window: "Notes",
            text: "I\u{200B}'ll send the {artifact} to {first} by {weekday}.",
        }],
        gold: &[GoldTpl::Commitment {
            direction: "mine",
            description: "I'll send the {artifact} to {first} by {weekday}",
            due: Some("{due}"),
        }],
    },
    Template {
        key: "loop_waiting_chat",
        lang: Lang::En,
        exercises: "waiting on someone else",
        events: &[EventTpl {
            source: "slack",
            app: "Slack",
            window: "#{channel}",
            text: "{me}: Still waiting on {first} for the {artifact} numbers.",
        }],
        gold: &[GoldTpl::OpenLoop {
            kind: "waiting_on_them",
            description: "Still waiting on {first} for the {artifact} numbers",
        }],
    },
    Template {
        key: "loop_review_pr",
        lang: Lang::En,
        exercises: "review request on a code-review page",
        events: &[EventTpl {
            source: "capture",
            app: "Browser",
            window: "Pull request #{pr} · {project}",
            text: "{person} requested your review on this pull request.\nPlease review the {artifact} changes before the release branch is cut.",
        }],
        gold: &[GoldTpl::OpenLoop {
            kind: "review_pending",
            description: "Please review the {artifact} changes before the release branch is cut",
        }],
    },
    Template {
        key: "loop_decision_meeting",
        lang: Lang::En,
        exercises: "an open decision in a meeting transcript",
        events: &[EventTpl {
            source: "meeting",
            app: "Meeting notes",
            window: "{project} planning",
            text: "{person}: We still need to decide between the two vendors for {project}.\n{me}: Agreed.",
        }],
        gold: &[GoldTpl::OpenLoop {
            kind: "decision_pending",
            description: "We still need to decide between the two vendors for {project}",
        }],
    },
    Template {
        key: "loop_reply_email",
        lang: Lang::En,
        exercises: "an incoming question awaiting the user's reply",
        events: &[EventTpl {
            source: "gmail",
            app: "Mail",
            window: "{project} budget",
            text: "From: {person} <{email}>\nHi {me},\nCould you confirm who owns the budget for {project}?\nThanks, {first}",
        }],
        gold: &[GoldTpl::OpenLoop {
            kind: "reply_needed",
            description: "Could you confirm who owns the budget for {project}",
        }],
    },
    Template {
        key: "loop_followup_note",
        lang: Lang::En,
        exercises: "a follow-up the user noted for themselves",
        events: &[EventTpl {
            source: "capture",
            app: "Notes",
            window: "Notes",
            text: "Follow up with {first} about the {artifact} after the demo.",
        }],
        gold: &[GoldTpl::OpenLoop {
            kind: "follow_up",
            description: "Follow up with {first} about the {artifact} after the demo",
        }],
    },
    Template {
        key: "calendar_entry",
        lang: Lang::En,
        exercises: "people and projects from a calendar entry; no action",
        events: &[EventTpl {
            source: "gcal",
            app: "Calendar",
            window: "Calendar",
            text: "{weekday} 10:00-10:30 · {project} review\nAttendees: {person}, {person2}",
        }],
        gold: &[],
    },
    Template {
        key: "trap_hedge",
        lang: Lang::En,
        exercises: "a hedge is not a promise (FR-ST-11)",
        events: &[EventTpl {
            source: "slack",
            app: "Slack",
            window: "#{channel}",
            text: "{me}: Maybe I could look at the {artifact} next week if there is time.",
        }],
        gold: &[],
    },
    Template {
        key: "trap_quoted_example",
        lang: Lang::En,
        exercises: "a quoted example promise on a reference page",
        events: &[EventTpl {
            source: "capture",
            app: "Browser",
            window: "Writing guide · {org} wiki",
            text: "Example reply: \"I'll send the report by Friday.\" Use this tone for customer emails.",
        }],
        gold: &[],
    },
    Template {
        key: "trap_job_ad",
        lang: Lang::En,
        exercises: "a rhetorical question on a web page",
        events: &[EventTpl {
            source: "capture",
            app: "Browser",
            window: "Careers · Open roles",
            text: "Senior Analyst\nYou will partner with the data team every day.\nWill you be our next hire?\nApply today.",
        }],
        gold: &[],
    },
    Template {
        key: "trap_chrome",
        lang: Lang::En,
        exercises: "navigation labels that spell a cue",
        events: &[EventTpl {
            source: "capture",
            app: "Mail",
            window: "Inbox",
            text: "Inbox 12\nCompose\nFollow up\nSnoozed\nSent\nDrafts",
        }],
        gold: &[],
    },
    Template {
        key: "trap_let_me_know",
        lang: Lang::En,
        exercises: "\"let me know\" is an invitation, not a promise",
        events: &[EventTpl {
            source: "gmail",
            app: "Mail",
            window: "{project}: {artifact}",
            text: "To: {person} <{email}>\nAttached is the {artifact} for {project}.\nLet me know if anything looks off.\n{me}",
        }],
        gold: &[],
    },
    Template {
        key: "ja_commit_mine",
        lang: Lang::Ja,
        exercises: "Japanese first-person promise with a weekday deadline",
        events: &[EventTpl {
            source: "slack",
            app: "Slack",
            window: "#{channel}",
            text: "{me}: {first}さん、{artifact}は{weekday}までに送ります。",
        }],
        gold: &[GoldTpl::Commitment {
            direction: "mine",
            description: "{artifact}は{weekday}までに送ります",
            due: Some("{due}"),
        }],
    },
    Template {
        key: "ja_commit_theirs",
        lang: Lang::Ja,
        exercises: "speaker attribution: a humble-form promise written by someone else",
        events: &[EventTpl {
            source: "gmail",
            app: "メール",
            window: "Re: {project}",
            text: "差出人: {person} <{email}>\n{me}さん\n{artifact}は明日お送りします。\n{first}",
        }],
        gold: &[GoldTpl::Commitment {
            direction: "theirs",
            description: "{artifact}は明日お送りします",
            due: Some("{due_tomorrow}"),
        }],
    },
    Template {
        key: "ja_waiting",
        lang: Lang::Ja,
        exercises: "Japanese waiting-on-them",
        events: &[EventTpl {
            source: "capture",
            app: "メモ",
            window: "メモ",
            text: "{first}さんからの{artifact}の返信待ち。",
        }],
        gold: &[GoldTpl::OpenLoop {
            kind: "waiting_on_them",
            description: "{first}さんからの{artifact}の返信待ち",
        }],
    },
    Template {
        key: "ja_review",
        lang: Lang::Ja,
        exercises: "a review request whose politeness suffix contains a promise cue",
        events: &[EventTpl {
            source: "slack",
            app: "Slack",
            window: "#{channel}",
            text: "{first}: {project}の{artifact}のレビューをお願いします。",
        }],
        gold: &[GoldTpl::OpenLoop {
            kind: "review_pending",
            description: "{project}の{artifact}のレビューをお願いします",
        }],
    },
    Template {
        key: "ja_decision",
        lang: Lang::Ja,
        exercises: "Japanese open decision in a meeting transcript",
        events: &[EventTpl {
            source: "meeting",
            app: "会議メモ",
            window: "{project}定例",
            text: "{person}: {project}のリリース日はまだ未定です。\n{me}: 了解です。",
        }],
        gold: &[GoldTpl::OpenLoop {
            kind: "decision_pending",
            description: "{project}のリリース日はまだ未定です",
        }],
    },
    Template {
        key: "ja_reply",
        lang: Lang::Ja,
        exercises: "Japanese incoming question",
        events: &[EventTpl {
            source: "gmail",
            app: "メール",
            window: "{project}の予算",
            text: "差出人: {person} <{email}>\n{me}さん\n{project}の予算について、来週ご相談できますか？\n{first}",
        }],
        gold: &[GoldTpl::OpenLoop {
            kind: "reply_needed",
            description: "{project}の予算について、来週ご相談できますか",
        }],
    },
    Template {
        key: "ja_followup",
        lang: Lang::Ja,
        exercises: "Japanese self-noted follow-up",
        events: &[EventTpl {
            source: "capture",
            app: "メモ",
            window: "メモ",
            text: "デモの後で{first}さんに{artifact}について再度連絡。",
        }],
        gold: &[GoldTpl::OpenLoop {
            kind: "follow_up",
            description: "デモの後で{first}さんに{artifact}について再度連絡",
        }],
    },
    Template {
        key: "ja_calendar",
        lang: Lang::Ja,
        exercises: "Japanese calendar entry; people and projects only",
        events: &[EventTpl {
            source: "gcal",
            app: "カレンダー",
            window: "カレンダー",
            text: "{weekday} 14:00-15:00 {project}定例\n参加者: {person}、{person2}",
        }],
        gold: &[],
    },
    Template {
        key: "ja_trap_quoted",
        lang: Lang::Ja,
        exercises: "a quoted example promise in a Japanese style guide",
        events: &[EventTpl {
            source: "capture",
            app: "ブラウザ",
            window: "社内ガイド",
            text: "例文：「資料は明日までに送ります。」この書き方を参考にしてください。",
        }],
        gold: &[],
    },
    // An English-speaking persona writing with Japanese nouns (N7). Required-only: see
    // `Template::required_only`.
    Template {
        key: "mix_commit_mine",
        lang: Lang::En,
        exercises: "first-person promise in English with a Japanese object noun (N7)",
        events: &[EventTpl {
            source: "slack",
            app: "Slack",
            window: "#{channel}",
            text: "{me}: {first}, I'll send the 資料 for {project} by {weekday}.",
        }],
        gold: &[GoldTpl::Commitment {
            direction: "mine",
            description: "I'll send the 資料 for {project} by {weekday}",
            due: Some("{due}"),
        }],
    },
    Template {
        key: "mix_commit_theirs",
        lang: Lang::En,
        exercises: "someone else's promise in English with a Japanese object noun (N7)",
        events: &[EventTpl {
            source: "gmail",
            app: "Mail",
            window: "Re: {project}",
            text: "From: {person} <{email}>\nHi {me},\nWe will send you the 議事録 tomorrow.\n{first}",
        }],
        gold: &[GoldTpl::Commitment {
            direction: "theirs",
            description: "We will send you the 議事録 tomorrow",
            due: Some("{due_tomorrow}"),
        }],
    },
    Template {
        key: "mix_waiting",
        lang: Lang::En,
        exercises: "waiting on someone, English sentence with a Japanese noun (N7)",
        events: &[EventTpl {
            source: "slack",
            app: "Slack",
            window: "#{channel}",
            text: "{me}: Still waiting on {first} for the 承認 on {project}.",
        }],
        gold: &[GoldTpl::OpenLoop {
            kind: "waiting_on_them",
            description: "Still waiting on {first} for the 承認 on {project}",
        }],
    },
    Template {
        key: "mix_trap_quoted",
        lang: Lang::En,
        exercises: "a quoted mixed-language example promise in a style guide (N7)",
        events: &[EventTpl {
            source: "capture",
            app: "Browser",
            window: "Writing guide",
            text: "Example: \"I'll send 資料 by Friday.\" Use this phrasing in client emails.",
        }],
        gold: &[],
    },
    // Dashboard noise (2026-10-13)
    // Shapes the dashboard showed as commitments / open loops. Required-only: see
    // `Template::required_only`.
    Template {
        key: "dash_trap_ja_greeting",
        lang: Lang::Ja,
        exercises: "a polite greeting ends in いたします, which is not a promise",
        events: &[EventTpl {
            source: "slack",
            app: "Slack",
            window: "#{channel}",
            text: "{first}: 本日17時から{project}の定例です。よろしくお願いいたします。",
        }],
        gold: &[],
    },
    Template {
        key: "dash_trap_ja_request_closing",
        lang: Lang::Ja,
        exercises: "a request phrased with お願いいたします is not the user's promise",
        events: &[EventTpl {
            source: "gmail",
            app: "メール",
            window: "Re: {artifact}",
            text: "差出人: {person} <{email}>\n{me}さん\n{artifact}の件、明日、以下でお願いいたします。\n{first}",
        }],
        gold: &[],
    },
    Template {
        key: "dash_trap_ja_marketing",
        lang: Lang::Ja,
        exercises: "marketing copy (お約束します) on a web page",
        events: &[EventTpl {
            source: "capture",
            app: "ブラウザ",
            window: "料金プラン · ハコニワ",
            text: "ハコニワは安心をお約束します。\n今すぐ無料で始める",
        }],
        gold: &[],
    },
    Template {
        key: "dash_trap_ja_pr_rows",
        lang: Lang::Ja,
        exercises: "pull-request list rows whose titles contain レビュー",
        events: &[EventTpl {
            source: "capture",
            app: "ブラウザ",
            window: "プルリクエスト · {project}",
            text: "#{pr} · マージ済み コードレビューとE2Eテスト\n#{pr} · オープン {artifact}のレビュー依頼対応",
        }],
        gold: &[],
    },
    Template {
        key: "dash_trap_ja_agent_rows",
        lang: Lang::Ja,
        exercises: "coding-agent session rows: a status word before the session title",
        events: &[EventTpl {
            source: "capture",
            app: "エージェント",
            window: "セッション",
            text: "アイドル コードレビューと実装計画\n実行中 {artifact}のレビュー待ち対応",
        }],
        gold: &[],
    },
    Template {
        key: "dash_ja_commit_fix_mention",
        lang: Lang::Ja,
        exercises: "a first-person promise followed by a trailing mention",
        events: &[EventTpl {
            source: "slack",
            app: "Slack",
            window: "#{channel}",
            text: "{me}: アプリ全体で30以上のバグを見つけましたので、明日修正します @{handle}",
        }],
        gold: &[GoldTpl::Commitment {
            direction: "mine",
            description: "アプリ全体で30以上のバグを見つけましたので、明日修正します",
            due: Some("{due_tomorrow}"),
        }],
    },
    Template {
        key: "dash_ja_commit_humble",
        lang: Lang::Ja,
        exercises: "a humble-form promise (送付いたします) next to a polite closing",
        events: &[EventTpl {
            source: "gmail",
            app: "メール",
            window: "Re: {artifact}",
            text: "宛先: {person} <{email}>\n{first}さん\n{artifact}は{weekday}までに送付いたします。\nよろしくお願いいたします。\n{me}",
        }],
        gold: &[GoldTpl::Commitment {
            direction: "mine",
            description: "{artifact}は{weekday}までに送付いたします",
            due: Some("{due}"),
        }],
    },
    Template {
        key: "dash_ja_review_cc",
        lang: Lang::Ja,
        exercises: "a request to check (確認お願いします) is a review loop, not a promise",
        events: &[EventTpl {
            source: "slack",
            app: "Slack",
            window: "#{channel}",
            text: "{me}: ・リリース共有 cc @{handle} （確認お願いします）",
        }],
        gold: &[GoldTpl::OpenLoop {
            kind: "review_pending",
            description: "・リリース共有 cc @{handle} （確認お願いします）",
        }],
    },
    Template {
        key: "dash_ja_reply_offer",
        lang: Lang::Ja,
        exercises: "a question built on a promise verb (送りますか) asks; it does not promise",
        events: &[EventTpl {
            source: "slack",
            app: "Slack",
            window: "#{channel}",
            text: "{first}: {artifact}は明日送りますか？",
        }],
        gold: &[GoldTpl::OpenLoop {
            kind: "reply_needed",
            description: "{artifact}は明日送りますか",
        }],
    },
    Template {
        key: "dash_trap_announcement",
        lang: Lang::En,
        exercises: "a third-person future in an announcement (they'll go into effect) is not a promise",
        events: &[EventTpl {
            source: "gmail",
            app: "Mail",
            window: "Updates to our terms",
            text: "From: {org} <notices@larkspur.example>\nHi {me},\nWe've updated our terms of service. The details are here, and they'll go into effect on October 1.\nThe {org} team",
        }],
        gold: &[],
    },
    Template {
        key: "dash_trap_pr_rows",
        lang: Lang::En,
        exercises: "pull-request list rows whose titles or labels spell a cue",
        events: &[EventTpl {
            source: "capture",
            app: "Browser",
            window: "Pull requests · {project}",
            text: "#{pr} • Needs review · Fix {artifact} totals\n#{pr} · Merged · Code review and E2E tests",
        }],
        gold: &[],
    },
    Template {
        key: "dash_trap_agent_rows",
        lang: Lang::En,
        exercises: "coding-agent session rows: a status word before the session title",
        events: &[EventTpl {
            source: "capture",
            app: "Agents",
            window: "Sessions",
            text: "Idle Code review and follow up plan\nRunning Waiting for review on {project}",
        }],
        gold: &[],
    },
    Template {
        key: "dash_commit_theirs_to_you",
        lang: Lang::En,
        exercises: "a third-person promise aimed at the user (they'll send you)",
        events: &[EventTpl {
            source: "slack",
            app: "Slack",
            window: "#{channel}",
            text: "{person}: Legal signed off, they'll send you the countersigned {artifact} by {weekday}.",
        }],
        gold: &[GoldTpl::Commitment {
            direction: "theirs",
            description: "Legal signed off, they'll send you the countersigned {artifact} by {weekday}",
            due: Some("{due}"),
        }],
    },
];

pub fn template(key: &str) -> Option<&'static Template> {
    TEMPLATES.iter().find(|t| t.key == key)
}

/// English and Japanese weekday names, Monday first.
pub const EN_WEEKDAYS: [&str; 7] =
    ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
pub const JA_WEEKDAYS: [&str; 7] =
    ["月曜日", "火曜日", "水曜日", "木曜日", "金曜日", "土曜日", "日曜日"];
