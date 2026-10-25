//! The deterministic stream generator (benchmark v1).
//!
//! One synthetic principal per `(tier, seed)`: a stream of email, chat, meeting-note and
//! agent-turn events over a number of working days, with planted **commitments** (the principal's
//! own and other people's promises to the principal) and **open loops** (waiting on someone, a
//! reply owed, a review requested, a decision pending), their later **resolutions** and
//! **cancellations**, **supersessions** (a due date moved, once or twice), and **distractors**
//! that look like promises and are not (hypotheticals, other people's promises to third parties,
//! retracted offers, reports of promises already kept). Everything else is filler.
//!
//! Every name, artifact and project comes from the fixed fictional pools below; no real data can
//! enter. The output depends only on the seed, the tier and this file: the PRNG is
//! [`memory_bench::rng::Rng`] (SplitMix64, in-tree), never a crate whose output may change.

use memory_bench::rng::Rng;
use serde::{Deserialize, Serialize};

/// Monday 2026-09-07 00:00 UTC: day 0 of every stream.
pub const EPOCH_MS: i64 = 1_788_739_200_000;
pub const DAY_MS: i64 = 86_400_000;
pub const HOUR_MS: i64 = 3_600_000;

const PEOPLE: &[&str] = &[
    "Oren Vale", "Tamsin Ekberg", "Priya Nand", "Mateo Ruiz", "Ines Halvorsen", "Kenji Arai", "Dana Okafor", "Lucia Brandt", "Sven Morrow",
    "Amara Quist", "Felix Dorn", "Noor Haddad",
];
const PRINCIPALS: &[&str] = &["Rin Sato", "Alex Moreau", "Jo Lindqvist", "Sam Achebe", "Mika Tanabe"];
const ARTIFACTS: &[&str] = &[
    "the rollout plan", "the Q3 budget", "the vendor contract", "the security review", "the onboarding guide", "the pricing deck",
    "the API migration notes", "the hiring scorecard", "the latency report", "the launch checklist", "the customer FAQ",
    "the data retention policy", "the incident postmortem", "the capacity forecast", "the partner brief", "the release notes",
    "the support runbook", "the board memo", "the design spec", "the test plan", "the renewal quote", "the audit evidence",
    "the training schedule", "the roadmap draft",
];
const PROJECTS: &[&str] = &["Heron", "Atlas Ledger", "Kestrel", "Juniper", "Orchid", "Basalt"];
const WEEKDAYS: &[&str] = &["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
const MONTHS: &[&str] = &["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

/// Difficulty tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Easy,
    Medium,
    Hard,
}

impl Tier {
    pub const ALL: [Tier; 3] = [Tier::Easy, Tier::Medium, Tier::Hard];

    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Easy => "easy",
            Tier::Medium => "medium",
            Tier::Hard => "hard",
        }
    }

    pub fn parse(raw: &str) -> Option<Tier> {
        match raw {
            "easy" => Some(Tier::Easy),
            "medium" => Some(Tier::Medium),
            "hard" => Some(Tier::Hard),
            _ => None,
        }
    }

    fn spec(self) -> Spec {
        match self {
            Tier::Easy => Spec { days: 5, commitments: 4, loops: 4, resolve: 0.3, cancel: 0.0, supersede: 0, twice: 0, distractors: 4, filler_per_item: 3, wrapped: false, paraphrase: false, mixed_channels: false, bundled: false },
            Tier::Medium => Spec { days: 10, commitments: 8, loops: 8, resolve: 0.35, cancel: 0.1, supersede: 3, twice: 0, distractors: 8, filler_per_item: 6, wrapped: true, paraphrase: false, mixed_channels: true, bundled: false },
            Tier::Hard => Spec { days: 15, commitments: 12, loops: 12, resolve: 0.4, cancel: 0.1, supersede: 6, twice: 2, distractors: 16, filler_per_item: 10, wrapped: true, paraphrase: true, mixed_channels: true, bundled: true },
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Spec {
    days: i64,
    commitments: usize,
    loops: usize,
    /// Share of items resolved later in the stream.
    resolve: f64,
    /// Share of items cancelled later in the stream.
    cancel: f64,
    /// Commitments whose due date moves.
    supersede: usize,
    /// Of those, how many move twice.
    twice: usize,
    distractors: usize,
    filler_per_item: usize,
    /// Planted sentences sit inside a greeting / sign-off, not alone.
    wrapped: bool,
    /// Second template set (less canonical wording).
    paraphrase: bool,
    /// Resolutions and updates arrive on a different channel than the opening.
    mixed_channels: bool,
    /// Meeting notes carry several items in one event.
    bundled: bool,
}

/// Where an event came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Email,
    Chat,
    Meeting,
    AgentTurn,
}

impl Channel {
    pub const ALL: [Channel; 4] = [Channel::Email, Channel::Chat, Channel::Meeting, Channel::AgentTurn];

    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Email => "email",
            Channel::Chat => "chat",
            Channel::Meeting => "meeting",
            Channel::AgentTurn => "agent_turn",
        }
    }
}

