//! One agent process's stdio, and everything that happens on it.
//!
//! A single reader task owns stdout for the life of the process: replies go to
//! whoever is waiting, notifications become events, and questions from the
//! agent become events too — unless the product asked for them to be refused
//! outright.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, warn};

use crate::event::{map_update, permission_request, Event, EventKind};
use crate::wire::{
    answer_frame, classify, error_frame, id_key, notify_frame, request_frame, session_of, Inbound,
};
use crate::{Deadlines, Error, PermissionPolicy, Result};

/// The key for "the agent said something", regardless of which session. A
/// slash cannot appear in an id we would otherwise collide with.
const EVERYTHING: &str = "/any";

/// How much of the agent's stderr is worth keeping. Enough for a stack trace
/// or a refusal, far short of a session's logging.
const DIAGNOSTIC_LINES: usize = 50;

/// JSON-RPC's own code for a method the peer does not implement.
const METHOD_NOT_FOUND: i64 = -32601;

pub(crate) struct Connection {
    stdin: Mutex<ChildStdin>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<String, oneshot::Sender<Result<Value>>>>,
    /// When each session last showed a sign of life, and under [`EVERYTHING`]
    /// when the agent last said anything at all — so a busy session cannot
    /// keep a stuck one looking alive.
    activity: Mutex<HashMap<String, Instant>>,
    /// Sessions whose history is being replayed right now. `session/load`
    /// makes the agent repeat the whole conversation as ordinary updates;
    /// without this every resumed turn says everything back before answering.
    replaying: Mutex<HashSet<String>>,
    /// Turns nobody is waiting for any more, by the id of the reply that will
    /// end the quarantine. Until it arrives the agent is still finishing the
    /// old turn, and its words must not be attributed to the next one.
    stale: Mutex<HashMap<String, String>>,
    alive: AtomicBool,
    diagnostics: Mutex<VecDeque<String>>,
    deadlines: Deadlines,
    events: mpsc::Sender<Event>,
    policy: PermissionPolicy,
}

