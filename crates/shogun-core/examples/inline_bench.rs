//! Production-path Right Option benchmark: drives `compose_inline` itself (prompt, envelope
//! parse, echo guard) against a live model with synthetic text only. Never reads captured text or
//! the user's memory DB. Empty memory and no writing references, so production can differ.
//!
//! `cargo run -p shogun-core --features net --example inline_bench -- MODEL SAMPLES`
//! runs `SAMPLES` samples per case on Shogun's own BYOK Keychain entry (one paid request each,
//! two when the first comes back unchanged), printing one JSON line per sample, then one summary
//! line per case and a total. MODEL is an Anthropic id or `openai/<id>`, sent the way the desktop
//! sends ⌥-tap drafts; `MODEL@EFFORT` sets the reasoning effort instead (on Anthropic it also
//! keeps thinking on).
use shogun_core::context::ContextPack;
use shogun_core::inline::{
    compose_inline, CursorContext, CursorReader, InlineOutcome, InlineWriteAction,
    InlineWriteStatus, TextInserter,
};
use std::sync::Mutex;

/// `must_change`: an unchanged result is the bug. `may_stay`: unchanged is a fine answer.
struct Case {
    name: &'static str,
    app: &'static str,
    label: &'static str,
    text: &'static str,
    visible: &'static str,
    must_change: bool,
    /// Output containing this (lowercased) borrowed text from somewhere it does not belong.
    contamination: &'static str,
}

const fn case(
    name: &'static str,
    app: &'static str,
    text: &'static str,
    must_change: bool,
) -> Case {
    Case {
        name,
        app,
        label: "",
        text,
        visible: "",
        must_change,
        contamination: "",
    }
}

const CHROME: &str = "com.google.Chrome";
const SLACK: &str = "com.tinyspeck.slackmacgap";
const MAIL: &str = "com.apple.mail";

fn cases() -> Vec<Case> {
    vec![
        // Composition notes with nothing to reply to in view.
        case("note-reply-worked", CHROME, "reply to this email saying it worked", true),
        case("note-late", SLACK, "tell him ill be 10 min late", true),
        case("note-intro", MAIL, "say thanks for the intro and that ill follow up next week", true),
        case("note-invoice", MAIL, "write a follow up asking if they got the invoice", true),
        case("note-moved", SLACK, "let them know the meeting moved to 4pm", true),
        case("note-sounds-good", CHROME, "respond saying sounds good", true),
        case("note-professional", CHROME, "make this more professional: hey can u send the deck", true),
        Case {
            contamination: "translate to spanish",
            ..case("note-translate", CHROME, "translate to spanish: see you tomorrow", true)
        },
        // Composition notes with the thread in view.
        Case {
            visible: "From: Priya\nSubject: Staging login\nHey, can you check whether the staging login fix worked on your side?",
            label: "Message Body",
            ..case("note-reply-with-email", CHROME, "reply to this email saying it worked", true)
        },
        Case {
            visible: "Sam: are we still on for the design review thursday?\nSam: need to know by eod",
            label: "Message Sam",
            ..case("note-reply-with-thread", SLACK, "reply when you get a chance, yes thursday works", true)
        },
        // Empty composers: one with the thread in view, one with nothing to go on.
        Case {
            label: "Message Body",
            ..case("empty-no-context", MAIL, "", false)
        },
        Case {
            label: "Message Body",
            visible: "New Message\nTo: Priya Shah\nSubject: Staging login fix verified",
            ..case("empty-compose-subject", CHROME, "", true)
        },
        Case {
            visible: "Sam: are we still on for thursday?",
            label: "Message Sam",
            ..case("empty-reply-thread", SLACK, "", true)
        },
        // Rough drafts the user wants rewritten.
        case("rough-push", SLACK, "yeah i think we shud push it to tmrw bc auth is still broken", true),
        case("rough-thing", SLACK, "hey can u send me the thing from yesterday", true),
        case("rough-cant-make-it", SLACK, "i cant make it today sorry something came up can we do thurs", true),
        case("rough-build", SLACK, "build failing again idk why looking into it", true),
        case("rough-complaint", SLACK, "ugh staging is broken again and nobody is even looking at it", true),
        // Already fine: unchanged is a correct answer.
        case("good-thanks", SLACK, "sounds good, thanks!", false),
        case("good-standup", SLACK, "yep saw it, lets talk after standup", false),
        // Questions.
        case("q-recipient", MAIL, "can you reply by friday?", false),
        case("q-shogun-empty", SLACK, "what did i work on yesterday?", false),
        // Regression sentinel: unrelated visible question must not leak into the draft.
        Case {
            visible: "Jordan: also can you confirm the ship date?",
            contamination: "ship date",
            ..case("leak-deck", SLACK, "email me the deck tmrw pls", true)
        },
    ]
}