/// One event of the stream, as a system ingests it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Stable id within the dataset (`e0001`, …); predictions cite it.
    pub id: String,
    pub ts_ms: i64,
    pub channel: Channel,
    /// Conversation id (an email thread, a chat channel, a meeting, an agent session).
    pub thread: String,
    pub author: String,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Commitment,
    OpenLoop,
}

impl ItemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemKind::Commitment => "commitment",
            ItemKind::OpenLoop => "open_loop",
        }
    }
}

/// How a gold item ends the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinalStatus {
    Open,
    Resolved,
    Cancelled,
}

/// One planted commitment or open loop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoldItem {
    pub id: String,
    pub kind: ItemKind,
    /// `mine` / `theirs` for commitments; the open-loop kind otherwise.
    pub subtype: String,
    pub counterparty: String,
    pub artifact: String,
    /// A canonical one-line description (what a perfect system would store).
    pub text: String,
    /// The event that opened it and the exact sentence that proves it.
    pub opened_event: String,
    pub quote: String,
    /// Due date as first stated, then the latest one (start of the UTC day); `None` when none.
    pub first_due_ms: Option<i64>,
    pub due_ms: Option<i64>,
    /// Events that moved the due date, in order.
    pub superseded_by_events: Vec<String>,
    pub status: FinalStatus,
    /// The event that resolved or cancelled it.
    pub closed_event: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DistractorKind {
    /// "If the budget comes through, I will send …"
    Hypothetical,
    /// Someone else's promise to a third party, reported to the principal.
    ThirdParty,
    /// An offer withdrawn in the same breath.
    Retracted,
    /// A promise already kept, reported in the past tense.
    AlreadyDone,
}

/// A sentence that looks like a commitment or open loop of the principal's and is not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Distractor {
    pub id: String,
    pub kind: DistractorKind,
    pub event: String,
    /// The exact sentence.
    pub span: String,
}

/// The gold file of one `(tier, seed)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gold {
    pub benchmark: String,
    pub version: String,
    pub tier: Tier,
    pub seed: u64,
    pub principal: String,
    /// When the stream ends; systems are read at this time.
    pub end_ms: i64,
    pub items: Vec<GoldItem>,
    pub distractors: Vec<Distractor>,
}

/// One generated dataset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dataset {
    pub events: Vec<Event>,
    pub gold: Gold,
}

pub const BENCHMARK: &str = "open-loop-tracking";
pub const VERSION: &str = "1.0";

/// `YYYY-MM-DD` of an instant (UTC).
pub fn day_label(ms: i64) -> String {
    let d = shogun_memory::due::civil_from_days(ms.div_euclid(DAY_MS));
    format!("{:04}-{:02}-{:02}", d.year, d.month, d.day)
}

fn start_of_day(ms: i64) -> i64 {
    ms.div_euclid(DAY_MS) * DAY_MS
}

