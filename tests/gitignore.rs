//! Guards the rule that private DB files can never be committed: a pushed file stays
//! retrievable from history and archives even after it is deleted, so `.gitignore` must cover
//! every file SQLite and this tool write. Fails if a rule is dropped from `.gitignore`.

use std::process::Command;

#[test]
fn every_private_data_file_is_ignored_by_git() {
    let private_paths: &[&str] = &[
        "work.db",
        "personal.db",
        "shared.db",
        "nested/dir/shared.db",
        "work.db-wal",
        "work.db-shm",
        "work.db-journal",
        "lessons.sqlite",
        "lessons.sqlite3",
        "lessons.sqlite3-wal",
        ".lessons/config.toml",
        ".lessons/work.db",
        "config.toml",
        "nested/config.toml",
        ".env",
        ".env.local",
        "shared.jsonl",
        "sync/work.jsonl",
        "shared.conflicts.jsonl",
    ];
    // --no-index checks the rules alone, so the paths need not exist and nothing is tracked.
    let check_output = Command::new("git")
        .args(["check-ignore", "--no-index", "--verbose", "--non-matching"])
        .args(private_paths)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run git check-ignore");
    let report: String = String::from_utf8_lossy(&check_output.stdout).into_owned();
    // With --non-matching, a path no rule covers is printed with an empty source: "::\t<path>".
    let unignored: Vec<&str> = report
        .lines()
        .filter(|report_line| report_line.starts_with("::"))
        .collect();
    assert!(
        unignored.is_empty(),
        "not ignored by .gitignore: {unignored:?}"
    );
    assert_eq!(
        report.lines().count(),
        private_paths.len(),
        "git check-ignore output: {report}"
    );
}

#[test]
fn the_committed_sample_config_is_not_ignored() {
    // The ignore rules for private config must not catch the sample that documents it.
    let check_status = Command::new("git")
        .args([
            "check-ignore",
            "--no-index",
            "-q",
            "docs/config.sample.toml",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("run git check-ignore");
    assert!(
        !check_status.success(),
        "docs/config.sample.toml is ignored, so it could not be committed"
    );
}
