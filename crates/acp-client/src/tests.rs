//! Tests drive a **fake agent** — a shell script that speaks just enough ACP —
//! so the whole client is exercised without an agent binary, a model or a
//! network.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;
use tokio::sync::mpsc;

use super::*;

/// Writes an executable fake agent and returns its path (kept alive by `dir`).
fn fake_agent(dir: &tempfile::TempDir, body: &str) -> PathBuf {
    let path = dir.path().join("fake-agent");
    std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn config(bin: PathBuf) -> Config {
    Config::new(bin).deadlines(Deadlines {
        startup: Duration::from_secs(5),
        idle: Duration::from_secs(5),
        hard: Duration::from_secs(5),
        tick: Duration::from_millis(50),
    })
}

/// Answers the handshake, then whatever the test appends.
const HANDSHAKE: &str = r#"
read_line() { IFS= read -r line; }
read_line   # initialize
printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}\n'
read_line   # session/new
printf '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s-1"}}\n'
read_line   # session/prompt
"#;

async fn started(bin: PathBuf) -> (std::sync::Arc<Agent>, mpsc::Receiver<Event>) {
    started_with(config(bin)).await
}

async fn started_with(config: Config) -> (std::sync::Arc<Agent>, mpsc::Receiver<Event>) {
    let (tx, rx) = mpsc::channel(64);
    let agent = Agent::launch(config, tx).await.expect("the agent starts");
    (agent, rx)
}

/// Everything the agent said until the channel went quiet, drained after a
/// turn is over.
fn drain(rx: &mut mpsc::Receiver<Event>) -> Vec<EventKind> {
    let mut seen = Vec::new();
    while let Ok(event) = rx.try_recv() {
        seen.push(event.kind);
    }
    seen
}

fn text_of(events: &[EventKind]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            EventKind::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn streams_the_answer_and_ends_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_agent(
        &dir,
        &format!(
            r#"{HANDSHAKE}
printf '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s-1","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"ask about "}}}}}}}}\n'
printf '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s-1","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"the budget"}}}}}}}}\n'
printf '{{"jsonrpc":"2.0","id":3,"result":{{"stopReason":"end_turn"}}}}\n'
sleep 5
"#
        ),
    );

    let (agent, mut events) = started(bin).await;
    let session = agent.new_session(SessionOpts::default()).await.unwrap();
    let outcome = session.prompt("they said it is expensive").await.unwrap();

    assert_eq!(outcome.stop_reason.as_deref(), Some("end_turn"));
    assert_eq!(text_of(&drain(&mut events)), "ask about the budget");
}

/// An agent that spells our numeric ids back as strings is answering. Read as
/// integers only, its replies are dropped and every turn hangs to the deadline.
#[tokio::test]
async fn an_agent_that_answers_with_string_ids_is_answering() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_agent(
        &dir,
        r#"
read_line() { IFS= read -r line; }
read_line
printf '{"jsonrpc":"2.0","id":"1","result":{"protocolVersion":1}}\n'
read_line
printf '{"jsonrpc":"2.0","id":"2","result":{"sessionId":"s-1"}}\n'
read_line
printf '{"jsonrpc":"2.0","id":"3","result":{"stopReason":"end_turn"}}\n'
sleep 5
"#,
    );

    let (agent, _events) = started(bin).await;
    let session = agent.new_session(SessionOpts::default()).await.unwrap();

    assert_eq!(
        session
            .prompt("hello")
            .await
            .unwrap()
            .stop_reason
            .as_deref(),
        Some("end_turn")
    );
}

/// The answer, the reasoning and the tool calls are three different things,
/// and a product that shows all three the same way is unusable.
#[tokio::test]
async fn thoughts_and_tool_calls_are_told_apart_from_the_answer() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_agent(
        &dir,
        &format!(
            r#"{HANDSHAKE}
printf '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s-1","update":{{"sessionUpdate":"agent_thought_chunk","content":{{"type":"text","text":"let me look"}}}}}}}}\n'
printf '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s-1","update":{{"sessionUpdate":"tool_call","title":"Read(a.rs)","kind":"read"}}}}}}\n'
printf '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s-1","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"done"}}}}}}}}\n'
printf '{{"jsonrpc":"2.0","id":3,"result":{{"stopReason":"end_turn"}}}}\n'
sleep 5
"#
        ),
    );

    let (agent, mut events) = started(bin).await;
    let session = agent.new_session(SessionOpts::default()).await.unwrap();
    session.prompt("go").await.unwrap();

    let seen = drain(&mut events);
    assert!(matches!(seen[0], EventKind::Thought(_)));
    assert!(matches!(&seen[1], EventKind::Tool { title, kind, .. }
                     if title == "Read(a.rs)" && kind == "read"));
    assert_eq!(text_of(&seen), "done");
}