/// How a due day is written, said on `said_ms`: a weekday name (always the next occurrence,
/// 1–6 days on) or a month-and-day date.
fn due_phrase(rng: &mut Rng, said_ms: i64, due_ms: i64) -> String {
    let ahead = (start_of_day(due_ms) - start_of_day(said_ms)) / DAY_MS;
    let weekday = (due_ms.div_euclid(DAY_MS) + 3).rem_euclid(7) as usize; // 1970-01-01 was a Thursday.
    if (1..=6).contains(&ahead) && rng.chance(0.6) {
        WEEKDAYS[weekday].to_string()
    } else {
        let d = shogun_memory::due::civil_from_days(due_ms.div_euclid(DAY_MS));
        format!("{} {}", MONTHS[(d.month as usize).saturating_sub(1) % 12], d.day)
    }
}

/// A working-hours instant on `day` (09:00–17:59 UTC).
fn work_time(rng: &mut Rng, day: i64) -> i64 {
    EPOCH_MS + day * DAY_MS + 9 * HOUR_MS + rng.below(9 * 60) as i64 * 60_000 + rng.below(60) as i64 * 1_000
}

fn first_name(full: &str) -> &str {
    full.split(' ').next().unwrap_or(full)
}

/// A planted event before it is given an id: when, where, who, the full content and the exact
/// sentence inside it.
struct Draft {
    ts_ms: i64,
    channel: Channel,
    thread: String,
    author: String,
    content: String,
    role: Role,
    /// Further items this event opens (bundled meeting notes).
    extra_opens: Vec<(usize, String)>,
}

enum Role {
    Open { item: usize, sentence: String },
    Update { item: usize },
    Close { item: usize },
    Distractor { index: usize, sentence: String },
    Filler,
}

fn wrap(rng: &mut Rng, channel: Channel, to: &str, sentence: &str, wrapped: bool) -> String {
    match channel {
        Channel::Email if wrapped => {
            let greet = ["Hi", "Hello", "Hey"][rng.index(3)];
            let close = ["Thanks", "Best", "Cheers"][rng.index(3)];
            format!("{greet} {},\n\n{sentence}\n\n{close}", first_name(to))
        }
        Channel::AgentTurn => format!("user: {sentence}"),
        Channel::Meeting if wrapped => format!("Meeting notes.\n- {sentence}"),
        _ => sentence.to_string(),
    }
}

fn pick<'a>(rng: &mut Rng, items: &[&'a str]) -> &'a str {
    rng.pick(items).copied().unwrap_or_default()
}

/// The opening sentence of an item, its canonical text, and who says it.
/// Who and what an item is about.
struct Cast<'a> {
    person: &'a str,
    artifact: &'a str,
    project: &'a str,
    principal: &'a str,
}

