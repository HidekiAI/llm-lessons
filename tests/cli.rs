//! End-to-end tests: run the real `lessons` binary against a throwaway LESSONS_HOME.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

/// A `lessons` command isolated from the developer's own data, log setting and editor.
fn lessons_command(lessons_home: &Path) -> Command {
    let mut mut_command = Command::new(env!("CARGO_BIN_EXE_lessons"));
    mut_command
        .env("LESSONS_HOME", lessons_home)
        .env_remove("LESSONS_LOG")
        .env_remove("VISUAL")
        .env_remove("EDITOR");
    mut_command
}

fn run_lessons(lessons_home: &Path, args: &[&str]) -> Output {
    lessons_command(lessons_home)
        .args(args)
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
    mut_child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin_text.as_bytes())
        .expect("write stdin");
    mut_child.wait_with_output().expect("wait for lessons")
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Runs a command that must succeed and returns its stdout.
fn expect_success(output: Output) -> String {
    assert!(
        output.status.success(),
        "command failed: {}",
        stderr_of(&output)
    );
    stdout_of(&output)
}

fn init_all(lessons_home: &Path) {
    expect_success(run_lessons(lessons_home, &["init"]));
}

/// A made-up lesson, used as the round-trip fixture.
const FIRST_LESSON_ARGS: [&str; 16] = [
    "add",
    "--to",
    "shared",
    "--title",
    "CI build cannot pull from the private registry",
    "--problem",
    "pull_image: not_found_or_no_access for example-org/widget-api",
    "--root-cause",
    "The CI runner's registry token had expired; a local docker login does not apply to CI runners",
    "--resolution",
    "Rotated the registry token and stored it as a CI secret",
    "--failed",
    "Retrying the pull; re-running docker login locally; clearing the build cache",
    "--tags",
    "docker registry ssh",
    "--verbose",
];

#[test]
fn init_creates_all_three_files_and_is_safe_to_repeat() {
    let temp_dir = tempfile::tempdir().unwrap();
    let first_run: String = expect_success(run_lessons(temp_dir.path(), &["init"]));
    assert_eq!(first_run.matches("created").count(), 3, "{first_run}");
    ["work.db", "personal.db", "shared.db"]
        .iter()
        .for_each(|file_name| {
            assert!(
                temp_dir.path().join(file_name).exists(),
                "{file_name} missing"
            )
        });
    let second_run: String = expect_success(run_lessons(temp_dir.path(), &["init"]));
    assert_eq!(second_run.matches("ready").count(), 3, "{second_run}");
}

#[test]
fn init_with_one_domain_creates_only_that_file() {
    let temp_dir = tempfile::tempdir().unwrap();
    expect_success(run_lessons(temp_dir.path(), &["init", "--domain", "work"]));
    assert!(temp_dir.path().join("work.db").exists());
    assert!(!temp_dir.path().join("personal.db").exists());
    assert!(!temp_dir.path().join("shared.db").exists());
}

#[test]
fn add_without_to_is_refused() {
    let temp_dir = tempfile::tempdir().unwrap();
    init_all(temp_dir.path());
    let output: Output = run_lessons(temp_dir.path(), &["add", "--title", "t", "--problem", "p"]);
    assert!(!output.status.success());
    assert!(
        stderr_of(&output).contains("--to"),
        "{}",
        stderr_of(&output)
    );
}

#[test]
fn add_to_a_missing_file_points_at_init_and_creates_nothing() {
    let temp_dir = tempfile::tempdir().unwrap();
    let output: Output = run_lessons(
        temp_dir.path(),
        &["add", "--to", "work", "--title", "t", "--problem", "p"],
    );
    assert!(!output.status.success());
    assert!(
        stderr_of(&output).contains("lessons init --domain work"),
        "{}",
        stderr_of(&output)
    );
    assert!(!temp_dir.path().join("work.db").exists());
}

