//! Person and project pages: the model half (owner decision 2026-10-14).
//!
//! After an hourly review, up to four dirty pages (sixteen while more than ten wait,
//! `entity_pages::pages_per_run`; rows named in two or more sessions,
//! newest mention first, never one the user edited) are rewritten, one call each: the entity, its
//! current page and the new dated excerpts go in; a one-line summary and at most ten tagged claims
//! with the events they rest on come out. Selection, parsing, storage and the failure backoff are
//! `shogun_memory::entity_pages`; this file owns the instruction and the calls.
//!
//! Batch lane only (invariant 5): the relay's synchronous review, the Select KK key. Excerpts come
//! through the Batch source policy, are egress-redacted and fenced as data. Cost: at most sixteen
//! calls per hourly review while a backlog drains, four after, and [`DAILY_CALLS`] a day.

use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

use crate::daemon::Db;
use crate::llm::anthropic::{BatchItem, BatchLane};
use shogun_memory::entity_pages::{PageTask, BACKLOG_PAGES_PER_RUN};
use shogun_memory::state::StateTable;

/// The relay purpose: server-side shape `{"summary", "person_class"?, "project_class"?, "claims":[{text, basis, weight, event_ids}]}`.
pub const PAGE_PURPOSE: &str = "entity_page";

const PAGE_INSTRUCTION: &str = "\
    You keep one page of a personal memory up to date: what the user knows about one person or \
    project from their own work. The data names the entity (type, name, other names, its \
    class when known, related pages), shows its current page if there is one, and lists new evidence captured on \
    the user's screen as \"[event N] date app: text\" lines.\n\
    Rewrite the page from the current page and the new evidence. Return ONLY a JSON object (no \
    prose, no code fence): {\"summary\":\"one line of at most 140 characters: who or what this is \
    to the user\",\"person_class\":\"contact|public_figure|not_a_person (people only)\",\
    \"project_class\":\"user_project|media_or_public|not_a_project (projects only)\",\
    \"claims\":[{\"text\":\"one short fact\",\"basis\":\"user_action|user_said|on_screen|inferred\",\
    \"weight\":\"core|detail|noise\",\"event_ids\":[N]}]}\n\
    At most 10 claims. Each is one fact in plain words, the weakest wording the evidence supports, \
    dated and paraphrased in the third person (\"On 2026-10-14, Kai said he would send the deck\"). \
    Never copy message text: no quotations, no run of more than a few words taken from the \
    evidence. No handles, UI text or guesses. Address the user as you. Write a related page the \
    data lists by its name in double brackets, e.g. [[Ana Ito]].\n\
    basis: user_action = the user did it (sent, met, edited, reviewed, decided); user_said = the \
    user wrote or said it; on_screen = it was only shown to the user; inferred = your reading of \
    several lines together.\n\
    weight: core = defines what this is to the user; detail = a useful specific; noise = trivia.\n\
    event_ids: the N of every [event N] line that supports the claim; a current claim you keep \
    keeps its events. A claim without support is dropped.\n\
    Who said what: captured chats often lose who sent each message. Say that a person or the user \
    said, asked or did something only when the evidence shows the speaker (a name or handle heading \
    that message, or one of the user's own names the data lists). Otherwise word it neutrally \
    (\"Your Discord chat with Kai was about the Friday demo\") with basis on_screen. Never guess \
    who asked whom.\n\
    Keep current claims that still hold, merge repeats, and replace what newer evidence \
    contradicts. person_class: contact = someone the user deals with; public_figure = someone the \
    user only reads about or watches; not_a_person = a bot, brand, account or group. project_class: \
    user_project = work the user does or takes part in; media_or_public = a show, channel, game, \
    film, public product or topic the user only watches or reads about; not_a_project = a task \
    label, feature or to-do (\"Fix …\", \"Translate flip card\"), pull request or issue title, repository path, \
    document, or a single common word or generic label (\"dream\", \"portfolio\", \"general-purpose\") \
    the evidence does not show as the name of one specific product, repository or effort.";

/// Rewrite calls one UTC day may make.
/// ponytail: process-local, so a relaunch resets it; persist it if relaunches ever matter for cost.
pub const DAILY_CALLS: u32 = 200;
static CALL_DAY: AtomicI64 = AtomicI64::new(-1);
static CALLS_USED: AtomicU32 = AtomicU32::new(0);