/// INLINE_BENCH_FILE=path.jsonl replaces the built-in cases with local ones, one JSON object per
/// line: `{"name", "app", "text", "visible"?, "label"?, "must_change"?}`. For private evaluation
/// data that must never be committed; it is only read here.
fn file_cases(path: &str) -> Result<Vec<Case>, Box<dyn std::error::Error>> {
    let leak = |v: &serde_json::Value| -> &'static str {
        Box::leak(v.as_str().unwrap_or_default().to_string().into_boxed_str())
    };
    let mut out = Vec::new();
    for line in std::fs::read_to_string(path)?.lines() {
        let v: serde_json::Value = serde_json::from_str(line)?;
        out.push(Case {
            name: leak(&v["name"]),
            app: leak(&v["app"]),
            label: leak(&v["label"]),
            text: leak(&v["text"]),
            visible: leak(&v["visible"]),
            must_change: v["must_change"].as_bool().unwrap_or(false),
            contamination: "",
        });
    }
    Ok(out)
}

struct Reader<'a>(&'a Case);
impl CursorReader for Reader<'_> {
    fn read(&self) -> Option<CursorContext> {
        Some(CursorContext {
            is_address_bar: false,
            app: self.0.app.into(),
            field_label: self.0.label.into(),
            before: self.0.text.into(),
            after: String::new(),
        })
    }
    fn visible_context(&self) -> String {
        self.0.visible.into()
    }
}

#[derive(Default)]
struct Field(Mutex<String>);
impl TextInserter for Field {
    fn write(&self, _: InlineWriteAction, text: &str) -> Result<InlineWriteStatus, String> {
        *self.0.lock().map_err(|_| "field lock")? = text.into();
        Ok(InlineWriteStatus::Applied)
    }
}

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Word-level similarity: 2·LCS / (|a| + |b|). 1.0 means the same words in the same order.
fn similarity(a: &str, b: &str) -> f64 {
    let (a, b) = (words(a), words(b));
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let mut row = vec![0usize; b.len() + 1];
    for x in &a {
        let mut prev = 0;
        for (j, y) in b.iter().enumerate() {
            let keep = row[j + 1];
            row[j + 1] = if x == y {
                prev + 1
            } else {
                row[j + 1].max(row[j])
            };
            prev = keep;
        }
    }
    2.0 * row[b.len()] as f64 / (a.len() + b.len()) as f64
}

