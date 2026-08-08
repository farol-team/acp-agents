//! What the agent says while a turn is running, in this crate's vocabulary.
//!
//! Every product that has consumed ACP directly ended up mapping
//! `session/update` itself, and each mapped a different subset — one dropped
//! thoughts, another dropped usage, a third invented a title for updates that
//! carried none and filled its UI with the word "tool". The union lives here;
//! what a product shows is still the product's to decide.

use std::sync::Arc;

use serde_json::Value;

use crate::connection::Connection;
use crate::wire::session_of;
use crate::{Error, Result};

/// Something the agent said, and which session it belongs to.
///
/// The session is what makes one dispatcher enough for an agent serving
/// several conversations: without it, a permission dialog for one channel gets
/// shown as though another had asked, and somebody authorises a call they were
/// never shown.
#[derive(Debug)]
pub struct Event {
    pub session: Option<String>,
    pub kind: EventKind,
}

#[derive(Debug)]
pub enum EventKind {
    /// The answer, in pieces.
    Text(String),
    /// Reasoning. Recorded, never pushed at anybody — it is what lets somebody
    /// reconstruct why a turn went the way it did.
    Thought(String),
    Tool {
        title: String,
        /// ACP ToolKind — `read`, `edit`, `execute`, `search`, `other`, …
        /// Empty when the agent sent none.
        kind: String,
        /// `pending`, `in_progress`, `completed`, `failed`, when it says.
        status: Option<String>,
        /// False for the call, true for a later change to one already
        /// announced. A product that renders the two the same way says
        /// everything twice.
        update: bool,
    },
    Plan(Vec<PlanEntry>),
    Usage {
        used: Option<u64>,
        size: Option<u64>,
        cost: Option<Cost>,
    },
    /// The session's knobs changed — the agent's own list, as it now stands.
    Config(Vec<SessionOption>),
    /// The agent is blocked until somebody answers this.
    Permission(PermissionRequest),
    /// An update kind this crate does not model. Surfaced rather than dropped:
    /// the protocol is young and a silent gap looks like a bug in the product.
    Other {
        kind: String,
    },
    /// The process ended, however it went. The last thing it said on stderr
    /// comes with it — that is usually the only explanation there is.
    Closed {
        diagnostics: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlanEntry {
    pub content: String,
    pub priority: Option<String>,
    pub status: Option<String>,
}

/// The protocol's own shape: an amount and an ISO 4217 code. Reading it as a
/// bare number is how a cost column stays empty, and inventing "USD" for a
/// missing currency is how two teams' totals become one wrong number.
#[derive(Debug, Clone, PartialEq)]
pub struct Cost {
    pub amount: f64,
    pub currency: Option<String>,
}

/// One selectable value of a [`SessionOption`].
#[derive(Debug, Clone, PartialEq)]
pub struct SessionChoice {
    pub value: String,
    pub name: String,
}

/// A session knob the agent advertises — model, reasoning effort, permission
/// mode. The set is the agent's, not ours, which is what makes a UI built on
/// it honest: nothing to hardcode, nothing to fall out of date.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionOption {
    pub id: String,
    pub name: String,
    /// ACP category (`model`, `thought_level`, `mode`…). More stable across
    /// agents than the id: Claude Code calls its effort knob `effort`, Codex
    /// calls it `reasoning_effort`, and both file it under `thought_level`.
    pub category: String,
    pub kind: String,
    pub current: String,
    pub choices: Vec<SessionChoice>,
}

/// The reply to `session/prompt` — the run's own summary. Discarding it is how
/// a turn that stopped at max_tokens gets recorded exactly like one that
/// finished.
#[derive(Debug, Clone, Default)]
pub struct TurnOutcome {
    pub stop_reason: Option<String>,
    pub usage: Option<Usage>,
    /// Everything else the agent sent back, untouched.
    pub raw: Value,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    pub total_tokens: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_read_tokens: Option<u64>,
    pub cached_write_tokens: Option<u64>,
}

/// One option the agent offered for a permission request, as it sent it.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionOption {
    /// The agent's own `optionId` — an opaque string. Kinds like `allow_once`
    /// are *not* valid ids, and answering with one answers nobody.
    pub id: String,
    pub name: String,
    /// `allow_once`, `allow_always`, `reject_once`, … when the agent says.
    pub kind: String,
}

