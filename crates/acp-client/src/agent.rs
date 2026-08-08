//! Launching an agent, and what it says about itself when it starts.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tracing::{debug, info};

use crate::connection::Connection;
use crate::event::{session_options, Event};
use crate::session::{Session, SessionOpts};
use crate::{orphans, Deadlines, Error, PermissionPolicy, Result};

/// ACP revision this client speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// Everything about starting one agent.
#[derive(Debug, Clone)]
pub struct Config {
    /// The executable. Working out *which* executable — an adapter, a CLI with
    /// a subcommand, a package runner — is the `acp-agents` crate's job; this
    /// one spawns what it is handed.
    pub bin: PathBuf,
    pub args: Vec<String>,
    /// Working directory for the process. An agent scopes file access to it.
    pub cwd: PathBuf,
    /// Environment to add. `PATH` belongs here more often than it looks: an
    /// adapter fetched from npm is a `#!/usr/bin/env node` script and resolves
    /// `node` from the *child's* PATH, which for an app started from Finder
    /// has no node in it at all.
    pub env: Vec<(String, String)>,
    pub deadlines: Deadlines,
    /// Where to write down the process groups this app starts, so a launch
    /// after a crash can clean up what no destructor got to. `None` skips the
    /// bookkeeping: groups still go away on an ordinary exit, only a crashed
    /// run leaves them behind.
    pub registry: Option<PathBuf>,
    /// What to call ourselves in the handshake.
    pub client_name: String,
    pub client_version: String,
    /// What we tell the agent we can do. The default declares no filesystem
    /// access: an agent that wants to read a file should use its own tools,
    /// under its own sandbox, not ours.
    pub client_capabilities: Value,
    pub permissions: PermissionPolicy,
}

impl Config {
    pub fn new(bin: impl Into<PathBuf>) -> Self {
        Self {
            bin: bin.into(),
            args: Vec::new(),
            cwd: std::env::temp_dir(),
            env: Vec::new(),
            deadlines: Deadlines::default(),
            registry: None,
            client_name: "acp-client".into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            client_capabilities: json!({ "fs": { "readTextFile": false, "writeTextFile": false } }),
            permissions: PermissionPolicy::Ask,
        }
    }

    pub fn args<S: Into<String>>(mut self, args: impl IntoIterator<Item = S>) -> Self {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = cwd.into();
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn deadlines(mut self, deadlines: Deadlines) -> Self {
        self.deadlines = deadlines;
        self
    }

    pub fn registry(mut self, path: impl Into<PathBuf>) -> Self {
        self.registry = Some(path.into());
        self
    }

    pub fn client(mut self, name: impl Into<String>, version: impl Into<String>) -> Self {
        self.client_name = name.into();
        self.client_version = version.into();
        self
    }

    pub fn permissions(mut self, policy: PermissionPolicy) -> Self {
        self.permissions = policy;
        self
    }
}

/// What the agent said about itself when it was asked.
///
/// Kept whole, because the handshake is where an agent says the one thing that
/// decides whether a feature can work at all — and throwing it away meant
/// finding out never.
#[derive(Debug, Clone)]
pub struct Handshake(Value);

impl From<Value> for Handshake {
    fn from(value: Value) -> Self {
        Self(value)
    }
}

impl Handshake {
    pub fn raw(&self) -> &Value {
        &self.0
    }

    pub fn protocol_version(&self) -> Option<i64> {
        self.0.get("protocolVersion").and_then(Value::as_i64)
    }

    pub fn agent_name(&self) -> Option<&str> {
        self.0.pointer("/agentInfo/name").and_then(Value::as_str)
    }

    pub fn agent_version(&self) -> Option<&str> {
        self.0.pointer("/agentInfo/version").and_then(Value::as_str)
    }

