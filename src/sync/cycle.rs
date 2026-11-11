//! The decisions inside one sync cycle (plan step S4c-1): reading a file at a past commit, merging one domain, the push
//! retry rule, which commit is remembered, and which domains a run covers. Every function here is pure; the cycle that
//! calls them, with the lock, the database transaction and git, is S4c-2.

use anyhow::{Result, bail};

use super::git::PushOutcome;
use super::{Conflict, GroupKey, group_key, merge_lessons, merge_queue, settle_queue};
use crate::db::Domain;
use crate::store::Lesson;

/// How often one cycle may push: the first try and the retries after a lost race. A bounded number, so machines that
/// keep racing each other converge or fail, and never loop.
pub const PUSH_ATTEMPTS: u32 = 3;

/// What `git show <commit>:<file>` came back with, for a file that holds BASE or THEIRS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shown {
    Content(String),
    /// The commit is fine but has no such file: a domain's first sync. The file reads as empty.
    MissingFile,
    /// The commit itself cannot be read (a rewritten history, a recreated clone). BASE is empty, which is a union that
    /// deletes nothing; the cycle logs it and goes on, because it must never be a permanent error.
    UnreadableCommit,
}

/// The two ways git says a path is not in a commit.
const MISSING_FILE_WORDINGS: &[&str] = &["does not exist in", "exists on disk, but not in"];

/// Sorts the result of `git show <commit>:<file>`. Git prints the same "exists on disk, but not in" for a file missing
/// from a good commit and for a commit that does not exist, so this function trusts the commit: the cycle checks that
/// it exists (`git cat-file -e`) before reading anything at it, and treats a failed check as an unreadable commit.
pub fn interpret_show(exit_ok: bool, stdout: &str, stderr: &str) -> Shown {
    // A file missing at a good commit is normal for a new domain; any other failure means the commit cannot be trusted.
    let file_is_missing: bool = MISSING_FILE_WORDINGS
        .iter()
        .any(|wording| stderr.contains(wording));
    match (exit_ok, file_is_missing) {
        (true, _) => Shown::Content(stdout.to_string()),
        (false, true) => Shown::MissingFile,
        (false, false) => Shown::UnreadableCommit,
    }
}

/// Everything one domain's merge reads: BASE, OURS and THEIRS lessons, and the three conflict queues.
#[derive(Debug, Clone, Copy)]
pub struct DomainInputs<'rows> {
    pub base: &'rows [Lesson],
    pub ours: &'rows [Lesson],
    pub theirs: &'rows [Lesson],
    pub base_queue: &'rows [Conflict],
    pub live_queue: &'rows [Conflict],
    pub theirs_queue: &'rows [Conflict],
}

/// What one domain's merge decided.
#[derive(Debug, Clone, PartialEq)]
pub struct DomainPlan {
    pub merged: Vec<Lesson>,
    pub queue: Vec<Conflict>,
    /// Groups whose lesson no longer exists, for the cycle to log.
    pub cleared: Vec<Conflict>,
    /// `superseded_by` conflicts, which keep the default winner and are only logged.
    pub unqueued: Vec<Conflict>,
}

/// Merges one domain in the documented order: the lessons first, then the queues with this cycle's new conflicts, then
/// the queue is settled against the merged rows.
pub fn plan_domain_merge(inputs: &DomainInputs) -> DomainPlan {
    // The queue merge needs the new conflicts, and the settle step needs which fields conflicted this cycle, so the
    // lessons are merged first and the queue never sees a half-merged state.
    let outcome = merge_lessons(inputs.base, inputs.ours, inputs.theirs);
    let conflicted: Vec<GroupKey> = outcome.conflicts.iter().map(group_key).collect();
    let queue: Vec<Conflict> = merge_queue(
        inputs.base_queue,
        inputs.live_queue,
        inputs.theirs_queue,
        &outcome.conflicts,
    );
    let settled = settle_queue(outcome.merged, queue, &conflicted);
    DomainPlan {
        merged: settled.merged,
        queue: settled.queue,
        cleared: settled.cleared,
        unqueued: outcome.unqueued,
    }
}

/// What to do after a push attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterPush {
    Finish,
    /// Fetch, merge and push again.
    Retry,
    /// Stop with an error: a refusal, or a lost race with no attempts left.
    GiveUp,
}

/// The retry rule. `attempt` counts from 1.
pub fn next_attempt(attempt: u32, outcome: PushOutcome) -> AfterPush {
    // Only a lost race is worth another try, and only while attempts remain; a refusal would fail the same way again.
    match outcome {
        PushOutcome::Pushed => AfterPush::Finish,
        PushOutcome::LostRace if attempt < PUSH_ATTEMPTS => AfterPush::Retry,
        PushOutcome::LostRace | PushOutcome::Refused => AfterPush::GiveUp,
    }
}