/// The agent asking to do something, and waiting.
///
/// Nothing is decided here and nothing is auto-allowed: the options are the
/// agent's, carried as it sent them. A client that answers on somebody's
/// behalf has quietly moved the decision, and one that invents an option the
/// agent did not offer is answering a question it was not asked.
pub struct PermissionRequest {
    /// As the agent sent it, and as the answer must quote it back.
    pub id: Value,
    pub session: Option<String>,
    pub title: String,
    /// The command the agent means to run, when the tool call carries one.
    /// More precise than the title, which for a shell ask often *is* the
    /// command but is not obliged to be.
    pub command: Option<String>,
    pub tool_kind: String,
    pub options: Vec<PermissionOption>,
    pub(crate) conn: Arc<Connection>,
}

impl std::fmt::Debug for PermissionRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PermissionRequest")
            .field("id", &self.id)
            .field("session", &self.session)
            .field("title", &self.title)
            .field("tool_kind", &self.tool_kind)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl PermissionRequest {
    /// The option that means yes, if the agent offered one.
    pub fn allow_id(&self) -> Option<&str> {
        self.pick("allow")
    }

    /// The option that means no.
    pub fn reject_id(&self) -> Option<&str> {
        self.pick("reject")
    }

    fn pick(&self, prefix: &str) -> Option<&str> {
        self.options
            .iter()
            .find(|o| o.kind.starts_with(prefix))
            .map(|o| o.id.as_str())
    }

    /// Answer with one of the agent's own option ids.
    pub async fn answer(&self, option_id: &str) -> Result<()> {
        self.conn
            .respond(
                &self.id,
                serde_json::json!({ "outcome": { "outcome": "selected", "optionId": option_id } }),
            )
            .await
    }

    pub async fn allow(&self) -> Result<()> {
        let id = self
            .allow_id()
            .ok_or_else(|| Error::Protocol("the agent offered no option that allows".into()))?
            .to_string();
        self.answer(&id).await
    }

    pub async fn deny(&self) -> Result<()> {
        let id = self
            .reject_id()
            .ok_or_else(|| Error::Protocol("the agent offered no option that rejects".into()))?
            .to_string();
        self.answer(&id).await
    }

    /// Nobody chose, which the protocol calls cancelled. The agent finishes
    /// the turn with what it has instead of waiting forever.
    pub async fn cancel(&self) -> Result<()> {
        self.conn
            .respond(
                &self.id,
                serde_json::json!({ "outcome": { "outcome": "cancelled" } }),
            )
            .await
    }
}