#[test]
fn add_without_title_or_problem_is_refused() {
    let temp_dir = tempfile::tempdir().unwrap();
    init_all(temp_dir.path());
    let output: Output = run_lessons(
        temp_dir.path(),
        &["add", "--to", "shared", "--title", "only a title"],
    );
    assert!(!output.status.success());
    assert!(
        stderr_of(&output).contains("--problem"),
        "{}",
        stderr_of(&output)
    );
}

#[test]
fn add_with_flags_then_show_as_text_and_json() {
    let temp_dir = tempfile::tempdir().unwrap();
    init_all(temp_dir.path());
    let add_output: Output = run_lessons(temp_dir.path(), &FIRST_LESSON_ARGS);
    assert!(
        stderr_of(&add_output).contains("insert "),
        "--verbose should journal the insert"
    );
    let lesson_id: String = expect_success(add_output).trim().to_string();
    assert_eq!(
        lesson_id.len(),
        26,
        "a ULID is 26 characters, got {lesson_id:?}"
    );

    let shown_text: String = expect_success(run_lessons(temp_dir.path(), &["show", &lesson_id]));
    assert!(
        shown_text.starts_with(&format!("{lesson_id} [shared] CI build cannot")),
        "{shown_text}"
    );
    assert!(
        shown_text.contains("not_found_or_no_access"),
        "{shown_text}"
    );

    let shown_json: String = expect_success(run_lessons(
        temp_dir.path(),
        &["show", &lesson_id, "--format", "json"],
    ));
    let lesson_json: serde_json::Value = serde_json::from_str(&shown_json).expect("valid JSON");
    assert_eq!(lesson_json["domain"], "shared");
    assert_eq!(lesson_json["id"], lesson_id.as_str());
    assert_eq!(lesson_json["tags"], "docker registry ssh");
    assert_eq!(
        lesson_json["failed_tries"],
        "Retrying the pull; re-running docker login locally; clearing the build cache"
    );
    assert_eq!(lesson_json["context"], serde_json::Value::Null);
    assert_eq!(lesson_json["hits"], 0);
}

#[test]
fn add_json_from_stdin_then_show() {
    let temp_dir = tempfile::tempdir().unwrap();
    init_all(temp_dir.path());
    let lesson_json: &str =
        r#"{"title":"from an LLM","problem":"ld: library not found","tags":"linker"}"#;
    let lesson_id: String = expect_success(run_lessons_with_stdin(
        temp_dir.path(),
        &["add", "--to", "work", "--json", "-"],
        lesson_json,
    ))
    .trim()
    .to_string();
    let shown_text: String = expect_success(run_lessons(temp_dir.path(), &["show", &lesson_id]));
    assert!(shown_text.contains("[work] from an LLM"), "{shown_text}");
}

#[test]
fn add_json_with_an_unknown_key_is_refused() {
    let temp_dir = tempfile::tempdir().unwrap();
    init_all(temp_dir.path());
    let output: Output = run_lessons_with_stdin(
        temp_dir.path(),
        &["add", "--to", "work", "--json", "-"],
        r#"{"title":"t","problem":"p","rootcause":"typo"}"#,
    );
    assert!(!output.status.success());
    assert!(
        stderr_of(&output).contains("rootcause"),
        "{}",
        stderr_of(&output)
    );
}

#[test]
fn add_json_cannot_be_combined_with_field_flags() {
    let temp_dir = tempfile::tempdir().unwrap();
    init_all(temp_dir.path());
    let output: Output = run_lessons_with_stdin(
        temp_dir.path(),
        &["add", "--to", "work", "--json", "-", "--title", "t"],
        "{}",
    );
    assert!(!output.status.success());
    assert!(
        stderr_of(&output).contains("cannot be used with"),
        "{}",
        stderr_of(&output)
    );
}

