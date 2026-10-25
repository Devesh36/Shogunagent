//! 届いた端からテキストを取り出すSSEデコーダ。
//!
//! [`super::anthropic::parse_sse_text`] はボディが揃ってから解析するので、最初の一文字が
//! 出るのは応答が終わったあと — 「初トークン1s」というSLOはそれでは測りようがない。この型は
//! 同じイベント形を、チャンクが行の途中で切れても落とさずに読む。
//!
//! 対応する形は3つ（[`SseFlavor`]）。行の切り出し・持ち越し・壊れた行の握り潰しは全フレーバで
//! 完全に同じで、違うのは「1つの `data:` JSONからテキストをどう取り出すか」だけ — なので
//! そこだけを分岐させ、面倒な側（carry）は1本に保つ。プロバイダごとにデコーダを丸ごと
//! 書くと、この carry のバグを2箇所で踏むことになる。
//!
//! ネットワークには触らない純ロジックなので、feature gate も要らずLinuxでテストできる。
//!
//! `data:` 行はそれぞれ独立した完結したJSONとして読む。SSE仕様が認める複数行 `data:` の
//! 連結は実装していない — プロバイダが実際に送ってくる形に合わせた意図的な割り切りで、
//! 既存の `parse_sse_text` も同じ読み方をしている。

use serde_json::{json, Value};

/// どのプロバイダのイベント形を読むか。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SseFlavor {
    /// Anthropic Messages API: `content_block_delta` の `delta.text_delta.text`。
    #[default]
    Anthropic,
    /// OpenAI互換 `chat/completions`: `choices[].delta.content`。
    /// OpenRouter / OpenAI / Gemini の互換面はすべてこの形。
    OpenAi,
    /// OpenAI Responses API（`POST /v1/responses`, issue #390）: `response.output_text.delta` の
    /// `delta`。usage と終了理由は `response.completed` / `response.incomplete` /
    /// `response.failed` が運ぶ `response` オブジェクトに載る。ツール呼び出しの組み立ては
    /// [`crate::llm::openai_responses::ResponsesStream`] が `push_events` の側で行う。
    OpenAiResponses,
}

impl SseFlavor {
    /// `data:` 1行ぶんのJSONから、このフレーバのテキストデルタを取り出す。
    ///
    /// 該当しないイベント（ping, message_start, tool 呼び出しのデルタ, usage-only の
    /// 最終チャンク…）では `None`。空文字も `None` に畳む — 呼び出し側は「返ってきた＝
    /// 画面に出す文字がある」とだけ考えればよくなる。
    fn text_of(self, v: &Value) -> Option<String> {
        match self {
            Self::Anthropic => {
                if v.get("type").and_then(Value::as_str)? != "content_block_delta" {
                    return None;
                }
                let delta = v.get("delta")?;
                if delta.get("type").and_then(Value::as_str)? != "text_delta" {
                    return None;
                }
                non_empty(delta.get("text")?.as_str()?)
            }
            // `choices` が空の配列で来るチャンクがある（プロンプトフィルタの結果など）ので、
            // 添字ではなく `first()` で取る。`content` が null のチャンク（finish_reason
            // だけを運ぶ最後の1つ）も同じ経路で素通りする。
            Self::OpenAi => {
                let choice = v.get("choices")?.as_array()?.first()?;
                non_empty(choice.get("delta")?.get("content")?.as_str()?)
            }
            Self::OpenAiResponses => {
                if v.get("type").and_then(Value::as_str)? != "response.output_text.delta" {
                    return None;
                }
                non_empty(v.get("delta")?.as_str()?)
            }
        }
    }
}

/// The HTTP status an Anthropic in-stream `error` event stands for, when it has one that keeps
/// its retry policy: overload 529, server error 500, rate limit 429. Shared by the plain decoder
/// and the tool-turn assembler, so the two cannot drift apart.
fn anthropic_error_status(v: &Value) -> Option<u16> {
    match v.get("error").and_then(|e| e.get("type")).and_then(Value::as_str) {
        Some("overloaded_error") => Some(529),
        Some("api_error") => Some(500),
        Some("rate_limit_error") => Some(429),
        _ => None,
    }
}

/// A stream that closed before the provider's end-of-answer marker (#895): a dropped connection,
/// so a transport error. Before the first token it is retried like any other transport failure,
/// and chat words it as a connection problem, not an unreadable answer.
pub fn closed_early(step: &str) -> crate::llm::LlmError {
    crate::llm::LlmError::Transport(crate::llm::transport::TransportError::Io(format!(
        "{step} stream closed before the answer finished"
    )))
}

fn non_empty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_string())
}