pub(crate) fn permission_request(msg: &Value, conn: Arc<Connection>) -> Option<PermissionRequest> {
    let params = msg.get("params")?;
    let tool = params.get("toolCall");
    Some(PermissionRequest {
        id: msg.get("id")?.clone(),
        session: session_of(msg),
        title: tool
            .and_then(|t| t.get("title"))
            .and_then(Value::as_str)
            .unwrap_or("the agent is asking to do something")
            .to_string(),
        command: tool
            .and_then(|t| t.pointer("/rawInput/command"))
            .and_then(Value::as_str)
            .map(str::to_string),
        tool_kind: tool
            .and_then(|t| t.get("kind"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        options: params
            .get("options")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|o| {
                        let id = o.get("optionId")?.as_str()?.to_string();
                        Some(PermissionOption {
                            name: o
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or(&id)
                                .to_string(),
                            kind: o
                                .get("kind")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            id,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        conn,
    })
}

/// One `session/update` in this crate's vocabulary, or `None` for an update
/// that says nothing worth passing on.
pub(crate) fn map_update(params: &Value) -> Option<EventKind> {
    let update = params.get("update")?;
    let kind = update.get("sessionUpdate")?.as_str()?;
    match kind {
        "agent_message_chunk" => text_of(update).map(EventKind::Text),
        "agent_thought_chunk" => text_of(update).map(EventKind::Thought),
        "tool_call" | "tool_call_update" => {
            // Named by what it is, or not recorded. An id is not a name: a
            // step reading `call_00_hWMqa5NQ…` tells a colleague nothing and
            // crowds out the ones that do, and an update carries only what
            // changed — `title` is required on the call and optional here.
            let title = update
                .get("title")
                .and_then(Value::as_str)
                .or_else(|| update.get("kind").and_then(Value::as_str))?;
            Some(EventKind::Tool {
                update: kind == "tool_call_update",
                title: title.to_string(),
                kind: update
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                status: update
                    .get("status")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        }
        "plan" => {
            let entries = plan_entries(update);
            (!entries.is_empty()).then_some(EventKind::Plan(entries))
        }
        "usage_update" => Some(EventKind::Usage {
            used: update.get("used").and_then(Value::as_u64),
            size: update.get("size").and_then(Value::as_u64),
            cost: cost_of(update.get("cost")),
        }),
        "config_option_update" => {
            let options = session_options(update);
            (!options.is_empty()).then_some(EventKind::Config(options))
        }
        other => Some(EventKind::Other {
            kind: other.to_string(),
        }),
    }
}

fn text_of(update: &Value) -> Option<String> {
    update
        .pointer("/content/text")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

fn plan_entries(update: &Value) -> Vec<PlanEntry> {
    update
        .get("entries")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|e| {
                    Some(PlanEntry {
                        content: e.get("content")?.as_str()?.to_string(),
                        priority: e
                            .get("priority")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        status: e.get("status").and_then(Value::as_str).map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn cost_of(raw: Option<&Value>) -> Option<Cost> {
    let raw = raw?;
    if let Some(amount) = raw.as_f64() {
        return Some(Cost {
            amount,
            currency: None,
        });
    }
    Some(Cost {
        amount: raw.get("amount")?.as_f64()?,
        currency: raw
            .get("currency")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
            .map(str::to_string),
    })
}

/// The session knobs carried by a `session/new`, `session/load` or
/// `session/set_config_option` reply.
pub(crate) fn session_options(reply: &Value) -> Vec<SessionOption> {
    reply
        .get("configOptions")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|o| {
                    let id = o.get("id")?.as_str()?.to_string();
                    Some(SessionOption {
                        name: o
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or(&id)
                            .to_string(),
                        category: o
                            .get("category")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        kind: o
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        current: o
                            .get("currentValue")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        choices: o
                            .get("options")
                            .and_then(Value::as_array)
                            .map(|choices| {
                                choices
                                    .iter()
                                    .filter_map(|c| {
                                        let value = c.get("value")?.as_str()?.to_string();
                                        Some(SessionChoice {
                                            name: c
                                                .get("name")
                                                .and_then(Value::as_str)
                                                .unwrap_or(&value)
                                                .to_string(),
                                            value,
                                        })
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                        id,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn turn_outcome(reply: Value) -> TurnOutcome {
    let usage = reply.get("usage").map(|u| Usage {
        total_tokens: u.get("totalTokens").and_then(Value::as_u64),
        input_tokens: u.get("inputTokens").and_then(Value::as_u64),
        output_tokens: u.get("outputTokens").and_then(Value::as_u64),
        cached_read_tokens: u.get("cachedReadTokens").and_then(Value::as_u64),
        cached_write_tokens: u.get("cachedWriteTokens").and_then(Value::as_u64),
    });
    TurnOutcome {
        stop_reason: reply
            .get("stopReason")
            .and_then(Value::as_str)
            .map(str::to_string),
        usage,
        raw: reply,
    }
}

// ---------------------------------------------------------------------------
// The JSON a product hands to its own frontend
// ---------------------------------------------------------------------------
//
// Written out by hand rather than derived, and deliberately spelled the way
// the protocol spells it: a UI that already speaks ACP keeps its types, and
// nothing here bends the Rust API to suit one product's renderer.

impl Event {
    pub fn to_json(&self) -> Value {
        let mut value = self.kind.to_json();
        if let (Some(map), Some(session)) = (value.as_object_mut(), &self.session) {
            map.insert("session".into(), Value::String(session.clone()));
        }
        value
    }
}

impl EventKind {
    pub fn to_json(&self) -> Value {
        match self {
            EventKind::Text(text) => serde_json::json!({ "kind": "text", "text": text }),
            EventKind::Thought(text) => serde_json::json!({ "kind": "thought", "text": text }),
            EventKind::Tool {
                title,
                kind,
                status,
                update,
            } => serde_json::json!({
                "kind": "tool", "title": title, "toolKind": kind,
                "status": status, "update": update,
            }),
            EventKind::Plan(entries) => serde_json::json!({
                "kind": "plan",
                "entries": entries.iter().map(PlanEntry::to_json).collect::<Vec<_>>(),
            }),
            EventKind::Usage { used, size, cost } => serde_json::json!({
                "kind": "usage", "used": used, "size": size,
                "cost": cost.as_ref().map(Cost::to_json),
            }),
            EventKind::Config(options) => serde_json::json!({
                "kind": "config",
                "options": options.iter().map(SessionOption::to_json).collect::<Vec<_>>(),
            }),
            EventKind::Permission(request) => serde_json::json!({
                "kind": "permission", "request": request.to_json(),
            }),
            EventKind::Other { kind } => serde_json::json!({ "kind": "other", "label": kind }),
            EventKind::Closed { diagnostics } => serde_json::json!({
                "kind": "closed", "diagnostics": diagnostics,
            }),
        }
    }
}

impl PlanEntry {
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "content": self.content, "priority": self.priority, "status": self.status,
        })
    }
}

impl Cost {
    pub fn to_json(&self) -> Value {
        serde_json::json!({ "amount": self.amount, "currency": self.currency })
    }
}

impl SessionChoice {
    pub fn to_json(&self) -> Value {
        serde_json::json!({ "value": self.value, "name": self.name })
    }
}

impl SessionOption {
    /// Spelled as `session/new` spelled it, so a client that already reads the
    /// agent's own `configOptions` needs no second shape for ours.
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "id": self.id,
            "name": self.name,
            "category": self.category,
            "type": self.kind,
            "currentValue": self.current,
            "options": self.choices.iter().map(SessionChoice::to_json).collect::<Vec<_>>(),
        })
    }
}

impl PermissionRequest {
    /// Everything a person needs to answer it, and the id their answer must
    /// quote back. The connection behind it stays here — an answer goes
    /// through [`PermissionRequest::answer`], never through a frontend
    /// inventing a frame.
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "id": self.id,
            "session": self.session,
            "title": self.title,
            "command": self.command,
            "toolKind": self.tool_kind,
            "options": self.options.iter().map(|o| serde_json::json!({
                "optionId": o.id, "name": o.name, "kind": o.kind,
            })).collect::<Vec<_>>(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn update(v: Value) -> Option<EventKind> {
        map_update(&json!({ "update": v }))
    }

    #[test]
    fn the_answer_and_the_reasoning_are_different_things() {
        let Some(EventKind::Text(t)) = update(json!({
            "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "hi" }
        })) else {
            panic!("a message chunk is text")
        };
        assert_eq!(t, "hi");

        assert!(matches!(
            update(json!({
                "sessionUpdate": "agent_thought_chunk", "content": { "type": "text", "text": "hmm" }
            })),
            Some(EventKind::Thought(_))
        ));
    }

    #[test]
    fn an_empty_chunk_is_not_news() {
        assert!(update(json!({
            "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "" }
        }))
        .is_none());
    }

    #[test]
    fn an_untitled_tool_update_is_not_news_either() {
        // An update carries only what changed. Inventing a title produced a
        // stream of steps reading "tool" that said nothing.
        assert!(
            update(json!({ "sessionUpdate": "tool_call_update", "status": "completed" })).is_none()
        );

        let Some(EventKind::Tool {
            title,
            kind,
            status,
            update: is_update,
        }) = update(json!({
            "sessionUpdate": "tool_call", "title": "Read(src/main.rs)",
            "kind": "read", "status": "pending"
        }))
        else {
            panic!("a titled call is news")
        };
        assert_eq!(
            (title.as_str(), kind.as_str()),
            ("Read(src/main.rs)", "read")
        );
        assert_eq!(status.as_deref(), Some("pending"));
        assert!(!is_update, "this is the call itself");

        // The same title arriving again is a change to a step already shown,
        // not a second step — a product that renders both the same way says
        // everything twice.
        assert!(matches!(
            update(json!({ "sessionUpdate": "tool_call_update", "title": "Read(src/main.rs)" })),
            Some(EventKind::Tool { update: true, .. })
        ));
    }

    #[test]
    fn a_cost_is_an_amount_and_a_currency_not_a_number_we_hope_for() {
        let Some(EventKind::Usage { cost, used, .. }) = update(json!({
            "sessionUpdate": "usage_update", "used": 12, "size": 200_000,
            "cost": { "amount": 0.03, "currency": "USD" }
        })) else {
            panic!("usage is usage")
        };
        assert_eq!(used, Some(12));
        assert_eq!(
            cost,
            Some(Cost {
                amount: 0.03,
                currency: Some("USD".into())
            })
        );

        // A bare number is still an amount; a missing currency stays missing
        // rather than becoming somebody's dollars.
        let Some(EventKind::Usage { cost, .. }) =
            update(json!({ "sessionUpdate": "usage_update", "cost": 0.5 }))
        else {
            panic!("usage")
        };
        assert_eq!(
            cost,
            Some(Cost {
                amount: 0.5,
                currency: None
            })
        );
    }

    #[test]
    fn an_update_kind_we_do_not_model_surfaces_as_itself() {
        assert!(matches!(
            update(json!({ "sessionUpdate": "available_commands_update" })),
            Some(EventKind::Other { kind }) if kind == "available_commands_update"
        ));
    }

    #[test]
    fn session_options_carry_what_a_panel_needs_and_nothing_invented() {
        let options = session_options(&json!({ "configOptions": [
            { "id": "model", "name": "Model", "type": "select", "category": "model",
              "currentValue": "haiku",
              "options": [ { "value": "haiku", "name": "Haiku" }, { "value": "opus" } ] },
            { "id": "effort" }
        ] }));

        assert_eq!(options.len(), 2);
        assert_eq!(options[0].current, "haiku");
        assert_eq!(
            options[0].choices[1].name, "opus",
            "a choice with no label is its value"
        );
        assert_eq!(
            options[1].name, "effort",
            "an option with no name is its id"
        );
        assert_eq!(
            options[1].kind, "",
            "and nothing is invented for what it did not say"
        );
    }

    #[test]
    fn a_turn_reports_why_it_stopped() {
        let outcome = turn_outcome(json!({
            "stopReason": "max_tokens", "usage": { "totalTokens": 900, "inputTokens": 800 }
        }));
        assert_eq!(outcome.stop_reason.as_deref(), Some("max_tokens"));
        assert_eq!(outcome.usage.unwrap().total_tokens, Some(900));

        let quiet = turn_outcome(json!({}));
        assert!(quiet.stop_reason.is_none() && quiet.usage.is_none());
    }
}