#[test]
fn show_of_an_unknown_id_fails_and_names_the_files_searched() {
    let temp_dir = tempfile::tempdir().unwrap();
    expect_success(run_lessons(
        temp_dir.path(),
        &["init", "--domain", "shared"],
    ));
    let output: Output = run_lessons(temp_dir.path(), &["show", "01NOSUCHID"]);
    assert!(!output.status.success());
    assert!(
        stderr_of(&output).contains("no lesson with id 01NOSUCHID in shared"),
        "{}",
        stderr_of(&output)
    );
}

/// Adds a minimal lesson and returns its id.
fn add_lesson(lessons_home: &Path, domain: &str, title: &str, tags: &str) -> String {
    expect_success(run_lessons(
        lessons_home,
        &[
            "add",
            "--to",
            domain,
            "--title",
            title,
            "--problem",
            "p",
            "--tags",
            tags,
        ],
    ))
    .trim()
    .to_string()
}

#[test]
fn update_replaces_and_clears_fields_in_whichever_file_holds_the_lesson() {
    let temp_dir = tempfile::tempdir().unwrap();
    init_all(temp_dir.path());
    let lesson_id: String = add_lesson(temp_dir.path(), "work", "old title", "rust");
    let update_output: String = expect_success(run_lessons(
        temp_dir.path(),
        &["update", &lesson_id, "--title", "new title", "--tags", ""],
    ));
    assert!(update_output.contains("[work]"), "{update_output}");
    let lesson_json: serde_json::Value = serde_json::from_str(&expect_success(run_lessons(
        temp_dir.path(),
        &["show", &lesson_id, "--format", "json"],
    )))
    .unwrap();
    assert_eq!(lesson_json["title"], "new title");
    assert_eq!(lesson_json["tags"], serde_json::Value::Null);
    assert_eq!(lesson_json["problem"], "p");
}

#[test]
fn update_without_fields_or_of_an_unknown_id_fails() {
    let temp_dir = tempfile::tempdir().unwrap();
    init_all(temp_dir.path());
    let lesson_id: String = add_lesson(temp_dir.path(), "shared", "t", "x");
    assert!(
        !run_lessons(temp_dir.path(), &["update", &lesson_id])
            .status
            .success()
    );
    let unknown_output: Output =
        run_lessons(temp_dir.path(), &["update", "01NOSUCHID", "--title", "t"]);
    assert!(
        stderr_of(&unknown_output).contains("no lesson with id 01NOSUCHID"),
        "{}",
        stderr_of(&unknown_output)
    );
}

#[test]
fn hit_counts_up() {
    let temp_dir = tempfile::tempdir().unwrap();
    init_all(temp_dir.path());
    let lesson_id: String = add_lesson(temp_dir.path(), "personal", "t", "x");
    expect_success(run_lessons(temp_dir.path(), &["hit", &lesson_id]));
    let second_hit: String = expect_success(run_lessons(temp_dir.path(), &["hit", &lesson_id]));
    assert!(second_hit.contains("[personal]: 2 hits"), "{second_hit}");
}

#[test]
fn retire_checks_the_replacement_exists_and_is_not_itself() {
    let temp_dir = tempfile::tempdir().unwrap();
    init_all(temp_dir.path());
    let old_id: String = add_lesson(temp_dir.path(), "work", "old", "x");
    let new_id: String = add_lesson(temp_dir.path(), "shared", "new", "x");

    let typo_output: Output =
        run_lessons(temp_dir.path(), &["retire", &old_id, "--by", "01NOSUCHID"]);
    assert!(
        stderr_of(&typo_output).contains("the replacement"),
        "{}",
        stderr_of(&typo_output)
    );
    let self_output: Output = run_lessons(temp_dir.path(), &["retire", &old_id, "--by", &old_id]);
    assert!(
        stderr_of(&self_output).contains("cannot replace itself"),
        "{}",
        stderr_of(&self_output)
    );

    // The replacement lives in another file: retire must still accept it.
    expect_success(run_lessons(
        temp_dir.path(),
        &["retire", &old_id, "--by", &new_id],
    ));
    let shown_text: String = expect_success(run_lessons(temp_dir.path(), &["show", &old_id]));
    assert!(
        shown_text.contains(&format!("superseded by {new_id}")),
        "{shown_text}"
    );
    let listed: String = expect_success(run_lessons(temp_dir.path(), &["list"]));
    assert!(
        listed.contains(&format!("{old_id} [work] old (retired)")),
        "{listed}"
    );
}