/// 到着順にSSEを読み、テキストデルタだけを吐くデコーダ。1本のストリームにつき1つ作る。
///
/// テキストのほかに **usage も拾う**（FR-HN-01 / #313）: Anthropic は `message_start` が
/// 入力側（input + cache read/write）、`message_delta` が出力側（累積 `output_tokens`）を運び、
/// OpenAI互換は `stream_options.include_usage` 指定時の最終チャンクに `usage` が載る。どちらも
/// テキストデルタと同じ `data:` 行を流れるので、ここで観測するのが唯一の取りこぼさない場所。
/// ストリーム終了後に [`SseDecoder::usage`] で読む。
#[derive(Debug, Default)]
pub struct SseDecoder {
    malformed: bool,
    /// まだ行として完成していない末尾。次のチャンクの頭と繋がる。
    pending: String,
    flavor: SseFlavor,
    /// ストリーム中に観測した usage。イベントごとに上書き/加算し、`usage()` で確定する。
    seen_usage: Option<crate::llm::usage::Usage>,
    /// プロバイダが告げた終了理由（Anthropic `message_delta.delta.stop_reason` /
    /// OpenAI互換 `choices[0].finish_reason`）。`max_tokens` / `length` は「答えが途中で
    /// 切れた」の合図で、非ストリーミング側 (`parse_messages_response`) が拒否するのと同じ
    /// 情報。ストリーミング側はここで拾い、呼び出し側が末尾に注意書きを足す。
    stop_reason: Option<String>,
    /// The provider reported an error inside the 200 stream (Anthropic `{"type":"error"}`, an
    /// OpenAI-compatible chunk carrying `error` or `finish_reason: "error"`). `Some(status)` when
    /// the error has an HTTP equivalent that keeps its retry policy, `Some(None)` otherwise.
    stream_error: Option<Option<u16>>,
    /// The provider's own end-of-answer marker arrived: Anthropic `message_stop`, the `[DONE]`
    /// sentinel, or a Responses terminal event. A stop or finish reason counts too (see
    /// [`Self::finished`]).
    ended: bool,
}

impl SseDecoder {
    /// Anthropic 形のデコーダ。
    pub fn new() -> Self {
        Self::default()
    }

    /// OpenAI互換形のデコーダ。
    pub fn openai() -> Self {
        Self { flavor: SseFlavor::OpenAi, ..Self::default() }
    }

    /// OpenAI Responses API 形のデコーダ（issue #390）。
    pub fn openai_responses() -> Self {
        Self { flavor: SseFlavor::OpenAiResponses, ..Self::default() }
    }

    /// プロバイダが告げた終了理由。まだ告げていなければ `None`。
    pub fn stop_reason(&self) -> Option<&str> {
        self.stop_reason.as_deref()
    }

    /// 出力トークン上限で答えが途中で切れたか。
    pub fn truncated(&self) -> bool {
        matches!(
            self.stop_reason(),
            Some("max_tokens") | Some("length") | Some("max_output_tokens")
        )
    }

    /// A documented successful-response policy stop. This deliberately uses only the protocol
    /// termination field; free-form generated text is never a safety signal.
    pub fn policy_stopped(&self) -> bool {
        matches!(self.stop_reason(), Some("refusal") | Some("content_filter"))
    }

