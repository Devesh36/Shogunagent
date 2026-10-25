//! Explicit user attachments only. Bytes live in RAM, never in chat persistence or Debug output.
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;
pub const IMAGE_MARKER: &str = "[Image attached]\n\n";

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatImage {
    pub data_url: String,
}
impl std::fmt::Debug for ChatImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChatImage([redacted])")
    }
}
impl ChatImage {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.data_url.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4 + 32 {
            return Err("chat_image_size");
        }
        let (mime, encoded) = self.parts().ok_or("chat_image_type")?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| "chat_image_invalid")?;
        if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
            return Err("chat_image_size");
        }
        let valid = match mime {
            "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
            "image/jpeg" => {
                bytes.starts_with(&[0xff, 0xd8, 0xff]) && bytes.ends_with(&[0xff, 0xd9])
            }
            "image/webp" => bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"),
            _ => false,
        };
        if valid {
            Ok(())
        } else {
            Err("chat_image_invalid")
        }
    }
    fn parts(&self) -> Option<(&str, &str)> {
        let (mime, encoded) = self
            .data_url
            .strip_prefix("data:")?
            .split_once(";base64,")?;
        matches!(mime, "image/png" | "image/jpeg" | "image/webp").then_some((mime, encoded))
    }
    pub fn anthropic_block(&self) -> Value {
        let (mime, data) = self.parts().unwrap_or(("", ""));
        json!({"type":"image", "source":{"type":"base64", "media_type":mime, "data":data}})
    }
    pub fn compat_block(&self) -> Value {
        json!({"type":"image_url", "image_url":{"url":self.data_url}})
    }
    pub fn responses_block(&self) -> Value {
        json!({"type":"input_image", "image_url":self.data_url})
    }
}

/// Index into the request's unnormalized history; `turns.len()` denotes the new question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageTurn {
    pub index: usize,
    pub image: ChatImage,
}

/// Usage fallback only: base64 transport bytes are not text tokens. Provider usage wins.
pub fn estimated_input_bytes(payload: &str) -> usize {
    fn json_bytes(value: &Value) -> usize {
        match value {
            Value::Object(object)
                if matches!(
                    object.get("type").and_then(Value::as_str),
                    Some("image" | "image_url" | "input_image")
                ) =>
            {
                1568 * 4
            }
            Value::Object(object) => object.values().map(json_bytes).sum(),
            Value::Array(values) => values.iter().map(json_bytes).sum(),
            Value::String(text) => text.len(),
            _ => 0,
        }
    }
    if let Ok(value) = serde_json::from_str::<Value>(payload) {
        if payload.contains("\"image\"")
            || payload.contains("\"image_url\"")
            || payload.contains("\"input_image\"")
        {
            return json_bytes(&value);
        }
    }
    let mut rest = payload;
    let mut bytes = 0;
    while let Some(start) = rest.find("data:image/") {
        bytes += start;
        let image = &rest[start..];
        let Some(header) = image.find(";base64,") else {
            return bytes + image.len();
        };
        let encoded = &image[header + 8..];
        let end = encoded
            .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=')))
            .unwrap_or(encoded.len());
        bytes += 1568 * 4;
        rest = &encoded[end..];
    }
    bytes + rest.len()
}

