//! The scenario corpus.
//!
//! Hand-written, one mechanism per scenario. A generator that mixed every way a memory can repeat
//! into one big corpus would report a single duplication rate and hide which mechanism produced it;
//! isolating them is what makes each number actionable (the same lesson as `memory-bench`'s
//! single-fault noise sweep).
//!
//! **The capture stream is post-collapse.** Upstream of the state layer, `shogun_core::capture`
//! drops a body >= 98% similar to a recent one (FR-CAP-03) and stores a view that changed >= 40% of
//! its lines as a full checkpoint, which is re-extracted in full. Every same-window recapture below
//! is therefore written as a new view — older messages scrolled off, newer ones arrived — with the
//! memory under test still visible. Those are exactly the events that reach extraction twice.
//!
//! **Ground truth is the sentence, not the row.** Each capture declares which memory each
//! extractable sentence belongs to (`says`). A row's identity is read back from the candidate that
//! produced it, so the benchmark never infers truth from the same text-matching it is evaluating.
//!
//! **Semantics follow the product's own tests** (`extract.rs`): different senders with identical
//! words are different memories, and a recurrence after completion is a new memory. A fix that
//! merges those is wrong even though it lowers the row count, and the precision scenarios exist to
//! catch it.
//!
//! Every name, address and message here is synthetic.

use serde::Serialize;
use shogun_memory::knowledge::{Candidate as EntityCandidate, Kind};

/// How a single real memory came to be written more than once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mechanism {
    /// Cases the current dedup is designed to handle. A failure here is a regression.
    Control,
    /// The same message re-read after the view changed, everything else identical.
    Recapture,
    /// The window title carries a counter that changes between captures.
    TitleDrift,
    /// The attribution line carries a relative-day word that changes overnight.
    DayHeader,
    /// A later message quotes the original, so its sentence appears again with a `>` marker.
    QuotedReply,
    /// A message list shows a truncated preview of the message as well as the message itself.
    TruncatedPreview,
    /// The same message reaches the log through two surfaces: AX capture and a connector.
    CrossSurface,
    /// The same commitment restated in different words.
    Restatement,
    /// One entity mentioned across several messages, each with a different context span.
    EntityMentions,
    /// Distinct memories that look alike. These must stay separate.
    Precision,
}

impl Mechanism {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Control => "control",
            Self::Recapture => "recapture",
            Self::TitleDrift => "title_drift",
            Self::DayHeader => "day_header",
            Self::QuotedReply => "quoted_reply",
            Self::TruncatedPreview => "truncated_preview",
            Self::CrossSurface => "cross_surface",
            Self::Restatement => "restatement",
            Self::EntityMentions => "entity_mentions",
            Self::Precision => "precision",
        }
    }

    /// Precision scenarios are scored on wrong merges; everything else on extra rows.
    pub fn is_precision(self) -> bool {
        matches!(self, Self::Precision)
    }
}

/// Which table a memory lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Table {
    Commitments,
    OpenLoops,
    People,
    Projects,
    Entities,
}

impl Table {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Commitments => "commitments",
            Self::OpenLoops => "open_loops",
            Self::People => "people",
            Self::Projects => "projects",
            Self::Entities => "knowledge_entities",
        }
    }
}

/// One real memory in a scenario.
#[derive(Debug, Clone)]
pub struct Truth {
    pub label: &'static str,
    pub table: Table,
}