    fn observe_stop_reason(&mut self, v: &Value) {
        let reason = match self.flavor {
            SseFlavor::Anthropic => (v.get("type").and_then(Value::as_str) == Some("message_delta"))
                .then(|| v.get("delta")?.get("stop_reason")?.as_str())
                .flatten(),
            SseFlavor::OpenAi => v
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|c| c.first())
                .and_then(|c| c.get("finish_reason"))
                .and_then(Value::as_str),
            // Responses: `response.incomplete` carries `incomplete_details.reason`
            // (`max_output_tokens` / `content_filter`); a refusal content part streams as
            // `response.refusal.delta` and is recorded as `refusal` so `policy_stopped` reads
            // it the same way as Anthropic's stop reason.
            SseFlavor::OpenAiResponses => match v.get("type").and_then(Value::as_str) {
                Some("response.incomplete") => v
                    .get("response")
                    .and_then(|r| r.get("incomplete_details"))
                    .and_then(|d| d.get("reason"))
                    .and_then(Value::as_str),
                Some("response.refusal.delta") => Some("refusal"),
                _ => None,
            },
        };
        if let Some(r) = reason {
            self.stop_reason = Some(r.to_string());
        }
    }

    /// ストリーム中に観測した usage。プロバイダが送らなかった場合は `None`（呼び出し側が
    /// バイト長推定へフォールバックする）。
    pub fn usage(&self) -> Option<crate::llm::usage::Usage> {
        self.seen_usage
    }

    /// Did the provider say the answer is over? A stop / finish reason, `message_stop`, the
    /// `[DONE]` sentinel or a Responses terminal event all do. A stream that closes without any
    /// of them was cut short, so what arrived is not the whole answer.
    pub fn finished(&self) -> bool {
        self.ended || self.stop_reason.is_some()
    }

    /// The error the provider reported inside the stream, as the error the same failure is over
    /// HTTP: overload, server error and rate limit keep their retry policy. Only the kind is
    /// kept, never the provider's message, which can quote the request. `None` when the stream
    /// carried no error.
    pub fn stream_error(&self, step: &str) -> Option<crate::llm::LlmError> {
        Some(match self.stream_error? {
            Some(status) => crate::llm::status_error(step, status, ""),
            None => crate::llm::LlmError::Provider(format!("{step} stream error")),
        })
    }

    /// Record an in-stream error or the end-of-answer marker from one `data:` object.
    fn observe_end(&mut self, v: &Value) {
        let kind = v.get("type").and_then(Value::as_str);
        match self.flavor {
            SseFlavor::Anthropic => match kind {
                Some("message_stop") => self.ended = true,
                Some("error") => self.stream_error = Some(anthropic_error_status(v)),
                _ => {}
            },
            SseFlavor::OpenAi => {
                if let Some(error) = v.get("error").filter(|e| !e.is_null()) {
                    let status = error
                        .get("code")
                        .and_then(Value::as_u64)
                        .and_then(|c| u16::try_from(c).ok())
                        .filter(|c| (400..600).contains(c));
                    self.stream_error = Some(status);
                } else if self.stop_reason.as_deref() == Some("error")
                    && self.stream_error.is_none()
                {
                    self.stream_error = Some(None);
                }
            }
            // `error` events and `response.failed` are read by `ResponsesStream::failure`.
            SseFlavor::OpenAiResponses => {
                if matches!(
                    kind,
                    Some("response.completed" | "response.incomplete" | "response.failed")
                ) {
                    self.ended = true;
                }
            }
        }
    }

    /// `data:` 1行のJSONから usage を観測する。テキスト抽出と独立: usage を運ぶイベントは
    /// テキストデルタを運ばないが、どちらも同じ行ストリームに混ざって届く。
    fn observe_usage(&mut self, v: &Value) {
        use crate::llm::usage::Usage;
        match self.flavor {
            SseFlavor::Anthropic => {
                // message_start: 入力側。message_delta: 累積 output_tokens。
                match v.get("type").and_then(Value::as_str) {
                    Some("message_start") => {
                        if let Some(u) = v.get("message").and_then(crate::llm::usage::anthropic_usage)
                        {
                            let acc = self.seen_usage.get_or_insert(Usage::default());
                            acc.input_tokens = u.input_tokens;
                            acc.cache_read_tokens = u.cache_read_tokens;
                            acc.cache_write_tokens = u.cache_write_tokens;
                        }
                    }
                    Some("message_delta") => {
                        if let Some(out) = v
                            .get("usage")
                            .and_then(|u| u.get("output_tokens"))
                            .and_then(Value::as_u64)
                        {
                            self.seen_usage.get_or_insert(Usage::default()).output_tokens = out;
                        }
                    }
                    _ => {}
                }
            }
            SseFlavor::OpenAi => {
                if let Some(u) = crate::llm::usage::openai_usage(v) {
                    self.seen_usage = Some(u);
                }
            }
            // The terminal events carry the whole `response` object, `usage` included.
            SseFlavor::OpenAiResponses => {
                if matches!(
                    v.get("type").and_then(Value::as_str),
                    Some("response.completed" | "response.incomplete" | "response.failed")
                ) {
                    if let Some(u) = v.get("response").and_then(crate::llm::usage::responses_usage) {
                        self.seen_usage = Some(u);
                    }
                }
            }
        }
    }

    /// ボディのチャンクを1つ食わせ、この時点で確定したテキストデルタを順番に返す。
    ///
    /// 行が完成していない部分は内部に持ち越す。チャンクが `"data: {\"ty"` で切れても、次の
    /// push で残りと連結してから解釈されるので、デルタは失われない。
    ///
    /// 呼び出し側は有効なUTF-8を渡すこと。ネットワークのバイト列をそのまま `&str` にすると
    /// マルチバイト文字がチャンク境界で割れるので、途中で切れた文字のバイトを次のチャンクへ
    /// 持ち越すのはトランスポート側の責任（`llm::transport` の carry バッファ）。ここは
    /// 「行が途中で切れる」ことだけを引き受ける。
    pub fn push(&mut self, chunk: &str) -> Vec<String> {
        let flavor = self.flavor;
        self.push_events(chunk)
            .iter()
            .filter_map(|v| flavor.text_of(v))
            .collect()
    }

    /// [`Self::push`] for callers that need the whole event, not just its text: the Agent tool
    /// lane (`llm::tool_turns`) rebuilds `tool_use` blocks from `content_block_start` /
    /// `input_json_delta` / `content_block_stop`, which carry no text at all. Same line carry,
    /// same usage and stop-reason observation; every parsed `data:` object comes out in order.
    pub fn push_events(&mut self, chunk: &str) -> Vec<Value> {
        self.pending.push_str(chunk);
        let mut out = Vec::new();

        // Last newline completes the lines; the rest waits for the next chunk.
        let Some(cut) = self.pending.rfind('\n') else {
            return out;
        };
        let complete: String = self.pending.drain(..=cut).collect();

        for line in complete.lines() {
            let line = line.trim_start();
            let Some(data) = line.strip_prefix("data:") else { continue };
            let data = data.trim();
            if data == "[DONE]" {
                self.ended = true;
                continue;
            }
            if data.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(data) else { self.malformed = true; continue };
            self.observe_usage(&v);
            self.observe_stop_reason(&v);
            self.observe_end(&v);
            out.push(v);
        }
        out
    }
}