/// A permission request is the one message that must never be dropped: the
/// agent is blocked until it is answered, and the answer is the product's.
#[tokio::test]
async fn a_permission_request_reaches_the_product_and_its_answer_reaches_the_agent() {
    let dir = tempfile::tempdir().unwrap();
    let seen = dir.path().join("answer.json");
    let bin = fake_agent(
        &dir,
        &format!(
            r#"{HANDSHAKE}
printf '{{"jsonrpc":"2.0","id":"perm-1","method":"session/request_permission","params":{{"sessionId":"s-1","toolCall":{{"title":"Bash(rm -rf build)","kind":"execute","rawInput":{{"command":"rm -rf build"}}}},"options":[{{"optionId":"yes-once","name":"Allow","kind":"allow_once"}},{{"optionId":"no","name":"Deny","kind":"reject_once"}}]}}}}\n'
read_line
printf '%s\n' "$line" > {seen}
printf '{{"jsonrpc":"2.0","id":3,"result":{{"stopReason":"end_turn"}}}}\n'
sleep 5
"#,
            seen = seen.display()
        ),
    );

    let (agent, mut events) = started(bin).await;
    let session = agent.new_session(SessionOpts::default()).await.unwrap();

    let answering = tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if let EventKind::Permission(request) = event.kind {
                assert_eq!(request.session.as_deref(), Some("s-1"));
                assert_eq!(request.command.as_deref(), Some("rm -rf build"));
                assert_eq!(request.tool_kind, "execute");
                assert_eq!(request.allow_id(), Some("yes-once"));
                request.allow().await.unwrap();
                return;
            }
        }
        panic!("the request never reached the product");
    });

    session.prompt("clean up").await.unwrap();
    answering.await.unwrap();

    let answer: serde_json::Value =
        serde_json::from_str(std::fs::read_to_string(&seen).unwrap().trim()).unwrap();
    assert_eq!(
        answer["id"], "perm-1",
        "the agent is blocked on that exact token, not on our reading of it"
    );
    assert_eq!(answer["result"]["outcome"]["optionId"], "yes-once");
}

/// Unattended work has nobody to ask, and a dialog nobody can see is a turn
/// that never ends.
#[tokio::test]
async fn refusing_by_policy_answers_without_asking_anybody() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_agent(
        &dir,
        &format!(
            r#"{HANDSHAKE}
printf '{{"jsonrpc":"2.0","id":9,"method":"session/request_permission","params":{{"sessionId":"s-1","toolCall":{{"title":"Bash(x)"}},"options":[{{"optionId":"y","kind":"allow_once"}}]}}}}\n'
read_line   # our refusal
printf '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s-1","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"went on without it"}}}}}}}}\n'
printf '{{"jsonrpc":"2.0","id":3,"result":{{"stopReason":"end_turn"}}}}\n'
sleep 5
"#
        ),
    );

    let (agent, mut events) =
        started_with(config(bin).permissions(PermissionPolicy::RefuseAll)).await;
    let session = agent.new_session(SessionOpts::default()).await.unwrap();
    session.prompt("go").await.unwrap();

    let seen = drain(&mut events);
    assert_eq!(text_of(&seen), "went on without it");
    assert!(
        !seen.iter().any(|e| matches!(e, EventKind::Permission(_))),
        "a refused request must not also be shown to somebody"
    );
}

/// We declare no filesystem capability, so an agent asking for one is asking
/// for something never offered. Silence would block its turn forever.
#[tokio::test]
async fn a_request_we_do_not_support_is_refused_rather_than_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let seen = dir.path().join("answer.json");
    let bin = fake_agent(
        &dir,
        &format!(
            r#"{HANDSHAKE}
printf '{{"jsonrpc":"2.0","id":77,"method":"fs/read_text_file","params":{{"sessionId":"s-1","path":"/etc/passwd"}}}}\n'
read_line
printf '%s\n' "$line" > {seen}
printf '{{"jsonrpc":"2.0","id":3,"result":{{"stopReason":"end_turn"}}}}\n'
sleep 5
"#,
            seen = seen.display()
        ),
    );

    let (agent, _events) = started(bin).await;
    let session = agent.new_session(SessionOpts::default()).await.unwrap();
    session.prompt("read it").await.unwrap();

    let answer: serde_json::Value =
        serde_json::from_str(std::fs::read_to_string(&seen).unwrap().trim()).unwrap();
    assert_eq!(answer["id"], 77);
    assert_eq!(answer["error"]["code"], -32601);
}