fn opening(rng: &mut Rng, kind: ItemKind, subtype: &str, cast: &Cast<'_>, due: Option<&str>, paraphrase: bool) -> (String, String, String) {
    let Cast { person, artifact, project, principal } = *cast;
    let due = due.unwrap_or("");
    match (kind, subtype) {
        (ItemKind::Commitment, "mine") => {
            let canonical = format!("send {artifact} to {person} by {due}");
            let templates: &[&str] = if paraphrase {
                &["Sure, {a} is on me, you will have it by {d}.", "Leave {a} with me; it'll be with {p} by {d}.", "Count on me for {a}, {p} gets it by {d}."]
            } else {
                &["I'll send {a} to {p} by {d}.", "I will have {a} ready for {p} by {d}.", "I'll get {a} over to {p} by {d}."]
            };
            let sentence = pick(rng, templates).replace("{a}", artifact).replace("{p}", person).replace("{d}", due);
            (sentence, canonical, principal.to_string())
        }
        (ItemKind::Commitment, _) => {
            let canonical = format!("{person} sends {artifact} by {due}");
            let templates: &[&str] = if paraphrase {
                &["{p}: you will see {a} from me by {d}.", "{p}: {a} is coming your way by {d}, promise."]
            } else {
                &["{p}: I'll send you {a} by {d}.", "{p}: I will get {a} to you by {d}."]
            };
            let sentence = pick(rng, templates).replace("{a}", artifact).replace("{p}", person).replace("{d}", due);
            (sentence, canonical, person.to_string())
        }
        (ItemKind::OpenLoop, "waiting_on_them") => {
            let templates: &[&str] = if paraphrase {
                &["No word yet from {p} on {a}.", "{a} from {p} still hasn't arrived."]
            } else {
                &["Still waiting on {p} for {a}.", "Waiting for {p} to send {a}."]
            };
            let sentence = pick(rng, templates).replace("{a}", artifact).replace("{p}", person);
            (sentence, format!("waiting on {person} for {artifact}"), principal.to_string())
        }
        (ItemKind::OpenLoop, "reply_needed") => {
            let templates: &[&str] = if paraphrase {
                &["{p}: any chance you can confirm the numbers in {a}?", "{p}: does {a} look right to you?"]
            } else {
                &["{p}: can you confirm the numbers in {a}?", "{p}: could you check {a} and reply?"]
            };
            let sentence = pick(rng, templates).replace("{a}", artifact).replace("{p}", person);
            (sentence, format!("reply to {person} about {artifact}"), person.to_string())
        }
        (ItemKind::OpenLoop, "review_pending") => {
            let templates: &[&str] = if paraphrase {
                &["{p} would like your eyes on {a} before it goes out.", "{p} flagged {a} for your review."]
            } else {
                &["{p} asked you to review {a}.", "{p} requested your review of {a}."]
            };
            let sentence = pick(rng, templates).replace("{a}", artifact).replace("{p}", person);
            (sentence, format!("review {artifact} for {person}"), principal.to_string())
        }
        _ => {
            let templates: &[&str] = if paraphrase {
                &["Open question for {j}: does it ship with {a} or not.", "{j} is blocked until we settle {a}."]
            } else {
                &["We still need to decide whether {j} ships with {a}.", "Decision pending on {a} for {j}."]
            };
            let sentence = pick(rng, templates).replace("{a}", artifact).replace("{j}", project);
            (sentence, format!("decide whether {project} ships with {artifact}"), principal.to_string())
        }
    }
}

fn closing(rng: &mut Rng, kind: ItemKind, subtype: &str, person: &str, artifact: &str, project: &str, cancelled: bool) -> String {
    if cancelled {
        let templates: &[&str] = &["Never mind about {a} for {p}, it is no longer needed.", "Dropping {a} for {p}; the plan changed."];
        return pick(rng, templates).replace("{a}", artifact).replace("{p}", person);
    }
    let templates: &[&str] = match (kind, subtype) {
        (ItemKind::Commitment, "mine") => &["Sent {a} to {p}.", "Done: {a} is with {p} now."],
        (ItemKind::Commitment, _) => &["{p} sent {a} over, thanks.", "Got {a} from {p}."],
        (ItemKind::OpenLoop, "waiting_on_them") => &["{a} arrived from {p}, thanks.", "{p} delivered {a}."],
        (ItemKind::OpenLoop, "reply_needed") => &["Replied to {p} about {a}.", "Answered {p} on {a}."],
        (ItemKind::OpenLoop, "review_pending") => &["Finished the review of {a} for {p}.", "Review of {a} done and sent back to {p}."],
        _ => &["Decided: {j} ships with {a}.", "Settled it, {j} goes out with {a}."],
    };
    pick(rng, templates).replace("{a}", artifact).replace("{p}", person).replace("{j}", project)
}

fn update(rng: &mut Rng, artifact: &str, person: &str, new_due: &str) -> String {
    let templates: &[&str] = &["Update: {a} for {p} moves to {d}.", "Change of plan, {a} for {p} is now due {d}.", "Heads up, {a} for {p} slips to {d}."];
    pick(rng, templates).replace("{a}", artifact).replace("{p}", person).replace("{d}", new_due)
}