/// Reconstructs a Messages `message` object from Anthropic SSE so a tool turn can stream
/// text and still hand the loop the JSON `parse_tool_turn` reads.
///
/// Thinking text and signatures are assembled for exact replay, without emitting text deltas.
/// Redacted thinking and other block types retain their initial fields verbatim. Tool arguments
/// and terminal state are validated without echoing content.
pub struct AnthropicSseMessage {
    decoder: SseDecoder,
    blocks: Vec<Option<OpenBlock>>,
    closed: Vec<bool>,
    failed: bool,
    stopped: bool,
    retryable_error_status: Option<u16>,
}

enum OpenBlock {
    Text(String),
    Tool { id: String, name: String, json: String, input: Value },
    Other(Value),
}

impl Default for AnthropicSseMessage {
    fn default() -> Self {
        Self::new()
    }
}

impl AnthropicSseMessage {
    pub fn new() -> Self {
        Self { decoder: SseDecoder::new(), blocks: Vec::new(), closed: Vec::new(), failed: false, stopped: false, retryable_error_status: None }
    }

    /// Feed one body chunk. Returns the same text deltas [`SseDecoder::push`] would, so the
    /// caller can paint them without a second decoder.
    pub fn push(&mut self, chunk: &str) -> Vec<String> {
        let flavor = self.decoder.flavor;
        let events = self.decoder.push_events(chunk);
        let mut text = Vec::new();
        for v in &events {
            // Validate before painting: a delta after `message_stop` or an `error`, or for a
            // block that is closed or never opened, is not part of the answer. Once the stream
            // has gone wrong nothing more is painted; `finish` refuses it either way.
            let was_failed = self.failed;
            if self.observe(v).is_none() {
                self.failed = true;
                continue;
            }
            if was_failed {
                continue;
            }
            if let Some(t) = flavor.text_of(v) {
                text.push(t);
            }
        }
        text
    }

    pub fn truncated(&self) -> bool {
        self.decoder.truncated()
    }

    /// Known transient SSE errors use the same retry policy as their HTTP equivalents.
    /// Never retain the provider's potentially sensitive error message.
    pub fn retryable_error_status(&self) -> Option<u16> {
        self.retryable_error_status
    }

    fn observe(&mut self, v: &Value) -> Option<()> {
        if self.stopped { return None; }
        match v.get("type").and_then(Value::as_str) {
            Some("error") => {
                self.retryable_error_status = anthropic_error_status(v);
                return None;
            }
            Some("message_stop") => self.stopped = true,
            Some("content_block_start") => {
                let index = usize::try_from(v.get("index")?.as_u64()?).ok()?;
                if index > 64 || self.blocks.get(index).is_some_and(Option::is_some) { return None; }
                let block = v.get("content_block")?;
                if index >= self.blocks.len() {
                    self.blocks.resize_with(index + 1, || None);
                    self.closed.resize(index + 1, false);
                }
                let kind = block.get("type")?.as_str()?;
                self.blocks[index] = Some(match kind {
                    "text" => OpenBlock::Text(
                        block.get("text")?.as_str()?.to_string(),
                    ),
                    "tool_use" => OpenBlock::Tool {
                        id: block.get("id")?.as_str()?.to_string(),
                        name: block.get("name")?.as_str()?.to_string(),
                        json: String::new(),
                        input: block.get("input")?.clone(),
                    },
                    _ => OpenBlock::Other(block.clone()),
                });
            }
            Some("content_block_delta") => {
                let index = usize::try_from(v.get("index")?.as_u64()?).ok()?;
                if *self.closed.get(index)? { return None; }
                let slot = self.blocks.get_mut(index)?.as_mut()?;
                let delta = v.get("delta")?;
                match slot {
                    OpenBlock::Text(text) => {
                        if delta.get("type")?.as_str()? != "text_delta" { return None; }
                        text.push_str(delta.get("text")?.as_str()?);
                    }
                    OpenBlock::Tool { json, .. } => {
                        if delta.get("type")?.as_str()? != "input_json_delta" { return None; }
                        json.push_str(delta.get("partial_json")?.as_str()?);
                    }
                    OpenBlock::Other(block) if block["type"] == "thinking" => {
                        let field = match delta.get("type")?.as_str()? {
                            "thinking_delta" => "thinking",
                            "signature_delta" => "signature",
                            _ => return None,
                        };
                        let value = block
                            .as_object_mut()?
                            .entry(field)
                            .or_insert_with(|| Value::String(String::new()));
                        let Value::String(value) = value else { return None; };
                        value.push_str(delta.get(field)?.as_str()?);
                    }
                    OpenBlock::Other(_) => {}
                }
            }
            Some("content_block_stop") => {
                let index = usize::try_from(v.get("index")?.as_u64()?).ok()?;
                self.blocks.get(index)?.as_ref()?;
                let closed = self.closed.get_mut(index)?;
                if *closed { return None; }
                *closed = true;
            }
            _ => {}
        }
        Some(())
    }