/// A late answer is worse than none where somebody is waiting, and the turn
/// that was given up on must be told to stop.
#[tokio::test]
async fn a_turn_that_goes_silent_is_given_up_on_and_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let seen = dir.path().join("cancel.json");
    let bin = fake_agent(
        &dir,
        &format!(
            r#"{HANDSHAKE}
read_line   # the cancel we send when we stop waiting
printf '%s\n' "$line" > {seen}
sleep 5
"#,
            seen = seen.display()
        ),
    );

    let (agent, _events) =
        started_with(config(bin).deadlines(Deadlines::interactive(Duration::from_millis(300))))
            .await;
    let session = agent.new_session(SessionOpts::default()).await.unwrap();

    let err = session.prompt("slow one").await.unwrap_err();
    assert!(
        matches!(
            &err,
            Error::Timeout {
                silent_for: Some(_),
                ..
            }
        ),
        "a silent agent is reported as silent, not merely slow: {err}"
    );
    assert!(err.to_string().contains("session/prompt"));

    tokio::time::sleep(Duration::from_millis(200)).await;
    let cancel: serde_json::Value =
        serde_json::from_str(std::fs::read_to_string(&seen).unwrap().trim()).unwrap();
    assert_eq!(cancel["method"], "session/cancel");
    assert!(cancel.get("id").is_none(), "a cancel expects nothing back");
}

/// A turn that timed out keeps streaming on the agent's side: its late words
/// must not land in the next turn's answer.
#[tokio::test]
async fn late_chunks_do_not_leak_into_the_next_turn() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_agent(
        &dir,
        &format!(
            r#"{HANDSHAKE}
sleep 1
printf '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s-1","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"stale"}}}}}}}}\n'
printf '{{"jsonrpc":"2.0","id":3,"result":{{"stopReason":"end_turn"}}}}\n'
read_line   # session/cancel for the abandoned turn
read_line   # session/prompt #2
printf '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s-1","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"fresh"}}}}}}}}\n'
printf '{{"jsonrpc":"2.0","id":4,"result":{{"stopReason":"end_turn"}}}}\n'
sleep 5
"#
        ),
    );

    let (agent, mut events) =
        started_with(config(bin).deadlines(Deadlines::interactive(Duration::from_millis(500))))
            .await;
    let session = agent.new_session(SessionOpts::default()).await.unwrap();

    assert!(
        session.prompt("one").await.is_err(),
        "the slow turn is abandoned"
    );
    let _ = drain(&mut events);

    session.prompt("two").await.unwrap();
    assert_eq!(
        text_of(&drain(&mut events)),
        "fresh",
        "no words from the dead turn may leak in"
    );
}

/// A dead agent must fail the turn rather than wait out the deadline: the
/// waiters are let go the moment stdout runs out.
#[tokio::test]
async fn a_dead_agent_fails_every_waiting_turn_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_agent(
        &dir,
        r#"
IFS= read -r line
printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}\n'
IFS= read -r line
printf '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s-1"}}\n'
IFS= read -r line
exit 0
"#,
    );

    let (agent, mut events) = started_with(config(bin).deadlines(Deadlines {
        idle: Duration::from_secs(600),
        ..Deadlines::default()
    }))
    .await;
    let session = agent.new_session(SessionOpts::default()).await.unwrap();

    let failed = tokio::time::timeout(Duration::from_secs(3), session.prompt("hello"))
        .await
        .expect("a dead agent must not be waited out for ten minutes");
    assert!(matches!(failed, Err(Error::Closed)));

    // And the product is told, once, with whatever the agent last said.
    let closed = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(event) = events.recv().await {
            if matches!(event.kind, EventKind::Closed { .. }) {
                return true;
            }
        }
        false
    })
    .await
    .unwrap();
    assert!(closed, "a process that is gone is announced");
    assert!(!agent.alive());
}

