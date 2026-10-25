//! A synthetic agent conversation for the `memory.ingest` → sessionize → extract path (plan
//! E5-T2). One thousand `turns[]` (user / assistant, one row each) with ten explicit first-person
//! promises planted at fixed, well-spread positions; everything else is filler drawn from fixed
//! templates that carry none of the local extractor's cue words. Deterministic from the seed and
//! the templates, like the golden days; people, projects and artifacts come from the fictional
//! persona pools ([`crate::templates::PERSONAS`]), so no real data can enter.
//!
//! The gate that consumes it lives in `crates/shogun-core/tests/extract_eval.rs` (it needs the
//! store): ingest the turns in `memory.ingest` requests, fold the window into sessions the way
//! the consolidation lane does, and require that the local-rule path recovers at least
//! [`MIN_RECOVERED`] of the [`PLANTED`] promises — a pipeline check, not a point of recall.

use crate::rng::SplitMix64;
use crate::templates::{PERSONAS, EN_WEEKDAYS};

/// Turns in one conversation.
pub const TURNS: usize = 1_000;
/// Promises planted in it.
pub const PLANTED: usize = 10;
/// How many of them the local-rule path must recover.
pub const MIN_RECOVERED: usize = 8;
/// Seconds between turns: a thousand turns span five and a half hours, so the review folds the
/// conversation into several hour sessions.
pub const TURN_GAP_MS: i64 = 20_000;
/// Most turns one `memory.ingest` request carries (`shogun_mcp::ingest_api::MAX_ITEMS`, spelled
/// here so this crate stays independent of the MCP crate).
pub const INGEST_CHUNK: usize = 200;

/// One turn as `memory.ingest` takes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    /// `user` or `assistant`.
    pub role: &'static str,
    pub content: String,
    pub ts_ms: i64,
}

/// A promise the generator planted: the turn it sits in and its exact sentence, which is what the
/// local extractor stores as the commitment's description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planted {
    pub turn: usize,
    pub sentence: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntheticConversation {
    pub seed: u64,
    /// The `thread_key` the whole conversation is ingested under.
    pub thread_key: String,
    pub turns: Vec<Turn>,
    pub planted: Vec<Planted>,
}

impl SyntheticConversation {
    /// The `memory.ingest` request body for `turns[start..end]` (at most [`INGEST_CHUNK`]), as the
    /// shared parser takes it: `turns[]` with role, content and ts_ms, the conversation's
    /// `thread_key`, and an idempotency key derived from the seed and the chunk.
    pub fn ingest_body(&self, start: usize, end: usize) -> serde_json::Value {
        let end = end.min(self.turns.len()).min(start + INGEST_CHUNK);
        let turns: Vec<serde_json::Value> = self.turns[start..end]
            .iter()
            .map(|turn| serde_json::json!({ "role": turn.role, "content": turn.content, "ts_ms": turn.ts_ms }))
            .collect();
        serde_json::json!({
            "turns": turns,
            "source": "extract-eval",
            "actor": "agent:extract-eval",
            "thread_key": self.thread_key,
            "idempotency_key": format!("conv-{}-{start}", self.seed),
        })
    }

    /// The chunk bounds for ingesting the whole conversation in order.
    pub fn chunks(&self) -> Vec<(usize, usize)> {
        (0..self.turns.len())
            .step_by(INGEST_CHUNK)
            .map(|start| (start, (start + INGEST_CHUNK).min(self.turns.len())))
            .collect()
    }
}

/// The user's promise templates: every one is an explicit first-person commitment the local
/// rules read as `mine` (`i'll `, `i will `, `i'm going to `). Slots: `{artifact}`, `{first}`,
/// `{project}`, `{weekday}`.
const PROMISES: &[&str] = &[
    "I'll send the {artifact} to {first} by {weekday}.",
    "I will share the {artifact} with the {project} team on {weekday}.",
    "I'm going to draft the {artifact} for {project} before {weekday}.",
    "I'll take care of the {artifact} handoff to {first} this week.",
    "I'll get back to {first} about the {project} schedule by {weekday}.",
    "I'll handle the {artifact} update for {project} on {weekday}.",
    "I will send {first} the revised {artifact} by {weekday} morning.",
    "I'll circle back with {first} on the {artifact} after {weekday}.",
    "I'm going to prepare the {artifact} for the {project} sync on {weekday}.",
    "I'll follow up with {first} on the {project} {artifact} by {weekday}.",
];

/// Filler the user types: no promise, no question that asks the other side for something, none
/// of the waiting / review / decision cue words.
const USER_FILLER: &[&str] = &[
    "Open the {artifact} for {project}.",
    "Show me what changed on {project} since the standup.",
    "Summarize the {channel} thread from this morning.",
    "The {artifact} draft reads fine to me.",
    "Add {first} to the {project} page.",
    "How many items are still open on {project}?",
    "List the people on {project}.",
    "Note that {first} owns the {artifact} now.",
    "Move the {project} notes to the top.",
    "Thanks, that covers {project} for today.",
];