/// The commit to remember as BASE for the next cycle: the commit just pushed, else the remote tip this cycle agreed
/// with (nothing needed committing), else what was remembered before (an empty remote with nothing to commit).
pub fn new_last_synced(
    pushed_head: Option<&str>,
    remote_tip: Option<&str>,
    previous: Option<&str>,
) -> Option<String> {
    // Never called before the push has succeeded, so a crash leaves the older commit and the next cycle merges again.
    pushed_head.or(remote_tip).or(previous).map(String::from)
}

/// The domains a run covers: every domain of the clone, or the one asked for with `--domain`; asking for a domain this
/// remote does not carry is an error that names it.
pub fn domains_to_sync(clone_domains: &[Domain], requested: Option<Domain>) -> Result<Vec<Domain>> {
    // A domain that is not configured for this clone must fail loudly: skipping it would look like a successful sync.
    match requested {
        None => Ok(clone_domains.to_vec()),
        Some(domain) if clone_domains.contains(&domain) => Ok(vec![domain]),
        Some(domain) => bail!(
            "domain {} is not configured for this remote (configured: {})",
            domain.as_str(),
            clone_domains
                .iter()
                .map(|configured| configured.as_str())
                .collect::<Vec<&str>>()
                .join(", ")
        ),
    }
}

