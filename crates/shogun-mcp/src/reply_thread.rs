//! Reply source metadata shared by UI, tool proposals, MCP and REST.
use serde_json::{json, Value};
use shogun_agents::approval::ReplyThread;
use shogun_agents::permission::SendAction;

pub fn parse(action: &SendAction, input: &Value) -> Result<Option<ReplyThread>, &'static str> {
    let (thread_key, latest_key) = match action {
        SendAction::SendEmail { .. } => ("thread_id", "latest_message_id"),
        SendAction::PostMessage { .. } => ("thread_ts", "latest_reply_ts"),
        _ => return Ok(None),
    };
    if input.get(thread_key).is_none() && input.get(latest_key).is_none() {
        return Ok(None);
    }
    let field = |key| input.get(key).and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
        .map(str::to_string).ok_or("invalid_reply_thread");
    Ok(Some(ReplyThread { thread_id: field(thread_key)?, latest_message_id: field(latest_key)? }))
}

pub fn to_value(action: &SendAction, thread: &ReplyThread) -> Value {
    match action {
        SendAction::PostMessage { .. } => json!({"thread_ts": thread.thread_id, "latest_reply_ts": thread.latest_message_id}),
        _ => json!({"thread_id": thread.thread_id, "latest_message_id": thread.latest_message_id}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_round_trip_for_both_reply_providers_and_new_sends_stay_unmarked() {
        for (action, input) in [
            (SendAction::SendEmail { to: "a@b.com".into() }, json!({"thread_id":"t1", "latest_message_id":"m1"})),
            (SendAction::PostMessage { channel: "C0123ABCD".into() }, json!({"thread_ts":"123.001", "latest_reply_ts":"124.002"})),
        ] {
            let marker = parse(&action, &input).expect("test").expect("test");
            assert_eq!(to_value(&action, &marker), input);
            assert_eq!(parse(&action, &json!({})), Ok(None));
        }
    }

    #[test]
    fn partial_marker_is_refused() {
        let action = SendAction::SendEmail { to: "a@b.com".into() };
        assert!(parse(&action, &json!({"thread_id":"t1"})).is_err());
        assert!(parse(&action, &json!({"thread_id":"t1", "latest_message_id":""})).is_err());
    }
}