const META: [&str; 10] = [
    "i don't have",
    "i do not have",
    "i can't see",
    "i cannot see",
    "not in view",
    "don't see",
    "no email",
    "could you share",
    "could you paste",
    "i'm unable",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [model, samples] = args.as_slice() else {
        return Err("Usage: inline_bench MODEL SAMPLES (requires --features net)".into());
    };
    let samples: usize = samples.parse()?;
    // INLINE_BENCH_ONLY=empty runs only the cases whose name contains it.
    let only = std::env::var("INLINE_BENCH_ONLY").unwrap_or_default();
    let mut cases = match std::env::var("INLINE_BENCH_FILE") {
        Ok(path) => file_cases(&path)?,
        Err(_) => cases(),
    };
    cases.retain(|c| c.name.contains(&only));
    let jobs: Vec<(usize, usize)> = (0..cases.len())
        .flat_map(|c| (0..samples).map(move |s| (c, s)))
        .collect();
    let next = Mutex::new(0usize);
    let rows = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..6 {
            scope.spawn(|| {
                while let Some(&(c, s)) = next.lock().ok().and_then(|mut n| {
                    *n += 1;
                    jobs.get(*n - 1)
                }) {
                    let row = run(model, &cases[c], s);
                    println!("{row}");
                    if let Ok(mut rows) = rows.lock() {
                        rows.push((c, row));
                    }
                }
            });
        }
    });
    let rows = rows.into_inner().map_err(|_| "rows lock")?;
    let mut total = serde_json::Map::new();
    for (index, case) in cases.iter().enumerate() {
        let mine: Vec<&serde_json::Value> = rows
            .iter()
            .filter(|(c, _)| *c == index)
            .map(|(_, r)| r)
            .collect();
        let count = |key: &str| mine.iter().filter(|r| r[key] == true).count();
        let mut latency: Vec<u64> = mine.iter().filter_map(|r| r["ms"].as_u64()).collect();
        latency.sort_unstable();
        let summary = serde_json::json!({
            "summary": case.name, "n": mine.len(), "bad": count("bad"),
            "echo": count("echo"), "near_echo": count("near_echo"), "failed": count("failed"),
            "meta": count("meta"), "leak": count("leak"),
            "recased": count("recased"), "padded": count("padded"),
            "retried": mine.iter().filter(|r| r["calls"].as_u64() > Some(1)).count(),
            "p50_ms": latency.get(latency.len() / 2),
        });
        println!("{summary}");
        for key in [
            "bad",
            "echo",
            "near_echo",
            "failed",
            "meta",
            "leak",
            "recased",
            "padded",
        ] {
            let sum = total
                .get(key)
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            total.insert(
                key.into(),
                (sum + summary[key].as_u64().unwrap_or(0)).into(),
            );
        }
    }
    let mut latency: Vec<u64> = rows.iter().filter_map(|(_, r)| r["ms"].as_u64()).collect();
    latency.sort_unstable();
    total.insert("n".into(), rows.len().into());
    total.insert(
        "p50_ms".into(),
        latency.get(latency.len() / 2).copied().into(),
    );
    total.insert(
        "p95_ms".into(),
        latency.get(latency.len() * 95 / 100).copied().into(),
    );
    println!("{}", serde_json::json!({ "total": total }));
    Ok(())
}

fn run(model: &str, case: &Case, sample: usize) -> serde_json::Value {
    let field = Field::default();
    let start = std::time::Instant::now();
    let (outcome, (mode, calls, tokens)) = match agent(model) {
        Ok(agent) => (
            compose_inline(&Reader(case), &agent, &field, &ContextPack::default(), ""),
            agent.mode(),
        ),
        Err(e) => (
            InlineOutcome::GenerationFailed(e.to_string()),
            Default::default(),
        ),
    };
    let ms = start.elapsed().as_millis() as u64;
    let written = field.0.lock().map(|t| t.clone()).unwrap_or_default();
    let applied = matches!(outcome, InlineOutcome::Applied { .. });
    let lower = written.to_lowercase();
    let echo = applied && words(&written) == words(case.text);
    let near_echo = applied && similarity(&written, case.text) >= 0.9;
    let meta = applied && META.iter().any(|m| lower.contains(m));
    let leak = applied && !case.contamination.is_empty() && lower.contains(case.contamination);
    let failed = !applied;
    // Drafts the user typed in lowercase should stay lowercase; an acknowledgment should not grow.
    let draft = case.name.starts_with("rough") || case.name.starts_with("good");
    let recased = applied
        && draft
        && !case.text.chars().any(char::is_uppercase)
        && written.chars().any(char::is_uppercase);
    let padded = applied
        && case.name.starts_with("good")
        && words(&written).len() > words(case.text).len() + 2;
    // Leaving the text as it was is a fine answer where unchanged is fine.
    let fail_ok = !case.must_change && outcome == InlineOutcome::Unchanged;
    let bad = meta
        || leak
        || recased
        || padded
        || (failed && !fail_ok)
        || (case.must_change && near_echo);
    serde_json::json!({
        "case": case.name, "sample": sample, "ms": ms, "mode": mode, "calls": calls,
        "input_tokens": tokens[0], "output_tokens": tokens[1],
        "outcome": format!("{outcome:?}").split(['(', ' ', '{']).next().unwrap_or(""),
        // Provider/diagnostic reason only (redacted by the client); synthetic text only here.
        "detail": match &outcome {
            InlineOutcome::KeyRejected(e) | InlineOutcome::GenerationFailed(e) => e.as_str(),
            _ => "",
        },
        "written": written, "echo": echo, "near_echo": near_echo, "meta": meta, "leak": leak,
        "recased": recased, "padded": padded, "failed": failed, "bad": bad,
    })
}

