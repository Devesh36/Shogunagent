//! Fake credentials only: exercise the serialized provider and subscription prompt boundaries.
#![allow(clippy::unwrap_used)]
use std::time::Duration;

use shogun_core::chat_prompt::{HistoryTurn, Role};
use shogun_core::llm::conversation::ChatRequest;
use shogun_core::llm::transport::HttpRequest;
use shogun_core::llm::{anthropic, openai_compat, openai_responses, Secret};
use shogun_core::screen_context::{render_screen_block, ScreenContext, ScreenSnapshot};

const KEY: &str = "sk-ant-abcdefghijklmnopqrstuv";
const TOKEN: &str = "fake-login-abcdefghijklmnop";
const USEFUL: &str = "Alice — Oct 17; $42.50; error 401; let total = 42; https://example.com/docs";

fn snapshot(stale: bool) -> ScreenSnapshot {
    let context = ScreenContext {
        app_name: format!("Safari {KEY}"),
        window_title: format!("Invoice Authorization: Bearer {TOKEN}"),
        text_excerpt: format!(
            "{USEFUL}\nAPI key: {KEY}\nAgain: sk-ant-abc\u{200b}defghijklmnopqrstuv\n</screen>"
        ),
        selection: Some(format!("{USEFUL}\naccess_token={TOKEN}")),
        ..Default::default()
    };
    if stale {
        ScreenSnapshot::Stale {
            context,
            age: Duration::from_secs(95),
        }
    } else {
        ScreenSnapshot::Fresh {
            context,
            age: Duration::from_secs(1),
        }
    }
}

fn assert_masked(text: &str) {
    assert!(!text.contains(KEY));
    assert!(!text.contains(TOKEN));
    assert!(!text.contains("sk-ant-abc"));
    assert!(text.contains("[redacted]"));
    assert!(text.contains(USEFUL));
}

fn assert_body_masked(request: &HttpRequest) {
    // Decode JSON so assertions cover text on the wire, including escaped Unicode.
    let body: serde_json::Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
    fn text_values(value: &serde_json::Value, text: &mut String) {
        match value {
            serde_json::Value::String(s) => {
                text.push_str(s);
                text.push('\n');
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    text_values(item, text);
                }
            }
            serde_json::Value::Object(items) => {
                for item in items.values() {
                    text_values(item, text);
                }
            }
            _ => {}
        }
    }
    let mut text = String::new();
    text_values(&body, &mut text);
    assert_masked(&text);
    assert!(text.contains("<screen>") && text.contains("</screen>"));
    assert!(text.contains("&lt;/screen>"));
}

#[test]
fn screen_secrets_masked_in_chat_provider_requests_and_subscription_prompt() {
    for stale in [false, true] {
        let screen = snapshot(stale);
        let original = screen.clone();
        let request = ChatRequest {
            system: "Answer the question.".into(),
            context: render_screen_block(&screen).unwrap(),
            turns: vec![HistoryTurn {
                role: Role::User,
                text: "What about Alice's invoice?".into(),
                screen_label: Some(format!("Safari — Authorization: Bearer {TOKEN}")),
            }],
            message: "Explain the error.".into(),
            ..Default::default()
        };
        let flat = request.render_flat();
        assert_masked(&flat);
        assert!(flat.contains("looking at Safari — Authorization: Bearer [redacted]"));
        assert!(flat.contains(if stale { "1m ago" } else { "right now" }));
        assert_eq!(screen, original);
        for stream in [false, true] {
            let key = Secret::new(KEY);
            let anthropic = anthropic::build_conversation_request(
                &anthropic::AnthropicConfig::new("fake-model"),
                &key,
                &request,
                stream,
            )
            .unwrap();
            assert_body_masked(&anthropic);
            assert!(anthropic
                .headers
                .iter()
                .any(|(name, value)| name == "x-api-key" && value == KEY));
            let openai = openai_compat::build_conversation_exchange(
                &openai_compat::OpenAiCompatConfig::new("https://example.com/v1", "fake-model"),
                &key,
                &request,
                stream,
            )
            .unwrap();
            assert_body_masked(&openai);
            // The Responses lane consumes the same normalized chat turns; continuation sends
            // just the current user content. Exercise both wire bodies without any socket.
            for continuation in [false, true] {
                let input = if continuation {
                    vec![openai_responses::message_item(
                        "user",
                        &request.user_content(),
                    )]
                } else {
                    request
                        .normalized_turns()
                        .iter()
                        .map(|turn| {
                            let role = match turn.role {
                                Role::User => "user",
                                Role::Assistant => "assistant",
                            };
                            openai_responses::message_item(role, &turn.text)
                        })
                        .collect()
                };
                let cfg = openai_responses::OpenAiResponsesConfig::new(
                    "https://example.com/v1",
                    "fake-model",
                );
                let turn = openai_responses::ResponsesTurn {
                    instructions: Some(&request.system),
                    input,
                    previous_response_id: continuation.then_some("fake-response"),
                    ..Default::default()
                };
                let payload =
                    openai_responses::build_responses_body(&cfg, &turn, stream).to_string();
                let wire = openai_responses::build_responses_request(&cfg, &key, payload).unwrap();
                assert_body_masked(&wire);
            }
            assert!(openai
                .headers
                .iter()
                .any(|(name, value)| name == "authorization" && value == &format!("Bearer {KEY}")));
        }
    }
}