/// Take one call from today's budget; `false` once [`DAILY_CALLS`] are spent.
fn take_call(now_ms: i64) -> bool {
    let day = now_ms.div_euclid(86_400_000);
    if CALL_DAY.swap(day, Ordering::SeqCst) != day {
        CALLS_USED.store(0, Ordering::SeqCst);
    }
    CALLS_USED.fetch_add(1, Ordering::SeqCst) < DAILY_CALLS
}

/// One review item for `task`. The instruction is shared by every page (a cache prefix); the data
/// is the task's text, egress-redacted and fenced.
pub fn page_item(task: &PageTask, owner_names: &[String]) -> BatchItem {
    static SYSTEM: OnceLock<Arc<str>> = OnceLock::new();
    let system = SYSTEM.get_or_init(|| crate::llm::fence_instruction(PAGE_INSTRUCTION).into()).clone();
    BatchItem {
        custom_id: format!("{}-{}", if task.table == StateTable::People { "person" } else { "project" }, task.id),
        purpose: PAGE_PURPOSE.to_string(),
        chunk: crate::llm::fence_data(&shogun_redact::redact_egress(&with_owner(task.data(), owner_names))),
        system: Some(system),
    }
}

/// The task's data, headed by the names the user goes by when known.
pub(crate) fn with_owner(data: String, owner_names: &[String]) -> String {
    if owner_names.is_empty() {
        return data;
    }
    format!("The user goes by: {}\n{data}", owner_names.join(", "))
}

/// Rewrite the dirty pages due now. A page with no evidence the lane may see is not sent; it is
/// left clean until a new mention. A failed call counts against its page and ends the run:
/// the relay is down or capped, and the next review tries again. Returns the pages written.
pub async fn rewrite_pages<B: BatchLane>(db: &Db, lane: &B) -> usize {
    let mut written = 0;
    let owner = db.owner_names();
    for task in db.entity_page_tasks(BACKLOG_PAGES_PER_RUN) {
        if task.excerpts.is_empty() {
            db.skip_entity_page(&task);
            continue;
        }
        if !take_call(db.now_ms()) {
            break;
        }
        match lane.review(&[page_item(&task, &owner)]).await {
            Ok(results) => {
                let answer = results.first().and_then(|r| r.text.as_deref());
                if db.apply_entity_page(&task, answer) {
                    written += 1;
                }
            }
            Err(_) => {
                db.apply_entity_page(&task, None);
                break;
            }
        }
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_day_allows_its_budget_and_the_next_day_starts_over() {
        let day = 400 * 86_400_000;
        assert_eq!((0..DAILY_CALLS + 20).filter(|_| take_call(day)).count(), DAILY_CALLS as usize);
        assert!(take_call(day + 86_400_000));
    }

    #[test]
    fn a_page_item_carries_the_entity_as_fenced_data_under_the_shared_instruction() {
        let task = PageTask {
            table: StateTable::People,
            id: 7,
            name: "Dana Reyes".into(),
            aliases: vec!["Dana".into()],
            class: Some("contact".into()),
            description: String::new(),
            page: None,
            excerpts: vec![shogun_memory::entity_pages::Excerpt { event_id: 42, ts: 0, app: Some("Slack".into()), text: "Dana: can you review the deck?".into() }],
            citable: [(42, 0)].into_iter().collect(),
            related: vec![],
            seen: 1,
        };
        let item = page_item(&task, &["Sam Lee".to_string(), "sam_l".to_string()]);
        assert_eq!((item.custom_id.as_str(), item.purpose.as_str()), ("person-7", PAGE_PURPOSE));
        assert!(item.chunk.contains("[event 42]") && item.chunk.contains("Dana Reyes"));
        assert!(item.chunk.contains("The user goes by: Sam Lee, sam_l"), "the user's names let the model tell who spoke");
        let system = item.system.as_deref().unwrap_or_default();
        assert!(system.contains("event_ids") && !system.contains("Dana"), "the instruction holds no captured text");
        assert!(system.len() < 4 * 1024, "{} bytes", system.len());
    }
}