    /// The completed message: `stop_reason`, `content`, and `usage` when the stream sent it.
    pub fn finish(self) -> Result<Value, String> {
        let invalid = || "Incomplete or malformed tool response.".to_string();
        if self.failed || self.decoder.malformed || !self.stopped || self.closed.iter().any(|closed| !closed) || !self.decoder.pending.trim().is_empty() {
            return Err(invalid());
        }
        let stop_reason = self.decoder.stop_reason().ok_or_else(invalid)?;
        if !matches!(stop_reason, "end_turn" | "stop_sequence" | "tool_use" | "max_tokens") {
            return Err(invalid());
        }
        let content: Vec<Value> = self
            .blocks
            .into_iter()
            .map(|block| match block.ok_or_else(invalid)? {
                OpenBlock::Text(text) => Ok(json!({ "type": "text", "text": text })),
                OpenBlock::Tool { id, name, json, input } => {
                    let input = if json.is_empty() { input } else { serde_json::from_str(&json).map_err(|_| invalid())? };
                    if !input.is_object() || id.is_empty() || name.is_empty() { return Err(invalid()); }
                    Ok(json!({ "type": "tool_use", "id": id, "name": name, "input": input }))
                }
                OpenBlock::Other(v) => Ok(v),
            })
            .collect::<Result<_, String>>()?;
        let mut msg = json!({
            "stop_reason": stop_reason,
            "content": content,
        });
        if let Some(u) = self.decoder.usage() {
            msg["usage"] = json!({
                "input_tokens": u.input_tokens,
                "output_tokens": u.output_tokens,
                "cache_read_input_tokens": u.cache_read_tokens,
                "cache_creation_input_tokens": u.cache_write_tokens,
            });
        }
        Ok(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_stream_preserves_signed_blocks_without_painting_them() -> Result<(), String> {
        for initial_thinking in ["", "Initial "] {
            let events = [
                json!({"type":"content_block_start","index":0,"content_block":{
                    "type":"thinking","thinking":initial_thinking,"signature":"prefix-"}}),
                json!({"type":"content_block_delta","index":0,"delta":{
                    "type":"thinking_delta","thinking":"private "}}),
                json!({"type":"content_block_delta","index":0,"delta":{
                    "type":"thinking_delta","thinking":"reasoning"}}),
                json!({"type":"content_block_delta","index":0,"delta":{
                    "type":"signature_delta","signature":"signed-"}}),
                json!({"type":"content_block_delta","index":0,"delta":{
                    "type":"signature_delta","signature":"opaque"}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"content_block_start","index":1,"content_block":{
                    "type":"redacted_thinking","data":"opaque-redacted-data"}}),
                json!({"type":"content_block_stop","index":1}),
                json!({"type":"content_block_start","index":2,"content_block":{
                    "type":"tool_use","id":"call","name":"search_mail","input":{}}}),
                json!({"type":"content_block_stop","index":2}),
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}),
                json!({"type":"message_stop"}),
            ];
            let mut stream = AnthropicSseMessage::new();
            for event in events {
                assert!(stream.push(&format!("data: {event}\n\n")).is_empty());
            }
            let content = stream.finish()?["content"].clone();
            assert_eq!(content[0], json!({"type":"thinking",
                "thinking":format!("{initial_thinking}private reasoning"),
                "signature":"prefix-signed-opaque"}));
            assert_eq!(content[1], json!({"type":"redacted_thinking",
                "data":"opaque-redacted-data"}));
        }
        Ok(())
    }

