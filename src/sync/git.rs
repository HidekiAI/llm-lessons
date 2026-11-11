//! The git side of a sync clone (plan step S4b): the command runner, the key-isolating ssh command, the default-branch
//! lookup, push classification and the idempotent clone setup. Sync only ever fetches, commits on top of the remote
//! tip and pushes: it never runs `git pull` or `git merge`, and it never prompts, because it runs in the background.

use std::path::Path;
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};

use crate::journal;

/// The branch used when the remote is empty and so has no default branch to follow.
pub const SYNC_FALLBACK_BRANCH: &str = "main";
/// The one remote of a sync clone.
const REMOTE_NAME: &str = "origin";
/// `git ls-remote --symref <remote> HEAD` prints `ref: refs/heads/<branch>\tHEAD` for the default branch.
const SYMREF_PREFIX: &str = "ref: refs/heads/";
const SYMREF_SUFFIX: &str = "\tHEAD";
/// The words git uses when a push loses a race: another machine pushed first, so fetching and merging again can win.
const LOST_RACE_MARKERS: &[&str] = &["non-fast-forward", "fetch first"];

/// What a push came back with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    Pushed,
    /// Another machine pushed after the fetch: fetch, merge and push again, a bounded number of times.
    LostRace,
    /// Anything else (protected branch, permission, a declining hook, an unknown message): a failure, never retried.
    Refused,
}

/// The default branch of the remote, from the output of `git ls-remote --symref <remote> HEAD`; none when the remote
/// is empty (no HEAD) and so has nothing to follow.
pub fn parse_default_branch(ls_remote_output: &str) -> Option<String> {
    // Only the HEAD symref line counts, and an empty remote has none, so the caller falls back to a named branch.
    ls_remote_output
        .lines()
        .find_map(|line| {
            line.strip_prefix(SYMREF_PREFIX)
                .and_then(|rest| rest.strip_suffix(SYMREF_SUFFIX))
        })
        .filter(|branch| !branch.is_empty())
        .map(String::from)
}

/// Sorts the result of `git push`: success; a lost race, which a retry can win; or a refusal, which it cannot.
pub fn classify_push(exit_ok: bool, stderr: &str) -> PushOutcome {
    // The exit code decides first, so a refusal that merely mentions the words (a hook's message, say) is not a lost
    // race. A lost race is retried a bounded number of times; anything unknown is a failure.
    if exit_ok {
        PushOutcome::Pushed
    } else if LOST_RACE_MARKERS
        .iter()
        .any(|marker| stderr.contains(marker))
    {
        PushOutcome::LostRace
    } else {
        PushOutcome::Refused
    }
}

/// The host of an ssh remote: `user@host:path`, `host:path` or `ssh://[user@]host[:port]/path`. None for any other
/// remote (https, a local path), which has no ssh connection to configure.
pub fn ssh_host_of(remote: &str) -> Option<String> {
    // A scheme means ssh:// or something else (https has no ssh host); without one, a colon before any slash makes it
    // scp-like, and a local path has none. Limitation: an IPv6 literal such as `git@[::1]:repo` is not understood and
    // yields a wrong host, so its agent socket is not found; a key file still works.
    let authority: Option<&str> = match remote.split_once("://") {
        Some(("ssh", after_scheme)) => after_scheme.split('/').next(),
        Some(_) => None,
        None => remote
            .split_once(':')
            .map(|(before_colon, _)| before_colon)
            .filter(|before_colon| !before_colon.contains('/')),
    };
    authority
        .and_then(|with_user| with_user.rsplit('@').next())
        .and_then(|with_port| with_port.split(':').next())
        .filter(|host| !host.is_empty())
        .map(String::from)
}

/// The agent socket from the output of `ssh -G <host>` (its `identityagent` line); none when the line is missing or
/// says `none`. Lets a key that only an agent holds, such as a 1Password key given as a `.pub`, still sign.
pub fn parse_identity_agent(ssh_g_output: &str) -> Option<String> {
    // An agent that holds the key lets a .pub-only key sign; none means no agent line to pass on.
    ssh_g_output
        .lines()
        .find_map(|line| line.strip_prefix("identityagent "))
        .map(str::trim)
        .filter(|socket| !socket.is_empty() && *socket != "none")
        .map(String::from)
}