#[test]
fn list_merges_files_newest_first_and_filters_by_domain_and_tag() {
    let temp_dir = tempfile::tempdir().unwrap();
    init_all(temp_dir.path());
    let first_id: String = add_lesson(temp_dir.path(), "work", "first", "ssh");
    let second_id: String = add_lesson(temp_dir.path(), "shared", "second", "rust");
    let third_id: String = add_lesson(temp_dir.path(), "personal", "third", "ssh");

    let fn_listed_ids = |args: &[&str]| -> Vec<String> {
        expect_success(run_lessons(temp_dir.path(), args))
            .lines()
            .map(|line| line.split(' ').next().unwrap_or("").to_string())
            .collect()
    };
    // ULIDs created in the same second still sort by creation, so newest-first is deterministic.
    assert_eq!(
        fn_listed_ids(&["list"]),
        vec![third_id.clone(), second_id.clone(), first_id.clone()]
    );
    assert_eq!(
        fn_listed_ids(&["list", "--limit", "2"]),
        vec![third_id.clone(), second_id.clone()]
    );
    assert_eq!(
        fn_listed_ids(&["list", "--in", "work,shared"]),
        vec![second_id, first_id.clone()]
    );
    assert_eq!(
        fn_listed_ids(&["list", "--tag", "ssh"]),
        vec![third_id, first_id]
    );

    let bad_domain: Output = run_lessons(temp_dir.path(), &["list", "--in", "work,home"]);
    assert!(
        stderr_of(&bad_domain).contains("unknown domain `home`"),
        "{}",
        stderr_of(&bad_domain)
    );
}

/// Lessons in work and shared only, as on a work laptop; personal.db is absent.
fn work_laptop_home() -> (tempfile::TempDir, String, String) {
    let temp_dir = tempfile::tempdir().unwrap();
    expect_success(run_lessons(temp_dir.path(), &["init", "--domain", "work"]));
    expect_success(run_lessons(
        temp_dir.path(),
        &["init", "--domain", "shared"],
    ));
    let shared_id: String = expect_success(run_lessons(temp_dir.path(), &FIRST_LESSON_ARGS))
        .trim()
        .to_string();
    let work_id: String = add_lesson(
        temp_dir.path(),
        "work",
        "ssh agent refuses deploy key",
        "ssh",
    );
    (temp_dir, shared_id, work_id)
}

fn search_ids(lessons_home: &Path, args: &[&str]) -> Vec<String> {
    let search_json: String = expect_success(run_lessons(lessons_home, args));
    let hits: Vec<serde_json::Value> = serde_json::from_str(&search_json).expect("search JSON");
    hits.iter()
        .map(|hit| hit["id"].as_str().unwrap_or("").to_string())
        .collect()
}

#[test]
fn search_finds_the_first_lesson_by_its_exact_error_code() {
    let (temp_dir, shared_id, _work_id) = work_laptop_home();
    let search_text: String = expect_success(run_lessons(
        temp_dir.path(),
        &["search", "not_found_or_no_access"],
    ));
    assert!(
        search_text.starts_with(&format!("{shared_id} [shared] CI build cannot")),
        "{search_text}"
    );
    assert!(search_text.contains("fix"), "{search_text}");
}