impl Connection {
    pub(crate) fn spawn(
        stdin: ChildStdin,
        stdout: ChildStdout,
        stderr: Option<tokio::process::ChildStderr>,
        deadlines: Deadlines,
        events: mpsc::Sender<Event>,
        policy: PermissionPolicy,
    ) -> Arc<Self> {
        let conn = Arc::new(Self {
            stdin: Mutex::new(stdin),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            activity: Mutex::new(HashMap::new()),
            replaying: Mutex::new(HashSet::new()),
            stale: Mutex::new(HashMap::new()),
            alive: AtomicBool::new(true),
            diagnostics: Mutex::new(VecDeque::new()),
            deadlines,
            events,
            policy,
        });

        // A tail, not a log: the last thing the agent said, for somebody
        // asking why it is not answering. Thrown away — and one of the three
        // clients did throw it away — a logged-out agent reads as "online and
        // silent" with nothing to look at.
        if let Some(stderr) = stderr {
            let keep = conn.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut tail = keep.diagnostics.lock().await;
                    if tail.len() == DIAGNOSTIC_LINES {
                        tail.pop_front();
                    }
                    tail.push_back(line);
                }
            });
        }

        let reader = conn.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                debug!("acp <- {line}");
                reader.dispatch(classify(&line)).await;
            }
            reader.closed().await;
        });

        conn
    }

    /// Out of stdout is out of agent, however it went. The waiters are let go
    /// first: dropping their senders turns every pending wait into "the agent
    /// closed" now, not ten idle minutes from now.
    async fn closed(self: &Arc<Self>) {
        self.alive.store(false, Ordering::SeqCst);
        self.pending.lock().await.clear();
        let diagnostics = self.diagnostics().await;
        let _ = self
            .events
            .send(Event {
                session: None,
                kind: EventKind::Closed { diagnostics },
            })
            .await;
    }

    async fn dispatch(self: &Arc<Self>, inbound: Inbound) {
        // Any line is a sign of life; one that names a session is a sign of
        // life for that turn in particular.
        if let Inbound::Reply(_, msg) | Inbound::Ask(_, msg) | Inbound::Notify(msg) = &inbound {
            let now = Instant::now();
            let mut seen = self.activity.lock().await;
            seen.insert(EVERYTHING.to_string(), now);
            if let Some(session) = session_of(msg) {
                seen.insert(session, now);
            }
        }

        match inbound {
            Inbound::Reply(id, msg) => {
                let key = id_key(&id);
                // A reply to an abandoned turn also ends the quarantine on its
                // session — that turn is over for real now.
                self.stale.lock().await.remove(&key);
                let result = match msg.get("error") {
                    Some(err) => Err(Error::Agent(err.clone())),
                    None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                };
                if let Some(tx) = self.pending.lock().await.remove(&key) {
                    let _ = tx.send(result);
                }
            }
            Inbound::Ask(id, msg) => self.asked(id, msg).await,
            Inbound::Notify(msg) => self.notified(msg).await,
            // Nowhere better to put it, and what matters is that it is no
            // longer indistinguishable from a log line we were right to skip.
            Inbound::Unroutable(msg) => warn!("acp: cannot route {msg}"),
            Inbound::Ignore => {}
        }
    }

    async fn asked(self: &Arc<Self>, id: Value, msg: Value) {
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if method != "session/request_permission" {
            // We declare no filesystem or terminal capabilities, so an agent
            // asking for one is asking for something we never offered. An
            // honest error keeps it moving; silence blocks its turn forever.
            let _ = self
                .write(error_frame(
                    &id,
                    METHOD_NOT_FOUND,
                    &format!("{method} is not supported by this client"),
                ))
                .await;
            return;
        }

        let Some(request) = permission_request(&msg, self.clone()) else {
            warn!("acp: a permission request we could not read: {msg}");
            return;
        };
        if self.policy == PermissionPolicy::RefuseAll {
            // Nobody is watching, so the answer is no and the agent finishes
            // the turn with what it has.
            if let Err(err) = request.cancel().await {
                warn!(%err, "acp: could not refuse a permission request");
            }
            return;
        }
        let session = request.session.clone();
        let _ = self
            .events
            .send(Event {
                session,
                kind: EventKind::Permission(request),
            })
            .await;
    }

    async fn notified(&self, msg: Value) {
        if msg.get("method").and_then(Value::as_str) != Some("session/update") {
            return;
        }
        let session = session_of(&msg);

        // History replayed for `session/load` is not news: whoever asked for
        // the resume already has it.
        if let Some(session) = &session {
            if self.replaying.lock().await.contains(session) {
                return;
            }
            if self.stale.lock().await.values().any(|s| s == session) {
                debug!("acp: dropping a chunk from a turn nobody is waiting for");
                return;
            }
        }

        let Some(params) = msg.get("params") else {
            return;
        };
        if let Some(kind) = map_update(params) {
            let _ = self.events.send(Event { session, kind }).await;
        }
    }

    pub(crate) async fn diagnostics(&self) -> Vec<String> {
        self.diagnostics.lock().await.iter().cloned().collect()
    }

    pub(crate) fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    pub(crate) fn deadlines(&self) -> Deadlines {
        self.deadlines
    }

    pub(crate) async fn replay_guard(&self, session: &str, on: bool) {
        let mut replaying = self.replaying.lock().await;
        if on {
            replaying.insert(session.to_string());
        } else {
            replaying.remove(session);
        }
    }

    /// Stop trusting this session's updates until the reply to `id` arrives:
    /// its turn was abandoned, so whatever streams in now is that turn's tail.
    pub(crate) async fn quarantine(&self, id: &str, session: &str) {
        self.stale
            .lock()
            .await
            .insert(id.to_string(), session.to_string());
    }

    /// Ask, and wait as long as the deadlines allow.
    pub(crate) async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let (key, rx) = self.send(method, params.clone()).await?;
        let watching =
            session_of(&json!({ "params": params })).unwrap_or_else(|| EVERYTHING.into());
        self.awaited(rx, &key, &watching, method).await
    }

    /// Ask, and hand back the key the reply will arrive under so a caller that
    /// stops waiting can name the turn it is no longer listening for.
    pub(crate) async fn send(
        &self,
        method: &str,
        params: Value,
    ) -> Result<(String, oneshot::Receiver<Result<Value>>)> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let key = id_key(&json!(id));
        let (tx, rx) = oneshot::channel();
        // Filed under the same key the reply will be looked up by, whichever
        // way the agent chooses to spell the id back.
        self.pending.lock().await.insert(key.clone(), tx);

        // A frame that never reached the agent is not a request that was made:
        // take the slot back, or the map fills with replies that cannot come.
        if let Err(err) = self.write(request_frame(id, method, params)).await {
            self.pending.lock().await.remove(&key);
            return Err(err);
        }

        // Sending is a sign of life too, or a first request would be judged
        // against a clock that started when the agent last spoke.
        self.activity
            .lock()
            .await
            .insert(EVERYTHING.to_string(), Instant::now());
        Ok((key, rx))
    }

    /// Wait for a reply, and stop waiting when nothing is coming.
    ///
    /// Idle rather than total: a turn that is streaming is working however
    /// long it takes, and a turn that has said nothing for ten minutes has
    /// stopped whatever it believes. The wall clock is there because an agent
    /// can be talkative and stuck at the same time.
    pub(crate) async fn awaited(
        &self,
        mut rx: oneshot::Receiver<Result<Value>>,
        key: &str,
        watching: &str,
        method: &str,
    ) -> Result<Value> {
        let sent = Instant::now();
        loop {
            match tokio::time::timeout(self.deadlines.tick, &mut rx).await {
                Ok(Ok(result)) => return result,
                Ok(Err(_)) => {
                    self.pending.lock().await.remove(key);
                    return Err(Error::Closed);
                }
                Err(_) => {
                    let idle = self
                        .activity
                        .lock()
                        .await
                        .get(watching)
                        .map(Instant::elapsed)
                        .unwrap_or_else(|| sent.elapsed());

                    let silent = idle >= self.deadlines.idle;
                    if !silent && sent.elapsed() < self.deadlines.hard {
                        continue;
                    }
                    self.pending.lock().await.remove(key);
                    return Err(Error::Timeout {
                        method: method.to_string(),
                        waited: sent.elapsed(),
                        silent_for: silent.then_some(idle),
                    });
                }
            }
        }
    }

    pub(crate) async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.write(notify_frame(method, params)).await
    }

    pub(crate) async fn respond(&self, id: &Value, result: Value) -> Result<()> {
        self.write(answer_frame(id, result)).await
    }

    async fn write(&self, line: String) -> Result<()> {
        debug!("acp -> {}", line.trim_end());
        // The lock is on writing only: while one caller waits for a reply,
        // another must still be able to answer a permission request or cancel.
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn pending_count(&self) -> usize {
        self.pending.lock().await.len()
    }
}