/// `text` as one shell word in single quotes, so a path with a space or a quote survives `GIT_SSH_COMMAND`'s parsing.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The ssh command git is told to use. `-F /dev/null` drops the user's `~/.ssh/config`, so no other key is offered
/// (`-o IdentityFile=none` does not do that); `IdentitiesOnly` offers only `key`; `BatchMode` turns a passphrase or
/// host-key question into a failure, because sync never prompts; `IdentityAgent` names the agent that holds the key,
/// since `-F /dev/null` also drops the config's own.
pub fn ssh_command(key: &Path, identity_agent: Option<&str>) -> String {
    // NOTE: `/dev/null` is assumed to exist (Debian, macOS, WSL2); native Windows may need `NUL` or an empty
    // config file.
    let agent_option: String = identity_agent
        .map(|socket| format!(" -o IdentityAgent={}", shell_quote(socket)))
        .unwrap_or_default();
    format!(
        "ssh -F /dev/null -i {} -o IdentitiesOnly=yes -o BatchMode=yes{agent_option}",
        shell_quote(&key.to_string_lossy())
    )
}

/// What a git command printed and whether it succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitOutput {
    pub exit_ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Runs git for one sync clone. Every call has `GIT_TERMINAL_PROMPT=0` and the clone's ssh command, and is journalled;
/// `extra_env` is for tests, which point git at their own global config.
#[derive(Debug, Clone)]
pub struct GitRunner {
    working_dir: std::path::PathBuf,
    ssh_command: String,
    extra_env: Vec<(String, String)>,
}

impl GitRunner {
    pub fn new(working_dir: &Path, ssh_command: &str) -> GitRunner {
        GitRunner {
            working_dir: working_dir.to_path_buf(),
            ssh_command: ssh_command.to_string(),
            extra_env: Vec::new(),
        }
    }

    pub fn with_env(self, name: &str, value: &str) -> GitRunner {
        GitRunner {
            extra_env: self
                .extra_env
                .into_iter()
                .chain([(name.to_string(), value.to_string())])
                .collect(),
            ..self
        }
    }