/// A request whose write fails must not leave its response slot behind, or
/// every dead-agent turn piles a corpse into the pending map.
#[tokio::test]
async fn a_failed_write_leaves_no_pending_request() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_agent(
        &dir,
        r#"
IFS= read -r line
printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}\n'
IFS= read -r line
printf '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s-1"}}\n'
exit 0
"#,
    );

    let (agent, _events) = started(bin).await;
    let session = agent.new_session(SessionOpts::default()).await.unwrap();
    // Let the agent's exit settle so the write below is what fails.
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(session.prompt("hello").await.is_err());
    assert_eq!(
        agent.connection().pending_count().await,
        0,
        "a request that could not be written must not stay pending"
    );
}

/// Choosing a model can withdraw the other knobs, and the client must notice.
///
/// The real case, from Claude Code's adapter: `model=haiku` succeeds and comes
/// back with an option set that no longer has `effort` in it. Sending the
/// remembered effort anyway earns an "Unknown config option" on every session.
#[tokio::test]
async fn an_option_withdrawn_by_an_earlier_choice_is_not_asked_for() {
    let dir = tempfile::tempdir().unwrap();
    let asked = dir.path().join("asked.log");
    let bin = fake_agent(
        &dir,
        &format!(
            r#"
read_line() {{ IFS= read -r line; printf '%s\n' "$line" >> {log}; }}
read_line   # initialize
printf '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1}}}}\n'
read_line   # session/new — both knobs on offer
printf '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"s-1","configOptions":[{{"id":"model"}},{{"id":"effort"}}]}}}}\n'
read_line   # set_config_option(model) — and effort goes away with the answer
printf '{{"jsonrpc":"2.0","id":3,"result":{{"configOptions":[{{"id":"model"}}]}}}}\n'
read_line   # must already be the prompt
printf '{{"jsonrpc":"2.0","id":4,"result":{{"stopReason":"end_turn"}}}}\n'
sleep 5
"#,
            log = asked.display()
        ),
    );

    let (agent, _events) = started(bin).await;
    let session = agent
        .new_session(
            SessionOpts::default()
                .with_config("model", "haiku")
                .with_config("effort", "low"),
        )
        .await
        .unwrap();
    session.prompt("hello").await.unwrap();

    let log = std::fs::read_to_string(&asked).unwrap_or_default();
    assert!(
        log.contains(r#""configId":"model""#),
        "the model must still be applied: {log}"
    );
    assert!(
        !log.contains(r#""configId":"effort""#),
        "effort was withdrawn and must not be asked for: {log}"
    );
    assert_eq!(
        session.options().await.len(),
        1,
        "the session knows what is left"
    );
}

/// An agent that never answers a knob must not wedge the session behind a
/// turn-length wait: the knob is best-effort and the startup deadline applies.
#[tokio::test]
async fn a_silent_config_option_does_not_wedge_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_agent(
        &dir,
        r#"
read_line() { IFS= read -r line; }
read_line
printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}\n'
read_line
printf '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s-1"}}\n'
read_line   # set_config_option — never answered
read_line   # session/prompt
printf '{"jsonrpc":"2.0","id":4,"result":{"stopReason":"end_turn"}}\n'
sleep 5
"#,
    );

    let (agent, _events) = started_with(config(bin).deadlines(Deadlines {
        startup: Duration::from_millis(500),
        idle: Duration::from_secs(5),
        hard: Duration::from_secs(5),
        tick: Duration::from_millis(50),
    }))
    .await;

    let session = tokio::time::timeout(
        Duration::from_secs(3),
        agent.new_session(SessionOpts::default().with_config("model", "haiku")),
    )
    .await
    .expect("an unanswered knob must not wedge startup")
    .unwrap();

    assert!(session.prompt("hello").await.is_ok(), "the work carried on");
}

/// `session/load` makes the agent replay the whole conversation as ordinary
/// updates. Passed through, every resumed turn says the history back before
/// answering — into the thread it came from.
#[tokio::test]
async fn a_replayed_history_is_not_news() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_agent(
        &dir,
        r#"
read_line() { IFS= read -r line; }
read_line   # initialize
printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}\n'
read_line   # session/load — the replay comes before the reply
printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"OLD HISTORY"}}}}\n'
printf '{"jsonrpc":"2.0","id":2,"result":{}}\n'
read_line   # session/prompt
printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"the answer"}}}}\n'
printf '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}\n'
sleep 5
"#,
    );

    let (agent, mut events) = started(bin).await;
    let session = agent
        .load_session("s-1", SessionOpts::default())
        .await
        .unwrap();
    session.prompt("carry on").await.unwrap();

    assert_eq!(
        text_of(&drain(&mut events)),
        "the answer",
        "the replay belongs to the past, not to this turn"
    );
}