#[cfg(not(feature = "net"))]
fn agent(_: &str) -> Result<NoAgent, Box<dyn std::error::Error>> {
    Err("inline_bench requires --features net".into())
}

#[cfg(not(feature = "net"))]
struct NoAgent;
#[cfg(not(feature = "net"))]
impl NoAgent {
    fn mode(&self) -> (String, usize, [u64; 2]) {
        Default::default()
    }
}
#[cfg(not(feature = "net"))]
impl shogun_core::llm::AgentClient for NoAgent {
    fn complete(&self, _: &str) -> Result<String, shogun_core::llm::LlmError> {
        Err(shogun_core::llm::LlmError::NotConfigured)
    }
}

#[cfg(feature = "net")]
fn agent(model: &str) -> Result<Live, Box<dyn std::error::Error>> {
    use shogun_core::llm::anthropic::{AnthropicAgentClient, AnthropicConfig};
    use shogun_core::llm::openai_compat::{
        OpenAiCompatAgentClient, OpenAiCompatConfig, OPENAI_BASE_URL,
    };
    use shogun_core::llm::traceability::RecordingSink;
    use shogun_core::llm::transport::ReqwestTransport;
    use shogun_core::llm::{ByokKey, Secret};
    let (openai, model) = match model.strip_prefix("openai/") {
        Some(model) => (true, model),
        None => (false, model),
    };
    let account = if openai {
        "openai-byok"
    } else {
        "anthropic-byok"
    };
    let key = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "com.selectkk.shogun",
            "-a",
            account,
            "-w",
        ])
        .output()?;
    if !key.status.success() {
        return Err(format!("ShogunAI {account} key unavailable in Keychain").into());
    }
    let key = ByokKey::new(Secret::new(String::from_utf8(key.stdout)?.trim()));
    let tokens = std::sync::Arc::default();
    let transport = Meter(ReqwestTransport::new()?, std::sync::Arc::clone(&tokens));
    // Plain MODEL is the desktop's INLINE_DRAFT request shape: Anthropic with thinking off, or
    // OpenAI at the provider's default reasoning. MODEL@EFFORT sets that reasoning effort instead.
    let (model, effort) = match model.split_once('@') {
        Some((model, effort)) => (model, Some(effort)),
        None => (model, None),
    };
    let client = if openai {
        let config = OpenAiCompatConfig::new(OPENAI_BASE_URL, model).with_max_tokens(2048);
        let config = match effort {
            Some(effort) => config.with_reasoning_effort(effort),
            // The desktop keeps reasoning low on the short lanes for the reviewed GPT-6 models.
            None if model.starts_with("gpt-6-") => config.with_reasoning_effort("low"),
            None => config,
        };
        Client::OpenAi(OpenAiCompatAgentClient::new(
            transport,
            RecordingSink::new(),
            key,
            config,
        ))
    } else {
        let config = match effort {
            Some(effort) => AnthropicConfig::new(model).with_effort(effort),
            None => AnthropicConfig::new(model).with_quiet_thinking(),
        };
        Client::Anthropic(AnthropicAgentClient::new(
            transport,
            RecordingSink::new(),
            key,
            config.with_max_tokens(2048),
        ))
    };
    Ok(Live(
        Mutex::default(),
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?,
        client,
        tokens,
    ))
}

