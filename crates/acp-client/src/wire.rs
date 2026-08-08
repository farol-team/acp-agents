//! The frames, and what an incoming line turns out to be.
//!
//! Pure functions over JSON: no process, no sockets, no clocks. Everything the
//! protocol gets wrong in practice is decided here, which is why it is the one
//! module that can be exhaustively tested.

use serde_json::{json, Value};

/// JSON-RPC 2.0 says an id is a String or a Number. We only ever send numbers,
/// but what comes back is whatever the agent chose to send, and an agent that
/// answers `3` with `"3"` is answering — so both spell the same key. What is
/// quoted back to the agent is never this: that is the value as it arrived.
pub fn id_key(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// An id worth routing on. `null` is legal JSON-RPC and means "no id".
fn routable(id: &Value) -> bool {
    id.is_string() || id.is_number()
}

/// What a line from the agent turns out to be, and the message itself.
#[derive(Debug, PartialEq)]
pub enum Inbound {
    /// An answer to a request we made. Carries the id as it arrived.
    Reply(Value, Value),
    /// The agent asking us something, and waiting. Carries the id its answer
    /// must quote — an unanswered request is a turn that never ends.
    Ask(Value, Value),
    /// The agent telling us something. Nothing is expected back.
    Notify(Value),
    /// JSON-RPC we cannot route: an id that is neither string nor number, or a
    /// frame with neither. Distinct from `Ignore` on purpose — this is the
    /// agent speaking a protocol we both claim to speak, and dropping it in
    /// the same silence as a log line is how a turn hangs with nothing to look
    /// at.
    Unroutable(Value),
    /// Neither. A log line on stdout is not a reason to drop the connection.
    Ignore,
}

/// ACP runs in both directions, so an id does not mean "answer". A message
/// carrying a method is the agent speaking even when it carries an id.
pub fn classify(line: &str) -> Inbound {
    let Ok(msg) = serde_json::from_str::<Value>(line) else {
        return Inbound::Ignore;
    };
    if !msg.is_object() {
        return Inbound::Ignore;
    }

    if msg.get("method").is_some() {
        return match msg.get("id") {
            None | Some(Value::Null) => Inbound::Notify(msg),
            Some(id) if routable(id) => Inbound::Ask(id.clone(), msg),
            // A question addressed by something we cannot quote back. The
            // agent is waiting on it and always will be; saying so beats
            // silence.
            Some(_) => Inbound::Unroutable(msg),
        };
    }

    match msg.get("id") {
        Some(id) if routable(id) => Inbound::Reply(id.clone(), msg),
        _ if msg.get("jsonrpc").is_some() => Inbound::Unroutable(msg),
        _ => Inbound::Ignore,
    }
}

/// One message, one line — the agent reads until the newline.
pub fn request_frame(id: u64, method: &str, params: Value) -> String {
    format!(
        "{}\n",
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    )
}

/// A message with no id, which is what the protocol calls a notification and
/// what makes it one: nothing is expected back. `session/cancel` is one.
pub fn notify_frame(method: &str, params: Value) -> String {
    format!(
        "{}\n",
        json!({ "jsonrpc": "2.0", "method": method, "params": params })
    )
}

/// An answer to something the agent asked. It quotes the id **as it arrived**
/// or it answers nobody: the agent is blocked on that exact token, and a
/// number we re-serialised is not the string it sent.
pub fn answer_frame(id: &Value, result: Value) -> String {
    format!(
        "{}\n",
        json!({ "jsonrpc": "2.0", "id": id, "result": result })
    )
}

/// A refusal to something the agent asked. Same rule about the id.
pub fn error_frame(id: &Value, code: i64, message: &str) -> String {
    format!(
        "{}\n",
        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
    )
}

/// Which session a message is about, when it says. `session/update` carries
/// it, and so does every request that belongs to one — so a turn can be
/// measured by its own silence rather than by another channel's chatter.
pub fn session_of(msg: &Value) -> Option<String> {
    msg.pointer("/params/sessionId")
        .or_else(|| msg.get("sessionId"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_goes_to_whoever_asked() {
        let Inbound::Reply(id, msg) = classify(r#"{"jsonrpc":"2.0","id":7,"result":{"ok":true}}"#)
        else {
            panic!("a result is a reply")
        };
        assert_eq!(id, 7);
        assert_eq!(msg["result"]["ok"], true, "the reply arrives intact");
    }

    #[test]
    fn an_error_is_still_a_reply() {
        assert!(matches!(
            classify(r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32601}}"#),
            Inbound::Reply(id, _) if id == 7
        ));
    }

    // JSON-RPC 2.0 says an id is a String or a Number. Everything this client
    // has spoken to numbers them, which is why reading ids as integers was
    // invisible: the first agent to use strings does not fail, it hangs.

    #[test]
    fn a_reply_under_a_string_id_still_reaches_whoever_asked() {
        let Inbound::Reply(id, msg) =
            classify(r#"{"jsonrpc":"2.0","id":"7","result":{"ok":true}}"#)
        else {
            panic!("a string id is an id")
        };
        assert_eq!(msg["result"]["ok"], true);
        assert_eq!(
            id_key(&id),
            id_key(&json!(7)),
            "an agent that answers 7 with \"7\" is answering, and the caller is filed under one key"
        );
    }

    #[test]
    fn the_agent_asking_us_something_is_never_a_reply() {
        // ACP is bidirectional, and agent ids are numbered from one exactly
        // like ours. Routed by id alone, the agent's first question is handed
        // to whoever awaits our first request.
        assert!(matches!(
            classify(r#"{"jsonrpc":"2.0","id":1,"method":"session/request_permission"}"#),
            Inbound::Ask(id, _) if id == 1
        ));
    }

    #[test]
    fn a_question_under_a_string_id_is_a_question() {
        let Inbound::Ask(id, _) = classify(
            r#"{"jsonrpc":"2.0","id":"perm-1","method":"session/request_permission","params":{}}"#,
        ) else {
            panic!("a method with a string id is a question, not an announcement")
        };
        assert_eq!(id, "perm-1");
    }

    #[test]
    fn an_answer_quotes_a_string_id_as_it_arrived() {
        let line = answer_frame(&json!("perm-1"), json!({ "outcome": "cancelled" }));
        let parsed: Value = serde_json::from_str(line.trim_end()).unwrap();

        assert_eq!(
            parsed["id"], "perm-1",
            "the agent is blocked on that exact token, not on our reading of it"
        );
    }

    #[test]
    fn a_notification_carries_no_id_or_it_is_a_question() {
        let line = notify_frame("session/cancel", json!({ "sessionId": "s1" }));
        let parsed: Value = serde_json::from_str(line.trim_end()).unwrap();

        assert_eq!(parsed["method"], "session/cancel");
        assert_eq!(parsed["params"]["sessionId"], "s1");
        assert!(
            parsed.get("id").is_none(),
            "an id makes it a request, and nothing is coming back"
        );
        assert!(line.ends_with('\n'));
    }

    #[test]
    fn json_rpc_we_cannot_route_is_not_a_log_line() {
        // Both are dropped, and only one of them should be quiet about it.
        assert!(matches!(
            classify(r#"{"jsonrpc":"2.0","id":{"seq":1},"method":"session/request_permission"}"#),
            Inbound::Unroutable(_)
        ));
        assert_eq!(classify("Listening on stdio..."), Inbound::Ignore);
        assert_eq!(classify("[]"), Inbound::Ignore, "an array is not a frame");
    }

    #[test]
    fn a_turn_is_measured_by_its_own_silence() {
        // Otherwise one busy channel keeps a stuck turn in another looking
        // alive, on an agent that serves every room a person has open.
        assert_eq!(
            session_of(&json!({ "params": { "sessionId": "s1", "update": {} } })),
            Some("s1".to_string())
        );
        assert_eq!(
            session_of(&json!({ "sessionId": "s2" })),
            Some("s2".to_string())
        );
        assert_eq!(session_of(&json!({ "params": { "cwd": "/tmp" } })), None);
    }
}