/// One captured or synced item, as it reaches `event_log`.
#[derive(Debug, Clone)]
pub struct Capture {
    /// Minutes after the scenario starts.
    pub at_min: i64,
    /// `capture` for Accessibility capture; a connector name (`slack`, `gmail`) otherwise.
    pub source: &'static str,
    pub app: Option<&'static str>,
    pub title: Option<&'static str>,
    /// Native thread id for connector items. `None` for screen capture, which has none.
    pub thread: Option<&'static str>,
    pub body: String,
    /// Which memory each extractable sentence in `body` belongs to: `(label, sentence)`.
    pub says: Vec<(&'static str, &'static str)>,
    /// Entities a Batch pass would extract from this item: `(label, candidate)`.
    pub entities: Vec<(&'static str, EntityCandidate)>,
}

/// What happens next in a scenario.
#[derive(Debug, Clone)]
pub enum Step {
    Capture(Capture),
    /// The user completes the memory carrying this label.
    Complete(&'static str),
}

#[derive(Debug, Clone)]
pub struct Scenario {
    pub id: &'static str,
    pub mechanism: Mechanism,
    pub summary: &'static str,
    pub truths: Vec<Truth>,
    pub steps: Vec<Step>,
    /// A failure this scenario is *expected* to show, with the reason it is accepted. Reported on
    /// its own line rather than hidden, and excluded from the CI gate — the gate exists to catch
    /// regressions, and an accepted trade-off that is documented is not one.
    pub known_limitation: Option<&'static str>,
}

const SLACK: &str = "com.tinyspeck.slackmacgap";
const CHROME: &str = "com.google.Chrome";
const MAIL: &str = "com.apple.mail";
const TEAMS: &str = "com.microsoft.teams2";

/// A screen capture with no entities.
fn screen(at_min: i64, app: &'static str, title: &'static str, body: &str, says: &[(&'static str, &'static str)]) -> Step {
    Step::Capture(Capture {
        at_min,
        source: "capture",
        app: Some(app),
        title: Some(title),
        thread: None,
        body: body.to_string(),
        says: says.to_vec(),
        entities: Vec::new(),
    })
}

/// A connector item.
fn synced(
    at_min: i64,
    source: &'static str,
    thread: &'static str,
    title: &'static str,
    body: &str,
    says: &[(&'static str, &'static str)],
) -> Step {
    Step::Capture(Capture {
        at_min,
        source,
        app: None,
        title: Some(title),
        thread: Some(thread),
        body: body.to_string(),
        says: says.to_vec(),
        entities: Vec::new(),
    })
}

fn entity(kind: Kind, name: &str, context: &str, quote: &str) -> EntityCandidate {
    EntityCandidate {
        kind,
        name: name.to_string(),
        context: context.to_string(),
        quote: quote.to_string(),
        description: None,
        person_class: None,
        channel: None,
        ..Default::default()
    }
}

/// A screen capture carrying Batch entity candidates and no commitments.
fn screen_entities(
    at_min: i64,
    app: &'static str,
    title: &'static str,
    body: &str,
    entities: Vec<(&'static str, EntityCandidate)>,
) -> Step {
    Step::Capture(Capture {
        at_min,
        source: "capture",
        app: Some(app),
        title: Some(title),
        thread: None,
        body: body.to_string(),
        says: Vec::new(),
        entities,
    })
}

const DECK: &str = "I'll send the revised launch deck by Friday";
const REVIEW: &str = "Can you review the pricing page before standup?";
/// A Japanese promise, and the same message as the Slack app renders it with a trailing mention.
const FIX_JA: &str = "アプリ全体で30以上のバグを見つけましたので、明日修正します";
const FIX_JA_MENTION: &str = "アプリ全体で30以上のバグを見つけましたので、明日修正します @dana";

const DAY: i64 = 24 * 60;

/// Every scenario, in report order.
pub fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario {
            id: "exact-recapture",
            known_limitation: None,
            mechanism: Mechanism::Control,
            summary: "Byte-identical body read three times. insert_or_touch collapses it before extraction.",
            truths: vec![Truth { label: "deck", table: Table::Commitments }],
            steps: vec![
                screen(0, SLACK, "launch-plan (Channel) - Acme - Slack", &format!("Sam Reed  10:14 AM\n{DECK}."), &[("deck", DECK)]),
                screen(5, SLACK, "launch-plan (Channel) - Acme - Slack", &format!("Sam Reed  10:14 AM\n{DECK}."), &[("deck", DECK)]),
                screen(9, SLACK, "launch-plan (Channel) - Acme - Slack", &format!("Sam Reed  10:14 AM\n{DECK}."), &[("deck", DECK)]),
            ],
        },
        Scenario {
            id: "connector-thread-recapture",
            known_limitation: None,
            mechanism: Mechanism::Control,
            summary: "One Gmail thread synced twice with the same message. Same thread_key: merged today.",
            truths: vec![Truth { label: "deck", table: Table::Commitments }],
            steps: vec![
                synced(0, "gmail", "thread-launch-deck", "Launch deck", &format!("Hi Dana,\n{DECK}.\nSam"), &[("deck", DECK)]),
                synced(30, "gmail", "thread-launch-deck", "Launch deck", &format!("Hi Dana,\n{DECK}.\nSam\n\nSent from my phone"), &[("deck", DECK)]),
            ],
        },
        // Same window, the view changed
        Scenario {
            id: "scroll-checkpoint",
            known_limitation: None,
            mechanism: Mechanism::Recapture,
            summary: "Channel re-read four times as new messages arrive; the promise stays on screen. The 2026-10-02 fix targets this.",
            truths: vec![
                Truth { label: "deck", table: Table::Commitments },
                Truth { label: "review", table: Table::OpenLoops },
            ],
            steps: vec![
                screen(0, SLACK, "launch-plan (Channel) - Acme - Slack",
                    &format!("Dana Reyes  10:12 AM\nShall we lock the launch date this week\nSam Reed  10:14 AM\n{DECK}.\nDana Reyes  10:15 AM\n{REVIEW}"),
                    &[("deck", DECK), ("review", REVIEW)]),
                screen(12, SLACK, "launch-plan (Channel) - Acme - Slack",
                    &format!("Sam Reed  10:14 AM\n{DECK}.\nDana Reyes  10:15 AM\n{REVIEW}\nPriya Shah  10:22 AM\nVenue is confirmed for the 14th\nMarcus Lee  10:24 AM\nGreat, thanks all"),
                    &[("deck", DECK), ("review", REVIEW)]),
                screen(31, SLACK, "launch-plan (Channel) - Acme - Slack",
                    &format!("Sam Reed  10:14 AM\n{DECK}.\nDana Reyes  10:15 AM\n{REVIEW}\nPriya Shah  10:40 AM\nFloor plan uploaded to the shared drive\nMarcus Lee  10:41 AM\nCatering quote attached\nPriya Shah  10:43 AM\nParking passes are sorted"),
                    &[("deck", DECK), ("review", REVIEW)]),
                screen(95, SLACK, "launch-plan (Channel) - Acme - Slack",
                    &format!("Sam Reed  10:14 AM\n{DECK}.\nDana Reyes  10:15 AM\n{REVIEW}\nMarcus Lee  11:44 AM\nSpeaker list is final\nPriya Shah  11:45 AM\nBadges ordered"),
                    &[("deck", DECK), ("review", REVIEW)]),
            ],
        },
        Scenario {
            id: "title-badge-drift",
            known_limitation: None,
            mechanism: Mechanism::TitleDrift,
            summary: "Slack in a browser tab: the title's unread counter goes (2) -> (5) -> (7) between re-reads.",
            truths: vec![
                Truth { label: "deck", table: Table::Commitments },
                Truth { label: "review", table: Table::OpenLoops },
            ],
            steps: vec![
                screen(0, CHROME, "(2) launch-plan | Acme | Slack",
                    &format!("Sam Reed  10:14 AM\n{DECK}.\nDana Reyes  10:15 AM\n{REVIEW}\nPriya Shah  10:16 AM\nAgenda draft is in the doc"),
                    &[("deck", DECK), ("review", REVIEW)]),
                screen(14, CHROME, "(5) launch-plan | Acme | Slack",
                    &format!("Sam Reed  10:14 AM\n{DECK}.\nDana Reyes  10:15 AM\n{REVIEW}\nPriya Shah  10:22 AM\nVenue is confirmed for the 14th\nMarcus Lee  10:28 AM\nCatering quote attached"),
                    &[("deck", DECK), ("review", REVIEW)]),
                screen(40, CHROME, "(7) launch-plan | Acme | Slack",
                    &format!("Sam Reed  10:14 AM\n{DECK}.\nDana Reyes  10:15 AM\n{REVIEW}\nMarcus Lee  10:50 AM\nSpeaker list is final\nPriya Shah  10:52 AM\nBadges ordered"),
                    &[("deck", DECK), ("review", REVIEW)]),
            ],
        },
        Scenario {
            id: "inbox-count-drift",
            known_limitation: None,
            mechanism: Mechanism::TitleDrift,
            summary: "Gmail in a browser tab: '(3) Inbox' -> '(4) Inbox' while the same open email stays on screen.",
            truths: vec![Truth { label: "deck", table: Table::Commitments }],
            steps: vec![
                screen(0, CHROME, "(3) Inbox - sam@acme.example - Gmail",
                    &format!("Launch deck\nSam Reed <sam@acme.example>\nto Dana\nHi Dana,\n{DECK}.\nSam\nReply  Forward"),
                    &[("deck", DECK)]),
                screen(25, CHROME, "(4) Inbox - sam@acme.example - Gmail",
                    &format!("Launch deck\nSam Reed <sam@acme.example>\nto Dana\nHi Dana,\n{DECK}.\nSam\nDana Reyes <dana@acme.example>\nThanks Sam, that works\nReply  Forward"),
                    &[("deck", DECK)]),
            ],
        },
        Scenario {
            id: "relative-day-header",
            known_limitation: None,
            mechanism: Mechanism::DayHeader,
            summary: "Attribution reads 'Sam Reed, Today' then 'Sam Reed, Yesterday' when the chat is reopened the next morning.",
            truths: vec![Truth { label: "deck", table: Table::Commitments }],
            steps: vec![
                screen(0, TEAMS, "Launch planning | Microsoft Teams",
                    &format!("Dana Reyes, Today 3:58 PM\nWhere are we on the deck\nSam Reed, Today 4:02 PM\n{DECK}.\nDana Reyes, Today 4:03 PM\nPerfect"),
                    &[("deck", DECK)]),
                screen(DAY + 30, TEAMS, "Launch planning | Microsoft Teams",
                    &format!("Sam Reed, Yesterday 4:02 PM\n{DECK}.\nDana Reyes, Yesterday 4:03 PM\nPerfect\nMarcus Lee, Today 9:01 AM\nMorning all, venue walkthrough at noon\nPriya Shah, Today 9:04 AM\nI can join remotely"),
                    &[("deck", DECK)]),
            ],
        },
        Scenario {
            id: "quoted-email-reply",
            known_limitation: None,
            mechanism: Mechanism::QuotedReply,
            summary: "A synced reply quotes the original promise ('> ...'). The quoted line is extracted as a promise again.",
            truths: vec![Truth { label: "deck", table: Table::Commitments }],
            steps: vec![
                synced(0, "gmail", "thread-launch-deck", "Launch deck", &format!("Hi Dana,\n{DECK}.\nSam"), &[("deck", DECK)]),
                synced(90, "gmail", "thread-launch-deck", "Re: Launch deck",
                    &format!("Perfect, thanks Sam\n\nOn Tue, Sam Reed wrote:\n> Hi Dana,\n> {DECK}.\n> Sam"),
                    &[("deck", "> I'll send the revised launch deck by Friday")]),
            ],
        },
        Scenario {
            id: "quoted-reply-on-screen",
            known_limitation: None,
            mechanism: Mechanism::QuotedReply,
            summary: "The same conversation open in Gmail: the original and a reply quoting it are both visible.",
            truths: vec![Truth { label: "deck", table: Table::Commitments }],
            steps: vec![
                screen(0, CHROME, "Launch deck - sam@acme.example - Gmail",
                    &format!("Launch deck\nSam Reed\nHi Dana,\n{DECK}.\nSam\nReply  Forward"),
                    &[("deck", DECK)]),
                screen(90, CHROME, "Launch deck - sam@acme.example - Gmail",
                    &format!("Launch deck\nSam Reed\nHi Dana,\n{DECK}.\nSam\nDana Reyes\nPerfect, thanks Sam\nOn Tue, Sam Reed wrote:\n> {DECK}.\nReply  Forward"),
                    &[("deck", DECK), ("deck", "> I'll send the revised launch deck by Friday")]),
            ],
        },
        Scenario {
            id: "mail-list-preview",
            known_limitation: None,
            mechanism: Mechanism::TruncatedPreview,
            summary: "Mail shows the message list (a truncated preview line) next to the open message.",
            truths: vec![Truth { label: "deck", table: Table::Commitments }],
            steps: vec![
                screen(0, MAIL, "Sent - 214 messages",
                    &format!("Dana Reyes   Launch deck   Hi Dana, I'll send the revised launch deck by Fr…\nPriya Shah   Venue   Confirmed for the 14th\n\nLaunch deck\nTo: Dana Reyes\nHi Dana,\n{DECK}.\nSam"),
                    &[("deck", "Dana Reyes   Launch deck   Hi Dana, I'll send the revised launch deck by Fr…"), ("deck", DECK)]),
            ],
        },
        Scenario {
            id: "slack-app-and-connector",
            known_limitation: None,
            mechanism: Mechanism::CrossSurface,
            summary: "The message is read on screen in the Slack app and also arrives through the Slack connector.",
            truths: vec![Truth { label: "deck", table: Table::Commitments }],
            steps: vec![
                screen(0, SLACK, "launch-plan (Channel) - Acme - Slack",
                    &format!("Dana Reyes  10:12 AM\nShall we lock the launch date this week\nSam Reed  10:14 AM\n{DECK}."),
                    &[("deck", DECK)]),
                synced(3, "slack", "C04LAUNCH:1790000040.000100", "launch-plan", &format!("{DECK}."), &[("deck", DECK)]),
            ],
        },
        Scenario {
            id: "slack-app-mention-and-connector",
            known_limitation: None,
            mechanism: Mechanism::CrossSurface,
            summary: "The Slack app shows the message with a trailing @mention; the connector's text has no mention.",
            truths: vec![Truth { label: "fix", table: Table::Commitments }],
            steps: vec![
                screen(0, SLACK, "launch-plan (Channel) - Acme - Slack",
                    &format!("Dana Reyes  10:12 AM\nHow bad is the bug backlog\nSam Reed  10:14 AM\n{FIX_JA_MENTION}"),
                    &[("fix", FIX_JA_MENTION)]),
                synced(3, "slack", "C04LAUNCH:1790000040.000200", "launch-plan", FIX_JA, &[("fix", FIX_JA)]),
            ],
        },
        Scenario {
            id: "gmail-tab-and-connector",
            known_limitation: None,
            mechanism: Mechanism::CrossSurface,
            summary: "An email is read in a Gmail tab and also synced through the Gmail connector.",
            truths: vec![Truth { label: "deck", table: Table::Commitments }],
            steps: vec![
                screen(0, CHROME, "Launch deck - sam@acme.example - Gmail",
                    &format!("Launch deck\nSam Reed\nHi Dana,\n{DECK}.\nSam\nReply  Forward"),
                    &[("deck", DECK)]),
                synced(2, "gmail", "thread-launch-deck", "Launch deck", &format!("Hi Dana,\n{DECK}.\nSam"), &[("deck", DECK)]),
            ],
        },
        Scenario {
            id: "restated-promise",
            known_limitation: Some(
                "Not matched at write time, by design. Lexical similarity cannot tell a paraphrase \
                 ('get the deck over to you') from a different object ('the Q4 deck') without wrong \
                 merges. Deferred to Dream Cycle consolidation, where a model judges candidate pairs.",
            ),
            mechanism: Mechanism::Restatement,
            summary: "The promise is restated the next day in different words. One commitment, two sentences.",
            truths: vec![Truth { label: "deck", table: Table::Commitments }],
            steps: vec![
                screen(0, SLACK, "launch-plan (Channel) - Acme - Slack",
                    &format!("Sam Reed  10:14 AM\n{DECK}."),
                    &[("deck", DECK)]),
                screen(DAY, SLACK, "launch-plan (Channel) - Acme - Slack",
                    "Dana Reyes  9:40 AM\nAny update on the deck\nSam Reed  9:42 AM\nI'll get the launch deck over to you by Friday as promised.",
                    &[("deck", "I'll get the launch deck over to you by Friday as promised")]),
            ],
        },
        Scenario {
            id: "person-many-mentions",
            known_limitation: None,
            mechanism: Mechanism::EntityMentions,
            summary: "Dana is mentioned in four different messages in one channel; each mention has its own context span.",
            truths: vec![Truth { label: "dana", table: Table::People }],
            steps: vec![
                screen_entities(0, SLACK, "launch-plan (Channel) - Acme - Slack",
                    "Priya Shah  10:01 AM\nDana from legal signed off on the terms",
                    vec![("dana", entity(Kind::Person, "Dana", "Dana from legal", "Dana from legal signed off on the terms"))]),
                screen_entities(40, SLACK, "launch-plan (Channel) - Acme - Slack",
                    "Marcus Lee  10:41 AM\nDana approved the venue contract this morning",
                    vec![("dana", entity(Kind::Person, "Dana", "Dana approved the venue contract", "Dana approved the venue contract this morning"))]),
                screen_entities(DAY, SLACK, "launch-plan (Channel) - Acme - Slack",
                    "Priya Shah  2:10 PM\nWaiting on feedback from Dana on the press release",
                    vec![("dana", entity(Kind::Person, "Dana", "feedback from Dana", "Waiting on feedback from Dana on the press release"))]),
                screen_entities(DAY + 90, SLACK, "launch-plan (Channel) - Acme - Slack",
                    "Sam Reed  3:41 PM\nLooping in Dana for the final legal pass",
                    vec![("dana", entity(Kind::Person, "Dana", "Dana for the final legal pass", "Looping in Dana for the final legal pass"))]),
            ],
        },
        Scenario {
            id: "person-across-apps",
            known_limitation: None,
            mechanism: Mechanism::EntityMentions,
            summary: "The same Dana in Slack, then Mail, then a calendar invite.",
            truths: vec![Truth { label: "dana", table: Table::People }],
            steps: vec![
                screen_entities(0, SLACK, "launch-plan (Channel) - Acme - Slack",
                    "Priya Shah  10:01 AM\nDana Reyes from legal signed off on the terms",
                    vec![("dana", entity(Kind::Person, "Dana Reyes", "Dana Reyes from legal", "Dana Reyes from legal signed off on the terms"))]),
                screen_entities(60, MAIL, "Inbox - 1,204 messages, 3 unread",
                    "Contract redlines\nFrom: Dana Reyes\nAttached are the redlines Dana Reyes promised",
                    vec![("dana", entity(Kind::Person, "Dana Reyes", "redlines Dana Reyes promised", "Attached are the redlines Dana Reyes promised"))]),
                screen_entities(180, "com.apple.iCal", "Calendar",
                    "Legal review with Dana Reyes\nThursday 11:00 to 11:30",
                    vec![("dana", entity(Kind::Person, "Dana Reyes", "Legal review with Dana Reyes", "Legal review with Dana Reyes"))]),
            ],
        },
        Scenario {
            id: "project-many-mentions",
            known_limitation: None,
            mechanism: Mechanism::EntityMentions,
            summary: "Project Kestrel discussed in three messages with three different context spans.",
            truths: vec![Truth { label: "kestrel", table: Table::Projects }],
            steps: vec![
                screen_entities(0, SLACK, "eng-planning (Channel) - Acme - Slack",
                    "Marcus Lee  9:12 AM\nKestrel beta opens to ten pilot customers",
                    vec![("kestrel", entity(Kind::Project, "Kestrel", "Kestrel beta", "Kestrel beta opens to ten pilot customers"))]),
                screen_entities(120, SLACK, "eng-planning (Channel) - Acme - Slack",
                    "Priya Shah  11:15 AM\nThe Kestrel migration finished overnight",
                    vec![("kestrel", entity(Kind::Project, "Kestrel", "The Kestrel migration", "The Kestrel migration finished overnight"))]),
                screen_entities(DAY, SLACK, "eng-planning (Channel) - Acme - Slack",
                    "Dana Reyes  10:02 AM\nLegal needs the Kestrel data processing addendum",
                    vec![("kestrel", entity(Kind::Project, "Kestrel", "the Kestrel data processing addendum", "Legal needs the Kestrel data processing addendum"))]),
            ],
        },
        Scenario {
            id: "two-senders-same-words",
            known_limitation: None,
            mechanism: Mechanism::Precision,
            summary: "Two people each promise 'I'll send the contract by Friday' in the same channel. Two commitments.",
            truths: vec![
                Truth { label: "dana-contract", table: Table::Commitments },
                Truth { label: "marcus-contract", table: Table::Commitments },
            ],
            steps: vec![
                screen(0, SLACK, "legal (Channel) - Acme - Slack",
                    "Dana Reyes  10:12 AM\nI'll send the contract by Friday.\nPriya Shah  10:13 AM\nThanks Dana",
                    &[("dana-contract", "I'll send the contract by Friday")]),
                screen(DAY, SLACK, "legal (Channel) - Acme - Slack",
                    "Marcus Lee  2:30 PM\nI'll send the contract by Friday.\nPriya Shah  2:31 PM\nThanks Marcus",
                    &[("marcus-contract", "I'll send the contract by Friday")]),
            ],
        },
        Scenario {
            id: "different-objects",
            known_limitation: None,
            mechanism: Mechanism::Precision,
            summary: "'the Q3 deck' and 'the Q4 deck' from the same sender in the same channel. Two commitments.",
            truths: vec![
                Truth { label: "q3", table: Table::Commitments },
                Truth { label: "q4", table: Table::Commitments },
            ],
            steps: vec![
                screen(0, SLACK, "finance (Channel) - Acme - Slack",
                    "Sam Reed  10:14 AM\nI'll send the Q3 board deck by Friday.\nDana Reyes  10:15 AM\nThanks",
                    &[("q3", "I'll send the Q3 board deck by Friday")]),
                screen(DAY * 7, SLACK, "finance (Channel) - Acme - Slack",
                    "Sam Reed  10:20 AM\nI'll send the Q4 board deck by Friday.\nDana Reyes  10:21 AM\nThanks",
                    &[("q4", "I'll send the Q4 board deck by Friday")]),
            ],
        },
        Scenario {
            id: "weekly-recurrence",
            known_limitation: None,
            mechanism: Mechanism::Precision,
            summary: "A weekly report promised in the same thread, completed, then promised again next week. Two commitments.",
            truths: vec![
                Truth { label: "week-1", table: Table::Commitments },
                Truth { label: "week-2", table: Table::Commitments },
            ],
            steps: vec![
                synced(0, "gmail", "thread-weekly-report", "Weekly report", "Hi team,\nI'll send the weekly report by Friday.\nSam", &[("week-1", "I'll send the weekly report by Friday")]),
                Step::Complete("week-1"),
                synced(DAY * 7, "gmail", "thread-weekly-report", "Weekly report", "Hi team,\nI'll send the weekly report by Friday.\nSam\n\nWeek 2", &[("week-2", "I'll send the weekly report by Friday")]),
            ],
        },
        Scenario {
            id: "same-words-different-threads",
            known_limitation: None,
            mechanism: Mechanism::Precision,
            summary: "The same sentence sent to two different people in two different email threads. Two commitments.",
            truths: vec![
                Truth { label: "to-dana", table: Table::Commitments },
                Truth { label: "to-marcus", table: Table::Commitments },
            ],
            steps: vec![
                synced(0, "gmail", "thread-dana-nda", "NDA", "Hi Dana,\nI'll send the signed NDA by Friday.\nSam", &[("to-dana", "I'll send the signed NDA by Friday")]),
                synced(10, "gmail", "thread-marcus-nda", "NDA", "Hi Marcus,\nI'll send the signed NDA by Friday.\nSam", &[("to-marcus", "I'll send the signed NDA by Friday")]),
            ],
        },
        Scenario {
            id: "namesakes",
            known_limitation: Some(
                "Owner decision 2026-10-14: a person's name (one word included) resolves to the \
                 existing row of that name everywhere, so 'mikel' and 'Mikel' never become two \
                 people. Two different people who share only a first name are told apart by the \
                 review model (it is shown the known people and maps a mention with `existing_id`), \
                 and the user's split (FR-ST-10) corrects the rest.",
            ),
            mechanism: Mechanism::Precision,
            summary: "Alex in design and Alex in finance. Two people who share a first name.",
            truths: vec![
                Truth { label: "alex-design", table: Table::People },
                Truth { label: "alex-finance", table: Table::People },
            ],
            steps: vec![
                screen_entities(0, SLACK, "design (Channel) - Acme - Slack",
                    "Priya Shah  10:01 AM\nAlex from design is reworking the onboarding mockups",
                    vec![("alex-design", entity(Kind::Person, "Alex", "Alex from design", "Alex from design is reworking the onboarding mockups"))]),
                screen_entities(DAY, MAIL, "Inbox - 1,204 messages, 3 unread",
                    "Budget\nFrom: Alex Moreno\nAlex in finance approved the Q4 budget",
                    vec![("alex-finance", entity(Kind::Person, "Alex", "Alex in finance", "Alex in finance approved the Q4 budget"))]),
            ],
        },
        // Adversarial: each one aims at a specific identity rule
        Scenario {
            id: "numbered-windows",
            known_limitation: None,
            mechanism: Mechanism::Precision,
            summary: "The same reviewer writes the same sentence on PR #812 and PR #813. A number that names the window is not a counter.",
            truths: vec![
                Truth { label: "pr-812", table: Table::Commitments },
                Truth { label: "pr-813", table: Table::Commitments },
            ],
            steps: vec![
                screen(0, CHROME, "PR #812 - acme/api - GitHub",
                    "Fix login race\nSam Reed commented\nI'll merge this after CI passes.\nApprove  Comment",
                    &[("pr-812", "I'll merge this after CI passes")]),
                screen(20, CHROME, "PR #813 - acme/api - GitHub",
                    "Bump the rate limiter\nPriya Shah opened this\nSam Reed commented\nI'll merge this after CI passes.\nMerge pull request",
                    &[("pr-813", "I'll merge this after CI passes")]),
            ],
        },
        Scenario {
            id: "two-mail-accounts",
            known_limitation: None,
            mechanism: Mechanism::Precision,
            summary: "The same promise sent from the Work inbox and from the Personal inbox. The account segment of the title is identity.",
            truths: vec![
                Truth { label: "work", table: Table::Commitments },
                Truth { label: "personal", table: Table::Commitments },
            ],
            steps: vec![
                screen(0, MAIL, "Inbox — Work",
                    "Lease paperwork\nSam Reed\nI'll send the signed lease by Friday.\nSam",
                    &[("work", "I'll send the signed lease by Friday")]),
                screen(30, MAIL, "Inbox — Personal",
                    "Apartment\nSam Reed\nI'll send the signed lease by Friday.\nThanks again\nSam",
                    &[("personal", "I'll send the signed lease by Friday")]),
            ],
        },
        Scenario {
            id: "screen-and-connector-different-threads",
            known_limitation: None,
            mechanism: Mechanism::Precision,
            summary: "Synced thread 'NDA' to Dana; on screen, a different thread 'NDA for Marcus' with the same promise. Containment is not identity.",
            truths: vec![
                Truth { label: "to-dana", table: Table::Commitments },
                Truth { label: "to-marcus", table: Table::Commitments },
            ],
            steps: vec![
                synced(0, "gmail", "thread-dana-nda", "NDA", "Hi Dana,\nI'll send the signed NDA by Friday.\nSam", &[("to-dana", "I'll send the signed NDA by Friday")]),
                screen(4, CHROME, "NDA for Marcus - sam@acme.example - Gmail",
                    "NDA for Marcus\nSam Reed\nHi Marcus,\nI'll send the signed NDA by Friday.\nSam\nReply  Forward",
                    &[("to-marcus", "I'll send the signed NDA by Friday")]),
            ],
        },
        Scenario {
            id: "same-words-different-mentions",
            known_limitation: None,
            mechanism: Mechanism::Precision,
            summary: "One sender makes the same long promise to two people minutes apart, each ending in a different @mention. Two commitments.",
            truths: vec![
                Truth { label: "to-dana", table: Table::Commitments },
                Truth { label: "to-lee", table: Table::Commitments },
            ],
            steps: vec![
                screen(0, SLACK, "launch-plan (Channel) - Acme - Slack",
                    "Sam Reed  10:14 AM\nI'll review the pricing page copy and the launch checklist with you before the customer call today @dana",
                    &[("to-dana", "I'll review the pricing page copy and the launch checklist with you before the customer call today @dana")]),
                screen(3, SLACK, "launch-plan (Channel) - Acme - Slack",
                    "Sam Reed  10:17 AM\nI'll review the pricing page copy and the launch checklist with you before the customer call today @lee",
                    &[("to-lee", "I'll review the pricing page copy and the launch checklist with you before the customer call today @lee")]),
            ],
        },
        Scenario {
            id: "two-alexes-one-channel",
            known_limitation: Some(
                "One-word person names are taken as one person. Two people \
                 called only 'Alex' in the same channel are fused; the user's split (FR-ST-10) \
                 separates them and makes the name ambiguous, so it is never auto-resolved again. \
                 Accepted against one row per mention, which every multi-mention person paid.",
            ),
            mechanism: Mechanism::Precision,
            summary: "Two different people both referred to only as 'Alex' in the same channel.",
            truths: vec![
                Truth { label: "alex-a", table: Table::People },
                Truth { label: "alex-b", table: Table::People },
            ],
            steps: vec![
                screen_entities(0, SLACK, "general (Channel) - Acme - Slack",
                    "Priya Shah  10:01 AM\nAlex from design is reworking the onboarding mockups",
                    vec![("alex-a", entity(Kind::Person, "Alex", "Alex from design", "Alex from design is reworking the onboarding mockups"))]),
                screen_entities(DAY, SLACK, "general (Channel) - Acme - Slack",
                    "Marcus Lee  2:10 PM\nAlex in finance approved the Q4 budget",
                    vec![("alex-b", entity(Kind::Person, "Alex", "Alex in finance", "Alex in finance approved the Q4 budget"))]),
            ],
        },
        Scenario {
            id: "two-senders-one-capture",
            known_limitation: Some(
                "Pre-existing, not introduced here: inside one capture, RECAPTURE_CONTEXT_SQL's \
                 same-event arm treats an identical sentence as one message regardless of sender. \
                 Fixing it means applying attribution inside a single event, which is a separate change.",
            ),
            mechanism: Mechanism::Precision,
            summary: "Two people make the identical promise and both messages are visible in one capture.",
            truths: vec![
                Truth { label: "dana", table: Table::Commitments },
                Truth { label: "marcus", table: Table::Commitments },
            ],
            steps: vec![Step::Capture(Capture {
                at_min: 0,
                source: "capture",
                app: Some(SLACK),
                title: Some("legal (Channel) - Acme - Slack"),
                thread: None,
                body: "Dana Reyes  10:12 AM\nI'll review the contract by Friday.\nMarcus Lee  10:13 AM\nI'll review the contract by Friday!".to_string(),
                // Identical sentences: labelled by position, Dana's first.
                says: vec![
                    ("dana", "I'll review the contract by Friday"),
                    ("marcus", "I'll review the contract by Friday"),
                ],
                entities: Vec::new(),
            })],
        },
    ]
}