    #[test]
    fn empty_thinking_keeps_its_signature_and_rejects_malformed_deltas() -> Result<(), String> {
        for signature in [json!("opaque signature"), json!(7), Value::Null] {
            let mut stream = AnthropicSseMessage::new();
            for event in [
                json!({"type":"content_block_start","index":0,"content_block":{
                    "type":"thinking","thinking":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{
                    "type":"thinking_delta","thinking":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{
                    "type":"signature_delta","signature":signature}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
                json!({"type":"message_stop"}),
            ] {
                assert!(stream.push(&format!("data: {event}\n\n")).is_empty());
            }
            if signature.is_string() {
                assert_eq!(stream.finish()?["content"][0], json!({"type":"thinking",
                    "thinking":"","signature":signature}));
            } else {
                assert_eq!(stream.finish(), Err("Incomplete or malformed tool response.".into()));
            }
        }
        Ok(())
    }

    #[test]
    fn tool_stream_requires_valid_terminal_and_object_arguments() -> Result<(), String> {
        let tool = "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"one\",\"name\":\"read\",\"input\":{}}}\n\n";
        let end = "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n";
        let close = "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n";
        let mut valid = AnthropicSseMessage::new();
        valid.push(&format!("{tool}{close}{end}"));
        assert_eq!(valid.finish()?["content"][0]["input"], json!({}));
        for invalid in [
            tool.to_string(),
            format!("{tool}{end}"),
            format!("{tool}{close}{close}{end}"),
            format!("{tool}{close}data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"{{}}\"}}}}\n\n{end}"),
            format!("{tool}data: {{\"type\":\"error\",\"error\":\"private\"}}\n\n{end}"),
            format!("{tool}data: invalid-private-json\n\n{end}"),
            format!("{tool}data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"[]\"}}}}\n\n{end}"),
            format!("{tool}data: {{\"type\":\"message_stop\"}}\n\n"),
        ] {
            let mut stream = AnthropicSseMessage::new();
            stream.push(&invalid);
            assert_eq!(stream.finish(), Err("Incomplete or malformed tool response.".into()));
        }
        Ok(())
    }

    const DELTA_A: &str =
        "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n";
    const DELTA_B: &str =
        "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}\n\n";

    /// 上限で切れた答えは、切れたことが分かる。非ストリーミング側は `max_tokens` を拒否するが、
    /// ストリーミング側は途中まで画面に出ているので、拒否ではなく「切れた」を伝える。
    #[test]
    fn a_max_tokens_stop_is_reported_as_truncated() {
        let mut d = SseDecoder::new();
        assert!(!d.truncated());
        d.push(DELTA_A);
        d.push("data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":1024}}\n\n");
        assert_eq!(d.stop_reason(), Some("max_tokens"));
        assert!(d.truncated());

        let mut d = SseDecoder::new();
        d.push("data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n");
        assert_eq!(d.stop_reason(), Some("end_turn"));
        assert!(!d.truncated());

        let mut d = SseDecoder::openai();
        d.push("data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n");
        assert!(d.truncated());

        let mut d = SseDecoder::new();
        d.push("data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"refusal\"}}\n\n");
        assert!(d.policy_stopped());

        let mut d = SseDecoder::openai();
        d.push("data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"content_filter\"}]}\n\n");
        assert!(d.policy_stopped());
    }

    #[test]
    fn a_complete_event_yields_its_text() {
        let mut d = SseDecoder::new();
        assert_eq!(d.push(DELTA_A), vec!["Hel".to_string()]);
    }

    #[test]
    fn several_events_in_one_chunk_all_come_out() {
        let mut d = SseDecoder::new();
        let both = format!("{DELTA_A}{DELTA_B}");
        assert_eq!(d.push(&both), vec!["Hel".to_string(), "lo".to_string()]);
    }

    #[test]
    fn an_event_split_across_chunks_is_not_lost() {
        let mut d = SseDecoder::new();
        let (head, tail) = DELTA_A.split_at(30);

        assert!(d.push(head).is_empty(), "行が完成する前に何か返している");
        assert_eq!(d.push(tail), vec!["Hel".to_string()]);
    }

    /// JSONは揃っているのに改行がまだ来ていないチャンク。行が完成するまで出さない、という
    /// 判断がここで効く（byte 30 の分割はJSONの途中なので、この経路を通らない）。
    #[test]
    fn a_complete_json_without_its_newline_waits() {
        let mut d = SseDecoder::new();
        let (head, tail) = DELTA_A.split_at(DELTA_A.len() - 2);

        assert!(d.push(head).is_empty(), "改行が来る前に出している");
        assert_eq!(d.push(tail), vec!["Hel".to_string()]);
    }

    /// text_delta 以外のイベント（ping, message_start, content_block_stop …）は無視する。
    #[test]
    fn non_text_events_are_ignored() {
        let mut d = SseDecoder::new();
        let noise = "event: ping\ndata: {\"type\":\"ping\"}\n\n\
                     data: {\"type\":\"message_start\",\"message\":{}}\n\n";
        assert!(d.push(noise).is_empty());
    }

    #[test]
    fn the_done_sentinel_is_ignored() {
        let mut d = SseDecoder::new();
        assert!(d.push("data: [DONE]\n\n").is_empty());
    }

    #[test]
    fn malformed_json_does_not_stop_the_stream() {
        let mut d = SseDecoder::new();
        assert!(d.push("data: {not json\n\n").is_empty());
        assert_eq!(d.push(DELTA_A), vec!["Hel".to_string()]);
    }

    #[test]
    fn the_incremental_result_matches_the_whole_body_parser() {
        let body = format!("{DELTA_A}{DELTA_B}data: [DONE]\n\n");
        let mut d = SseDecoder::new();
        let incremental: String = d.push(&body).concat();

        assert_eq!(incremental, super::super::anthropic::parse_sse_text(&body));
    }

    const OA_A: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n";
    const OA_B: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n";

    #[test]
    fn openai_deltas_come_out_in_order() {
        let mut d = SseDecoder::openai();
        assert_eq!(d.push(&format!("{OA_A}{OA_B}")), vec!["Hel".to_string(), "lo".to_string()]);
    }

    /// carry のロジックはフレーバ共通 — OpenAI形でも行の途中で切れたチャンクを落とさない。
    #[test]
    fn an_openai_event_split_across_chunks_is_not_lost() {
        let mut d = SseDecoder::openai();
        let (head, tail) = OA_A.split_at(20);

        assert!(d.push(head).is_empty(), "行が完成する前に何か返している");
        assert_eq!(d.push(tail), vec!["Hel".to_string()]);
    }

    /// 最後のチャンクは `content` が null で `finish_reason` だけを運ぶ。`role` だけの
    /// 最初のチャンクと、`choices` が空のチャンクも同様に何も出さない。
    #[test]
    fn openai_chunks_without_content_are_ignored() {
        let mut d = SseDecoder::openai();
        let noise = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n\
                     data: {\"choices\":[{\"delta\":{\"content\":null},\"finish_reason\":\"stop\"}]}\n\n\
                     data: {\"choices\":[],\"usage\":{\"total_tokens\":9}}\n\n\
                     data: [DONE]\n\n";
        assert!(d.push(noise).is_empty());
    }

    /// フレーバは取り違えられない: 相手方の形は無視されるだけで、例外にも空振りの
    /// 文字列にもならない。
    #[test]
    fn each_flavor_ignores_the_others_events() {
        assert!(SseDecoder::openai().push(DELTA_A).is_empty());
        assert!(SseDecoder::new().push(OA_A).is_empty());
    }

    /// Anthropic: message_start が入力側、message_delta が累積出力を運ぶ。両方観測して
    /// 1つの Usage に合成される。
    #[test]
    fn anthropic_usage_is_assembled_from_start_and_delta_events() {
        let mut d = SseDecoder::new();
        let stream = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":42,\
             \"cache_read_input_tokens\":30,\"cache_creation_input_tokens\":12}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}\n\n",
        );
        assert_eq!(d.push(stream), vec!["hi".to_string()]);
        let u = d.usage().expect("usage observed");
        assert_eq!(u.input_tokens, 42);
        assert_eq!(u.output_tokens, 7);
        assert_eq!(u.cache_read_tokens, 30);
        assert_eq!(u.cache_write_tokens, 12);
        assert!(!u.estimated);
    }

    /// message_delta は累積値なので、複数届いたら最後の値が残る。
    #[test]
    fn a_later_message_delta_overwrites_the_cumulative_output_count() {
        let mut d = SseDecoder::new();
        d.push("data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":3}}\n\n");
        d.push("data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}\n\n");
        assert_eq!(d.usage().unwrap().output_tokens, 9);
    }

    /// OpenAI互換: include_usage 指定時の最終チャンク（choices は空）から拾う。
    #[test]
    fn openai_usage_comes_from_the_final_chunk() {
        let mut d = SseDecoder::openai();
        let stream = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":5,\
             \"prompt_tokens_details\":{\"cached_tokens\":16}}}\n\n",
            "data: [DONE]\n\n",
        );
        assert_eq!(d.push(stream), vec!["hi".to_string()]);
        let u = d.usage().expect("usage observed");
        assert_eq!((u.input_tokens, u.output_tokens, u.cache_read_tokens), (20, 5, 16));
    }

    const RS_A: &str = "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\
                        \"output_index\":0,\"content_index\":0,\"delta\":\"Hel\"}\n\n";
    const RS_B: &str = "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\
                        \"output_index\":0,\"content_index\":0,\"delta\":\"lo\"}\n\n";

    #[test]
    fn responses_text_deltas_survive_a_split_across_chunks() {
        let mut d = SseDecoder::openai_responses();
        let both = format!("event: response.output_text.delta\n{RS_A}{RS_B}");
        let (head, tail) = both.split_at(57);
        let mut got = d.push(head);
        got.extend(d.push(tail));
        assert_eq!(got, vec!["Hel".to_string(), "lo".to_string()]);
        // The other flavors do not read this shape, and this one does not read theirs.
        assert!(SseDecoder::openai().push(RS_A).is_empty());
        assert!(SseDecoder::openai_responses().push(OA_A).is_empty());
        assert!(SseDecoder::openai_responses().push(DELTA_A).is_empty());
    }

    #[test]
    fn responses_usage_and_truncation_come_from_the_terminal_event() {
        let mut d = SseDecoder::openai_responses();
        d.push(RS_A);
        d.push(concat!(
            "data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp_1\",",
            "\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"},",
            "\"usage\":{\"input_tokens\":30,\"output_tokens\":12,",
            "\"input_tokens_details\":{\"cached_tokens\":16},",
            "\"output_tokens_details\":{\"reasoning_tokens\":4}}}}\n\n"
        ));
        assert!(d.truncated());
        let u = d.usage().expect("usage observed");
        assert_eq!((u.input_tokens, u.output_tokens, u.cache_read_tokens), (30, 12, 16));

        let mut d = SseDecoder::openai_responses();
        d.push("data: {\"type\":\"response.refusal.delta\",\"delta\":\"no\"}\n\n");
        assert!(d.policy_stopped());
    }