fn distractor(rng: &mut Rng, kind: DistractorKind, person: &str, other: &str, artifact: &str, due: &str) -> String {
    let templates: &[&str] = match kind {
        DistractorKind::Hypothetical => &[
            "If the budget comes through, I will send {a} to {p} by {d}.",
            "Maybe I could look at {a} at some point, no promises.",
            "In theory we would ship {a} by {d}, if legal ever signs off.",
        ],
        DistractorKind::ThirdParty => &[
            "FYI, {p} told {o} she will send {a} to the client by {d}.",
            "{p} promised {o} that {a} will reach the auditors by {d}.",
        ],
        DistractorKind::Retracted => &["I was going to send {a} to {p}, but that is off now.", "Scratch my offer on {a} for {p}, someone else has it."],
        DistractorKind::AlreadyDone => &["Last month I sent {a} to {p} as promised.", "Back in August {p} got {a} from me, all settled."],
    };
    pick(rng, templates).replace("{a}", artifact).replace("{p}", person).replace("{o}", other).replace("{d}", due)
}

fn filler(rng: &mut Rng, channel: Channel, project: &str, person: &str) -> String {
    let templates: &[&str] = match channel {
        Channel::Email => &[
            "Attached are the slides from the {j} sync for reference.",
            "The {j} dashboard numbers look stable this week.",
            "Forwarding the notes from {p} on the {j} kickoff, nothing urgent.",
            "Calendar invite for the {j} retro is in your inbox.",
        ],
        Channel::Chat => &["lunch at noon?", "the {j} build is green again", "{p} is out today", "nice work on the {j} demo", "coffee later?"],
        Channel::Meeting => &[
            "Meeting notes: discussed the {j} roadmap, no action items.",
            "Standup: {p} shared progress on {j}; nothing blocked.",
            "Notes: reviewed {j} metrics, all within range.",
        ],
        Channel::AgentTurn => &["user: summarize the {j} channel from today", "assistant: The {j} channel was quiet today.", "user: what is on my calendar this afternoon"],
    };
    pick(rng, templates).replace("{j}", project).replace("{p}", person)
}