    /// Runs `git <args>` in the working directory. A git that could not start is an error; one that ran and failed is
    /// an `exit_ok: false` output, so the caller decides what a failure means.
    pub fn run(&self, args: &[&str]) -> Result<GitOutput> {
        journal!("git {} (in {})", args.join(" "), self.working_dir.display());
        let output: Output = Command::new("git")
            .args(args)
            .current_dir(&self.working_dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_SSH_COMMAND", &self.ssh_command)
            .envs(self.extra_env.iter().map(|(name, value)| (name, value)))
            .output()
            .with_context(|| format!("git {}: could not start git", args.join(" ")))?;
        let git_output = GitOutput {
            exit_ok: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        };
        journal!(
            "git {}: exit_ok={} stderr={:?}",
            args.join(" "),
            git_output.exit_ok,
            git_output.stderr.trim()
        );
        Ok(git_output)
    }

    /// Like `run`, but a failure is an error naming the command and its stderr, and the result is stdout.
    pub fn run_checked(&self, args: &[&str]) -> Result<String> {
        let git_output: GitOutput = self.run(args)?;
        if git_output.exit_ok {
            Ok(git_output.stdout)
        } else {
            bail!(
                "git {} failed: {}",
                args.join(" "),
                git_output.stderr.trim()
            )
        }
    }
}

/// The remote's default branch, or `SYNC_FALLBACK_BRANCH` when the remote is empty. Run from any directory: it asks
/// the remote, not a clone, so it works before the clone exists.
pub fn detect_default_branch(runner: &GitRunner, remote: &str) -> Result<String> {
    let listing: String = runner.run_checked(&["ls-remote", "--symref", remote, "HEAD"])?;
    Ok(parse_default_branch(&listing).unwrap_or_else(|| SYNC_FALLBACK_BRANCH.to_string()))
}

/// Makes the clone exist and be configured, whatever state it is in: `git init -b <branch>` when there is no
/// repository yet (so the branch name never depends on `init.defaultBranch`), the remote URL, the ssh command, and
/// commit signing off (a signing key may need a prompt or an agent a background process lacks). Safe to repeat.
pub fn ensure_clone(
    runner: &GitRunner,
    clone_dir: &Path,
    remote: &str,
    branch: &str,
    ssh_command: &str,
) -> Result<()> {
    std::fs::create_dir_all(clone_dir)
        .with_context(|| format!("create {}", clone_dir.display()))?;
    if !clone_dir.join(".git").exists() {
        runner.run_checked(&["init", "-q", "-b", branch])?;
    }
    let remote_known: bool = runner.run(&["remote", "get-url", REMOTE_NAME])?.exit_ok;
    let remote_verb: &str = if remote_known { "set-url" } else { "add" };
    runner.run_checked(&["remote", remote_verb, REMOTE_NAME, remote])?;
    runner.run_checked(&["config", "core.sshCommand", ssh_command])?;
    runner.run_checked(&["config", "commit.gpgsign", "false"])?;
    Ok(())
}

// Unit tests for the pure parsers and E2E tests against temp bare repos (local paths, no network, no ssh). Race: a lost
// push race is produced for real by two clones pushing to one bare repo, in both orders; nothing depends on timing.
// Back-to-back: `ensure_clone` twice is pinned. Each test points git at its own empty global config, so the tester's
// `init.defaultBranch` and signing settings cannot change the result.
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    const SOME_KEY: &str = "/keys/id_work";
    const SOME_SOCKET: &str = "/run/agent.sock";

    /// A runner whose git ignores the tester's global and system config, with a throwaway identity for commits.
    fn isolated_runner(working_dir: &Path, global_config: &Path) -> GitRunner {
        GitRunner::new(working_dir, &ssh_command(Path::new(SOME_KEY), None))
            .with_env("GIT_CONFIG_GLOBAL", &global_config.to_string_lossy())
            .with_env("GIT_CONFIG_NOSYSTEM", "1")
            .with_env("GIT_AUTHOR_NAME", "sync test")
            .with_env("GIT_AUTHOR_EMAIL", "sync@test.invalid")
            .with_env("GIT_COMMITTER_NAME", "sync test")
            .with_env("GIT_COMMITTER_EMAIL", "sync@test.invalid")
    }

    /// A bare remote whose HEAD names `head_branch`, plus the global config file every runner of the test shares.
    fn bare_remote(temp_dir: &Path, head_branch: &str) -> (PathBuf, PathBuf) {
        let remote_path: PathBuf = temp_dir.join("remote.git");
        let global_config: PathBuf = temp_dir.join("global.gitconfig");
        fs::write(&global_config, "").unwrap();
        let runner = isolated_runner(temp_dir, &global_config);
        runner
            .run_checked(&[
                "init",
                "-q",
                "--bare",
                "-b",
                head_branch,
                &remote_path.to_string_lossy(),
            ])
            .unwrap();
        (remote_path, global_config)
    }

    /// Commits one file in `clone_dir` on its current branch and pushes it to `origin`; returns the push result.
    fn commit_and_push(
        runner: &GitRunner,
        clone_dir: &Path,
        file_name: &str,
        branch: &str,
    ) -> GitOutput {
        fs::write(clone_dir.join(file_name), file_name).unwrap();
        runner.run_checked(&["add", file_name]).unwrap();
        runner
            .run_checked(&["commit", "-q", "-m", file_name])
            .unwrap();
        runner
            .run(&["push", "-q", "origin", &format!("HEAD:refs/heads/{branch}")])
            .unwrap()
    }