/// The agent runs with the environment we hand it, not the one we inherited.
///
/// This is what breaks a packaged app while dev works fine: an adapter fetched
/// from npm is a `#!/usr/bin/env node` script, and an app started from Finder
/// is handed a PATH with no node in it.
#[tokio::test]
async fn the_agent_is_given_the_environment_we_configured() {
    let dir = tempfile::tempdir().unwrap();
    let seen = dir.path().join("path.txt");
    let bin = fake_agent(
        &dir,
        &format!(
            r#"printf '%s' "$PATH" > {seen}
{HANDSHAKE}
printf '{{"jsonrpc":"2.0","id":3,"result":{{"stopReason":"end_turn"}}}}\n'
sleep 5
"#,
            seen = seen.display()
        ),
    );

    let (agent, _events) =
        started_with(config(bin).env("PATH", "/node-lives-here:/usr/bin:/bin")).await;
    let session = agent.new_session(SessionOpts::default()).await.unwrap();
    let _ = session.prompt("hello").await;

    let path = std::fs::read_to_string(&seen).expect("the agent recorded its PATH");
    assert!(
        path.starts_with("/node-lives-here"),
        "the agent must run with the PATH it was given, got {path}"
    );
}

/// A binary that does not speak ACP is the common case on a machine being set
/// up, and the message has to name it — "slow" and "this is not an ACP agent"
/// look identical from here otherwise.
#[tokio::test]
async fn a_binary_that_does_not_speak_acp_says_so_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_agent(&dir, "sleep 30\n");

    let (tx, _rx) = mpsc::channel(8);
    let err = Agent::launch(
        config(bin.clone()).deadlines(Deadlines {
            startup: Duration::from_millis(400),
            ..Deadlines::default()
        }),
        tx,
    )
    .await
    .expect_err("a script that says nothing cannot pass a handshake");

    assert!(matches!(err, Error::Handshake(_)));
    assert!(err.to_string().contains(&bin.display().to_string()));
    assert!(err.to_string().contains("does it speak ACP"));
}

/// What an agent says on the way down is the only explanation there is, and
/// one of the three clients this crate replaces threw it away.
#[tokio::test]
async fn a_failed_handshake_carries_what_the_agent_said_on_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_agent(
        &dir,
        "echo 'not logged in: run `claude /login`' >&2\nexit 1\n",
    );

    let (tx, _rx) = mpsc::channel(8);
    let err = Agent::launch(config(bin), tx).await.expect_err("it exited");

    assert!(
        err.to_string().contains("claude /login"),
        "the reason must survive: {err}"
    );
}

/// The leak that produced the orphans, pinned end to end.
///
/// A shell stands in for a package runner and a `sleep` for the agent: neither
/// speaks ACP, so the handshake times out — which is exactly the path that
/// used to leak. Passing means the grandchild is gone and the registry is
/// clean, without anyone calling the reaper.
#[tokio::test]
async fn a_failed_handshake_takes_the_whole_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let registry = dir.path().join("agents.json");
    let marker = dir.path().join("grandchild.pid");

    let config = Config::new("/bin/sh")
        .args([
            "-c".to_string(),
            format!(
                "sleep 60 & echo $! > {}; sleep 60",
                marker.to_string_lossy()
            ),
        ])
        .registry(registry.clone())
        .deadlines(Deadlines {
            startup: Duration::from_millis(600),
            tick: Duration::from_millis(50),
            ..Deadlines::default()
        });

    let (tx, _rx) = mpsc::channel(8);
    let err = Agent::launch(config, tx)
        .await
        .expect_err("a shell that says nothing cannot pass an ACP handshake");
    assert!(
        matches!(err, Error::Handshake(_)),
        "unexpected failure: {err}"
    );

    let pid: i32 = std::fs::read_to_string(&marker)
        .expect("the stand-in agent recorded its pid")
        .trim()
        .parse()
        .expect("a pid");

    // The signal travels the group; give the kernel a moment to deliver it.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    assert!(
        !alive,
        "the grandchild outlived the failed handshake — this is the leak"
    );

    let left = std::fs::read_to_string(&registry).unwrap_or_default();
    assert!(
        !left.contains("/bin/sh"),
        "a group that was cleaned up must not stay on the reaper's list: {left}"
    );
}