#[test]
fn search_spans_files_skips_the_missing_one_and_honours_in() {
    let (temp_dir, shared_id, work_id) = work_laptop_home();
    let both_ids: Vec<String> = search_ids(temp_dir.path(), &["search", "ssh", "--format", "json"]);
    assert_eq!(both_ids.len(), 2, "{both_ids:?}");
    assert!(both_ids.contains(&shared_id) && both_ids.contains(&work_id));
    assert_eq!(
        search_ids(
            temp_dir.path(),
            &["search", "ssh", "--in", "work", "--format", "json"]
        ),
        vec![work_id]
    );
    assert!(
        !temp_dir.path().join("personal.db").exists(),
        "search must not create the missing file"
    );
}

#[test]
fn search_json_carries_domain_and_score() {
    let (temp_dir, _shared_id, _work_id) = work_laptop_home();
    let search_json: String = expect_success(run_lessons(
        temp_dir.path(),
        &["search", "ssh", "--in", "work", "--format", "json"],
    ));
    let hits: Vec<serde_json::Value> = serde_json::from_str(&search_json).unwrap();
    assert_eq!(hits[0]["domain"], "work");
    assert!(hits[0]["score"].is_f64(), "{search_json}");
}

#[test]
fn search_uses_default_search_from_config() {
    let (temp_dir, _shared_id, work_id) = work_laptop_home();
    std::fs::write(
        temp_dir.path().join("config.toml"),
        "default_search = [\"work\"]\n",
    )
    .unwrap();
    assert_eq!(
        search_ids(temp_dir.path(), &["search", "ssh", "--format", "json"]),
        vec![work_id]
    );
}

#[test]
fn search_hides_a_retired_lesson() {
    let (temp_dir, shared_id, work_id) = work_laptop_home();
    expect_success(run_lessons(
        temp_dir.path(),
        &["retire", &shared_id, "--by", &work_id],
    ));
    assert_eq!(
        search_ids(temp_dir.path(), &["search", "ssh", "--format", "json"]),
        vec![work_id]
    );
}

#[test]
fn search_survives_hostile_text_and_prints_nothing_when_nothing_is_searchable() {
    let (temp_dir, _shared_id, _work_id) = work_laptop_home();
    [
        "say \"hi",
        "NOT ssh",
        "-flag (foo",
        "title:ssh*",
        "^ssh AND",
    ]
    .iter()
    .for_each(|hostile_text| {
        expect_success(run_lessons(temp_dir.path(), &["search", hostile_text]));
    });
    assert_eq!(
        expect_success(run_lessons(temp_dir.path(), &["search", "the a of"])),
        ""
    );
    assert_eq!(
        expect_success(run_lessons(
            temp_dir.path(),
            &["search", "!!!", "--format", "json"]
        ))
        .trim(),
        "[]"
    );
}

#[test]
fn search_raw_passes_fts5_syntax_and_reports_a_bad_expression() {
    let (temp_dir, _shared_id, work_id) = work_laptop_home();
    assert_eq!(
        search_ids(
            temp_dir.path(),
            &["search", "ssh NOT docker", "--raw", "--format", "json"]
        ),
        vec![work_id]
    );
    let bad_output: Output = run_lessons(temp_dir.path(), &["search", "say \"hi", "--raw"]);
    assert!(!bad_output.status.success());
    assert!(
        stderr_of(&bad_output).contains("invalid FTS5 query from --raw"),
        "{}",
        stderr_of(&bad_output)
    );
}

#[test]
fn move_carries_a_lesson_to_another_file_and_search_follows_it() {
    let (temp_dir, _shared_id, work_id) = work_laptop_home();
    let move_output: String = expect_success(run_lessons(
        temp_dir.path(),
        &["move", &work_id, "--to", "shared"],
    ));
    assert_eq!(
        move_output.trim(),
        format!("moved {work_id} [work] -> [shared]")
    );
    let shown_text: String = expect_success(run_lessons(temp_dir.path(), &["show", &work_id]));
    assert!(
        shown_text.contains("[shared] ssh agent refuses deploy key"),
        "{shown_text}"
    );
    assert!(
        search_ids(
            temp_dir.path(),
            &["search", "agent", "--in", "work", "--format", "json"]
        )
        .is_empty()
    );
    assert_eq!(
        search_ids(
            temp_dir.path(),
            &["search", "agent", "--in", "shared", "--format", "json"]
        ),
        vec![work_id]
    );
}