/// Generate the dataset of `(tier, seed)`.
pub fn generate(tier: Tier, seed: u64) -> Dataset {
    let spec = tier.spec();
    // Mix the tier into the stream so two tiers of one seed are independent.
    let mut rng = Rng::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (tier as u64 + 1));
    let principal = pick(&mut rng, PRINCIPALS).to_string();
    let mut artifacts: Vec<&str> = ARTIFACTS.to_vec();
    rng.shuffle(&mut artifacts);
    let n_items = spec.commitments + spec.loops;
    let loop_kinds = ["waiting_on_them", "reply_needed", "review_pending", "decision_pending"];

    let mut items: Vec<GoldItem> = Vec::with_capacity(n_items);
    let mut drafts: Vec<Draft> = Vec::new();
    let last_open_day = (spec.days - 3).max(1);
    let mut superseded = 0usize;
    let mut twice = 0usize;
    for i in 0..n_items {
        let kind = if i < spec.commitments { ItemKind::Commitment } else { ItemKind::OpenLoop };
        let subtype = match kind {
            ItemKind::Commitment => if rng.chance(0.6) { "mine" } else { "theirs" },
            ItemKind::OpenLoop => loop_kinds[(i - spec.commitments) % loop_kinds.len()],
        };
        let person = pick(&mut rng, PEOPLE).to_string();
        // Each item has its own artifact, so a resolution can only name one item.
        let artifact = artifacts[i % artifacts.len()].to_string();
        let project = pick(&mut rng, PROJECTS).to_string();
        let open_day = rng.below(last_open_day as u64) as i64;
        let open_ts = work_time(&mut rng, open_day);
        let channel = *rng.pick(&Channel::ALL).unwrap_or(&Channel::Email);
        let (due_ms, due_text) = if kind == ItemKind::Commitment {
            let due = start_of_day(open_ts) + (2 + rng.below(5) as i64) * DAY_MS;
            (Some(due), Some(due_phrase(&mut rng, open_ts, due)))
        } else {
            (None, None)
        };
        let paraphrase = spec.paraphrase && rng.chance(0.6);
        let cast = Cast { person: &person, artifact: &artifact, project: &project, principal: &principal };
        let (sentence, canonical, author) = opening(&mut rng, kind, subtype, &cast, due_text.as_deref(), paraphrase);
        let thread = format!("{}-{}", channel.as_str(), i);
        let content = wrap(&mut rng, channel, &person, &sentence, spec.wrapped);
        drafts.push(Draft { ts_ms: open_ts, channel, thread: thread.clone(), author, content, role: Role::Open { item: i, sentence: sentence.clone() }, extra_opens: Vec::new() });

        let mut item = GoldItem {
            id: format!("{}{:02}", if kind == ItemKind::Commitment { "c" } else { "l" }, i + 1),
            kind,
            subtype: subtype.to_string(),
            counterparty: person.clone(),
            artifact: artifact.clone(),
            text: canonical,
            opened_event: String::new(),
            quote: sentence,
            first_due_ms: due_ms,
            due_ms,
            superseded_by_events: Vec::new(),
            status: FinalStatus::Open,
            closed_event: None,
        };
        let mut cursor = open_ts;
        let other_channel = |rng: &mut Rng| if spec.mixed_channels { *rng.pick(&Channel::ALL).unwrap_or(&channel) } else { channel };
        // Supersession: move the due date later, once or twice.
        if kind == ItemKind::Commitment && superseded < spec.supersede {
            superseded += 1;
            let moves = if twice < spec.twice { twice += 1; 2 } else { 1 };
            for _ in 0..moves {
                let day = (cursor - EPOCH_MS) / DAY_MS + 1;
                if day >= spec.days {
                    break;
                }
                let ts = work_time(&mut rng, day);
                let new_due = item.due_ms.unwrap_or(ts) + (1 + rng.below(4) as i64) * DAY_MS;
                let phrase = due_phrase(&mut rng, ts, new_due);
                let ch = other_channel(&mut rng);
                let sentence = update(&mut rng, &artifact, &person, &phrase);
                let content = wrap(&mut rng, ch, &person, &sentence, spec.wrapped);
                drafts.push(Draft { ts_ms: ts, channel: ch, thread: thread.clone(), author: principal.clone(), content, role: Role::Update { item: i }, extra_opens: Vec::new() });
                item.due_ms = Some(new_due);
                cursor = ts;
            }
        }
        // Resolution or cancellation, after everything else about the item.
        let roll = rng.below(1_000) as f64 / 1_000.0;
        let close = if roll < spec.resolve { Some(false) } else if roll < spec.resolve + spec.cancel { Some(true) } else { None };
        if let Some(cancelled) = close {
            let day = (cursor - EPOCH_MS) / DAY_MS + 1 + rng.below(2) as i64;
            if day < spec.days {
                let ts = work_time(&mut rng, day);
                let ch = other_channel(&mut rng);
                let sentence = closing(&mut rng, kind, subtype, &person, &artifact, &project, cancelled);
                let content = wrap(&mut rng, ch, &person, &sentence, spec.wrapped);
                drafts.push(Draft { ts_ms: ts, channel: ch, thread: thread.clone(), author: principal.clone(), content, role: Role::Close { item: i }, extra_opens: Vec::new() });
                item.status = if cancelled { FinalStatus::Cancelled } else { FinalStatus::Resolved };
            }
        }
        items.push(item);
    }

    // Distractors: every kind, round robin, artifacts not used by any item.
    let kinds = [DistractorKind::Hypothetical, DistractorKind::ThirdParty, DistractorKind::Retracted, DistractorKind::AlreadyDone];
    let mut distractors = Vec::with_capacity(spec.distractors);
    for d in 0..spec.distractors {
        let kind = kinds[d % kinds.len()];
        let person = pick(&mut rng, PEOPLE).to_string();
        let other = pick(&mut rng, PEOPLE).to_string();
        let artifact = artifacts[(n_items + d) % artifacts.len()];
        let day = rng.below(spec.days as u64) as i64;
        let ts = work_time(&mut rng, day);
        let due = start_of_day(ts) + (2 + rng.below(4) as i64) * DAY_MS;
        let phrase = due_phrase(&mut rng, ts, due);
        let sentence = distractor(&mut rng, kind, &person, &other, artifact, &phrase);
        let channel = *rng.pick(&Channel::ALL).unwrap_or(&Channel::Chat);
        let content = wrap(&mut rng, channel, &person, &sentence, spec.wrapped);
        distractors.push(Distractor { id: format!("d{:02}", d + 1), kind, event: String::new(), span: sentence.clone() });
        drafts.push(Draft { ts_ms: ts, channel, thread: format!("{}-d{d}", channel.as_str()), author: person, content, role: Role::Distractor { index: d, sentence }, extra_opens: Vec::new() });
    }

    // Bundled meeting notes (hard): fold some same-day meeting openings into one note.
    if spec.bundled {
        bundle_meetings(&mut drafts);
    }

    // Filler.
    for _ in 0..(n_items * spec.filler_per_item) {
        let day = rng.below(spec.days as u64) as i64;
        let ts = work_time(&mut rng, day);
        let channel = *rng.pick(&Channel::ALL).unwrap_or(&Channel::Chat);
        let project = pick(&mut rng, PROJECTS);
        let person = pick(&mut rng, PEOPLE).to_string();
        let content = filler(&mut rng, channel, project, &person);
        drafts.push(Draft { ts_ms: ts, channel, thread: format!("{}-filler-{}", channel.as_str(), rng.below(6)), author: person, content, role: Role::Filler, extra_opens: Vec::new() });
    }

    // Order by time (ties by insertion order), give ids, and point the gold at them.
    let mut order: Vec<usize> = (0..drafts.len()).collect();
    order.sort_by_key(|&i| (drafts[i].ts_ms, i));
    let mut events = Vec::with_capacity(drafts.len());
    let mut seen_contents = std::collections::HashSet::new();
    for (n, &i) in order.iter().enumerate() {
        let draft = &drafts[i];
        let id = format!("e{:04}", n + 1);
        // Byte-identical filler collapses in any store; vary it so every event is distinct.
        let mut content = draft.content.clone();
        if matches!(draft.role, Role::Filler) && !seen_contents.insert(content.clone()) {
            content = format!("{content} ({})", day_label(draft.ts_ms));
            if !seen_contents.insert(content.clone()) {
                content = format!("{content} #{n}");
                seen_contents.insert(content.clone());
            }
        }
        match &draft.role {
            Role::Open { item, sentence } => {
                if let Some(gold) = items.get_mut(*item) {
                    gold.opened_event = id.clone();
                    gold.quote = sentence.clone();
                }
            }
            Role::Update { item } => {
                if let Some(gold) = items.get_mut(*item) {
                    gold.superseded_by_events.push(id.clone());
                }
            }
            Role::Close { item } => {
                if let Some(gold) = items.get_mut(*item) {
                    gold.closed_event = Some(id.clone());
                }
            }
            Role::Distractor { index, sentence } => {
                if let Some(d) = distractors.get_mut(*index) {
                    d.event = id.clone();
                    d.span = sentence.clone();
                }
            }
            Role::Filler => {}
        }
        for (item, sentence) in &draft.extra_opens {
            if let Some(gold) = items.get_mut(*item) {
                gold.opened_event = id.clone();
                gold.quote = sentence.clone();
            }
        }
        events.push(Event { id, ts_ms: draft.ts_ms, channel: draft.channel, thread: draft.thread.clone(), author: draft.author.clone(), content });
    }
    let end_ms = EPOCH_MS + spec.days * DAY_MS;
    Dataset {
        events,
        gold: Gold { benchmark: BENCHMARK.into(), version: VERSION.into(), tier, seed, principal, end_ms, items, distractors },
    }
}

