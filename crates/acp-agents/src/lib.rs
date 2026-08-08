//! Which coding agents speak ACP, and how to reach one on a machine.
//!
//! The table below is the only product decision in this crate: which CLIs are
//! looked for, what actually speaks the protocol for each, and what each agent
//! calls its knobs. Everything else serves it.
//!
//! The distinction that matters, and that costs a support ticket to learn: the
//! CLI a person has installed is often **not** the thing that speaks ACP.
//! `claude` is an interactive REPL — pipe an ACP `initialize` into it and
//! nothing comes back until the handshake deadline — so Claude Code and Codex
//! are reached through adapter packages, while Cursor and OpenCode speak the
//! protocol themselves behind a subcommand.
//!
//! Nothing here spawns anything or installs anything. [`acp-client`] takes a
//! [`Launch`] and runs it; the product decides whether to offer an install.
//!
//! [`acp-client`]: https://github.com/farol-team/acp-agents

pub mod path;

use std::path::PathBuf;

pub use path::{available, installed, resolve, spawn_path, user_path, PROXIES};

/// One agent this project knows how to reach.
#[derive(Debug, Clone)]
pub struct Harness {
    /// Stable id, persisted in preferences. Never shown to a person.
    pub id: &'static str,
    /// As the vendor spells it.
    pub name: &'static str,
    /// Command names the person may have, most specific first.
    pub cli: &'static [&'static str],
    /// The adapter that speaks ACP for this CLI, when the CLI does not.
    pub adapter_bin: Option<&'static [&'static str]>,
    /// Arguments that put the CLI itself into ACP mode. Empty when only an
    /// adapter will do.
    pub cli_acp_args: &'static [&'static str],
    /// What to install to get this agent, for a product that offers to.
    pub package: Option<&'static str>,
    /// What a package runner can fetch on first use, for a product willing to
    /// pay the wait instead of asking. `None` where there is nothing to fetch.
    pub npx_package: Option<&'static str>,
    /// A model worth defaulting to when a product wants something cheap and
    /// fast. Applied only if the agent advertises it — never assumed.
    pub preferred_model: Option<&'static str>,
    /// Same, for the reasoning-effort knob.
    pub preferred_effort: Option<&'static str>,
    /// What this agent calls that knob: Claude Code says `effort`, Codex says
    /// `reasoning_effort`, and both file it under the `thought_level` category.
    pub effort_config_id: &'static str,
    pub docs_url: &'static str,
}

/// The agents, in the order a product should offer them.
///
/// Commands and packages are pinned by value in the tests below, and verified
/// against the registry: the command is the one `npm view <package> bin`
/// reports, which is what ends up on the machine.
pub const HARNESSES: &[Harness] = &[
    Harness {
        id: "claude",
        name: "Claude Code",
        cli: &["claude"],
        // Only the official adapter. The Zed-era `claude-code-acp` still
        // exists on npm and still works, but it is superseded — and an old
        // copy sitting on a machine used to win simply by being there, which
        // is how somebody ends up on a version nobody chose, reading the
        // documentation for the other one.
        adapter_bin: Some(&["claude-agent-acp"]),
        cli_acp_args: &[],
        package: Some("@agentclientprotocol/claude-agent-acp"),
        npx_package: Some("@agentclientprotocol/claude-agent-acp"),
        preferred_model: Some("haiku"),
        preferred_effort: Some("low"),
        effort_config_id: "effort",
        docs_url: "https://docs.claude.com/en/docs/claude-code/overview",
    },
    Harness {
        id: "codex",
        name: "Codex",
        cli: &["codex"],
        adapter_bin: Some(&["codex-acp"]),
        cli_acp_args: &[],
        package: Some("@agentclientprotocol/codex-acp"),
        npx_package: Some("@agentclientprotocol/codex-acp"),
        // "Fast and affordable" in Codex's own words. If a future adapter
        // drops the value, applying it is skipped and the agent's default
        // stands — preferences are matched against what is advertised.
        preferred_model: Some("gpt-5.6-luna"),
        preferred_effort: Some("low"),
        effort_config_id: "reasoning_effort",
        docs_url: "https://developers.openai.com/codex/cli/",
    },
    Harness {
        id: "cursor",
        name: "Cursor",
        // Cursor renamed its CLI from `cursor-agent` to `agent`. The specific
        // name goes first: `agent` is generic enough to belong to something
        // else entirely on a given machine — see [`name_verified`].
        cli: &["cursor-agent", "agent"],
        adapter_bin: None,
        cli_acp_args: &["acp"],
        package: None,
        npx_package: None,
        preferred_model: None,
        preferred_effort: None,
        effort_config_id: "effort",
        docs_url: "https://cursor.com/docs/cli",
    },
    Harness {
        id: "opencode",
        name: "OpenCode",
        cli: &["opencode"],
        adapter_bin: None,
        // Speaks the protocol itself: `opencode acp` starts an ACP server.
        cli_acp_args: &["acp"],
        package: Some("opencode-ai"),
        npx_package: None,
        // Nothing preferred, because there is nothing universal to prefer:
        // opencode's model list is whatever providers the person configured,
        // so the names differ from machine to machine.
        preferred_model: None,
        preferred_effort: None,
        // Unused — opencode advertises `model` and `mode`, no thinking tier.
        effort_config_id: "effort",
        docs_url: "https://opencode.ai/docs/acp/",
    },
];

