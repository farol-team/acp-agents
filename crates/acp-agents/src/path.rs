//! Where a bare command becomes the path a product runs.
//!
//! Three codebases solved this separately and each learned something the
//! others did not: an app opened from the Dock inherits launchd's PATH and no
//! profile is read for it, Homebrew writes itself into the file only a login
//! shell reads while nvm writes into the one only an interactive shell reads,
//! and a package runner found on the machine says nothing about whether the
//! adapter behind it is there.
//!
//! Nothing here spawns an agent or installs one. It answers one question —
//! where would this command be — and the product decides what to do with it.

use std::path::{Path, PathBuf};

/// How long the person's shell has to answer. An rc file that blocks — a
/// network drive that is not there, a prompt nobody will answer — must not
/// become an application that does not start.
const SHELL_ANSWERS_WITHIN: std::time::Duration = std::time::Duration::from_secs(2);

/// Package runners. Finding one of these says nothing about whether the
/// adapter behind it is installed: `npx -y <pkg>` resolves, announces itself
/// as ready, and then spends a minute fetching — or fails — inside a turn
/// somebody is waiting on. A product that is willing to pay that cost asks
/// for it explicitly through [`Lookup::allow_npx`].
pub const PROXIES: &[&str] = &["npx", "npm", "pnpm", "pnpx", "yarn", "bunx", "uvx"];

/// The PATH a person has, which is not the one this process was handed.
///
/// Asked once — a machine does not change its shell configuration under a
/// running window, and asking per probe would pay a shell start-up for every
/// row of a settings panel.
pub fn user_path() -> &'static Vec<PathBuf> {
    static PATH: std::sync::OnceLock<Vec<PathBuf>> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        let given: Vec<PathBuf> =
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
        let merged = match asked_of_the_persons_shell() {
            Some(said) => merged_path(given, &said),
            None => given,
        };
        add_missing(merged, version_manager_dirs())
    })
}

/// What the person's own shell says their PATH is.
///
/// Login **and** interactive, because the two read different files and asking
/// for either alone answers on half the machines. `None` for anything that is
/// not an answer — no shell, a shell that failed, a shell still thinking.
/// Windows is `None` by design: a child there starts with the person's own
/// environment already.
fn asked_of_the_persons_shell() -> Option<String> {
    if cfg!(windows) {
        return None;
    }
    let shell = std::env::var("SHELL").ok()?;

    let (said, heard) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let answer = std::process::Command::new(&shell)
            .args(["-lic", "printf %s \"$PATH\""])
            .stdin(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).into_owned());
        let _ = said.send(answer);
    });

    // A shell that has not answered by now is one we stop waiting for. The
    // child finishes on its own and its answer is dropped — the alternative is
    // a window that never opens.
    heard.recv_timeout(SHELL_ANSWERS_WITHIN).ok().flatten()
}

/// What the shell said, folded into what this process already had.
///
/// The last line, because an rc file greets people and warns them about flags
/// before anything is printed on purpose. Absolute directories only, because a
/// shell that failed prints its error and fish prints its PATH space-separated
/// — neither is a set of directories, and the pieces would sit in front of
/// every lookup.
fn merged_path(given: Vec<PathBuf>, said: &str) -> Vec<PathBuf> {
    let answer = said
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or_default();

    add_missing(
        given,
        std::env::split_paths(answer)
            .filter(|d| d.is_absolute())
            .collect(),
    )
}

/// Where version managers and package managers put binaries, for a machine
/// whose shell could not be asked. Not a guess about this machine — a list of
/// the places that exist on it.
fn version_manager_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join(".claude/local"));
        dirs.push(home.join(".npm-global/bin"));
        dirs.push(home.join(".volta/bin"));
        dirs.push(home.join(".bun/bin"));
        // nvm keeps one directory per installed node version.
        if let Ok(versions) = std::fs::read_dir(home.join(".nvm/versions/node")) {
            for v in versions.flatten() {
                dirs.push(v.path().join("bin"));
            }
        }
    }
    dirs.into_iter().filter(|d| d.is_dir()).collect()
}