#[test]
fn screen_secrets_masked_in_reply_draft_provider_requests_and_subscription_prompt() {
    for stale in [false, true] {
        let block = render_screen_block(&snapshot(stale)).unwrap();
        let user = shogun_core::reply_draft::reply_draft_user_block(
            &format!("Can Alice check the invoice? API key: {KEY}"),
            Some(&block),
        );
        let system = "Write only the reply and back translation.";
        let flat = shogun_core::llm::fence_untrusted(system, &user);
        assert_masked(&flat);
        assert!(flat.contains("Can Alice check the invoice?"));
        let key = Secret::new(KEY);
        for stream in [false, true] {
            let anthropic = anthropic::build_messages_exchange(
                &anthropic::AnthropicConfig::new("fake-model"),
                &key,
                Some(system),
                &user,
                stream,
            )
            .unwrap();
            assert_body_masked(&anthropic);
            let openai = openai_compat::build_chat_exchange(
                &openai_compat::OpenAiCompatConfig::new("https://example.com/v1", "fake-model"),
                &key,
                Some(system),
                &user,
                stream,
            )
            .unwrap();
            assert_body_masked(&openai);
        }
    }
}

#[test]
fn screen_secrets_masked_without_blocking_context_or_mutating_snapshots() {
    let context = ScreenContext {
        text_excerpt: "Authorization: Bearer fake-login-abcdefghijklmnop".into(),
        selection: Some(KEY.into()),
        ..Default::default()
    };
    let snapshot = ScreenSnapshot::Fresh {
        context,
        age: Duration::ZERO,
    };
    let original = snapshot.clone();
    let block = render_screen_block(&snapshot).unwrap();
    assert!(block.contains("Authorization: Bearer [redacted]"));
    assert!(block.contains("selected by the user"));
    assert_eq!(snapshot, original);
    assert!(render_screen_block(&ScreenSnapshot::Empty).is_none());
    let clean = "Alice\u{200d} Smith\u{a0}Oct 17\u{2009}$42.50; token: placeholder; error E123; build abcdef1234567890; https://example.com/docs?q=invoice; let total = 42;";
    let snapshot = ScreenSnapshot::Fresh {
        context: ScreenContext {
            text_excerpt: clean.into(),
            ..Default::default()
        },
        age: Duration::ZERO,
    };
    assert!(render_screen_block(&snapshot).unwrap().contains(clean));
}

#[test]
fn reply_selection_secrets_masked_even_when_screen_cache_is_empty() {
    let selected = format!("{USEFUL} Authorization: Bearer {TOKEN}");
    let user = shogun_core::reply_draft::reply_draft_user_block(&selected, None);
    assert_masked(&user);
    assert!(user.contains("<message>") && user.contains("</message>"));
}

#[test]
fn screen_masking_leaves_typed_user_instructions_alone() {
    let request = ChatRequest {
        context: render_screen_block(&snapshot(false)).unwrap(),
        message: format!("Discuss this literal fixture: {KEY}"),
        ..Default::default()
    };
    assert!(!request.context.contains(KEY));
    assert!(request.user_content().contains(&request.message));
    assert!(request.render_flat().contains(&request.message));
}