    /// Whether the agent says it can mount an HTTP MCP server, which for some
    /// products is the only way anything they know reaches it. An agent that
    /// cannot accepts the session, ignores the server it cannot use, and
    /// answers the turn from nothing — which reads as a bad model rather than
    /// as a client that never checked.
    ///
    /// Read from either place agents put it: the protocol nests it under
    /// `agentCapabilities`, and opencode has been observed reporting it at the
    /// top level.
    pub fn mounts_http_mcp(&self) -> bool {
        self.0
            .pointer("/agentCapabilities/mcpCapabilities/http")
            .or_else(|| self.0.pointer("/mcpCapabilities/http"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// Whether the agent says it can close a session. Guarded rather than
    /// called blind: an agent that never said it could is one whose answer we
    /// invented. The capability is an object, not a flag — its presence is the
    /// yes.
    pub fn closes_sessions(&self) -> bool {
        self.0
            .pointer("/agentCapabilities/sessionCapabilities/close")
            .or_else(|| self.0.pointer("/sessionCapabilities/close"))
            .is_some_and(|v| !v.is_null())
    }

    /// How this agent says a person logs in, if it says at all. The one
    /// actionable sentence an agent offers before a session can be opened, and
    /// it arrives only here — `session/new` on a logged-out agent answers with
    /// an opaque internal error.
    pub fn auth_hint(&self) -> Option<String> {
        let methods = self.0.get("authMethods")?.as_array()?;
        let hints: Vec<String> = methods
            .iter()
            .filter_map(|m| {
                let name = m.get("name").and_then(Value::as_str);
                let how = m.get("description").and_then(Value::as_str);
                match (name, how) {
                    (Some(n), Some(d)) => Some(format!("{n} — {d}")),
                    (Some(n), None) => Some(n.to_string()),
                    (None, Some(d)) => Some(d.to_string()),
                    _ => None,
                }
            })
            .collect();
        (!hints.is_empty()).then(|| hints.join("; "))
    }
}

/// One running agent process that has answered the handshake.
pub struct Agent {
    conn: Arc<Connection>,
    handshake: Handshake,
    /// Kills the whole process group when this agent is dropped.
    _child: ChildGuard,
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent")
            .field("agent", &self.handshake.agent_name().unwrap_or("?"))
            .field("alive", &self.alive())
            .finish_non_exhaustive()
    }
}

impl Agent {
    /// Start the agent and complete the handshake.
    ///
    /// Everything it says from here on arrives on `events`. A handshake that
    /// fails takes the whole process group with it — including the wrapper
    /// chain a package runner started, which is the leak this crate exists to
    /// have fixed once.
    pub async fn launch(config: Config, events: mpsc::Sender<Event>) -> Result<Arc<Self>> {
        let mut command = Command::new(&config.bin);
        command
            .args(&config.args)
            .current_dir(&config.cwd)
            .envs(config.env.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Kept, not discarded. An agent that starts and then cannot work
            // says why here — a missing credential, a config it could not read
            // — and some adapters start perfectly well having never been
            // logged in to.
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Its own process group, so the whole wrapper chain can be signalled
        // at once. Without this only the wrapper dies.
        #[cfg(unix)]
        command.process_group(0);

        let mut child = command.spawn().map_err(|source| Error::Spawn {
            command: config.bin.display().to_string(),
            source,
        })?;

        // The child leads the group it was just put in, so its pid is the
        // group's.
        #[cfg(unix)]
        let pgid = child.id().map(|id| id as i32);
        #[cfg(not(unix))]
        let pgid: Option<i32> = None;
        if let (Some(path), Some(pgid)) = (&config.registry, pgid) {
            orphans::register(path, pgid, &config.bin.to_string_lossy());
        }

        let stdin = child.stdin.take().ok_or(Error::NoStdio("stdin"))?;
        let stdout = child.stdout.take().ok_or(Error::NoStdio("stdout"))?;
        let stderr = child.stderr.take();
        // Guarded from here on, not at the end: everything below can fail —
        // the handshake times out whenever the binary does not speak ACP,
        // which is the common case on a machine being set up — and each of
        // those returns has to take the whole process group with it.
        let guard = ChildGuard {
            child,
            pgid,
            registry: config.registry.clone(),
        };

        let conn = Connection::spawn(
            stdin,
            stdout,
            stderr,
            config.deadlines,
            events,
            config.permissions,
        );

        let handshake = match tokio::time::timeout(
            config.deadlines.startup,
            conn.request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "clientCapabilities": config.client_capabilities,
                    "clientInfo": { "name": config.client_name, "version": config.client_version },
                }),
            ),
        )
        .await
        {
            Ok(Ok(reply)) => Handshake(reply),
            Ok(Err(err)) => {
                // Whatever it said on the way down is the only thing that
                // explains it — "cannot start" covers only a binary that was
                // never there.
                let tail = conn.diagnostics().await;
                return Err(Error::Handshake(if tail.is_empty() {
                    err.to_string()
                } else {
                    format!("{err}\n\n{} said:\n{}", config.bin.display(), tail.join("\n"))
                }));
            }
            // Name the binary. The overwhelmingly likely cause is that it does
            // not speak ACP at all — an interactive coding CLI started instead
            // of its adapter reads every byte we send and answers nothing,
            // which is indistinguishable from "slow" until you know which
            // command ran.
            Err(_) => {
                return Err(Error::Handshake(format!(
                    "`{}` did not answer the ACP handshake within {:?} — does it speak ACP? \
                     (an interactive CLI never will; Claude Code needs the claude-agent-acp adapter)",
                    config.bin.display(),
                    config.deadlines.startup,
                )))
            }
        };

        // An agent answering a version we did not ask for is worth saying
        // before the first turn rather than after it behaves oddly. Not fatal:
        // the protocol is young and this is information, not a verdict.
        match handshake.protocol_version() {
            Some(v) if v == PROTOCOL_VERSION as i64 => {}
            other => debug!("acp: asked for protocol {PROTOCOL_VERSION}, agent answered {other:?}"),
        }
        // Which adapter actually answered. Packages are often fetched
        // unversioned, so this moves under us by design — and when a path
        // breaks on a Tuesday, this line is the difference between "the
        // adapter changed" and a week of guessing.
        info!(
            agent = handshake.agent_name().unwrap_or("?"),
            version = handshake.agent_version().unwrap_or("?"),
            "ACP agent"
        );

        Ok(Arc::new(Self {
            conn,
            handshake,
            _child: guard,
        }))
    }

