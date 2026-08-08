//! A client for the **Agent Client Protocol**: spawn a coding agent that runs
//! on this machine, under this person's own credentials, and speak JSON-RPC
//! 2.0 to it over stdio — one message per line.
//!
//! ```text
//! → {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,…}}
//! → {"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"…","mcpServers":[]}}
//! → {"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":"…","prompt":[…]}}
//! ← {"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk",…}}}
//! ← {"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}
//! ```
//!
//! ## What this crate decides, and what it does not
//!
//! It decides how the wire works: which id spellings are the same id, when a
//! silent agent has stopped, that a replayed history is not news, that a
//! timed-out turn's tail must not be attributed to the next one, and that the
//! whole process group goes when the agent does.
//!
//! It decides nothing about your product. Permission requests arrive as
//! events; whether a person is asked, a rule decides, or the answer is always
//! no is yours ([`PermissionPolicy`]). Which agent to run, and where it lives
//! on the machine, is the `acp-agents` crate's question.
//!
//! ## Getting started
//!
//! ```no_run
//! # async fn run() -> Result<(), acp_client::Error> {
//! use acp_client::{Agent, Config, Event, EventKind, SessionOpts};
//!
//! let (tx, mut events) = tokio::sync::mpsc::channel::<Event>(64);
//! let agent = Agent::launch(Config::new("claude-agent-acp").cwd("/tmp"), tx).await?;
//!
//! tokio::spawn(async move {
//!     while let Some(event) = events.recv().await {
//!         if let EventKind::Text(text) = event.kind {
//!             print!("{text}");
//!         }
//!     }
//! });
//!
//! let session = agent.new_session(SessionOpts::default().cwd("/tmp")).await?;
//! let outcome = session.prompt("what changed in this repo today?").await?;
//! println!("\nstopped because: {:?}", outcome.stop_reason);
//! # Ok(()) }
//! ```

mod agent;
mod connection;
mod event;
pub mod orphans;
mod session;
pub mod wire;

use std::time::Duration;

pub use agent::{Agent, Config, Handshake, PROTOCOL_VERSION};
pub use event::{
    Cost, Event, EventKind, PermissionOption, PermissionRequest, PlanEntry, SessionChoice,
    SessionOption, TurnOutcome, Usage,
};
pub use orphans::reap;
pub use session::{Session, SessionOpts};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot start `{command}`: {source}")]
    Spawn {
        command: String,
        source: std::io::Error,
    },
    /// The process started and the handshake did not complete. Carries what
    /// the agent said on stderr where it said anything, because that is
    /// usually the whole explanation.
    #[error("{0}")]
    Handshake(String),
    #[error("the agent closed the connection")]
    Closed,
    /// The agent answered with a JSON-RPC error, carried verbatim.
    #[error("the agent answered with an error: {0}")]
    Agent(serde_json::Value),
    /// Nothing came back in time. `silent_for` is set when the agent had gone
    /// quiet, and unset when it was talkative and stuck — an agent can be
    /// both, and the difference is what a person needs to read.
    #[error("{}", timeout_message(.method, .waited, .silent_for))]
    Timeout {
        method: String,
        waited: Duration,
        silent_for: Option<Duration>,
    },
    /// The agent spoke the protocol in a way we cannot act on.
    #[error("{0}")]
    Protocol(String),
    #[error("the agent process has no {0}")]
    NoStdio(&'static str),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

fn timeout_message(method: &str, waited: &Duration, silent_for: &Option<Duration>) -> String {
    match silent_for {
        Some(idle) => format!(
            "{method} was given up on: the agent said nothing for {} seconds. Its session is \
             still open — ask again, or stop the agent.",
            idle.as_secs()
        ),
        None => format!(
            "{method} was given up on: the agent has been at it for {} seconds.",
            waited.as_secs()
        ),
    }
}

/// The clocks a request is measured against.
///
/// Fields rather than constants, because these are not one product's numbers:
/// a suggestion during a meeting is worthless after twenty seconds, while a
/// coding turn that streams for an hour is working exactly as intended. And a
/// test suite cannot wait out ten idle minutes.
#[derive(Debug, Clone, Copy)]
pub struct Deadlines {
    /// How long the agent has to answer the handshake. Separate from the rest:
    /// a cold agent may be slow once — a package runner may be fetching it —
    /// and failing here disables a feature rather than losing one turn.
    pub startup: Duration,
    /// A turn that is streaming is working, however long it takes. A turn that
    /// has said nothing for this long has stopped, whatever it believes. Idle
    /// rather than total, because total punishes the long turns.
    pub idle: Duration,
    /// And a wall clock, because an agent can be talkative and stuck at once.
    pub hard: Duration,
    /// How often the wait wakes up to ask whether it has been abandoned.
    pub tick: Duration,
}

impl Default for Deadlines {
    fn default() -> Self {
        Self {
            startup: Duration::from_secs(30),
            idle: Duration::from_secs(620),
            hard: Duration::from_secs(7200),
            tick: Duration::from_secs(5),
        }
    }
}

impl Deadlines {
    /// Deadlines for work somebody is waiting on right now — a suggestion, a
    /// completion — where a late answer is worse than none.
    pub fn interactive(within: Duration) -> Self {
        Self {
            startup: Duration::from_secs(30),
            idle: within,
            hard: within,
            tick: Duration::from_millis(100),
        }
    }
}

/// Who answers when the agent asks to do something.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionPolicy {
    /// The request arrives as [`EventKind::Permission`] and is nobody's until
    /// somebody answers it. The agent is blocked meanwhile, so a product that
    /// chooses this must always answer — including when its window is closed.
    Ask,
    /// Answered "cancelled" the moment it arrives, and never surfaced. For
    /// unattended work where a dialog nobody can see would hang the turn.
    RefuseAll,
}

/// An HTTP MCP server in the protocol's own shape.
///
/// `headers` is an array of `{name, value}` — not a JSON object, however much
/// it looks like one should work. The difference is not cosmetic and it is not
/// loud: one agent rejects `session/new` outright, another accepts the request
/// and silently drops the server, so the agent comes up with none of the tools
/// it was promised and answers from nothing.
pub fn http_mcp_server(name: &str, url: &str, headers: &[(&str, &str)]) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "type": "http",
        "url": url,
        "headers": headers.iter()
            .map(|(n, v)| serde_json::json!({ "name": n, "value": v }))
            .collect::<Vec<_>>(),
    })
}

/// The same, with the one header almost everybody needs.
pub fn http_mcp_server_with_bearer(name: &str, url: &str, token: &str) -> serde_json::Value {
    http_mcp_server(name, url, &[("Authorization", &format!("Bearer {token}"))])
}

#[cfg(test)]
mod tests;