/// The harness for an id, when this project pinned one. An agent nobody pinned
/// is not an error — it is somebody's own, and belongs beside these.
pub fn harness(id: &str) -> Option<&'static Harness> {
    let id = id.to_lowercase();
    HARNESSES.iter().find(|h| h.id == id)
}

/// How a product wants an agent looked for.
#[derive(Debug, Clone, Default)]
pub struct Lookup {
    /// A directory the product installs into, searched before the person's own
    /// PATH. `<prefix>/bin`, because that is where `npm install --prefix` puts
    /// what it fetched.
    pub prefix: Option<PathBuf>,
    /// A command the person named outright, which wins over everything. This
    /// is also how somebody points a product at a wrapper script or an
    /// in-house adapter.
    pub override_bin: Option<PathBuf>,
    /// Whether a missing adapter may be fetched by `npx` on first use, the way
    /// the editors that pioneered this do it. Costs a minute inside the first
    /// turn; a product with a settings panel should offer an install instead.
    pub allow_npx: bool,
    /// Look in exactly these directories instead of the prefix and the
    /// person's own PATH. For a product that wants to decide the search order
    /// itself — and for tests, which must not find whatever the machine they
    /// run on happens to have installed.
    pub search: Option<Vec<PathBuf>>,
}

impl Lookup {
    /// The directories this lookup searches, in order.
    pub fn dirs(&self) -> Vec<PathBuf> {
        self.search
            .clone()
            .unwrap_or_else(|| path::directories(self.prefix.clone()))
    }
}

/// What to spawn, once the question of where it lives has been answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub bin: PathBuf,
    pub args: Vec<String>,
    /// True when the first run has to fetch the agent before it can answer, so
    /// the caller can allow a longer startup deadline for it and not mistake a
    /// download for a hang.
    pub fetches: bool,
}

impl Harness {
    /// An adapter for this harness already on the machine.
    pub fn installed_adapter(&self, look: &Lookup) -> Option<PathBuf> {
        Self::first_installed(self.adapter_bin?, look)
    }

    /// The harness's own CLI, if the person has it.
    pub fn installed_cli(&self, look: &Lookup) -> Option<PathBuf> {
        Self::first_installed(self.cli, look)
    }

    fn first_installed(names: &[&str], look: &Lookup) -> Option<PathBuf> {
        let dirs = look.dirs();
        names.iter().find_map(|name| {
            let bin = path::find(name, &dirs)?;
            name_verified(name, &bin).then_some(bin)
        })
    }

    /// The one command that would install this agent, into a prefix the
    /// product owns rather than the person's own node installation. The prefix
    /// is quoted because an app data directory on macOS has a space in it, and
    /// an unquoted one reads as another package to install.
    pub fn install_command(&self, prefix: &std::path::Path) -> Option<String> {
        Some(format!(
            "npm install -g --prefix \"{}\" {}",
            prefix.display(),
            self.package?
        ))
    }
}