    /// usage を運ばないストリームでは None — 呼び出し側が推定にフォールバックする。
    #[test]
    fn a_stream_without_usage_yields_none() {
        let mut d = SseDecoder::new();
        d.push(DELTA_A);
        assert!(d.usage().is_none());
    }

    #[test]
    fn a_text_stream_assembles_the_message_the_loop_would_parse() -> Result<(), String> {
        let mut m = AnthropicSseMessage::new();
        assert_eq!(
            m.push(concat!(
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n",
            )),
            vec!["Hel".to_string()]
        );
        assert_eq!(
            m.push(concat!(
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}\n\n",
                "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
            )),
            vec!["lo".to_string()]
        );
        m.push("data: {\"type\":\"message_stop\"}\n\n");
        let msg = m.finish()?;
        assert_eq!(msg["stop_reason"], "end_turn");
        assert_eq!(msg["content"], json!([{ "type": "text", "text": "Hello" }]));
        assert_eq!(msg["usage"]["output_tokens"], 2);
        Ok(())
    }

    #[test]
    fn deltas_after_the_end_or_for_a_closed_block_are_never_painted() {
        const START: &str = "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n";
        const DELTA: &str = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"stray\"}}\n\n";
        const STOP: &str = "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n";

        // After message_stop.
        let mut m = AnthropicSseMessage::new();
        assert!(m.push(START).is_empty());
        m.push("data: {\"type\":\"message_stop\"}\n\n");
        assert!(m.push(DELTA).is_empty());
        assert!(m.finish().is_err());

        // For a closed block.
        let mut m = AnthropicSseMessage::new();
        m.push(START);
        m.push(STOP);
        assert!(m.push(DELTA).is_empty());

        // For a block that never opened.
        let mut m = AnthropicSseMessage::new();
        assert!(m.push(DELTA).is_empty());

        // After an in-stream error, even for a block that is still open.
        let mut m = AnthropicSseMessage::new();
        m.push(START);
        m.push("data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"x\"}}\n\n");
        assert!(m.push(DELTA).is_empty());
        assert_eq!(m.retryable_error_status(), Some(529));
    }

    #[test]
    fn a_tool_use_stream_reassembles_split_json_and_paints_no_text() -> Result<(), String> {
        let mut m = AnthropicSseMessage::new();
        let deltas = m.push(concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"list_calendar_events\",\"input\":{}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"day\\\":\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"tomorrow\\\"}\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
        ));
        assert!(deltas.is_empty(), "tool_use deltas must not paint as chat text: {deltas:?}");
        m.push("data: {\"type\":\"message_stop\"}\n\n");
        let msg = m.finish()?;
        assert_eq!(msg["stop_reason"], "tool_use");
        assert_eq!(
            msg["content"],
            json!([{
                "type": "tool_use",
                "id": "toolu_1",
                "name": "list_calendar_events",
                "input": { "day": "tomorrow" }
            }])
        );
        Ok(())
    }

    #[test]
    fn malformed_tool_json_fails_without_echoing_content() {
        let mut m = AnthropicSseMessage::new();
        m.push(concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"x\",\"input\":{}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"not-json\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
        ));
        m.push("data: {\"type\":\"message_stop\"}\n\n");
        assert_eq!(m.finish(), Err("Incomplete or malformed tool response.".into()));
    }

    // in-stream errors and the end-of-answer marker (#895)

    fn anthropic_error(kind: &str) -> String {
        format!("data: {{\"type\":\"error\",\"error\":{{\"type\":\"{kind}\",\"message\":\"PRIVATE quote of the request\"}}}}\n\n")
    }

    #[test]
    fn an_anthropic_error_event_is_its_http_equivalent_without_the_message() {
        use crate::llm::LlmError;
        let mut d = SseDecoder::new();
        d.push(DELTA_A);
        assert!(d.stream_error("messages").is_none());
        d.push(&anthropic_error("rate_limit_error"));
        assert!(matches!(
            d.stream_error("messages"),
            Some(LlmError::RateLimited(429, _))
        ));

        let mut d = SseDecoder::new();
        d.push(&anthropic_error("overloaded_error"));
        let e = d.stream_error("messages").unwrap();
        assert_eq!(e.to_string(), "provider error: messages HTTP 529");

        let mut d = SseDecoder::new();
        d.push(&anthropic_error("invalid_request_error"));
        let e = d.stream_error("messages").unwrap();
        assert_eq!(e.to_string(), "provider error: messages stream error");
        assert!(!e.to_string().contains("PRIVATE"));
    }

    #[test]
    fn an_anthropic_stream_is_finished_only_by_its_end_marker() {
        let mut d = SseDecoder::new();
        d.push(DELTA_A);
        d.push(DELTA_B);
        assert!(!d.finished(), "text alone is not the end of the answer");
        d.push("data: {\"type\":\"message_stop\"}\n\n");
        assert!(d.finished());

        let mut d = SseDecoder::new();
        d.push("data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n");
        assert!(
            d.finished(),
            "a stop reason means every text delta has arrived"
        );

        let mut d = SseDecoder::new();
        d.push("data: [DONE]\n\n");
        assert!(d.finished());
    }

    #[test]
    fn an_openai_error_chunk_or_error_finish_is_an_error() {
        use crate::llm::LlmError;
        let mut d = SseDecoder::openai();
        d.push(OA_A);
        assert!(d.stream_error("chat/completions").is_none());
        assert!(!d.finished());
        d.push("data: {\"error\":{\"code\":502,\"message\":\"upstream PRIVATE\"},\"choices\":[{\"delta\":{\"content\":\"\"},\"finish_reason\":\"error\"}]}\n\n");
        let e = d.stream_error("chat/completions").unwrap();
        assert_eq!(e.to_string(), "provider error: chat/completions HTTP 502");

        let mut d = SseDecoder::openai();
        d.push("data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"error\"}]}\n\n");
        assert!(matches!(
            d.stream_error("chat/completions"),
            Some(LlmError::Provider(_))
        ));

        let mut d = SseDecoder::openai();
        d.push(OA_A);
        d.push("data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n");
        assert!(d.finished() && d.stream_error("chat/completions").is_none());
    }

    #[test]
    fn a_responses_stream_is_finished_by_a_terminal_event() {
        let mut d = SseDecoder::openai_responses();
        d.push("data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n");
        assert!(!d.finished());
        d.push("data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n");
        assert!(d.finished());
    }
}
