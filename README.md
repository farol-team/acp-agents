# acp-agents

Two small Rust crates for talking to a coding agent that runs on the user's own
machine, over the [Agent Client Protocol](https://agentclientprotocol.com).

| crate | answers |
|---|---|
| [`acp-client`](crates/acp-client) | spawn it, speak JSON-RPC to it, and never lose it |
| [`acp-agents`](crates/acp-agents) | which agents speak ACP, and where they live on this machine |

```rust
use acp_client::{Agent, Config, EventKind, SessionOpts};

let (tx, mut events) = tokio::sync::mpsc::channel(64);
let agent = Agent::launch(Config::new("claude-agent-acp").cwd("/tmp"), tx).await?;
let session = agent.new_session(SessionOpts::default()).await?;
let outcome = session.prompt("what changed here today?").await?;
```

## Why these exist

Three products at Farol wrote this client separately — a Slack runner, a shared
workspace, and a meeting assistant — and each learned something the other two
did not:

- **ids are strings sometimes.** JSON-RPC allows it, every agent we had spoken
  to numbered them, and the first one that did not did not fail — it hung.
- **a package runner is a wrapper.** `npx → node → agent`: killing the process
  you hold leaves the agent running, reparented, holding its memory. Two piled
  up in fifteen minutes of ordinary use.
- **`headers` is an array.** Sent as an object, one agent rejects the session
  outright and another accepts it and silently drops the MCP server — the agent
  then answers from nothing, which reads as a bad model.
- **the handshake is the only place** an agent says whether it can mount an
  HTTP MCP server, how to log in, and whether a session can be closed.
  Discarded, all three get assumed.
- **knobs withdraw each other.** Choosing a small model can remove the
  reasoning-effort option in the same reply; applying a remembered pair in
  order then asks for something that no longer exists.
- **a replayed history is not news.** `session/load` makes the agent repeat the
  whole conversation as ordinary updates.
- **silence needs a deadline, and streaming does not.** A turn is measured by
  its own silence, not by a total that punishes the long ones.

Each of those cost somebody a day. They are now in one place, with the tests
that pin them.

## What is not here

Nothing about any product. Permission requests arrive as events and the answer
is the caller's — ask a person, apply a rule, or refuse everything unattended
(`PermissionPolicy`). Memory, capability rails, workspace layout, transcripts,
what to render and where: all outside.

The two crates do not depend on each other. `acp-agents` decides *what* to run;
`acp-client` runs what it is handed.

## Status

`0.2` — extracted from three working implementations and adopted by all three:
[gilb](https://github.com/gilb-ai/gilb-recorder/pull/60),
[OpenTag](https://github.com/farol-team/opentag/pull/33),
[WorkRoom](https://github.com/farol-team/workroom/pull/294). 71 tests here;
4 344 lines of duplicated client and its specs deleted across the three
against 1 280 written, so about 3 000 net. Pin by tag while the API settles.

## Licence

MIT.
