//! End-to-end rows of the PUBLIC warning: the real `lessons` binary, a local bare repository as the sync remote, and
//! a state file that records the remote as public. The warning is a stderr line for every command but `init`, and it
//! never reaches stdout (MCP stdout is JSON-RPC and the hook's stdout carries only lessons).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// A `lessons` command isolated from the developer's own data and git configuration.
fn lessons_command(lessons_home: &Path) -> Command {
    let global_git_config: PathBuf = lessons_home.join("global.gitconfig");
    fs::write(&global_git_config, "").expect("write the empty git config");
    let mut mut_command = Command::new(env!("CARGO_BIN_EXE_lessons"));
    mut_command
        .env("LESSONS_HOME", lessons_home)
        .env("GIT_CONFIG_GLOBAL", &global_git_config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "warning test")
        .env("GIT_AUTHOR_EMAIL", "warning@test.invalid")
        .env("GIT_COMMITTER_NAME", "warning test")
        .env("GIT_COMMITTER_EMAIL", "warning@test.invalid")
        .env_remove("LESSONS_LOG")
        .env_remove("VISUAL")
        .env_remove("EDITOR");
    mut_command
}

fn run_lessons(lessons_home: &Path, args: &[&str]) -> Output {
    lessons_command(lessons_home)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("run lessons")
}

fn run_lessons_with_stdin(lessons_home: &Path, args: &[&str], stdin_text: &str) -> Output {
    let mut mut_child = lessons_command(lessons_home)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn lessons");
    // The command may exit before reading its stdin; a broken pipe is its answer, not a failure.
    drop(std::io::Write::write_all(
        &mut mut_child.stdin.take().expect("stdin"),
        stdin_text.as_bytes(),
    ));
    mut_child.wait_with_output().expect("wait for lessons")
}

fn text_of(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A LESSONS_HOME whose `shared` domain syncs to a local bare repository, synced once so the clone's state file exists,
/// and then rewritten to record the remote as public. Returns the home and the remote path.
fn home_recording_a_public_remote(root: &Path) -> (PathBuf, String) {
    let lessons_home: PathBuf = root.join("home");
    fs::create_dir_all(&lessons_home).expect("create the home");
    let remote_path: PathBuf = root.join("remote.git");
    let key_path: PathBuf = root.join("key");
    fs::write(&key_path, "not a real key").expect("write the key file");
    let init_remote = Command::new("git")
        .args(["init", "-q", "--bare", &remote_path.to_string_lossy()])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("git init --bare");
    assert!(init_remote.success(), "git init --bare failed");
    fs::write(
        lessons_home.join("config.toml"),
        format!(
            "[sync.shared]\nremote = \"{}\"\nssh_key = \"{}\"\n",
            remote_path.display(),
            key_path.display()
        ),
    )
    .expect("write config.toml");
    assert!(
        run_lessons(&lessons_home, &["init", "--domain", "shared"])
            .status
            .success()
    );
    let synced: Output = run_lessons(&lessons_home, &["sync"]);
    assert!(synced.status.success(), "{}", text_of(&synced.stderr));
    let state_files: Vec<PathBuf> = fs::read_dir(lessons_home.join("sync"))
        .expect("the sync directory exists after a sync")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "state")
        })
        .collect();
    assert_eq!(
        state_files.len(),
        1,
        "one clone, one state file: {state_files:?}"
    );
    fs::write(&state_files[0], "{\"visibility\": \"public\"}\n").expect("record public");
    (lessons_home, remote_path.display().to_string())
}

#[test]
fn a_public_remote_warns_on_stderr_for_every_command_but_init_and_never_on_stdout() {
    let temp_dir = tempfile::tempdir().unwrap();
    let (lessons_home, remote) = home_recording_a_public_remote(temp_dir.path());
    let commands: &[&[&str]] = &[&["search", "anything"], &["list"]];
    commands.iter().for_each(|args| {
        let output: Output = run_lessons(&lessons_home, args);
        assert!(
            text_of(&output.stderr).contains(&format!("WARNING: {remote} is PUBLIC")),
            "{args:?}: {}",
            text_of(&output.stderr)
        );
        assert!(
            !text_of(&output.stdout).contains("PUBLIC"),
            "{args:?}: the warning must stay off stdout"
        );
    });
    let hook: Output = run_lessons_with_stdin(
        &lessons_home,
        &["hook", "user-prompt-submit"],
        "{\"prompt\":\"ok thanks\"}",
    );
    assert!(text_of(&hook.stderr).contains("is PUBLIC"), "hook stderr");
    assert!(
        text_of(&hook.stdout).is_empty(),
        "the hook's stdout carries only lessons"
    );
    let init: Output = run_lessons(&lessons_home, &["init", "--domain", "shared"]);
    assert!(
        !text_of(&init.stderr).contains("PUBLIC"),
        "init never warns: {}",
        text_of(&init.stderr)
    );
}

#[test]
fn a_remote_recorded_private_or_never_probed_prints_no_warning() {
    let temp_dir = tempfile::tempdir().unwrap();
    let (lessons_home, _remote) = home_recording_a_public_remote(temp_dir.path());
    let state_file: PathBuf = fs::read_dir(lessons_home.join("sync"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "state")
        })
        .unwrap();
    ["private", "unknown"].iter().for_each(|recorded| {
        fs::write(&state_file, format!("{{\"visibility\": \"{recorded}\"}}\n")).unwrap();
        let output: Output = run_lessons(&lessons_home, &["list"]);
        assert!(
            !text_of(&output.stderr).contains("PUBLIC"),
            "{recorded}: {}",
            text_of(&output.stderr)
        );
    });
}

#[test]
fn the_mcp_server_and_sync_keep_the_warning_off_stdout() {
    let temp_dir = tempfile::tempdir().unwrap();
    let (lessons_home, remote) = home_recording_a_public_remote(temp_dir.path());
    // `mcp` with its stdin closed answers nothing: its stdout is JSON-RPC only, so it must hold no warning.
    let mcp: Output = run_lessons(&lessons_home, &["mcp"]);
    assert!(
        text_of(&mcp.stderr).contains("is PUBLIC"),
        "mcp warns on stderr: {}",
        text_of(&mcp.stderr)
    );
    assert!(
        !text_of(&mcp.stdout).contains("PUBLIC"),
        "mcp stdout is JSON-RPC only"
    );
    // `sync` prints its outcome on stdout and the warning on stderr.
    let synced: Output = run_lessons(&lessons_home, &["sync"]);
    assert!(
        text_of(&synced.stdout).contains("synced"),
        "{}",
        text_of(&synced.stdout)
    );
    assert!(
        !text_of(&synced.stdout).contains("PUBLIC"),
        "sync stdout stays clean"
    );
    assert!(
        text_of(&synced.stderr).contains(&format!("WARNING: {remote} is PUBLIC")),
        "sync warns on stderr: {}",
        text_of(&synced.stderr)
    );
}