    #[test]
    fn parse_default_branch_reads_the_symref_line() {
        assert_eq!(
            parse_default_branch(
                "ref: refs/heads/trunk\tHEAD\n7ce10b8176f5273992698e0c504e2a7cc5d8db5d\tHEAD\n"
            ),
            Some("trunk".to_string())
        );
        assert_eq!(
            parse_default_branch("ref: refs/heads/feature/x\tHEAD\n0000\tHEAD\n"),
            Some("feature/x".to_string()),
            "a branch name may contain a slash"
        );
        assert_eq!(
            parse_default_branch(""),
            None,
            "an empty remote prints nothing"
        );
        assert_eq!(
            parse_default_branch("0000\tHEAD\n"),
            None,
            "a detached HEAD has no symref"
        );
        assert_eq!(
            parse_default_branch("ref: refs/heads/\tHEAD\n"),
            None,
            "an empty name is no branch"
        );
        assert_eq!(
            parse_default_branch("ref: refs/heads/trunk\tother\n"),
            None,
            "only the HEAD line counts"
        );
    }

    #[test]
    fn classify_push_tells_success_a_lost_race_and_a_refusal_apart() {
        assert_eq!(classify_push(true, ""), PushOutcome::Pushed);
        assert_eq!(
            classify_push(true, "remote: rejected non-fast-forward in a hook message"),
            PushOutcome::Pushed,
            "the exit code decides first"
        );
        assert_eq!(
            classify_push(
                false,
                " ! [rejected]        HEAD -> main (non-fast-forward)"
            ),
            PushOutcome::LostRace
        );
        assert_eq!(
            classify_push(false, " ! [rejected]        HEAD -> main (fetch first)"),
            PushOutcome::LostRace
        );
        assert_eq!(
            classify_push(
                false,
                "remote: error: GH006: Protected branch update failed"
            ),
            PushOutcome::Refused
        );
        assert_eq!(
            classify_push(false, "ERROR: Permission to o/r.git denied"),
            PushOutcome::Refused
        );
        assert_eq!(
            classify_push(false, ""),
            PushOutcome::Refused,
            "an unknown failure is never retried"
        );
    }

    #[test]
    fn ssh_host_of_reads_scp_like_and_ssh_url_remotes_only() {
        assert_eq!(
            ssh_host_of("git@github.com:owner/repo.git"),
            Some("github.com".to_string())
        );
        assert_eq!(
            ssh_host_of("github.com:owner/repo.git"),
            Some("github.com".to_string())
        );
        assert_eq!(
            ssh_host_of("ssh://git@example.org:2222/o/r.git"),
            Some("example.org".to_string())
        );
        assert_eq!(
            ssh_host_of("ssh://example.org/o/r.git"),
            Some("example.org".to_string())
        );
        assert_eq!(ssh_host_of("https://github.com/o/r.git"), None);
        assert_eq!(
            ssh_host_of("/srv/git/r.git"),
            None,
            "a local path has no host"
        );
        assert_eq!(
            ssh_host_of("./a:b/r.git"),
            None,
            "a slash before the colon is a path"
        );
        assert_eq!(ssh_host_of(""), None);
    }

    #[test]
    fn parse_identity_agent_reads_the_socket_and_skips_none() {
        assert_eq!(
            parse_identity_agent(
                "user me\nhostname h\nidentityagent /run/agent.sock\nbatchmode no\n"
            ),
            Some("/run/agent.sock".to_string())
        );
        assert_eq!(parse_identity_agent("identityagent none\n"), None);
        assert_eq!(parse_identity_agent("user me\n"), None, "no line, no agent");
        assert_eq!(parse_identity_agent("identityagent \n"), None);
    }

    /// The pin test: this exact string is what `core.sshCommand` carries, so a change must be deliberate.
    #[test]
    fn ssh_command_is_pinned_and_quotes_awkward_paths() {
        assert_eq!(
            ssh_command(Path::new(SOME_KEY), None),
            "ssh -F /dev/null -i '/keys/id_work' -o IdentitiesOnly=yes -o BatchMode=yes"
        );
        assert_eq!(
            ssh_command(Path::new(SOME_KEY), Some(SOME_SOCKET)),
            "ssh -F /dev/null -i '/keys/id_work' -o IdentitiesOnly=yes -o BatchMode=yes \
             -o IdentityAgent='/run/agent.sock'"
        );
        assert_eq!(
            ssh_command(Path::new("/my keys/it's"), None),
            "ssh -F /dev/null -i '/my keys/it'\\''s' -o IdentitiesOnly=yes -o BatchMode=yes"
        );
    }