/// `extra` appended to `base`, keeping order and dropping what is already
/// there. A PATH that repeats directories is longer to walk and reads as
/// though something went wrong.
fn add_missing(base: Vec<PathBuf>, extra: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen: std::collections::HashSet<PathBuf> = base.iter().cloned().collect();
    let mut merged = base;
    for dir in extra {
        if seen.insert(dir.clone()) {
            merged.push(dir);
        }
    }
    merged
}

/// Those directories as a child process is given them. A list that cannot be
/// joined — a directory with a separator in its name — leaves the child with
/// what this process has, which is what it had before any of this.
pub fn path_env(dirs: &[PathBuf]) -> std::ffi::OsString {
    std::env::join_paths(dirs).unwrap_or_else(|_| std::env::var_os("PATH").unwrap_or_default())
}

/// The PATH to hand a spawned agent: an owned prefix first, then the person's.
///
/// Not decoration. An adapter fetched from npm is a `#!/usr/bin/env node`
/// script, so it resolves `node` from the *child's* PATH — and an `.app`
/// started from Finder is handed launchd's, where there is no node. The agent
/// then dies in five milliseconds and the connection closes on the first read.
pub fn spawn_path(prefix: Option<&Path>) -> std::ffi::OsString {
    path_env(&directories(prefix.map(Path::to_path_buf)))
}

/// The directories a bare command is looked for in, in the order they are
/// trusted: a prefix the product installs into, then the person's own PATH.
/// Somebody naming an agent they already have means that one, and this must
/// not take it away from them.
pub fn directories(prefix: Option<PathBuf>) -> Vec<PathBuf> {
    prefix
        .map(|dir| dir.join("bin"))
        .into_iter()
        .chain(user_path().iter().cloned())
        .collect()
}

/// Where that command would be in each of them.
///
/// A command given as a path is that path and nothing else: it is the one
/// place somebody has said exactly what they mean, and looking up its last
/// segment would run something they did not name.
pub fn candidates(command: &str, dirs: Vec<PathBuf>) -> Vec<PathBuf> {
    if command.contains(std::path::MAIN_SEPARATOR) {
        return vec![PathBuf::from(command)];
    }
    dirs.into_iter().map(|dir| dir.join(command)).collect()
}

/// The first of those that is really there.
pub fn found(places: &[PathBuf]) -> Option<PathBuf> {
    places.iter().find(|path| path.is_file()).cloned()
}

/// Where the command is, or the name as it was typed — an agent we cannot find
/// is still worth trying to spawn, and the error it gives is the person's to
/// read.
pub fn located(command: &str, places: &[PathBuf]) -> PathBuf {
    found(places).unwrap_or_else(|| PathBuf::from(command))
}

/// Where the machine says this command is, or `None` for nowhere.
pub fn resolve(command: &str, prefix: Option<&Path>) -> Option<PathBuf> {
    found(&candidates(
        command,
        directories(prefix.map(Path::to_path_buf)),
    ))
}

/// The same question asked of an explicit list of directories, for a product
/// that wants to decide the search order itself — and for a test that must not
/// find whatever the machine it runs on happens to have installed.
pub fn find(command: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    found(&candidates(command, dirs.to_vec()))
}

/// Whether a command can actually be spawned right now. An absolute path is
/// checked where it points; a bare name is looked up. A package runner is
/// never an installed agent — see [`PROXIES`].
pub fn installed(command: &str, prefix: Option<&Path>) -> bool {
    let p = Path::new(command);
    let leaf = p.file_name().and_then(|n| n.to_str()).unwrap_or(command);
    if PROXIES.contains(&leaf) {
        return false;
    }
    if p.components().count() > 1 {
        return p.is_file();
    }
    resolve(command, prefix).is_some()
}

/// Same question, asked of a path a product already resolved. Kept separate
/// because a settings panel holds paths, not names.
pub fn available(bin: &Path) -> bool {
    if bin.is_absolute() {
        return bin.is_file();
    }
    found(&candidates(&bin.to_string_lossy(), user_path().to_vec())).is_some()
}

#[cfg(test)]
mod resolution {
    use super::*;