#[test]
fn move_refuses_the_same_file_a_missing_target_and_an_unknown_id() {
    let (temp_dir, shared_id, _work_id) = work_laptop_home();
    let same_file: Output = run_lessons(temp_dir.path(), &["move", &shared_id, "--to", "shared"]);
    assert!(
        stderr_of(&same_file).contains("is already in shared"),
        "{}",
        stderr_of(&same_file)
    );
    let missing_target: Output =
        run_lessons(temp_dir.path(), &["move", &shared_id, "--to", "personal"]);
    assert!(
        stderr_of(&missing_target).contains("lessons init --domain personal"),
        "{}",
        stderr_of(&missing_target)
    );
    assert!(!temp_dir.path().join("personal.db").exists());
    let unknown_id: Output = run_lessons(temp_dir.path(), &["move", "01NOSUCHID", "--to", "work"]);
    assert!(
        stderr_of(&unknown_id).contains("no lesson with id 01NOSUCHID"),
        "{}",
        stderr_of(&unknown_id)
    );
}

#[cfg(unix)]
mod editor {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Writes a fake editor: a shell script that overwrites the file it is given with `edited_toml`.
    fn fake_editor(script_dir: &Path, edited_toml: &str) -> std::path::PathBuf {
        let script_path = script_dir.join("fake-editor.sh");
        std::fs::write(
            &script_path,
            format!("#!/bin/sh\ncat > \"$1\" <<'EOF'\n{edited_toml}\nEOF\n"),
        )
        .unwrap();
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        script_path
    }

    fn leftover_templates(temp_root: &Path) -> usize {
        std::fs::read_dir(temp_root)
            .unwrap()
            .filter(|dir_entry| {
                dir_entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("lessons-")
            })
            .count()
    }

    fn run_add_edit(lessons_home: &Path, temp_root: &Path, editor_path: &Path) -> Output {
        lessons_command(lessons_home)
            .env("EDITOR", editor_path)
            .env("TMPDIR", temp_root) // std::env::temp_dir() reads TMPDIR, so templates land where the test can see them
            .args(["add", "--to", "shared", "--edit"])
            .output()
            .expect("run lessons")
    }

    #[test]
    fn add_edit_stores_what_the_editor_wrote_and_removes_the_template() {
        let lessons_home = tempfile::tempdir().unwrap();
        let temp_root = tempfile::tempdir().unwrap();
        init_all(lessons_home.path());
        let editor_path = fake_editor(
            temp_root.path(),
            "title = \"from the editor\"\nproblem = \"typed in vi\"",
        );
        let lesson_id: String = expect_success(run_add_edit(
            lessons_home.path(),
            temp_root.path(),
            &editor_path,
        ))
        .trim()
        .to_string();
        let shown_text: String =
            expect_success(run_lessons(lessons_home.path(), &["show", &lesson_id]));
        assert!(
            shown_text.contains("[shared] from the editor"),
            "{shown_text}"
        );
        assert_eq!(leftover_templates(temp_root.path()), 0);
    }

    #[test]
    fn add_edit_keeps_the_template_when_the_lesson_is_refused() {
        let lessons_home = tempfile::tempdir().unwrap();
        let temp_root = tempfile::tempdir().unwrap();
        init_all(lessons_home.path());
        let editor_path = fake_editor(
            temp_root.path(),
            "title = \"no problem filled in\"\nproblem = \"\"",
        );
        let output: Output = run_add_edit(lessons_home.path(), temp_root.path(), &editor_path);
        assert!(!output.status.success());
        assert!(
            stderr_of(&output).contains("kept at"),
            "{}",
            stderr_of(&output)
        );
        assert_eq!(leftover_templates(temp_root.path()), 1);
    }
}