    #[test]
    fn the_default_branch_is_followed_not_forced_to_main() {
        let temp_dir = tempfile::tempdir().unwrap();
        let (remote_path, global_config) = bare_remote(temp_dir.path(), "trunk");
        let runner = isolated_runner(temp_dir.path(), &global_config);
        let work_dir: PathBuf = temp_dir.path().join("seed");
        ensure_clone(
            &runner_in(&runner, &work_dir),
            &work_dir,
            &remote_path.to_string_lossy(),
            "trunk",
            "ssh",
        )
        .unwrap();
        commit_and_push(
            &runner_in(&runner, &work_dir),
            &work_dir,
            "one.txt",
            "trunk",
        );
        assert_eq!(
            detect_default_branch(&runner, &remote_path.to_string_lossy()).unwrap(),
            "trunk"
        );
    }

    #[test]
    fn an_empty_remote_falls_back_to_the_named_branch() {
        let temp_dir = tempfile::tempdir().unwrap();
        let (remote_path, global_config) = bare_remote(temp_dir.path(), "trunk");
        let runner = isolated_runner(temp_dir.path(), &global_config);
        assert_eq!(
            detect_default_branch(&runner, &remote_path.to_string_lossy()).unwrap(),
            SYNC_FALLBACK_BRANCH
        );
    }

    /// A runner for `directory`, sharing the environment of `template`.
    fn runner_in(template: &GitRunner, directory: &Path) -> GitRunner {
        GitRunner {
            working_dir: directory.to_path_buf(),
            ..template.clone()
        }
    }

    #[test]
    fn ensure_clone_creates_configures_and_is_safe_to_repeat() {
        let temp_dir = tempfile::tempdir().unwrap();
        let (remote_path, global_config) = bare_remote(temp_dir.path(), "trunk");
        // The tester's global config asks for signing and a different default branch; the clone must not obey either.
        fs::write(
            &global_config,
            "[commit]\n\tgpgsign = true\n[init]\n\tdefaultBranch = other\n",
        )
        .unwrap();
        let clone_dir: PathBuf = temp_dir.path().join("sync").join("clone");
        let runner = runner_in(
            &isolated_runner(temp_dir.path(), &global_config),
            &clone_dir,
        );
        let remote: String = remote_path.to_string_lossy().into_owned();
        let isolating_command: String = ssh_command(Path::new(SOME_KEY), Some(SOME_SOCKET));
        ensure_clone(&runner, &clone_dir, &remote, "trunk", &isolating_command).unwrap();
        ensure_clone(&runner, &clone_dir, &remote, "trunk", &isolating_command).unwrap(); // back-to-back
        let fn_config = |key: &str| {
            runner
                .run_checked(&["config", "--get", key])
                .unwrap()
                .trim()
                .to_string()
        };
        assert_eq!(
            fn_config("commit.gpgsign"),
            "false",
            "the clone's own setting beats the global one"
        );
        assert_eq!(fn_config("core.sshCommand"), isolating_command);
        assert_eq!(fn_config("remote.origin.url"), remote);
        assert_eq!(
            runner
                .run_checked(&["symbolic-ref", "--short", "HEAD"])
                .unwrap()
                .trim(),
            "trunk",
            "the branch name never depends on init.defaultBranch"
        );
        assert_eq!(
            runner.run_checked(&["remote"]).unwrap().lines().count(),
            1,
            "a second run does not add a second remote"
        );
    }

