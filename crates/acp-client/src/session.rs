//! One conversation with an agent: its knobs, its turns, and how it ends.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};
use tracing::{debug, warn};

use crate::connection::Connection;
use crate::event::{session_options, turn_outcome, SessionOption, TurnOutcome};
use crate::{Error, Result};

/// What a session is opened with.
#[derive(Debug, Clone, Default)]
pub struct SessionOpts {
    /// Where the agent works. `None` leaves it to the process's own directory.
    pub cwd: Option<PathBuf>,
    /// MCP servers to mount, in the protocol's own shape — see
    /// [`crate::http_mcp_server`], because getting it wrong is silent.
    pub mcp_servers: Vec<Value>,
    /// Deliver the history a `session/load` replays instead of suppressing it.
    ///
    /// Off by default, because the replay is not news: whoever asked to resume
    /// already has the conversation, and passing it through makes every
    /// resumed turn repeat the whole thread before answering. On, it is the
    /// protocol's own way to read a session back — which is how a transcript
    /// is taken from an agent that has no exporter.
    pub replay: bool,
    /// Knobs to apply right after the session opens, as `(configId, value)` —
    /// e.g. `("model", "haiku")`. Best-effort by design: the option set
    /// differs per agent, and a knob that does not exist must not cost a
    /// feature.
    pub config: Vec<(String, String)>,
}

impl SessionOpts {
    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn mcp(mut self, server: Value) -> Self {
        self.mcp_servers.push(server);
        self
    }

    /// Ask for the replayed history to be delivered — see [`SessionOpts::replay`].
    pub fn replaying(mut self) -> Self {
        self.replay = true;
        self
    }

    pub fn with_config(mut self, id: impl Into<String>, value: impl Into<String>) -> Self {
        self.config.push((id.into(), value.into()));
        self
    }

    pub(crate) fn cwd_str(&self) -> String {
        self.cwd
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
            .to_string_lossy()
            .to_string()
    }
}

pub struct Session {
    conn: Arc<Connection>,
    id: String,
    options: Vec<SessionOption>,
    reply: Value,
}

impl Session {
    pub(crate) fn new(
        conn: Arc<Connection>,
        id: String,
        options: Vec<SessionOption>,
        reply: Value,
    ) -> Self {
        Self {
            conn,
            id,
            options,
            reply,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// The knobs this session offers, as the agent last stated them.
    pub fn options(&self) -> &[SessionOption] {
        &self.options
    }

    /// The `session/new` or `session/load` reply verbatim, for whatever this
    /// crate does not model yet.
    pub fn raw(&self) -> &Value {
        &self.reply
    }

    /// Send a turn and wait for the agent to declare it over.
    ///
    /// A turn that goes silent past the deadline is abandoned: the session is
    /// told to cancel, and whatever the agent streams afterwards belongs to
    /// the dead turn and is dropped rather than attributed to the next one.
    /// The session itself survives — that is the whole difference between this
    /// and killing the process.
    pub async fn prompt(&self, text: &str) -> Result<TurnOutcome> {
        self.prompt_blocks(vec![json!({ "type": "text", "text": text })])
            .await
    }

    pub async fn prompt_blocks(&self, blocks: Vec<Value>) -> Result<TurnOutcome> {
        let (key, rx) = self
            .conn
            .send(
                "session/prompt",
                json!({ "sessionId": self.id, "prompt": blocks }),
            )
            .await?;

        match self
            .conn
            .awaited(rx, &key, &self.id, "session/prompt")
            .await
        {
            Ok(reply) => Ok(turn_outcome(reply)),
            Err(err @ Error::Timeout { .. }) => {
                self.conn.quarantine(&key, &self.id).await;
                let _ = self.cancel().await;
                Err(err)
            }
            Err(err) => Err(err),
        }
    }

    /// Change one knob. Returns the full updated list, which is what the agent
    /// sends back — so the caller never has to guess what took effect.
    pub async fn set_config(&mut self, config_id: &str, value: &str) -> Result<&[SessionOption]> {
        let reply = self
            .conn
            .request(
                "session/set_config_option",
                json!({ "sessionId": self.id, "configId": config_id, "value": value }),
            )
            .await?;
        let updated = session_options(&reply);
        if !updated.is_empty() {
            self.options = updated;
        }
        Ok(&self.options)
    }

    /// Apply a list of knobs in order, skipping any the agent has stopped
    /// offering.
    ///
    /// The knobs depend on each other, which is not obvious until it costs a
    /// morning: choosing Claude Code's `haiku` withdraws `effort` in the same
    /// breath, because that model has no thinking tiers. A client that applies
    /// a remembered model and a remembered effort in order then asks for
    /// something the agent stopped offering one message ago, and gets "Unknown
    /// config option: effort" for its trouble — on every session.
    ///
    /// Nothing here fails: a knob is best-effort, and an agent that has none
    /// simply keeps its own defaults.
    pub async fn apply(&mut self, wanted: &[(String, String)]) {
        for (config_id, value) in wanted {
            // An empty option set means the agent said nothing about its
            // knobs — no grounds to second-guess it, so try anyway.
            let offered =
                self.options.is_empty() || self.options.iter().any(|o| &o.id == config_id);
            if !offered {
                debug!(
                    config_id,
                    value, "acp: session knob withdrawn by an earlier choice — skipped"
                );
                continue;
            }
            // The startup deadline applies here too, and it has to be said
            // out loud: a knob is best-effort, so an agent that never answers
            // one must not wedge the session behind a turn-length wait.
            let startup = self.conn.deadlines().startup;
            match tokio::time::timeout(startup, self.set_config(config_id, value)).await {
                Ok(Ok(_)) => debug!(config_id, value, "acp: session knob set"),
                Ok(Err(err)) => warn!(%err, config_id, value, "acp: session knob not applied"),
                Err(_) => warn!(
                    config_id,
                    value, "acp: session knob not applied — the agent did not answer in time"
                ),
            }
        }
    }

    /// Ask the agent to abandon the turn it is on. A notification, so nothing
    /// comes back.
    pub async fn cancel(&self) -> Result<()> {
        self.conn
            .notify("session/cancel", json!({ "sessionId": self.id }))
            .await
    }

    /// Close the session the agent is holding.
    ///
    /// Killing the process is not the same act: an open session keeps its MCP
    /// servers registered and leaves a row in the agent's own storage. Answers
    /// `false` where the agent never said it could close one, which is not a
    /// failure — see [`crate::Handshake::closes_sessions`].
    pub async fn close(&self, agent_closes_sessions: bool) -> Result<bool> {
        if !agent_closes_sessions {
            return Ok(false);
        }
        self.conn
            .request("session/close", json!({ "sessionId": self.id }))
            .await?;
        Ok(true)
    }
}