/// Bounded, process-only sidecar keyed by the durable thread and message position.
#[derive(Default)]
pub struct SessionImages {
    entries: std::collections::VecDeque<(i64, i64, ChatImage)>,
}
impl SessionImages {
    pub fn insert(&mut self, thread: i64, pos: i64, image: ChatImage) {
        self.entries.retain(|(t, p, _)| (*t, *p) != (thread, pos));
        self.entries.push_back((thread, pos, image));
        while self
            .entries
            .iter()
            .map(|(_, _, image)| image.data_url.len())
            .sum::<usize>()
            > 20 * 1024 * 1024
        {
            self.entries.pop_front();
        }
    }
    pub fn get(&self, thread: i64, pos: i64) -> Option<ChatImage> {
        self.entries
            .iter()
            .find(|(t, p, _)| (*t, *p) == (thread, pos))
            .map(|(_, _, image)| image.clone())
    }
    pub fn delete_thread(&mut self, thread: i64) {
        self.entries.retain(|(t, _, _)| *t != thread);
    }
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn png() -> ChatImage {
        ChatImage {
            data_url: format!(
                "data:image/png;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(b"\x89PNG\r\n\x1a\nfixture")
            ),
        }
    }
    #[test]
    fn input_is_bounded_typed_and_redacted() {
        let image = png();
        assert!(image.validate().is_ok());
        assert!(!format!("{image:?}").contains(&image.data_url));
        for url in [
            "https://example.com/a.png",
            "data:image/svg+xml;base64,PHN2Zz4=",
            "data:image/png;base64,ZmFrZQ==",
            "data:image/png;base64,!",
        ] {
            assert!(ChatImage {
                data_url: url.into()
            }
            .validate()
            .is_err());
        }
        assert!(ChatImage {
            data_url: "A".repeat(MAX_IMAGE_BYTES * 2)
        }
        .validate()
        .is_err());
    }
    #[test]
    fn each_provider_gets_a_real_image_block() {
        let image = png();
        assert_eq!(image.anthropic_block()["source"]["media_type"], "image/png");
        assert_eq!(image.compat_block()["image_url"]["url"], image.data_url);
        assert_eq!(image.responses_block()["type"], "input_image");
    }
    #[test]
    fn cache_is_thread_scoped_bounded_and_deletable() {
        let mut images = SessionImages::default();
        images.insert(1, 7, png());
        assert!(images.get(2, 7).is_none());
        assert!(images.get(1, 8).is_none());
        assert!(images.get(1, 7).is_some());
        images.delete_thread(1);
        assert!(images.get(1, 7).is_none());
        for pos in 0..6 {
            images.insert(
                2,
                pos,
                ChatImage {
                    data_url: "x".repeat(4 * 1024 * 1024),
                },
            );
        }
        assert!(images.get(2, 0).is_none());
        assert!(images.get(2, 5).is_some());
        images.clear();
        assert!(images.get(2, 5).is_none());
    }
    #[test]
    fn image_stays_with_its_user_turn_when_failed_questions_are_merged() {
        use crate::chat_prompt::{HistoryTurn, Role};
        use crate::llm::conversation::ChatRequest;
        let request = ChatRequest {
            turns: vec![HistoryTurn {
                role: Role::User,
                text: "first".into(),
                screen_label: None,
            }],
            message: "retry".into(),
            images: vec![ImageTurn {
                index: 0,
                image: png(),
            }],
            ..Default::default()
        };
        let turns = request.normalized_turns();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].images.len(), 1);
        assert_eq!(turns[0].anthropic_content()[0]["type"], "image");
        assert_eq!(turns[0].compat_content()[1]["type"], "image_url");
        assert_eq!(
            turns[0].responses_item()["content"][1]["type"],
            "input_image"
        );
        assert!(request.untrusted_text().contains(&png().data_url));
        assert!(!request.render_flat().contains(&png().data_url));
    }
    #[test]
    fn plain_and_tool_builders_keep_image_blocks() {
        use crate::llm::conversation::ChatRequest;
        use crate::llm::{anthropic, openai_compat, Secret};
        let request = ChatRequest {
            message: "read this".into(),
            images: vec![ImageTurn {
                index: 0,
                image: png(),
            }],
            ..Default::default()
        };
        let key = Secret::new("fixture");
        let anthropic = anthropic::build_conversation_request(
            &anthropic::AnthropicConfig::new("fixture"),
            &key,
            &request,
            true,
        )
        .unwrap();
        let body: Value = serde_json::from_str(anthropic.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["messages"][0]["content"][0]["type"], "image");
        let compat = openai_compat::build_conversation_exchange(
            &openai_compat::OpenAiCompatConfig::new("https://example.com/v1", "fixture"),
            &key,
            &request,
            true,
        )
        .unwrap();
        let body: Value = serde_json::from_str(compat.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["messages"][0]["content"][1]["type"], "image_url");
        #[cfg(any(feature = "db", feature = "net"))]
        {
            let tool = crate::llm::tool_turns::ToolConversation::from_chat_request(&request);
            assert_eq!(tool.messages()[0]["content"][0]["type"], "image");
        }
    }

    #[test]
    fn usage_fallback_does_not_count_base64_as_text_tokens() {
        let image = png();
        let text = format!("question\n{}", image.data_url);
        assert_eq!(estimated_input_bytes(&text), 9 + 1568 * 4);
        for block in [
            image.anthropic_block(),
            image.compat_block(),
            image.responses_block(),
        ] {
            let body = json!({"content":[block]}).to_string();
            assert_eq!(estimated_input_bytes(&body), 1568 * 4);
        }
    }
}