    #[test]
    fn ensure_clone_repairs_a_changed_remote_and_ssh_command() {
        let temp_dir = tempfile::tempdir().unwrap();
        let (remote_path, global_config) = bare_remote(temp_dir.path(), "trunk");
        let clone_dir: PathBuf = temp_dir.path().join("clone");
        let runner = runner_in(
            &isolated_runner(temp_dir.path(), &global_config),
            &clone_dir,
        );
        ensure_clone(
            &runner,
            &clone_dir,
            "git@old.example:o/r.git",
            "trunk",
            "ssh old",
        )
        .unwrap();
        ensure_clone(
            &runner,
            &clone_dir,
            &remote_path.to_string_lossy(),
            "trunk",
            "ssh new",
        )
        .unwrap();
        assert_eq!(
            runner
                .run_checked(&["config", "--get", "remote.origin.url"])
                .unwrap()
                .trim(),
            remote_path.to_string_lossy()
        );
        assert_eq!(
            runner
                .run_checked(&["config", "--get", "core.sshCommand"])
                .unwrap()
                .trim(),
            "ssh new"
        );
    }

    /// What the git child process really sees: terminal prompts are off and the clone's ssh command is set.
    #[test]
    fn every_git_call_runs_with_prompts_off_and_the_clones_ssh_command() {
        let temp_dir = tempfile::tempdir().unwrap();
        let (_remote_path, global_config) = bare_remote(temp_dir.path(), "trunk");
        let runner = isolated_runner(temp_dir.path(), &global_config);
        let seen: String = runner
            .run_checked(&[
                "-c",
                "alias.showenv=!echo \"$GIT_TERMINAL_PROMPT|$GIT_SSH_COMMAND\"",
                "showenv",
            ])
            .unwrap();
        assert_eq!(
            seen,
            format!("0|{}\n", ssh_command(Path::new(SOME_KEY), None))
        );
    }

    #[test]
    fn a_failed_command_names_itself_and_its_stderr() {
        let temp_dir = tempfile::tempdir().unwrap();
        let (_remote_path, global_config) = bare_remote(temp_dir.path(), "trunk");
        let runner = isolated_runner(temp_dir.path(), &global_config);
        let failure: String = format!(
            "{:#}",
            runner.run_checked(&["no-such-subcommand"]).unwrap_err()
        );
        assert!(
            failure.contains("no-such-subcommand") && failure.contains("failed"),
            "{failure}"
        );
    }

    /// Two clones push to one bare remote; the second loses the race for real, whichever clone goes first, and the real
    /// stderr git printed is what `classify_push` reads.
    #[test]
    fn a_real_lost_push_race_is_classified_as_one_in_both_orders() {
        [(0usize, 1usize), (1, 0)]
            .iter()
            .for_each(|&(first, second)| {
                let temp_dir = tempfile::tempdir().unwrap();
                let (remote_path, global_config) = bare_remote(temp_dir.path(), "trunk");
                let remote: String = remote_path.to_string_lossy().into_owned();
                let clone_dirs: Vec<PathBuf> = ["clone_a", "clone_b"]
                    .iter()
                    .map(|name| temp_dir.path().join(name))
                    .collect();
                let runners: Vec<GitRunner> = clone_dirs
                    .iter()
                    .map(|clone_dir| {
                        runner_in(&isolated_runner(temp_dir.path(), &global_config), clone_dir)
                    })
                    .collect();
                clone_dirs
                    .iter()
                    .zip(&runners)
                    .for_each(|(clone_dir, runner)| {
                        ensure_clone(runner, clone_dir, &remote, "trunk", "ssh").unwrap()
                    });
                let winner =
                    commit_and_push(&runners[first], &clone_dirs[first], "first.txt", "trunk");
                assert_eq!(
                    classify_push(winner.exit_ok, &winner.stderr),
                    PushOutcome::Pushed
                );
                let loser =
                    commit_and_push(&runners[second], &clone_dirs[second], "second.txt", "trunk");
                assert!(!loser.exit_ok, "the second push must be rejected");
                assert_eq!(
                    classify_push(loser.exit_ok, &loser.stderr),
                    PushOutcome::LostRace,
                    "{}",
                    loser.stderr
                );
            });
    }
}