/// What to run for this harness, or `None` when the person does not have it.
///
/// An adapter already on disk beats fetching one; the CLI itself is used only
/// where it speaks the protocol; `npx` is the last resort and only when asked
/// for. The CLI must be present for either of the last two: fetching an
/// adapter for a CLI that is not there fails slowly instead of quickly.
pub fn launch(h: &Harness, look: &Lookup) -> Option<Launch> {
    if let Some(bin) = &look.override_bin {
        // Checked rather than taken on faith: an override with a typo would
        // otherwise report the feature as ready and then fail the handshake
        // once per turn, which looks like a hang and says nothing about why.
        return path::available(bin).then(|| Launch {
            bin: bin.clone(),
            args: Vec::new(),
            fetches: false,
        });
    }

    if let Some(bin) = h.installed_adapter(look) {
        return Some(Launch {
            bin,
            args: Vec::new(),
            fetches: false,
        });
    }

    let cli = h.installed_cli(look)?;
    if !h.cli_acp_args.is_empty() {
        return Some(Launch {
            bin: cli,
            args: h.cli_acp_args.iter().map(|a| a.to_string()).collect(),
            fetches: false,
        });
    }

    if !look.allow_npx {
        return None;
    }
    let package = h.npx_package?;
    let npx = path::find("npx", &look.dirs())?;
    Some(Launch {
        bin: npx,
        // `-y` so a first run installs without waiting on a prompt nobody is
        // there to answer.
        args: vec!["-y".into(), package.into()],
        fetches: true,
    })
}