#[cfg(feature = "net")]
enum Client {
    Anthropic(
        shogun_core::llm::anthropic::AnthropicAgentClient<
            Meter,
            shogun_core::llm::traceability::RecordingSink,
        >,
    ),
    OpenAi(
        shogun_core::llm::openai_compat::OpenAiCompatAgentClient<
            Meter,
            shogun_core::llm::traceability::RecordingSink,
        >,
    ),
}

/// Sums input and output tokens (thinking included) from each response's usage; nothing else.
#[cfg(feature = "net")]
struct Meter(
    shogun_core::llm::transport::ReqwestTransport,
    std::sync::Arc<Mutex<[u64; 2]>>,
);
#[cfg(feature = "net")]
impl shogun_core::llm::transport::HttpTransport for Meter {
    async fn send(
        &self,
        request: shogun_core::llm::transport::HttpRequest,
    ) -> Result<
        shogun_core::llm::transport::HttpResponse,
        shogun_core::llm::transport::TransportError,
    > {
        let response = self.0.send(request).await?;
        let mut used = [0u64; 2];
        for line in response.body.lines() {
            let raw = line.strip_prefix("data: ").unwrap_or(line);
            let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
                continue;
            };
            let usage = value.pointer("/message/usage").or(value.get("usage"));
            // Anthropic names, then OpenAI's.
            for (slot, key) in [
                (0, "input_tokens"),
                (1, "output_tokens"),
                (0, "prompt_tokens"),
                (1, "completion_tokens"),
            ] {
                if let Some(n) = usage
                    .and_then(|u| u.get(key))
                    .and_then(serde_json::Value::as_u64)
                {
                    used[slot] = used[slot].max(n);
                }
            }
        }
        if let Ok(mut total) = self.1.lock() {
            total[0] += used[0];
            total[1] += used[1];
        }
        Ok(response)
    }
}

#[cfg(feature = "net")]
struct Live(
    Mutex<Vec<String>>,
    tokio::runtime::Runtime,
    Client,
    std::sync::Arc<Mutex<[u64; 2]>>,
);
#[cfg(feature = "net")]
impl Live {
    /// The MODE header of the last raw answer, how many requests were made, and their tokens.
    fn mode(&self) -> (String, usize, [u64; 2]) {
        let raws = self.0.lock().map(|r| r.clone()).unwrap_or_default();
        let mode = raws.last().and_then(|raw| {
            raw.lines().find_map(|l| {
                let l = l.replace(['*', '`'], "");
                l.trim().strip_prefix("MODE:").map(|m| m.trim().to_string())
            })
        });
        let tokens = self.3.lock().map(|t| *t).unwrap_or_default();
        (mode.unwrap_or_default(), raws.len(), tokens)
    }
}
#[cfg(feature = "net")]
impl shogun_core::llm::AgentClient for Live {
    fn complete(&self, prompt: &str) -> Result<String, shogun_core::llm::LlmError> {
        match &self.2 {
            Client::Anthropic(c) => self.1.block_on(c.complete(prompt)),
            Client::OpenAi(c) => self.1.block_on(c.complete(prompt)),
        }
    }
    fn complete_split(
        &self,
        system: &str,
        user: &str,
    ) -> Result<String, shogun_core::llm::LlmError> {
        let raw = match &self.2 {
            Client::Anthropic(c) => self.1.block_on(c.complete_split(system, user)),
            Client::OpenAi(c) => self.1.block_on(c.complete_split(system, user)),
        }?;
        if let Ok(mut raws) = self.0.lock() {
            raws.push(raw.clone());
        }
        Ok(raw)
    }
}
