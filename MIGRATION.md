# Moving a product onto these crates

Three implementations were folded in. This is what each product deletes, what
it keeps, and what it gains by switching — in the order the switches should
happen, smallest delta first, so the API is validated before the widest
surface commits to it.

## Phase 1 — gilb (`gilb-recorder`, `crates/gilb-assist-acp`) — **done**

Branch `acp/shared-crate`: −1125 lines, 11 tests, the app's catalogue and
resolution now come from `acp-agents`.

The seed. Its shape survives almost unchanged, which is why it goes first.

**Deletes:** `lib.rs` (the `Connection`, the bootstrap, the child guard) and
`orphans.rs`. `AcpBackend`/`AcpSession` shrink to an `AssistBackend` impl over
`acp_client::{Agent, Session}`, roughly:

```rust
let agent = Agent::launch(
    Config::new(launch.bin).args(launch.args).cwd(cwd)
        .env("PATH", acp_agents::spawn_path(None))
        .registry(data_dir.join("agents.json"))
        .deadlines(Deadlines::interactive(Duration::from_secs(20)))
        .permissions(PermissionPolicy::RefuseAll),
    tx,
).await?;
let session = agent.new_session(SessionOpts::default().cwd(cwd)
    .with_config("model", model).with_config("effort", effort)).await?;
```

Turn text is collected from `EventKind::Text` on the channel; a `prompt` that
returns `Err(Error::Timeout { .. })` is the old `Ok(None)` — stay silent.

**Keeps:** `apps/gilb-app-tauri/src/assist/harness.rs` — but the table becomes
`acp_agents::HARNESSES` and the resolution becomes `acp_agents::launch`. What
stays is the product's own: which agent the person chose, what is persisted in
preferences, what the settings panel shows, and the `GILB_ASSIST_*` overrides
(map them onto `Lookup::override_bin` and `SessionOpts::with_config`).

**Gains:** string ids, stderr diagnostics on a failed handshake, the login
hint, per-session replay suppression.

## Phase 2 — OpenTag (`slack/runner/crates/opentag-core`) — **done**

Branch `flow/acp-shared-crate`: −291 lines net, `acp.rs` and `agents.rs`
gone, four latent bugs closed by the move (integer-only ids, no prompt
deadline, discarded stderr, orphaned process groups).

Biggest bug payoff, no UI, no test-first gate.

**Deletes:** `acp.rs` and `agents.rs` entirely.

**Keeps:** `session.rs` minus the transport — `auto_allowed`, the memory MCP
server, attachments, the cwd allowlist, `protocol.rs`, `workspace.rs`. The
permission policy stays exactly as it is, now expressed against the event:

```rust
EventKind::Permission(request) => {
    if auto_allowed(&request.title, &request.tool_kind) { request.allow().await?; }
    else { /* Slack buttons; request.answer(id) when one is pressed */ }
}
```

Memory mounting becomes `acp_client::http_mcp_server_with_bearer("team-memory",
url, token)` — the same shape, now with the reason written down next to it.

**Gains:** the string-id hang, a deadline on `session/prompt` (today a silent
agent leaves the Slack thread "working" forever), the agent's stderr in the
failure message, process-group cleanup, and a capability check before mounting
memory on an agent that cannot hold it.

## Phase 3 — WorkRoom (`workroom/desktop`)

Widest surface, TypeScript on top, strictest CI. Last.

**Deletes:** `src-tauri/src/acp.rs` (all but the Tauri command wrappers and the
`Registry` of agents by name), `src-tauri/src/path.rs`, and the parts of
`src/rules/turn.ts` that parse the wire — `translateAcp`, `permissionAsked`,
`sessionOf`, `mcpServersFor`. `src/agents/catalog.ts` becomes a type plus
`invoke("agent_catalog")`, so the table has one home.

**Keeps:** the rail and context store, transcripts, per-channel sessions,
everything the room renders.

**Gains:** orphan reaping, the knob-withdrawal rule (`agent.ts` applies a
remembered model at session birth and would hit it), Cursor and the npx path,
and a settings panel that can offer an install for what it found.

## House rules

- Pin by tag (`rev = "v0.1.0"`), not by branch: three repositories with three
  release rhythms must not break on each other.
- One consumer at a time, and the API only changes while fewer than two are on
  it.
- A bug found in any consumer is fixed here, with the test, before the product
  works around it.