/// Guard against a binary that merely shares a name with the one we want.
///
/// `agent` is generic enough to belong to something else entirely — Grok's CLI
/// installs the same name into the same `~/.local/bin`, and file existence
/// cannot tell the two apart. Asking the binary proves nothing either: `agent
/// --version` prints a bare date, and an unrelated tool would only error on
/// the `acp` subcommand we hand it later, once the turn is already running.
///
/// What does identify Cursor's CLI is where it really lives: the official
/// installer keeps the binary under `~/.local/share/cursor-agent/versions/…`
/// and symlinks `agent` to it, and the Homebrew cask links into the
/// `cursor-cli` caskroom. So the bare `agent` name is believed only when the
/// real file behind it sits in a cursor-named path. Every other name is
/// specific enough to take at face value.
pub fn name_verified(name: &str, bin: &std::path::Path) -> bool {
    if name != "agent" {
        return true;
    }
    let real = std::fs::canonicalize(bin).unwrap_or_else(|_| bin.to_path_buf());
    real.to_string_lossy().to_lowercase().contains("cursor")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("acp-agents-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn binary(dir: &std::path::Path, command: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(command);
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// A machine with exactly what the test put on it. Searching the real PATH
    /// would make these tests pass or fail on whether the box they run on has
    /// a coding agent installed.
    fn look(prefix: &std::path::Path) -> Lookup {
        Lookup {
            prefix: Some(prefix.to_path_buf()),
            search: Some(vec![prefix.join("bin")]),
            ..Lookup::default()
        }
    }

    #[test]
    fn every_harness_can_be_reached_some_way() {
        // A row with no adapter, no ACP subcommand and no package is an agent
        // this table claims to know and cannot start.
        for h in HARNESSES {
            assert!(
                h.adapter_bin.is_some() || !h.cli_acp_args.is_empty(),
                "{}: nothing here speaks ACP",
                h.id
            );
            assert!(!h.cli.is_empty(), "{}: no command to look for", h.id);
        }
    }

    #[test]
    fn the_adapters_are_pinned_by_value() {
        // These move when vendors move, and a silent drift is a feature that
        // stops working on a Tuesday.
        let claude = harness("claude").unwrap();
        assert_eq!(claude.adapter_bin, Some(&["claude-agent-acp"][..]));
        assert_eq!(
            claude.package,
            Some("@agentclientprotocol/claude-agent-acp"),
            "the superseded claude-code-acp must not come back"
        );
        assert_eq!(
            harness("codex").unwrap().adapter_bin,
            Some(&["codex-acp"][..])
        );
        assert_eq!(harness("opencode").unwrap().cli_acp_args, &["acp"]);
        assert_eq!(harness("cursor").unwrap().cli, &["cursor-agent", "agent"]);
    }

    #[test]
    fn an_agent_nobody_pinned_is_not_an_error() {
        assert!(harness("kimi").is_none());
        assert!(
            harness("CLAUDE").is_some(),
            "the id is matched case-insensitively"
        );
    }

    #[test]
    fn an_installed_adapter_beats_fetching_one() {
        let prefix = temp("adapter-installed");
        let adapter = binary(&prefix.join("bin"), "claude-agent-acp");

        let chosen = launch(
            harness("claude").unwrap(),
            &Lookup {
                allow_npx: true,
                ..look(&prefix)
            },
        )
        .expect("an adapter on disk is reachable");

        assert_eq!(chosen.bin, adapter);
        assert!(chosen.args.is_empty());
        assert!(!chosen.fetches, "nothing to fetch: it is already here");
    }

    #[test]
    fn an_agent_that_speaks_acp_itself_is_run_with_its_subcommand() {
        let prefix = temp("opencode-installed");
        let cli = binary(&prefix.join("bin"), "opencode");

        let chosen = launch(harness("opencode").unwrap(), &look(&prefix)).unwrap();

        assert_eq!(chosen.bin, cli);
        assert_eq!(chosen.args, vec!["acp".to_string()]);
    }

    #[test]
    fn a_cli_whose_adapter_is_missing_is_not_started_bare() {
        // `claude` is an interactive REPL: started instead of its adapter it
        // reads every byte we send and answers nothing, which is
        // indistinguishable from "slow" until the handshake deadline.
        let prefix = temp("claude-without-adapter");
        binary(&prefix.join("bin"), "claude");

        assert!(
            launch(harness("claude").unwrap(), &look(&prefix)).is_none(),
            "without npx there is nothing here that speaks ACP"
        );
    }

    #[test]
    fn fetching_is_offered_only_when_the_cli_is_there_and_only_if_allowed() {
        let prefix = temp("npx-fallback");
        binary(&prefix.join("bin"), "claude");
        let npx = binary(&prefix.join("bin"), "npx");

        let chosen = launch(
            harness("claude").unwrap(),
            &Lookup {
                allow_npx: true,
                ..look(&prefix)
            },
        )
        .expect("npx may fetch the adapter");

        assert_eq!(chosen.bin, npx);
        assert_eq!(
            chosen.args,
            vec![
                "-y".to_string(),
                "@agentclientprotocol/claude-agent-acp".to_string()
            ]
        );
        assert!(
            chosen.fetches,
            "the caller needs a longer deadline for this"
        );
    }

    #[test]
    fn nothing_is_fetched_for_a_cli_the_person_does_not_have() {
        // The adapter drives that CLI. Fetching one for an absent CLI fails
        // slowly instead of quickly.
        let prefix = temp("no-cli-at-all");
        binary(&prefix.join("bin"), "npx");

        assert!(launch(
            harness("claude").unwrap(),
            &Lookup {
                allow_npx: true,
                ..look(&prefix)
            }
        )
        .is_none());
    }

    #[test]
    fn an_override_wins_outright_but_must_exist() {
        let prefix = temp("override");
        let mine = binary(&prefix, "my-acp-wrapper");

        let chosen = launch(
            harness("claude").unwrap(),
            &Lookup {
                override_bin: Some(mine.clone()),
                ..Lookup::default()
            },
        )
        .unwrap();
        assert_eq!(chosen.bin, mine);

        assert!(
            launch(
                harness("claude").unwrap(),
                &Lookup {
                    override_bin: Some(prefix.join("typo")),
                    ..Lookup::default()
                }
            )
            .is_none(),
            "an override with a typo must read as missing, not as ready"
        );
    }

    #[test]
    fn a_bare_agent_is_only_believed_when_it_is_really_cursors() {
        // Grok's CLI installs the same name into the same directory.
        let dir = temp("agent-name-collision");
        let stranger = binary(&dir, "agent");
        assert!(!name_verified("agent", &stranger));

        let cursor_dir = dir.join("cursor-agent/versions/1.0");
        let real = binary(&cursor_dir, "agent");
        assert!(name_verified("agent", &real));

        // Every other name is specific enough to take at face value.
        assert!(name_verified("opencode", &stranger));
    }

    #[test]
    fn the_install_command_quotes_a_prefix_with_a_space_in_it() {
        let cmd = harness("opencode")
            .unwrap()
            .install_command(std::path::Path::new("/Users/a/Application Support/x"))
            .unwrap();

        assert_eq!(
            cmd,
            "npm install -g --prefix \"/Users/a/Application Support/x\" opencode-ai"
        );
        assert!(
            harness("cursor")
                .unwrap()
                .install_command(std::path::Path::new("/x"))
                .is_none(),
            "an agent with no package cannot be installed this way"
        );
    }
}