/// Merge same-day meeting openings into the earliest one's note: one event then opens several
/// items, and each item's sentence stays an exact span of it.
fn bundle_meetings(drafts: &mut Vec<Draft>) {
    let mut by_day: std::collections::BTreeMap<i64, Vec<usize>> = std::collections::BTreeMap::new();
    for (i, d) in drafts.iter().enumerate() {
        if d.channel == Channel::Meeting && matches!(d.role, Role::Open { .. }) {
            by_day.entry(d.ts_ms.div_euclid(DAY_MS)).or_default().push(i);
        }
    }
    let mut remove = Vec::new();
    for group in by_day.values().filter(|g| g.len() > 1) {
        let mut group = group.clone();
        group.sort_by_key(|&i| drafts[i].ts_ms);
        let host = group[0];
        for &other in &group[1..] {
            if let Role::Open { item, sentence } = &drafts[other].role {
                let (item, sentence) = (*item, sentence.clone());
                let note = &mut drafts[host].content;
                note.push_str("\n- ");
                note.push_str(&sentence);
                drafts[host].extra_opens.push((item, sentence));
                remove.push(other);
            }
        }
    }
    remove.sort_unstable();
    for &i in remove.iter().rev() {
        drafts.remove(i);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn a_seed_reproduces_its_dataset_and_tiers_differ() {
        for tier in Tier::ALL {
            assert_eq!(generate(tier, 42), generate(tier, 42));
            assert_ne!(generate(tier, 42), generate(tier, 43));
        }
        assert_ne!(generate(Tier::Easy, 42).events, generate(Tier::Hard, 42).events);
    }

    #[test]
    fn every_gold_quote_is_an_exact_span_of_its_event_and_ids_resolve() {
        for tier in Tier::ALL {
            for seed in 42..47 {
                let data = generate(tier, seed);
                let event = |id: &str| data.events.iter().find(|e| e.id == id).unwrap_or_else(|| panic!("{tier:?}/{seed}: no event {id}"));
                for item in &data.gold.items {
                    assert!(event(&item.opened_event).content.contains(&item.quote), "{tier:?}/{seed} {}: {:?}", item.id, item.quote);
                    for e in item.superseded_by_events.iter().chain(item.closed_event.iter()) {
                        assert!(event(e).ts_ms >= event(&item.opened_event).ts_ms, "updates follow the opening");
                    }
                    if item.superseded_by_events.is_empty() {
                        assert_eq!(item.first_due_ms, item.due_ms);
                    } else {
                        assert!(item.due_ms > item.first_due_ms, "a supersession moves the date later");
                    }
                }
                for d in &data.gold.distractors {
                    assert!(event(&d.event).content.contains(&d.span));
                }
                assert!(data.events.windows(2).all(|w| w[0].ts_ms <= w[1].ts_ms), "time-ordered");
                assert!(data.events.iter().all(|e| e.ts_ms < data.gold.end_ms));
            }
        }
    }

    #[test]
    fn tiers_scale_as_specified() {
        let easy = generate(Tier::Easy, 7);
        let hard = generate(Tier::Hard, 7);
        assert_eq!(easy.gold.items.len(), 8);
        assert_eq!(hard.gold.items.len(), 24);
        assert!(hard.events.len() > 3 * easy.events.len());
        assert!(hard.gold.items.iter().filter(|i| !i.superseded_by_events.is_empty()).count() >= 4);
        assert!(easy.gold.items.iter().all(|i| i.superseded_by_events.is_empty()));
        assert_eq!(hard.gold.distractors.len(), 16);
    }
}