    fn dirs(raw: &[&str]) -> Vec<PathBuf> {
        raw.iter().map(PathBuf::from).collect()
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("acp-resolve-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn binary(dir: &Path, command: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(command);
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        path
    }

    #[test]
    fn what_this_process_was_given_keeps_its_place_and_the_shell_only_adds() {
        let merged = merged_path(
            dirs(&["/usr/bin", "/bin"]),
            "/opt/homebrew/bin:/usr/bin:/bin:/Users/alice/.nvm/versions/node/v22/bin",
        );

        assert_eq!(
            merged,
            dirs(&[
                "/usr/bin",
                "/bin",
                "/opt/homebrew/bin",
                "/Users/alice/.nvm/versions/node/v22/bin",
            ])
        );
    }

    #[test]
    fn a_directory_this_process_already_had_is_not_added_twice() {
        let merged = merged_path(dirs(&["/usr/bin"]), "/usr/bin:/usr/bin:/opt/homebrew/bin");

        assert_eq!(merged, dirs(&["/usr/bin", "/opt/homebrew/bin"]));
    }

    #[test]
    fn a_shell_that_said_nothing_leaves_the_path_exactly_as_it_was() {
        let given = dirs(&["/usr/bin", "/bin"]);

        assert_eq!(merged_path(given.clone(), ""), given);
        assert_eq!(merged_path(given.clone(), "  \n \n"), given);
    }

    #[test]
    fn the_answer_is_the_last_line_because_a_profile_prints_its_own() {
        // Somebody's rc file greets them, or warns about a deprecated flag.
        // Taking the first line takes the greeting and loses the PATH.
        let merged = merged_path(
            dirs(&["/usr/bin"]),
            "Welcome back, Alice\nnvm: using node v22\n/opt/homebrew/bin:/usr/bin\n",
        );

        assert_eq!(merged, dirs(&["/usr/bin", "/opt/homebrew/bin"]));
    }

    #[test]
    fn what_a_shell_says_is_only_believed_when_it_looks_like_a_path() {
        // fish keeps PATH as a list and prints it space-separated; a shell that
        // fails prints its own error. Neither is a set of directories.
        let given = dirs(&["/usr/bin"]);

        assert_eq!(
            merged_path(given.clone(), "command not found: printf"),
            given
        );
    }

    #[test]
    fn an_owned_prefix_is_looked_in_before_the_persons_own_path() {
        // `npm install --prefix P` puts binaries in `P/bin`, so anything else
        // here is a directory nothing will ever be found in.
        let data = temp("own-prefix");
        let installed_here = binary(&data.join("bin"), "opencode");

        assert_eq!(
            found(&candidates("opencode", directories(Some(data)))),
            Some(installed_here)
        );
    }

    #[test]
    fn a_command_given_as_a_path_is_that_path_and_nothing_else() {
        let elsewhere = temp("elsewhere");
        let named = binary(&elsewhere, "opencode");

        assert_eq!(
            candidates(&named.to_string_lossy(), vec![temp("ignored")]),
            vec![named]
        );
    }

    #[test]
    fn a_command_nobody_has_is_left_as_it_was_typed() {
        let nowhere = temp("nowhere");
        let looked = candidates("kimi-acp", vec![nowhere]);

        assert_eq!(found(&looked), None);
        assert_eq!(located("kimi-acp", &looked), PathBuf::from("kimi-acp"));
    }

    #[test]
    fn a_package_runner_is_never_an_installed_agent() {
        // `npx` on the machine says nothing about the adapter behind it, and
        // reporting it as ready is how a turn spends a minute fetching.
        assert!(!installed("npx", None));
        assert!(!installed("/opt/homebrew/bin/npx", None));
        assert!(!installed("bunx", None));
    }

    #[test]
    fn a_path_that_is_there_is_available_and_one_that_is_not_is_not() {
        let dir = temp("availability");
        let bin = binary(&dir, "fake-agent");

        assert!(available(&bin));
        assert!(!available(&dir.join("nope")));
    }

    #[test]
    fn a_join_that_cannot_be_made_leaves_the_child_with_what_we_have() {
        // A directory with a separator in its name cannot be joined; the child
        // then gets this process's PATH rather than an empty one.
        let broken = vec![PathBuf::from("/usr/bin"), PathBuf::from("/bad:dir")];
        assert_eq!(
            path_env(&broken),
            std::env::var_os("PATH").unwrap_or_default()
        );
    }
}