/// Filler the assistant answers with: status, never a promise and never a request.
const ASSISTANT_FILLER: &[&str] = &[
    "Here is the current state of {project}: the {artifact} is in draft and {first} is listed as owner.",
    "Done. {project} now lists {first} on the page.",
    "The {channel} thread has three updates since this morning, none marked urgent.",
    "Noted: the {artifact} draft is marked fine.",
    "Four items remain open on {project}; two are assigned to {first}.",
    "The people on {project} are {first} and two others from the pool.",
    "Opened the {artifact} for {project}.",
    "Recorded: {first} owns the {artifact}.",
    "The {project} notes are at the top now.",
    "Understood. {project} is set for today.",
];

fn fill(template: &str, rng: &mut SplitMix64, persona: &crate::templates::PersonaDef) -> String {
    let person = rng.pick(persona.people).map(|p| p.short).unwrap_or("Oren");
    let project = rng.pick(persona.projects).copied().unwrap_or("Heron Rollout");
    let artifact = rng.pick(persona.artifacts).copied().unwrap_or("rollout plan");
    let channel = rng.pick(persona.channels).copied().unwrap_or("eng-platform");
    let weekday = rng.pick(&EN_WEEKDAYS).copied().unwrap_or("Friday");
    template
        .replace("{first}", person)
        .replace("{project}", project)
        .replace("{artifact}", artifact)
        .replace("{channel}", channel)
        .replace("{weekday}", weekday)
}

/// The turn indices the promises are planted at: evenly spread over the user turns (even
/// indices), so every hour of the conversation holds one or two.
fn planted_positions() -> Vec<usize> {
    (0..PLANTED).map(|k| {
        let position = (TURNS * (2 * k + 1)) / (2 * PLANTED);
        // The user speaks on even turns.
        position - position % 2
    }).collect()
}

/// Generate the conversation for `seed`. Always [`TURNS`] turns and [`PLANTED`] promises; the
/// filler and the slot values vary with the seed.
pub fn generate(seed: u64) -> SyntheticConversation {
    let mut rng = SplitMix64::new(seed);
    let persona = &PERSONAS[0];
    let positions = planted_positions();
    let mut turns = Vec::with_capacity(TURNS);
    let mut planted = Vec::with_capacity(PLANTED);
    // A fixed start instant: 2026-03-02 09:00 UTC.
    let start_ms: i64 = 1_772_442_000_000;
    for index in 0..TURNS {
        let ts_ms = start_ms + (index as i64) * TURN_GAP_MS;
        let user = index % 2 == 0;
        let content = if let Some(k) = positions.iter().position(|p| *p == index) {
            let sentence = fill(PROMISES[k % PROMISES.len()], &mut rng, persona);
            planted.push(Planted { turn: index, sentence: sentence.clone() });
            sentence
        } else if user {
            fill(rng.pick(USER_FILLER).copied().unwrap_or(USER_FILLER[0]), &mut rng, persona)
        } else {
            fill(rng.pick(ASSISTANT_FILLER).copied().unwrap_or(ASSISTANT_FILLER[0]), &mut rng, persona)
        };
        turns.push(Turn { role: if user { "user" } else { "assistant" }, content, ts_ms });
    }
    SyntheticConversation { seed, thread_key: format!("eval-conversation-{seed}"), turns, planted }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn the_conversation_is_deterministic_and_plants_ten_distinct_promises() {
        let a = generate(317);
        let b = generate(317);
        assert_eq!(a, b);
        assert_eq!(a.turns.len(), TURNS);
        assert_eq!(a.planted.len(), PLANTED);
        let mut sentences: Vec<&str> = a.planted.iter().map(|p| p.sentence.as_str()).collect();
        sentences.sort_unstable();
        sentences.dedup();
        assert_eq!(sentences.len(), PLANTED, "each promise is its own sentence");
        for p in &a.planted {
            assert_eq!(a.turns[p.turn].role, "user");
            assert_eq!(a.turns[p.turn].content, p.sentence);
        }
        assert_ne!(generate(318).turns, a.turns, "the seed matters");
    }

    #[test]
    fn filler_carries_none_of_the_local_cue_words_and_every_promise_is_one() {
        let conversation = generate(317);
        let planted: std::collections::HashSet<usize> = conversation.planted.iter().map(|p| p.turn).collect();
        for (index, turn) in conversation.turns.iter().enumerate() {
            let candidates = shogun_memory::extract::extract_untrusted(&turn.content);
            let promised = candidates.iter().any(|c| matches!(c, shogun_memory::extract::Candidate::Commitment { .. }));
            assert_eq!(promised, planted.contains(&index), "turn {index}: {}", turn.content);
            assert!(candidates.iter().all(|c| !matches!(c, shogun_memory::extract::Candidate::OpenLoop { .. })), "turn {index}");
        }
    }

    #[test]
    fn ingest_bodies_cover_the_conversation_in_bounded_chunks() {
        let conversation = generate(1);
        let chunks = conversation.chunks();
        assert_eq!(chunks.len(), TURNS.div_ceil(INGEST_CHUNK));
        let mut covered = 0;
        for (start, end) in chunks {
            let body = conversation.ingest_body(start, end);
            let turns = body["turns"].as_array().expect("turns");
            assert!(turns.len() <= INGEST_CHUNK);
            covered += turns.len();
            assert_eq!(body["thread_key"], conversation.thread_key);
        }
        assert_eq!(covered, TURNS);
    }
}