// The handshake is the only place an agent says whether a capability rail can
// reach it, how somebody logs in, and whether a session can be closed.
// Discarded, all three were assumed.

#[test]
fn an_agent_that_mounts_http_mcp_says_so_wherever_it_says_it() {
    let nested: Handshake = json!({
        "agentCapabilities": { "mcpCapabilities": { "http": true, "sse": true } }
    })
    .into();
    let top: Handshake = json!({ "mcpCapabilities": { "http": true } }).into();

    assert!(nested.mounts_http_mcp());
    assert!(top.mounts_http_mcp());
}

#[test]
fn silence_about_http_mcp_is_not_a_yes() {
    let quiet: Handshake = json!({}).into();
    let no: Handshake = json!({
        "agentCapabilities": { "mcpCapabilities": { "http": false } }
    })
    .into();

    assert!(!quiet.mounts_http_mcp());
    assert!(
        !no.mounts_http_mcp(),
        "an agent that says no is not an agent that said nothing"
    );
}

#[test]
fn an_agent_that_did_not_say_it_closes_sessions_is_not_asked_to() {
    let closes: Handshake = json!({
        "agentCapabilities": { "sessionCapabilities": { "close": {}, "fork": {} } }
    })
    .into();
    let older: Handshake = json!({
        "agentCapabilities": { "sessionCapabilities": { "fork": {}, "list": {} } }
    })
    .into();

    assert!(
        closes.closes_sessions(),
        "the capability is an object; presence is the yes"
    );
    assert!(!older.closes_sessions());
}

#[test]
fn the_way_in_is_whatever_the_agent_said_it_was() {
    // A logged-out agent answers session/new with an opaque internal error and
    // puts the instruction here.
    let handshake: Handshake = json!({
        "authMethods": [ { "id": "claude-login", "name": "Log in with Claude Code",
                           "description": "Run `claude /login` in the terminal" } ]
    })
    .into();

    let hint = handshake
        .auth_hint()
        .expect("an agent that says how to log in is quoted");
    assert!(hint.contains("claude /login"));

    let silent: Handshake = json!({ "authMethods": [] }).into();
    assert!(silent.auth_hint().is_none());
}

/// `headers` is an array of `{name, value}`. Sent as an object, one agent
/// rejects the session outright and another accepts it and silently drops the
/// server — so the agent comes up with none of the tools it was promised.
#[test]
fn an_mcp_server_carries_its_headers_as_a_list() {
    let server = http_mcp_server_with_bearer("team-memory", "https://x/memory/mcp", "t0ken");

    assert_eq!(server["type"], "http");
    assert!(
        server["headers"].is_array(),
        "an object here is silently dropped"
    );
    assert_eq!(server["headers"][0]["name"], "Authorization");
    assert_eq!(server["headers"][0]["value"], "Bearer t0ken");
}

/// The JSON a product hands to its own frontend is spelled the way the
/// protocol spells it, so a UI that already speaks ACP keeps its types.
#[test]
fn an_event_is_rendered_the_way_the_protocol_spells_it() {
    let event = Event {
        session: Some("s-1".into()),
        kind: EventKind::Tool {
            title: "Read(a.rs)".into(),
            kind: "read".into(),
            status: Some("pending".into()),
            update: true,
        },
    };
    let json = event.to_json();

    assert_eq!(json["kind"], "tool");
    assert_eq!(json["session"], "s-1");
    assert_eq!(json["toolKind"], "read");
    assert_eq!(json["update"], true);
}

#[test]
fn a_session_option_keeps_the_agents_own_field_names() {
    let option = SessionOption {
        id: "model".into(),
        name: "Model".into(),
        category: "model".into(),
        kind: "select".into(),
        current: "haiku".into(),
        choices: vec![SessionChoice {
            value: "haiku".into(),
            name: "Haiku".into(),
        }],
    };
    let json = option.to_json();

    assert_eq!(json["type"], "select", "as `session/new` spells it");
    assert_eq!(json["currentValue"], "haiku");
    assert_eq!(json["options"][0]["value"], "haiku");
}