// Unit tests only; `interpret_show` is checked against the stderr a real git prints, in a temp repository. These
// functions are pure, so no race or back-to-back case applies here: the cycle's own races and repeats are S4c-2's E2E
// rows. `plan_domain_merge` is also run with the two sides swapped, because every machine must compute the same result.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::git::GitRunner;
    use crate::sync::{MergedField, Version};
    use std::fs;

    const EARLY: &str = "2026-11-01T00:00:00Z";
    const MIDDLE: &str = "2026-11-02T00:00:00Z";
    const LATE: &str = "2026-11-03T00:00:00Z";
    const ID_A: &str = "01KAAAAAAAAAAAAAAAAAAAAAAA";
    const ID_B: &str = "01KBBBBBBBBBBBBBBBBBBBBBBB";

    fn lesson(lesson_id: &str, updated_at: &str, resolution: &str) -> Lesson {
        Lesson {
            id: lesson_id.to_string(),
            created_at: EARLY.to_string(),
            updated_at: updated_at.to_string(),
            title: format!("title of {lesson_id}"),
            context: None,
            problem: "problem".to_string(),
            root_cause: None,
            resolution: Some(resolution.to_string()),
            failed_tries: None,
            tags: None,
            env_versions: None,
            superseded_by: None,
            hits: 0,
        }
    }

    fn group(lesson_id: &str, versions: &[(&str, &str)]) -> Conflict {
        Conflict {
            id: lesson_id.to_string(),
            field: MergedField::Resolution,
            base: None,
            versions: versions
                .iter()
                .map(|(value, updated_at)| Version {
                    value: Some((*value).to_string()),
                    updated_at: (*updated_at).to_string(),
                })
                .collect(),
        }
    }

    /// Runs the real `git show <commit>:<file>` in a temp repository and sorts what it prints.
    fn show_in_repo(commit_spec: &str) -> Shown {
        let temp_dir = tempfile::tempdir().unwrap();
        let global_config = temp_dir.path().join("global.gitconfig");
        fs::write(&global_config, "").unwrap();
        let runner = GitRunner::new(temp_dir.path(), "ssh")
            .with_env("GIT_CONFIG_GLOBAL", &global_config.to_string_lossy())
            .with_env("GIT_CONFIG_NOSYSTEM", "1")
            .with_env("GIT_AUTHOR_NAME", "t")
            .with_env("GIT_AUTHOR_EMAIL", "t@t.invalid")
            .with_env("GIT_COMMITTER_NAME", "t")
            .with_env("GIT_COMMITTER_EMAIL", "t@t.invalid");
        runner.run_checked(&["init", "-q", "-b", "trunk"]).unwrap();
        fs::write(temp_dir.path().join("work.jsonl"), "{\"kept\":1}\n").unwrap();
        runner.run_checked(&["add", "work.jsonl"]).unwrap();
        runner.run_checked(&["commit", "-q", "-m", "one"]).unwrap();
        let output = runner.run(&["show", commit_spec]).unwrap();
        interpret_show(output.exit_ok, &output.stdout, &output.stderr)
    }

    #[test]
    fn interpret_show_reads_what_a_real_git_prints() {
        assert_eq!(
            show_in_repo("HEAD:work.jsonl"),
            Shown::Content("{\"kept\":1}\n".to_string())
        );
        assert_eq!(
            show_in_repo("HEAD:shared.jsonl"),
            Shown::MissingFile,
            "no such file at a good commit"
        );
        assert_eq!(
            show_in_repo("0123456789abcdef0123456789abcdef01234567:work.jsonl"),
            Shown::MissingFile,
            "git words a missing commit like a missing file here, which is why the cycle checks the commit first"
        );
        assert_eq!(
            show_in_repo("not-a-ref:work.jsonl"),
            Shown::UnreadableCommit
        );
    }

    #[test]
    fn interpret_show_treats_an_unknown_failure_as_an_unreadable_commit() {
        assert_eq!(
            interpret_show(false, "", "fatal: something new"),
            Shown::UnreadableCommit
        );
        assert_eq!(
            interpret_show(true, "", ""),
            Shown::Content(String::new()),
            "an empty file is content"
        );
    }

    #[test]
    fn two_edits_to_different_lessons_both_survive_and_nothing_is_queued() {
        let base = [lesson(ID_A, EARLY, "one"), lesson(ID_B, EARLY, "two")];
        let ours = [
            lesson(ID_A, LATE, "one edited here"),
            lesson(ID_B, EARLY, "two"),
        ];
        let theirs = [
            lesson(ID_A, EARLY, "one"),
            lesson(ID_B, MIDDLE, "two edited there"),
        ];
        let plan = plan_domain_merge(&DomainInputs {
            base: &base,
            ours: &ours,
            theirs: &theirs,
            base_queue: &[],
            live_queue: &[],
            theirs_queue: &[],
        });
        assert_eq!(
            plan.merged
                .iter()
                .map(|row| row.resolution.clone().unwrap())
                .collect::<Vec<_>>(),
            vec!["one edited here", "two edited there"]
        );
        assert!(plan.queue.is_empty() && plan.cleared.is_empty() && plan.unqueued.is_empty());
    }

    #[test]
    fn a_real_conflict_keeps_the_newer_value_and_queues_both() {
        let base = [lesson(ID_A, EARLY, "base")];
        let ours = [lesson(ID_A, LATE, "ours")];
        let theirs = [lesson(ID_A, MIDDLE, "theirs")];
        let inputs = DomainInputs {
            base: &base,
            ours: &ours,
            theirs: &theirs,
            base_queue: &[],
            live_queue: &[],
            theirs_queue: &[],
        };
        let plan = plan_domain_merge(&inputs);
        assert_eq!(
            plan.merged[0].resolution.as_deref(),
            Some("ours"),
            "the newer edit is the default winner"
        );
        assert_eq!(plan.queue.len(), 1);
        assert_eq!(
            plan.queue[0]
                .versions
                .iter()
                .map(|version| version.value.clone().unwrap())
                .collect::<Vec<_>>(),
            vec!["ours", "theirs"]
        );
    }

    /// Every machine must compute the same rows and the same queue, whichever side it calls ours.
    #[test]
    fn the_plan_is_the_same_with_the_two_sides_swapped() {
        let base = [lesson(ID_A, EARLY, "base"), lesson(ID_B, EARLY, "keep")];
        let ours = [lesson(ID_A, LATE, "ours"), lesson(ID_B, EARLY, "keep")];
        let theirs = [
            lesson(ID_A, MIDDLE, "theirs"),
            lesson(ID_B, MIDDLE, "keep edited"),
        ];
        let ours_queue = [group(ID_B, &[("x", EARLY), ("y", MIDDLE)])];
        let theirs_queue = [group(ID_B, &[("y", LATE), ("z", EARLY)])];
        let forward = plan_domain_merge(&DomainInputs {
            base: &base,
            ours: &ours,
            theirs: &theirs,
            base_queue: &[],
            live_queue: &ours_queue,
            theirs_queue: &theirs_queue,
        });
        let swapped = plan_domain_merge(&DomainInputs {
            base: &base,
            ours: &theirs,
            theirs: &ours,
            base_queue: &[],
            live_queue: &theirs_queue,
            theirs_queue: &ours_queue,
        });
        assert_eq!(forward, swapped);
    }

    /// The settle step's all-versions winner applies only to a field that conflicted THIS cycle.
    #[test]
    fn an_existing_group_does_not_override_a_field_that_did_not_conflict_this_cycle() {
        let base = [lesson(ID_A, EARLY, "base")];
        let ours = [lesson(ID_A, MIDDLE, "base")];
        let theirs = [lesson(ID_A, LATE, "theirs")];
        let old_group = [group(
            ID_A,
            &[("base", EARLY), ("older", EARLY), ("newest ever", LATE)],
        )];
        let plan = plan_domain_merge(&DomainInputs {
            base: &base,
            ours: &ours,
            theirs: &theirs,
            base_queue: &old_group,
            live_queue: &old_group,
            theirs_queue: &old_group,
        });
        assert_eq!(
            plan.merged[0].resolution.as_deref(),
            Some("theirs"),
            "theirs changed and ours did not: theirs wins, not the group's newest version"
        );
    }

    /// The converse: a field that DOES conflict this cycle takes the newest of all versions of its existing group.
    #[test]
    fn a_field_that_conflicts_this_cycle_takes_the_newest_version_of_its_group() {
        let base = [lesson(ID_A, EARLY, "base")];
        let ours = [lesson(ID_A, MIDDLE, "ours")];
        let theirs = [lesson(ID_A, EARLY, "theirs")];
        let old_group = [group(ID_A, &[("newest ever", LATE), ("older", EARLY)])];
        let plan = plan_domain_merge(&DomainInputs {
            base: &base,
            ours: &ours,
            theirs: &theirs,
            base_queue: &[],
            live_queue: &old_group,
            theirs_queue: &[],
        });
        assert_eq!(plan.merged[0].resolution.as_deref(), Some("newest ever"));
        assert_eq!(plan.queue.len(), 1, "one group per lesson and field");
    }

    #[test]
    fn a_group_of_a_deleted_lesson_is_cleared_and_reported() {
        let base = [lesson(ID_A, EARLY, "base")];
        let orphan = [group(ID_A, &[("a", EARLY), ("b", MIDDLE)])];
        let plan = plan_domain_merge(&DomainInputs {
            base: &base,
            ours: &[],
            theirs: &[],
            base_queue: &orphan,
            live_queue: &orphan,
            theirs_queue: &orphan,
        });
        assert!(plan.merged.is_empty() && plan.queue.is_empty());
        assert_eq!(plan.cleared, orphan);
    }

    #[test]
    fn a_superseded_by_conflict_is_reported_and_not_queued() {
        let base = [lesson(ID_A, EARLY, "x")];
        let ours = [Lesson {
            superseded_by: Some("01KOURS".to_string()),
            updated_at: LATE.to_string(),
            ..base[0].clone()
        }];
        let theirs = [Lesson {
            superseded_by: Some("01KTHEIRS".to_string()),
            updated_at: MIDDLE.to_string(),
            ..base[0].clone()
        }];
        let plan = plan_domain_merge(&DomainInputs {
            base: &base,
            ours: &ours,
            theirs: &theirs,
            base_queue: &[],
            live_queue: &[],
            theirs_queue: &[],
        });
        assert!(plan.queue.is_empty());
        assert_eq!(plan.unqueued.len(), 1);
        assert_eq!(plan.merged[0].superseded_by.as_deref(), Some("01KOURS"));
    }

    #[test]
    fn next_attempt_retries_only_a_lost_race_and_only_while_attempts_remain() {
        assert_eq!(next_attempt(1, PushOutcome::Pushed), AfterPush::Finish);
        assert_eq!(
            next_attempt(PUSH_ATTEMPTS, PushOutcome::Pushed),
            AfterPush::Finish
        );
        assert_eq!(next_attempt(1, PushOutcome::LostRace), AfterPush::Retry);
        assert_eq!(
            next_attempt(PUSH_ATTEMPTS - 1, PushOutcome::LostRace),
            AfterPush::Retry
        );
        assert_eq!(
            next_attempt(PUSH_ATTEMPTS, PushOutcome::LostRace),
            AfterPush::GiveUp,
            "the last attempt lost: stop"
        );
        assert_eq!(
            next_attempt(1, PushOutcome::Refused),
            AfterPush::GiveUp,
            "a refusal is never retried"
        );
    }

    #[test]
    fn new_last_synced_prefers_the_pushed_commit_then_the_tip_then_what_was_remembered() {
        assert_eq!(
            new_last_synced(Some("pushed"), Some("tip"), Some("old")).as_deref(),
            Some("pushed")
        );
        assert_eq!(
            new_last_synced(None, Some("tip"), Some("old")).as_deref(),
            Some("tip")
        );
        assert_eq!(
            new_last_synced(None, None, Some("old")).as_deref(),
            Some("old"),
            "an empty remote, nothing to commit"
        );
        assert_eq!(new_last_synced(None, None, None), None);
    }

    #[test]
    fn domains_to_sync_covers_the_clone_or_the_one_asked_for() {
        let carried: &[Domain] = &[Domain::Work, Domain::Shared];
        assert_eq!(
            domains_to_sync(carried, None).unwrap(),
            vec![Domain::Work, Domain::Shared]
        );
        assert_eq!(
            domains_to_sync(carried, Some(Domain::Shared)).unwrap(),
            vec![Domain::Shared]
        );
        let refusal: String = format!(
            "{:#}",
            domains_to_sync(carried, Some(Domain::Personal)).unwrap_err()
        );
        assert!(
            refusal.contains("personal") && refusal.contains("work, shared"),
            "{refusal}"
        );
        assert!(domains_to_sync(&[], None).unwrap().is_empty());
    }
}