    pub fn handshake(&self) -> &Handshake {
        &self.handshake
    }

    /// Whether the reader still has an agent on the other end. False is a
    /// process that is gone, however it went.
    pub fn alive(&self) -> bool {
        self.conn.alive()
    }

    /// The process id, while there is one. For a product that shows it, and
    /// for a test that needs to kill the agent from outside — a death this
    /// client's own `stop` has no part in, and the only kind the reader has to
    /// notice by itself.
    pub fn pid(&self) -> Option<u32> {
        self._child.child.id()
    }

    /// The last lines the agent wrote to stderr, oldest first.
    pub async fn diagnostics(&self) -> Vec<String> {
        self.conn.diagnostics().await
    }

    /// Open a session.
    ///
    /// Fails with the agent's own login instructions attached where it offered
    /// any: a logged-out agent answers this with an opaque internal error and
    /// puts the one useful sentence in `authMethods`.
    pub async fn new_session(&self, opts: SessionOpts) -> Result<Session> {
        let reply = self
            .conn
            .request(
                "session/new",
                json!({
                    "cwd": opts.cwd_str(),
                    "mcpServers": opts.mcp_servers,
                }),
            )
            .await
            .map_err(|err| self.with_auth_hint(err))?;

        let id = reply
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Protocol("session/new returned no sessionId".into()))?
            .to_string();

        let session = Session::new(self.conn.clone(), id, session_options(&reply), reply);
        session.apply(&opts.config).await;
        Ok(session)
    }

    /// Pick up a session the agent still has.
    ///
    /// The replay ACP mandates — the agent repeats the whole conversation as
    /// ordinary updates — is suppressed for this session while the call is in
    /// flight. Without that, every resumed turn says the entire history back
    /// before answering it.
    pub async fn load_session(&self, id: &str, opts: SessionOpts) -> Result<Session> {
        self.conn.replay_guard(id, !opts.replay).await;
        let reply = self
            .conn
            .request(
                "session/load",
                json!({
                    "sessionId": id,
                    "cwd": opts.cwd_str(),
                    "mcpServers": opts.mcp_servers,
                }),
            )
            .await;
        self.conn.replay_guard(id, false).await;
        let reply = reply.map_err(|err| self.with_auth_hint(err))?;

        let session = Session::new(
            self.conn.clone(),
            id.to_string(),
            session_options(&reply),
            reply,
        );
        session.apply(&opts.config).await;
        Ok(session)
    }

    fn with_auth_hint(&self, err: Error) -> Error {
        match self.handshake.auth_hint() {
            Some(hint) => Error::Protocol(format!("{err} — this agent offers: {hint}")),
            None => err,
        }
    }

    /// Stop the agent, and everything it started.
    pub fn stop(&self) {
        self._child.stop();
    }

    #[cfg(test)]
    pub(crate) fn connection(&self) -> &Arc<Connection> {
        &self.conn
    }
}

/// Kills the agent — and the whole wrapper chain behind it — when dropped.
struct ChildGuard {
    child: Child,
    /// The child's process group (its own pid — it leads the group). `None`
    /// where process groups do not apply.
    pgid: Option<i32>,
    /// Where the group was written down, to strike it out again.
    registry: Option<PathBuf>,
}

impl ChildGuard {
    fn stop(&self) {
        if let Some(pgid) = self.pgid {
            orphans::kill_group(pgid);
        }
        if let (Some(path), Some(pgid)) = (&self.registry, self.pgid) {
            orphans::unregister(path, pgid);
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        debug!("acp: stopping the agent");
        match self.pgid {
            Some(pgid) => orphans::kill_group(pgid),
            // No group to signal: at least take the process we hold.
            None => {
                let _ = self.child.start_kill();
            }
        }
        if let (Some(path), Some(pgid)) = (&self.registry, self.pgid) {
            orphans::unregister(path, pgid);
        }
    }
}
