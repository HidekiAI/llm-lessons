//! One sync of one clone (plan step S4c-2b): the lock, the fetch, the merge of each domain in one database transaction,
//! the reset and rewrite of the clone, the commit and the push. Git only ever fast-forwards: every commit is made on
//! top of the fetched remote tip, so there is no `git pull` and no `git merge` here. The decisions it takes are the
//! pure functions of `cycle.rs`; this file is the order of the steps and the effects.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::access::{
    AccessDecision, ProbeFn, Visibility, WorkGuard, probe_remote, visibility_after_probe,
    work_guard_decision,
};
use super::cycle::{
    AfterPush, CycleOutcome, CycleResult, DomainInputs, DomainPlan, base_from_shown,
    combine_outcomes, domains_to_sync, interpret_show, new_last_synced, next_attempt,
    plan_domain_merge, planned_clone_files, state_after_cycle,
};
use super::files::{
    ClonePaths, SYNC_DIR_NAME, SyncState, from_jsonl, lessons_file_name, queue_file_name,
    read_state, read_text_or_empty, to_jsonl, write_atomically, write_state,
};
use super::git::{
    GitRunner, PushOutcome, classify_push, detect_default_branch, ensure_clone, identity_agent_of,
    redact_credentials, ssh_command, ssh_host_of,
};
use super::{Conflict, group_key};
use crate::config::{CloneSpec, SyncSection, clone_specs, sync_warnings};
use crate::db::{Domain, OpenMode, open};
use crate::journal;
use crate::store::{Lesson, all_lessons, apply_merged, begin_sync_transaction};

/// How often the clone lock is tried before another sync is taken to hold it.
const LOCK_ATTEMPTS: u32 = 5;
/// The pause between two tries of the clone lock.
const LOCK_RETRY_PAUSE: Duration = Duration::from_millis(20);

/// The points of a cycle where a test can stop it, to prove that a crash there loses nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStep {
    AfterFetch,
    /// The live queue is written and the database transaction is still open.
    AfterQueueWrite,
    /// The database transaction is committed; nothing is written to the clone yet.
    AfterDbCommit,
    /// The clone holds the commit; it is not pushed.
    BeforePush,
}

/// What one clone's sync needs to know.
pub struct CloneJob<'job> {
    pub lessons_home: &'job Path,
    pub remote: &'job str,
    pub normalized_remote: &'job str,
    pub ssh_command: String,
    pub domains: &'job [Domain],
    /// True when `domains` is every domain the clone carries. The single remembered BASE describes the whole clone, so
    /// a run of only some domains must not advance it: the others would then look deleted on the next full run. The
    /// cost: until a run covers the whole clone, BASE stays old, so a local delete does not propagate yet and the row
    /// returns from THEIRS (nothing is lost).
    pub whole_clone: bool,
    /// Extra environment for every git call; tests point git at their own config.
    pub git_env: &'job [(String, String)],
    /// Runs the access probes; tests feed fake answers.
    pub probe: ProbeFn<'job>,
    /// The work key guard inputs, set only for a clone that carries `work` with a remote of its own.
    pub work_guard: Option<WorkGuard>,
}

/// Syncs one clone. An `Err` is a failed cycle; a held lock is `Ok(LockHeld)`.
pub fn sync_clone(
    job: &CloneJob,
    on_step: &dyn Fn(SyncStep) -> Result<()>,
    now_secs: u64,
) -> Result<CycleOutcome> {
    let paths = ClonePaths::new(job.lessons_home, job.normalized_remote);
    match lock_clone(&paths)? {
        None => {
            journal!(
                "sync {}: another sync holds the lock",
                redact_credentials(job.remote)
            );
            Ok(CycleOutcome::LockHeld)
        }
        Some(_held_lock) => {
            let state_read: SyncState = read_state(&paths.state_file());
            // The access guards run under the lock and before anything is fetched. A refusal fails the cycle (and
            // counts as a failure) but is never remembered as a pass; the visibility step cannot fail and is recorded
            // either way.
            let (state_before, guard_result): (SyncState, Result<()>) =
                check_access(job, state_read, now_secs);
            let cycle: Result<Option<String>> =
                guard_result.and_then(|()| run_cycle(job, &paths, &state_before, on_step));
            let recorded: CycleResult = match &cycle {
                Ok(last_synced) => CycleResult::Succeeded {
                    // A partial run leaves BASE where it was; see `CloneJob::whole_clone`.
                    last_synced: last_synced.clone().filter(|_| job.whole_clone),
                },
                Err(_) => CycleResult::Failed,
            };
            let state_written: Result<()> = write_state(
                &paths.state_file(),
                &state_after_cycle(state_before, recorded, now_secs),
            );
            // The cycle's own error is the one worth reporting; a state file that cannot be written is only logged
            // then.
            match (cycle, state_written) {
                (Ok(_), Ok(())) => Ok(CycleOutcome::Synced),
                (Ok(_), Err(state_error)) => Err(state_error),
                (Err(cycle_error), Ok(())) => Err(cycle_error),
                (Err(cycle_error), Err(state_error)) => {
                    journal!(
                        "sync {}: the state file was not written either: {state_error:#}",
                        redact_credentials(job.remote)
                    );
                    Err(cycle_error)
                }
            }
        }
    }
}

/// The access step of one cycle: the work key guard for a clone with its own `work` remote, then the public-visibility
/// probe. A refused guard is an error naming why; the returned state carries the visibility the probe found.
fn check_access(job: &CloneJob, state: SyncState, now_secs: u64) -> (SyncState, Result<()>) {
    // The work key guard compares ssh keys, and over https or a local path no key is ever used, so every key would read
    // the same: such a work remote is refused without a probe instead of being judged by a verdict that means nothing there.
    let guard_result: Result<()> = job.work_guard.as_ref().map_or(Ok(()), |guard| {
        ssh_host_of(job.remote).map_or_else(
            || bail!("work is refused: the work key guard needs an ssh remote, and this remote has no ssh form"),
            |_ssh_host| match work_guard_decision(job.remote, guard, job.probe) {
                AccessDecision::Allow => Ok(()),
                AccessDecision::Refuse(reason) => bail!("work is refused: {reason}"),
            },
        )
    });
    // The visibility probe never depends on the work key guard: a remote that is public and refused is still recorded
    // public, so it is still warned about. The refusal reason and a change of visibility reach the journal, so a refused cycle or a new PUBLIC warning can be
    // traced from the journal alone. The remote is redacted: it may carry a token.
    let visibility_before: Visibility = state.visibility;
    let probed_state: SyncState = visibility_after_probe(state, job.remote, job.probe, now_secs);
    journal!(
        "access {}: work key guard {}, visibility {visibility_before:?} -> {:?}",
        redact_credentials(job.remote),
        match (&job.work_guard, &guard_result) {
            (None, _) => "not applicable".to_string(),
            (Some(_), Ok(())) => "allowed".to_string(),
            (Some(_), Err(refusal)) => format!("refused ({refusal:#})"),
        },
        probed_state.visibility
    );
    (probed_state, guard_result)
}

/// Takes the clone lock without waiting: the lock is the open file, so the operating system frees it when the process
/// dies and there is no stale lock to clean up. `None` means another process holds it.
fn lock_clone(paths: &ClonePaths) -> Result<Option<File>> {
    let lock_path: PathBuf = paths.lock_file();
    if let Some(sync_dir) = lock_path.parent() {
        fs::create_dir_all(sync_dir).with_context(|| format!("create {}", sync_dir.display()))?;
    }
    let lock_file: File = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("open {}", lock_path.display()))?;
    match try_lock_for(&lock_file, LOCK_ATTEMPTS) {
        Ok(true) => Ok(Some(lock_file)),
        Ok(false) => Ok(None),
        Err(lock_error) => Err(lock_error).with_context(|| format!("lock {}", lock_path.display())),
    }
}

/// Tries the lock up to `attempts_left` times with a short pause, so a real holder is still given up on quickly. A
/// single failed try is not proof of a holder: a process that spawns a child (every `git` call does) copies its open
/// files for an instant, and a lock released just before can still look held while that copy exists.
fn try_lock_for(lock_file: &File, attempts_left: u32) -> std::io::Result<bool> {
    match (lock_file.try_lock(), attempts_left) {
        (Ok(()), _) => Ok(true),
        (Err(TryLockError::WouldBlock), 0 | 1) => Ok(false),
        (Err(TryLockError::WouldBlock), _) => {
            std::thread::sleep(LOCK_RETRY_PAUSE);
            try_lock_for(lock_file, attempts_left - 1)
        }
        (Err(TryLockError::Error(lock_error)), _) => Err(lock_error),
    }
}

/// What every step of one cycle shares.
struct Cycle<'cycle> {
    job: &'cycle CloneJob<'cycle>,
    paths: &'cycle ClonePaths,
    state: &'cycle SyncState,
    branch: &'cycle str,
    clone_dir: PathBuf,
    runner: GitRunner,
    on_step: &'cycle dyn Fn(SyncStep) -> Result<()>,
}

/// A git runner for `directory` with the job's ssh command and test environment.
fn runner_in(job: &CloneJob, directory: &Path) -> GitRunner {
    job.git_env.iter().fold(
        GitRunner::new(directory, &job.ssh_command),
        |runner, (name, value)| runner.with_env(name, value),
    )
}

/// The cycle under the lock: find the branch, set the clone up, then try to push, retrying a lost race. Returns the
/// commit to remember as BASE.
fn run_cycle(
    job: &CloneJob,
    paths: &ClonePaths,
    state: &SyncState,
    on_step: &dyn Fn(SyncStep) -> Result<()>,
) -> Result<Option<String>> {
    let sync_dir: PathBuf = job.lessons_home.join(SYNC_DIR_NAME);
    fs::create_dir_all(&sync_dir).with_context(|| format!("create {}", sync_dir.display()))?;
    let branch: String = detect_default_branch(&runner_in(job, &sync_dir), job.remote)?;
    let clone_dir: PathBuf = paths.clone_dir();
    let runner: GitRunner = runner_in(job, &clone_dir);
    ensure_clone(&runner, &clone_dir, job.remote, &branch, &job.ssh_command)?;
    let cycle = Cycle {
        job,
        paths,
        state,
        branch: &branch,
        clone_dir,
        runner,
        on_step,
    };
    attempt(&cycle, 1)
}

/// The commit `revision` names, or none when it names nothing (an empty remote, a branch with no commits yet).
fn verified_commit(runner: &GitRunner, revision: &str) -> Result<Option<String>> {
    let output = runner.run(&["rev-parse", "--verify", "-q", revision])?;
    Ok(output.exit_ok.then(|| output.stdout.trim().to_string()))
}

/// The remote's tip of `branch`, or none when the remote is still empty.
fn remote_tip(runner: &GitRunner, branch: &str) -> Result<Option<String>> {
    verified_commit(runner, &format!("refs/remotes/origin/{branch}"))
}

/// The text of `file` at `commit` (BASE or THEIRS), empty for no commit or a file the commit does not have.
fn text_at(runner: &GitRunner, commit: Option<&str>, file: &str) -> Result<String> {
    match commit {
        None => Ok(String::new()),
        Some(commit_id) => {
            let shown = runner.run(&["show", &format!("{commit_id}:{file}")])?;
            Ok(base_from_shown(interpret_show(shown.exit_ok, &shown.stdout, &shown.stderr)).text)
        }
    }
}

/// One try at the whole cycle; `attempt_number` counts from 1. A lost push race calls the next attempt, which fetches
/// again and merges against the database this attempt already updated; merging identical rows is a no-op.
fn attempt(cycle: &Cycle, attempt_number: u32) -> Result<Option<String>> {
    cycle.runner.run_checked(&["fetch", "-q", "origin"])?;
    (cycle.on_step)(SyncStep::AfterFetch)?;
    let tip: Option<String> = remote_tip(&cycle.runner, cycle.branch)?;
    journal!("sync attempt {attempt_number}: remote tip {tip:?}");
    let base_commit: Option<String> = readable_base_commit(cycle);
    let plans: Vec<(Domain, DomainPlan)> = cycle
        .job
        .domains
        .iter()
        .map(|domain| {
            merge_domain(cycle, *domain, base_commit.as_deref(), tip.as_deref())
                .map(|plan| (*domain, plan))
        })
        .collect::<Result<Vec<(Domain, DomainPlan)>>>()?;
    match &tip {
        Some(tip_commit) => {
            // Drops any file a crashed cycle left uncommitted, and follows the remote.
            cycle
                .runner
                .run_checked(&["checkout", "-q", "-f", "-B", cycle.branch, tip_commit])?;
        }
        // No remote tip to reset to: put the index back to what the clone's own commit holds (nothing, when it has
        // none), so a file a crashed cycle left staged is not committed by a later run that covers other domains.
        // Planned files are staged again right below.
        None => {
            let own_commit: Option<String> = verified_commit(&cycle.runner, "HEAD")?;
            let index_reset: &[&str] = own_commit
                .as_ref()
                .map_or(&["read-tree", "--empty"], |_| &["reset", "-q"]);
            cycle.runner.run_checked(index_reset)?;
        }
    }
    plans
        .iter()
        .try_for_each(|(domain, plan)| write_clone_files(cycle, *domain, plan))?;
    // `diff --quiet` exits 1 for changes and 128 for a git error; both count, and the commit then fails loudly.
    let has_staged_changes: bool = !cycle.runner.run(&["diff", "--cached", "--quiet"])?.exit_ok;
    if has_staged_changes {
        let domain_names: Vec<&str> = plans.iter().map(|(domain, _)| domain.as_str()).collect();
        cycle.runner.run_checked(&[
            "commit",
            "-q",
            "--no-verify", // skips pre-commit and commit-msg; the user's hooks are for their own repositories
            "-m",
            &format!("sync {}", domain_names.join(", ")),
        ])?;
    }
    let head: Option<String> = verified_commit(&cycle.runner, "HEAD")?;
    // Also a commit left by a crashed cycle on an unborn remote (with a remote tip the checkout above discarded it):
    // nothing new to commit, but the remote does not have it yet.
    let needs_push: bool = head.is_some() && head != tip;
    (cycle.on_step)(SyncStep::BeforePush)?;
    if !needs_push {
        return Ok(new_last_synced(
            None,
            tip.as_deref(),
            cycle.state.last_synced_commit.as_deref(),
        ));
    }
    let pushed = cycle.runner.run(&[
        "push",
        "-q",
        "--no-verify", // skips pre-push; a gate of the user's for their work repositories must not block a sync
        "origin",
        &format!("HEAD:refs/heads/{}", cycle.branch),
    ])?;
    let outcome: PushOutcome = classify_push(pushed.exit_ok, &pushed.stderr);
    match next_attempt(attempt_number, outcome) {
        AfterPush::Finish => Ok(new_last_synced(
            head.as_deref(),
            tip.as_deref(),
            cycle.state.last_synced_commit.as_deref(),
        )),
        AfterPush::Retry => attempt(cycle, attempt_number + 1),
        AfterPush::GiveUp => bail!(
            "git push failed after {attempt_number} attempt(s): {}",
            redact_credentials(pushed.stderr.trim())
        ),
    }
}

/// The commit remembered as BASE, if the clone can still read it. Git words a missing commit like a missing file, so
/// the commit is checked first; an unreadable one means an empty BASE (a union that deletes nothing), logged.
fn readable_base_commit(cycle: &Cycle) -> Option<String> {
    cycle.state.last_synced_commit.clone().filter(|commit_id| {
        let readable: bool = cycle
            .runner
            .run(&["cat-file", "-e", &format!("{commit_id}^{{commit}}")])
            .is_ok_and(|output| output.exit_ok);
        if !readable {
            journal!(
                "sync: the remembered commit {commit_id} is unreadable, merging from an empty BASE"
            );
        }
        readable
    })
}

/// Merges one domain in one database transaction: read OURS, merge, apply, write the live queue, then commit. The queue
/// is written BEFORE the commit, so a crash between the two rolls the database back and leaves a queue whose values
/// are all still in the database or in THEIRS; the reverse order could lose the losing value.
fn merge_domain(
    cycle: &Cycle,
    domain: Domain,
    base_commit: Option<&str>,
    tip: Option<&str>,
) -> Result<DomainPlan> {
    let lessons_name: String = lessons_file_name(domain);
    let queue_name: String = queue_file_name(domain);
    let fn_lessons_at = |commit: Option<&str>| -> Result<Vec<Lesson>> {
        let text: String = text_at(&cycle.runner, commit, &lessons_name)?;
        from_jsonl(&text, |row: &Lesson| row.id.clone())
            .with_context(|| format!("{lessons_name} at {commit:?}"))
    };
    let fn_queue_at = |commit: Option<&str>| -> Result<Vec<Conflict>> {
        let text: String = text_at(&cycle.runner, commit, &queue_name)?;
        from_jsonl(&text, group_key).with_context(|| format!("{queue_name} at {commit:?}"))
    };
    let (base, theirs): (Vec<Lesson>, Vec<Lesson>) =
        (fn_lessons_at(base_commit)?, fn_lessons_at(tip)?);
    let (base_queue, theirs_queue): (Vec<Conflict>, Vec<Conflict>) =
        (fn_queue_at(base_commit)?, fn_queue_at(tip)?);
    let live_queue_path: PathBuf = cycle.paths.live_queue(domain);
    let live_queue: Vec<Conflict> =
        from_jsonl(&read_text_or_empty(&live_queue_path)?, group_key)
            .with_context(|| format!("parse live queue {}", live_queue_path.display()))?;
    let database_path: PathBuf = domain.file_path(cycle.job.lessons_home);
    let mut mut_conn = open(&database_path, OpenMode::Existing).with_context(|| {
        format!(
            "sync of {} needs {} (run `lessons init`)",
            domain.as_str(),
            database_path.display()
        )
    })?;
    let transaction = begin_sync_transaction(&mut mut_conn)?;
    let ours: Vec<Lesson> = all_lessons(&transaction)?;
    let plan: DomainPlan = plan_domain_merge(&DomainInputs {
        base: &base,
        ours: &ours,
        theirs: &theirs,
        base_queue: &base_queue,
        live_queue: &live_queue,
        theirs_queue: &theirs_queue,
    });
    let counts = apply_merged(&transaction, &ours, &plan.merged)?;
    journal!(
        "sync {}: {counts:?}, {} queued group(s), {} cleared, {} superseded_by conflict(s) not queued",
        domain.as_str(),
        plan.queue.len(),
        plan.cleared.len(),
        plan.unqueued.len()
    );
    write_live_queue(&live_queue_path, &plan.queue)?;
    (cycle.on_step)(SyncStep::AfterQueueWrite)?;
    transaction
        .commit()
        .with_context(|| format!("commit {}", database_path.display()))?;
    (cycle.on_step)(SyncStep::AfterDbCommit)?;
    Ok(plan)
}

/// The live queue file: written atomically while there are conflicts, removed when there are none.
fn write_live_queue(path: &Path, queue: &[Conflict]) -> Result<()> {
    let queue_text: String = to_jsonl(queue, group_key)?;
    if queue_text.is_empty() {
        remove_if_present(path)
    } else {
        write_atomically(path, &queue_text)
    }
}

fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(remove_error) if remove_error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(remove_error) => {
            Err(remove_error).with_context(|| format!("remove {}", path.display()))
        }
    }
}

/// Writes the files one domain commits into the clone and stages them. `-f` because a data repository set up with a
/// whitelist `.gitignore` would otherwise ignore them; a removed file is removed from the index too.
fn write_clone_files(cycle: &Cycle, domain: Domain, plan: &DomainPlan) -> Result<()> {
    planned_clone_files(domain, plan)?
        .iter()
        .try_for_each(|file| {
            let file_path: PathBuf = cycle.clone_dir.join(&file.name);
            match &file.contents {
                Some(contents) => {
                    write_atomically(&file_path, contents)?;
                    cycle
                        .runner
                        .run_checked(&["add", "-f", "-A", "--", &file.name])
                        .map(|_| ())
                }
                None => {
                    remove_if_present(&file_path)?;
                    cycle
                        .runner
                        .run_checked(&[
                            "rm",
                            "-q",
                            "-f",
                            "--cached",
                            "--ignore-unmatch",
                            "--",
                            &file.name,
                        ])
                        .map(|_| ())
                }
            }
        })
}

/// The work key guard inputs for `clone`, or none: only a clone that carries `work` alone has a remote of its own; when `work`
/// shares its remote with another domain the guard is skipped and the other domains' keys are the same key. Every
/// other clone's key that differs from the work key is probed, named by the domains it serves.
fn work_guard_of(clone: &CloneSpec, clones: &[CloneSpec]) -> Option<WorkGuard> {
    // One entry per distinct other key, named by every domain it serves, so a key shared by two clones is probed once. The
    // map is built only for a work clone, in one pass, instead of a set plus a rescan of every clone per key.
    (clone.domains == [Domain::Work]).then(|| {
        let domains_by_other_key: BTreeMap<&PathBuf, Vec<&str>> = clones
            .iter()
            .filter(|other_clone| other_clone.ssh_key != clone.ssh_key)
            .fold(BTreeMap::new(), |mut mut_domains_by_key, other_clone| {
                mut_domains_by_key
                    .entry(&other_clone.ssh_key)
                    .or_default()
                    .extend(other_clone.domains.iter().map(|domain| domain.as_str()));
                mut_domains_by_key
            });
        WorkGuard {
            work_key: clone.ssh_key.clone(),
            other_keys: domains_by_other_key
                .into_iter()
                .map(|(other_key, domain_names)| (domain_names.join("+"), other_key.clone()))
                .collect(),
        }
    })
}

/// Syncs every configured clone, or the ones carrying `requested`; a clone that fails does not stop the others, and
/// each outcome is printed. Returns how the whole run ended.
pub fn sync_configured(
    lessons_home: &Path,
    sync_section: &SyncSection,
    requested: Option<Domain>,
    git_env: &[(String, String)],
) -> Result<CycleOutcome> {
    let clones: Vec<CloneSpec> = clone_specs(sync_section)?;
    sync_warnings(&clones)
        .iter()
        .for_each(|warning| eprintln!("lessons sync: {warning}"));
    let all_domains: Vec<Domain> = clones
        .iter()
        .flat_map(|clone| clone.domains.iter().copied())
        .collect();
    // A domain no clone carries is an error that names it, whatever clone it would have been looked for in.
    domains_to_sync(&all_domains, requested)?;
    if all_domains.is_empty() {
        println!(
            "lessons sync: nothing configured; see docs/config.sample.toml for the [sync.<domain>] sections"
        );
    }
    let now_secs: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since_epoch| since_epoch.as_secs()); // a clock before 1970 counts as 0 rather than failing the run
    let outcomes: Vec<CycleOutcome> = clones
        .iter()
        .filter(|clone| requested.is_none_or(|domain| clone.domains.contains(&domain)))
        .map(|clone| {
            let shown_remote: String = redact_credentials(&clone.remote);
            let outcome: Result<CycleOutcome> = domains_to_sync(&clone.domains, requested)
                .and_then(|domains| {
                    let job = CloneJob {
                        lessons_home,
                        remote: &clone.remote,
                        normalized_remote: &clone.normalized_remote,
                        ssh_command: ssh_command(
                            &clone.ssh_key,
                            identity_agent_of(&clone.remote).as_deref(),
                        ),
                        domains: &domains,
                        whole_clone: domains.len() == clone.domains.len(),
                        git_env,
                        probe: &probe_remote,
                        work_guard: work_guard_of(clone, &clones),
                    };
                    sync_clone(&job, &|_| Ok(()), now_secs)
                });
            match outcome {
                Ok(CycleOutcome::LockHeld) => {
                    println!("{shown_remote}: another sync is running, nothing to do");
                    CycleOutcome::LockHeld
                }
                Ok(CycleOutcome::Synced) => {
                    println!("{shown_remote}: synced");
                    CycleOutcome::Synced
                }
                // `sync_clone` reports a failed cycle as `Err`, so this arm is not reached today; it is listed so a new
                // variant of the outcome cannot be printed as "synced".
                Ok(CycleOutcome::Failed) => {
                    eprintln!("lessons sync: {shown_remote}: the cycle failed");
                    CycleOutcome::Failed
                }
                Err(sync_error) => {
                    eprintln!(
                        "lessons sync: {shown_remote}: {}",
                        redact_credentials(&format!("{sync_error:#}"))
                    );
                    CycleOutcome::Failed
                }
            }
        })
        .collect();
    Ok(combine_outcomes(&outcomes))
}

// End-to-end tests: real git against bare repositories in temp directories (local paths, no network, no ssh) and real
// SQLite files, one `LESSONS_HOME` per machine. A crash is a step callback that returns an error, never a timeout.
// Races are forced, in both orders, by a callback that lets the other machine push first. Nothing depends on timing.
#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs;
    use std::path::PathBuf;

    use rusqlite::Connection;

    use super::*;
    use crate::db::{OpenMode, open};
    use crate::store::{Lesson, LessonPatch, NewLesson, all_lessons, find_by_id, insert, update};
    use crate::sync::access::{ProbeClass, ProbeRequest, Visibility, WorkGuard};
    use crate::sync::files::{ClonePaths, SyncState, from_jsonl, read_state, write_state};
    use crate::sync::git::{GitRunner, SYNC_FALLBACK_BRANCH};

    /// The branch an EMPTY remote gets: it has no commits, so it has no default branch to follow and the cycle falls
    /// back.
    const REMOTE_BRANCH: &str = SYNC_FALLBACK_BRANCH;
    const SOME_NOW: u64 = 1_762_000_000;
    const ONLY_SHARED: &[Domain] = &[Domain::Shared];
    /// `rwxr-xr-x`: a hook only runs when it is executable.
    #[cfg(unix)]
    const EXECUTABLE_MODE: u32 = 0o755;

    /// A machine: its own `LESSONS_HOME`, its own `shared.db`, and a git that ignores the tester's configuration.
    struct Machine {
        home: PathBuf,
        git_env: Vec<(String, String)>,
    }

    impl Machine {
        fn new(root: &Path, name: &str) -> Machine {
            let home: PathBuf = root.join(name);
            fs::create_dir_all(&home).unwrap();
            let global_config: PathBuf = home.join("global.gitconfig");
            fs::write(&global_config, "").unwrap();
            let git_env: Vec<(String, String)> = [
                (
                    "GIT_CONFIG_GLOBAL",
                    global_config.to_string_lossy().into_owned(),
                ),
                ("GIT_CONFIG_NOSYSTEM", "1".to_string()),
                ("GIT_AUTHOR_NAME", "sync test".to_string()),
                ("GIT_AUTHOR_EMAIL", "sync@test.invalid".to_string()),
                ("GIT_COMMITTER_NAME", "sync test".to_string()),
                ("GIT_COMMITTER_EMAIL", "sync@test.invalid".to_string()),
            ]
            .into_iter()
            .map(|(name, value)| (name.to_string(), value))
            .collect();
            drop(open(&Domain::Shared.file_path(&home), OpenMode::Create).unwrap());
            Machine { home, git_env }
        }

        fn conn(&self) -> Connection {
            open(&Domain::Shared.file_path(&self.home), OpenMode::Existing).unwrap()
        }

        fn add(&self, title: &str) -> String {
            insert(
                &self.conn(),
                &NewLesson {
                    title: title.to_string(),
                    problem: format!("problem of {title}"),
                    resolution: Some("first resolution".to_string()),
                    ..NewLesson::default()
                },
            )
            .unwrap()
        }

        fn set_resolution(&self, lesson_id: &str, text: &str) {
            let patch = LessonPatch {
                resolution: Some(text.to_string()),
                ..LessonPatch::default()
            };
            assert!(update(&self.conn(), lesson_id, &patch).unwrap());
        }

        fn set_tags(&self, lesson_id: &str, text: &str) {
            let patch = LessonPatch {
                tags: Some(text.to_string()),
                ..LessonPatch::default()
            };
            assert!(update(&self.conn(), lesson_id, &patch).unwrap());
        }

        fn delete(&self, lesson_id: &str) {
            self.conn()
                .execute("DELETE FROM lesson WHERE id = ?1", [lesson_id])
                .unwrap();
        }

        fn lesson(&self, lesson_id: &str) -> Option<Lesson> {
            find_by_id(&self.conn(), lesson_id).unwrap()
        }

        fn ids(&self) -> Vec<String> {
            all_lessons(&self.conn())
                .unwrap()
                .into_iter()
                .map(|row| row.id)
                .collect()
        }

        fn job<'job>(&'job self, remote: &'job str) -> CloneJob<'job> {
            CloneJob {
                lessons_home: &self.home,
                remote,
                normalized_remote: remote,
                ssh_command: "ssh".to_string(),
                domains: ONLY_SHARED,
                whole_clone: true,
                git_env: &self.git_env,
                probe: &|_| ProbeClass::Inconclusive,
                work_guard: None,
            }
        }

        fn sync_with(
            &self,
            remote: &str,
            on_step: &dyn Fn(SyncStep) -> Result<()>,
        ) -> Result<CycleOutcome> {
            sync_clone(&self.job(remote), on_step, SOME_NOW)
        }

        fn sync(&self, remote: &str) -> CycleOutcome {
            self.sync_with(remote, &|_| Ok(())).unwrap()
        }

        fn paths(&self, remote: &str) -> ClonePaths {
            ClonePaths::new(&self.home, remote)
        }

        /// A git runner inside this machine's clone of `remote`.
        fn git(&self, remote: &str) -> GitRunner {
            self.git_env.iter().fold(
                GitRunner::new(&self.paths(remote).clone_dir(), "ssh"),
                |runner, (name, value)| runner.with_env(name, value),
            )
        }

        fn live_queue_text(&self, remote: &str) -> String {
            fs::read_to_string(self.paths(remote).live_queue(Domain::Shared)).unwrap_or_default()
        }
    }

    /// A git runner that ignores the tester's global and system configuration, like `Machine`'s.
    fn isolated_git(directory: &Path) -> GitRunner {
        [
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
        ]
        .into_iter()
        .fold(GitRunner::new(directory, "ssh"), |runner, (name, value)| {
            runner.with_env(name, value)
        })
    }

    /// A bare remote whose HEAD names `REMOTE_BRANCH`, as a local path.
    fn bare_remote(root: &Path) -> String {
        let remote_path: PathBuf = root.join("remote.git");
        let runner = isolated_git(root);
        runner
            .run_checked(&[
                "init",
                "-q",
                "--bare",
                "-b",
                REMOTE_BRANCH,
                &remote_path.to_string_lossy(),
            ])
            .unwrap();
        remote_path.to_string_lossy().into_owned()
    }

    fn remote_commit_count(root: &Path, remote: &str) -> usize {
        let output: String = isolated_git(root)
            .run(&["--git-dir", remote, "rev-list", "--count", REMOTE_BRANCH])
            .map(|git_output| git_output.stdout)
            .unwrap_or_default();
        output.trim().parse().unwrap_or(0)
    }

    fn remote_file(root: &Path, remote: &str, name: &str) -> Option<String> {
        let output = isolated_git(root)
            .run(&[
                "--git-dir",
                remote,
                "show",
                &format!("{REMOTE_BRANCH}:{name}"),
            ])
            .unwrap();
        output.exit_ok.then_some(output.stdout)
    }

    fn crash_at(step_to_crash_at: SyncStep) -> impl Fn(SyncStep) -> Result<()> {
        move |step| {
            if step == step_to_crash_at {
                bail!("injected crash at {step:?}")
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn first_sync_unions_two_machines_and_deletes_nothing() {
        let temp_dir = tempfile::tempdir().unwrap();
        let remote = bare_remote(temp_dir.path());
        let (left, right) = (
            Machine::new(temp_dir.path(), "left"),
            Machine::new(temp_dir.path(), "right"),
        );
        let left_id = left.add("left lesson");
        let right_id = right.add("right lesson");
        assert_eq!(left.sync(&remote), CycleOutcome::Synced);
        assert_eq!(right.sync(&remote), CycleOutcome::Synced);
        assert_eq!(left.sync(&remote), CycleOutcome::Synced);
        let mut mut_expected: Vec<String> = vec![left_id, right_id];
        mut_expected.sort();
        assert_eq!(left.ids(), mut_expected);
        assert_eq!(right.ids(), mut_expected);
    }

    #[test]
    fn an_empty_remote_gets_its_first_push_on_the_remotes_default_branch() {
        let temp_dir = tempfile::tempdir().unwrap();
        let remote = bare_remote(temp_dir.path());
        let machine = Machine::new(temp_dir.path(), "only");
        let lesson_id = machine.add("first");
        assert_eq!(remote_commit_count(temp_dir.path(), &remote), 0);
        assert_eq!(machine.sync(&remote), CycleOutcome::Synced);
        assert_eq!(remote_commit_count(temp_dir.path(), &remote), 1);
        assert!(
            remote_file(temp_dir.path(), &remote, "shared.jsonl")
                .unwrap()
                .contains(&lesson_id)
        );
        let state: SyncState = read_state(&machine.paths(&remote).state_file());
        assert!(state.last_synced_commit.is_some() && state.consecutive_failures == 0);
    }

    /// A remote that already has commits on `trunk` is followed: nothing appears on `main`.
    #[test]
    fn a_remote_whose_default_branch_is_trunk_is_followed_not_forced_to_main() {
        let temp_dir = tempfile::tempdir().unwrap();
        let remote = bare_remote(temp_dir.path());
        let seed = Machine::new(temp_dir.path(), "seed");
        seed.add("seeded on main");
        seed.sync(&remote); // the first push creates `main` on the empty remote
        let trunk_remote: String = temp_dir
            .path()
            .join("trunk-remote.git")
            .to_string_lossy()
            .into_owned();
        let runner = isolated_git(temp_dir.path());
        runner
            .run_checked(&["init", "-q", "--bare", "-b", "trunk", &trunk_remote])
            .unwrap();
        seed.git(&remote)
            .run_checked(&["push", "-q", &trunk_remote, "HEAD:refs/heads/trunk"])
            .unwrap();
        let follower = Machine::new(temp_dir.path(), "follower");
        let follower_id = follower.add("from the follower");
        assert_eq!(follower.sync(&trunk_remote), CycleOutcome::Synced);
        let branches: String = runner
            .run_checked(&["--git-dir", &trunk_remote, "branch", "--list"])
            .unwrap();
        assert!(
            branches.contains("trunk") && !branches.contains("main"),
            "{branches}"
        );
        let shown = runner
            .run(&["--git-dir", &trunk_remote, "show", "trunk:shared.jsonl"])
            .unwrap();
        assert!(shown.stdout.contains(&follower_id));
    }

    fn sync_config(remote: &str) -> crate::config::SyncConfig {
        crate::config::SyncConfig {
            remote: remote.to_string(),
            ssh_key: PathBuf::from("/keys/not-used-for-a-local-remote"),
            update: crate::config::UpdateMode::OnDemand,
            pull_every_minutes: 1,
        }
    }

    #[test]
    fn sync_configured_syncs_the_configured_clone_and_names_a_domain_nobody_carries() {
        let temp_dir = tempfile::tempdir().unwrap();
        let remote = bare_remote(temp_dir.path());
        let machine = Machine::new(temp_dir.path(), "only");
        let lesson_id = machine.add("through the command layer");
        let configured = SyncSection {
            shared: Some(sync_config(&remote)),
            ..SyncSection::default()
        };
        assert_eq!(
            sync_configured(&machine.home, &configured, None, &machine.git_env).unwrap(),
            CycleOutcome::Synced
        );
        assert!(
            remote_file(temp_dir.path(), &remote, "shared.jsonl")
                .unwrap()
                .contains(&lesson_id)
        );
        assert_eq!(
            sync_configured(
                &machine.home,
                &configured,
                Some(Domain::Shared),
                &machine.git_env
            )
            .unwrap(),
            CycleOutcome::Synced
        );
        let refusal: String = format!(
            "{:#}",
            sync_configured(
                &machine.home,
                &configured,
                Some(Domain::Work),
                &machine.git_env
            )
            .unwrap_err()
        );
        assert!(
            refusal.contains("work") && refusal.contains("shared"),
            "{refusal}"
        );
    }

    #[test]
    fn nothing_configured_is_a_success_and_one_failing_clone_does_not_stop_the_other() {
        let temp_dir = tempfile::tempdir().unwrap();
        let machine = Machine::new(temp_dir.path(), "only");
        let nothing = SyncSection::default();
        assert_eq!(
            sync_configured(&machine.home, &nothing, None, &machine.git_env).unwrap(),
            CycleOutcome::Synced
        );
        let good_remote = bare_remote(temp_dir.path());
        let missing_remote: String = temp_dir
            .path()
            .join("no-such-remote")
            .to_string_lossy()
            .into_owned();
        drop(open(&Domain::Personal.file_path(&machine.home), OpenMode::Create).unwrap());
        let lesson_id = machine.add("must still sync");
        let configured = SyncSection {
            personal: Some(sync_config(&missing_remote)),
            shared: Some(sync_config(&good_remote)),
            ..SyncSection::default()
        };
        assert_eq!(
            sync_configured(&machine.home, &configured, None, &machine.git_env).unwrap(),
            CycleOutcome::Failed,
            "one clone failed"
        );
        assert!(
            remote_file(temp_dir.path(), &good_remote, "shared.jsonl")
                .unwrap()
                .contains(&lesson_id),
            "the other clone was still synced"
        );
        let state: SyncState = read_state(&machine.paths(&missing_remote).state_file());
        assert_eq!(state.consecutive_failures, 1, "the failed clone counted it");
    }

    /// The user's own hooks (a pre-push gate, a commit scanner, in a hooks directory or as git's config-based
    /// `[hook "name"]`) are meant for their work repositories: none of the pre-commit and pre-push ones may block a
    /// sync. Config-based hooks of other events (post-commit, reference-transaction) are not turned off on git 2.54+,
    /// so they are not covered here.
    #[cfg(unix)]
    #[test]
    fn the_users_global_hooks_never_block_a_sync() {
        use std::os::unix::fs::PermissionsExt;
        let temp_dir = tempfile::tempdir().unwrap();
        let remote = bare_remote(temp_dir.path());
        let machine = Machine::new(temp_dir.path(), "only");
        let hooks_dir: PathBuf = temp_dir.path().join("global-hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        ["pre-commit", "commit-msg", "pre-push"]
            .iter()
            .for_each(|hook_name| {
                let hook_path: PathBuf = hooks_dir.join(hook_name);
                fs::write(
                    &hook_path,
                    "#!/bin/sh\necho 'blocked by a global hook' >&2\nexit 1\n",
                )
                .unwrap();
                fs::set_permissions(&hook_path, fs::Permissions::from_mode(EXECUTABLE_MODE))
                    .unwrap();
            });
        let config_hooks: &str = concat!(
            "[hook \"gate\"]\n\tevent = pre-push\n\tcommand = false\n",
            "[hook \"scan\"]\n\tevent = pre-commit\n\tcommand = false\n"
        );
        fs::write(
            machine.home.join("global.gitconfig"),
            format!(
                "[core]\n\thooksPath = {}\n{config_hooks}",
                hooks_dir.display()
            ),
        )
        .unwrap();
        let lesson_id = machine.add("synced past the user's hooks");
        assert_eq!(machine.sync(&remote), CycleOutcome::Synced);
        assert!(
            remote_file(temp_dir.path(), &remote, "shared.jsonl")
                .unwrap()
                .contains(&lesson_id)
        );
    }

    #[test]
    fn a_second_sync_with_no_change_makes_no_commit() {
        let temp_dir = tempfile::tempdir().unwrap();
        let remote = bare_remote(temp_dir.path());
        let machine = Machine::new(temp_dir.path(), "only");
        machine.add("first");
        machine.sync(&remote);
        let after_first: usize = remote_commit_count(temp_dir.path(), &remote);
        machine.sync(&remote);
        machine.sync(&remote);
        assert_eq!(
            remote_commit_count(temp_dir.path(), &remote),
            after_first,
            "back-to-back syncs commit nothing"
        );
    }

    #[test]
    fn edits_to_different_fields_of_one_lesson_both_survive_with_no_queue() {
        let temp_dir = tempfile::tempdir().unwrap();
        let remote = bare_remote(temp_dir.path());
        let (left, right) = (
            Machine::new(temp_dir.path(), "left"),
            Machine::new(temp_dir.path(), "right"),
        );
        let lesson_id = left.add("shared lesson");
        left.sync(&remote);
        right.sync(&remote);
        left.set_resolution(&lesson_id, "left resolution");
        right.set_tags(&lesson_id, "right tags");
        left.sync(&remote);
        right.sync(&remote);
        left.sync(&remote);
        for machine in [&left, &right] {
            let row: Lesson = machine.lesson(&lesson_id).unwrap();
            assert_eq!(row.resolution.as_deref(), Some("left resolution"));
            assert_eq!(row.tags.as_deref(), Some("right tags"));
            assert_eq!(
                machine.live_queue_text(&remote),
                "",
                "different fields never queue"
            );
        }
    }

    #[test]
    fn a_real_conflict_keeps_one_value_queues_both_and_converges() {
        let temp_dir = tempfile::tempdir().unwrap();
        let remote = bare_remote(temp_dir.path());
        let (left, right) = (
            Machine::new(temp_dir.path(), "left"),
            Machine::new(temp_dir.path(), "right"),
        );
        let lesson_id = left.add("contested");
        left.sync(&remote);
        right.sync(&remote);
        left.set_resolution(&lesson_id, "left says this");
        right.set_resolution(&lesson_id, "right says that");
        left.sync(&remote);
        right.sync(&remote);
        left.sync(&remote);
        let winner_left: Option<String> = left.lesson(&lesson_id).unwrap().resolution;
        assert_eq!(
            winner_left,
            right.lesson(&lesson_id).unwrap().resolution,
            "both machines hold the same value"
        );
        let queue_text: String = left.live_queue_text(&remote);
        assert_eq!(
            queue_text.lines().count(),
            1,
            "one group for (lesson, field): {queue_text}"
        );
        assert!(queue_text.contains("left says this") && queue_text.contains("right says that"));
        assert_eq!(
            queue_text.lines().count(),
            right.live_queue_text(&remote).lines().count()
        );
        let after_converging: usize = remote_commit_count(temp_dir.path(), &remote);
        left.sync(&remote);
        right.sync(&remote);
        assert_eq!(
            remote_commit_count(temp_dir.path(), &remote),
            after_converging,
            "a third cycle commits nothing"
        );
    }

    #[test]
    fn three_machines_editing_one_field_make_one_group_of_three() {
        let temp_dir = tempfile::tempdir().unwrap();
        let remote = bare_remote(temp_dir.path());
        let machines: Vec<Machine> = ["one", "two", "three"]
            .iter()
            .map(|name| Machine::new(temp_dir.path(), name))
            .collect();
        let lesson_id = machines[0].add("contested by three");
        machines[0].sync(&remote);
        machines.iter().skip(1).for_each(|machine| {
            machine.sync(&remote);
        });
        machines
            .iter()
            .zip(["x", "y", "z"])
            .for_each(|(machine, text)| machine.set_resolution(&lesson_id, text));
        for _round in ["first", "second", "third"] {
            machines.iter().for_each(|machine| {
                machine.sync(&remote);
            });
        }
        let texts: Vec<String> = machines
            .iter()
            .map(|machine| machine.live_queue_text(&remote))
            .collect();
        assert!(
            texts.iter().all(|text| text.lines().count() == 1),
            "{texts:?}"
        );
        assert!(
            texts.iter().all(|text| ["\"x\"", "\"y\"", "\"z\""]
                .iter()
                .all(|value| text.contains(value))),
            "{texts:?}"
        );
        let winners: Vec<Option<String>> = machines
            .iter()
            .map(|machine| machine.lesson(&lesson_id).unwrap().resolution)
            .collect();
        assert!(
            winners.iter().all(|winner| *winner == winners[0]),
            "same winner everywhere: {winners:?}"
        );
    }

    #[test]
    fn a_crash_before_the_db_commit_rolls_back_and_a_resync_converges() {
        let temp_dir = tempfile::tempdir().unwrap();
        let remote = bare_remote(temp_dir.path());
        let (left, right) = (
            Machine::new(temp_dir.path(), "left"),
            Machine::new(temp_dir.path(), "right"),
        );
        let lesson_id = left.add("one");
        left.sync(&remote);
        right.sync(&remote);
        left.set_resolution(&lesson_id, "left edit");
        right.set_resolution(&lesson_id, "right edit");
        left.sync(&remote);
        let before_crash: Vec<Lesson> = all_lessons(&right.conn()).unwrap();
        let crash = right.sync_with(&remote, &crash_at(SyncStep::AfterQueueWrite));
        assert!(crash.is_err());
        assert_eq!(
            all_lessons(&right.conn()).unwrap(),
            before_crash,
            "the database transaction rolled back"
        );
        assert_eq!(right.sync(&remote), CycleOutcome::Synced);
        left.sync(&remote);
        let all_values: String = right.live_queue_text(&remote);
        assert!(
            all_values.contains("left edit") && all_values.contains("right edit"),
            "no value lost: {all_values}"
        );
        assert_eq!(
            left.lesson(&lesson_id).unwrap().resolution,
            right.lesson(&lesson_id).unwrap().resolution
        );
    }

    #[test]
    fn a_crash_after_the_db_commit_and_one_before_the_push_both_resume() {
        for crash_step in [SyncStep::AfterDbCommit, SyncStep::BeforePush] {
            let temp_dir = tempfile::tempdir().unwrap();
            let remote = bare_remote(temp_dir.path());
            let (left, right) = (
                Machine::new(temp_dir.path(), "left"),
                Machine::new(temp_dir.path(), "right"),
            );
            let left_id = left.add("left");
            let right_id = right.add("right");
            left.sync(&remote);
            assert!(
                right.sync_with(&remote, &crash_at(crash_step)).is_err(),
                "{crash_step:?}"
            );
            assert_eq!(
                read_state(&right.paths(&remote).state_file()).consecutive_failures,
                1,
                "a failed cycle is counted: {crash_step:?}"
            );
            assert_eq!(right.sync(&remote), CycleOutcome::Synced);
            left.sync(&remote);
            let mut mut_expected: Vec<String> = vec![left_id, right_id];
            mut_expected.sort();
            assert_eq!(left.ids(), mut_expected, "{crash_step:?}");
            assert_eq!(right.ids(), mut_expected, "{crash_step:?}");
            let state: SyncState = read_state(&right.paths(&remote).state_file());
            assert_eq!(
                state.consecutive_failures, 0,
                "a later success resets the failures: {crash_step:?}"
            );
        }
    }

    #[test]
    fn a_crash_on_an_empty_remote_before_the_first_push_resumes() {
        let temp_dir = tempfile::tempdir().unwrap();
        let remote = bare_remote(temp_dir.path());
        let machine = Machine::new(temp_dir.path(), "only");
        machine.add("first");
        assert!(
            machine
                .sync_with(&remote, &crash_at(SyncStep::BeforePush))
                .is_err()
        );
        assert_eq!(remote_commit_count(temp_dir.path(), &remote), 0);
        assert_eq!(machine.sync(&remote), CycleOutcome::Synced);
        assert_eq!(
            remote_commit_count(temp_dir.path(), &remote),
            1,
            "one commit chain, no duplicate"
        );
    }

    #[test]
    fn a_lost_push_race_is_retried_in_both_orders() {
        for (first, second) in [("left", "right"), ("right", "left")] {
            let temp_dir = tempfile::tempdir().unwrap();
            let remote = bare_remote(temp_dir.path());
            let machines: Vec<Machine> = ["left", "right"]
                .iter()
                .map(|name| Machine::new(temp_dir.path(), name))
                .collect();
            let fn_by_name = |name: &str| {
                machines
                    .iter()
                    .find(|machine| machine.home.ends_with(name))
                    .unwrap()
            };
            let (racer, rival) = (fn_by_name(first), fn_by_name(second));
            let racer_id = racer.add("racer");
            let rival_id = rival.add("rival");
            racer.sync(&remote);
            let racer_second_id = racer.add("racer, second lesson"); // something new to push, so a race can happen
            let rival_pushed = Cell::new(false);
            let fn_race = |step: SyncStep| -> Result<()> {
                if step == SyncStep::BeforePush && !rival_pushed.replace(true) {
                    rival.sync(&remote); // the other machine pushes between our fetch and our push
                }
                Ok(())
            };
            assert_eq!(
                racer.sync_with(&remote, &fn_race).unwrap(),
                CycleOutcome::Synced,
                "{first} vs {second}"
            );
            assert!(rival_pushed.get());
            rival.sync(&remote);
            racer.sync(&remote);
            for machine in [racer, rival] {
                let ids: Vec<String> = machine.ids();
                assert!(
                    [&racer_id, &rival_id, &racer_second_id]
                        .iter()
                        .all(|expected| ids.contains(expected)),
                    "{first} vs {second}: {ids:?}"
                );
            }
        }
    }

    #[test]
    fn a_held_lock_exits_as_success_without_touching_anything() {
        let temp_dir = tempfile::tempdir().unwrap();
        let remote = bare_remote(temp_dir.path());
        let machine = Machine::new(temp_dir.path(), "only");
        machine.add("first");
        let lock_path: PathBuf = machine.paths(&remote).lock_file();
        fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        let held = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        held.lock().unwrap();
        let fetched = Cell::new(false);
        let fn_probe = |_step: SyncStep| -> Result<()> {
            fetched.set(true);
            Ok(())
        };
        assert_eq!(
            machine.sync_with(&remote, &fn_probe).unwrap(),
            CycleOutcome::LockHeld
        );
        assert!(!fetched.get(), "no step ran while the lock was held");
        assert_eq!(remote_commit_count(temp_dir.path(), &remote), 0);
        held.unlock().unwrap();
        assert_eq!(
            machine.sync(&remote),
            CycleOutcome::Synced,
            "the lock is free again, nothing was left behind"
        );
    }

    #[test]
    fn a_remote_with_a_whitelist_gitignore_still_syncs() {
        let temp_dir = tempfile::tempdir().unwrap();
        let remote = bare_remote(temp_dir.path());
        // A data repository set up with only a whitelist `.gitignore`: the JSON lines are new files it ignores.
        let setup_dir: PathBuf = temp_dir.path().join("setup");
        fs::create_dir_all(&setup_dir).unwrap();
        let setup = GitRunner::new(&setup_dir, "ssh")
            .with_env(
                "GIT_CONFIG_GLOBAL",
                &temp_dir.path().join("setup.gitconfig").to_string_lossy(),
            )
            .with_env("GIT_CONFIG_NOSYSTEM", "1")
            .with_env("GIT_AUTHOR_NAME", "sync test")
            .with_env("GIT_AUTHOR_EMAIL", "sync@test.invalid")
            .with_env("GIT_COMMITTER_NAME", "sync test")
            .with_env("GIT_COMMITTER_EMAIL", "sync@test.invalid");
        setup
            .run_checked(&["init", "-q", "-b", REMOTE_BRANCH])
            .unwrap();
        fs::write(setup_dir.join(".gitignore"), "*\n!.gitignore\n!shared.db\n").unwrap();
        setup.run_checked(&["add", "-f", ".gitignore"]).unwrap();
        setup
            .run_checked(&["commit", "-q", "-m", "whitelist"])
            .unwrap();
        setup
            .run_checked(&[
                "push",
                "-q",
                &remote,
                &format!("HEAD:refs/heads/{REMOTE_BRANCH}"),
            ])
            .unwrap();
        let fresh = Machine::new(temp_dir.path(), "fresh");
        let fresh_id = fresh.add("from a fresh machine");
        assert_eq!(fresh.sync(&remote), CycleOutcome::Synced);
        assert!(
            remote_file(temp_dir.path(), &remote, "shared.jsonl")
                .unwrap()
                .contains(&fresh_id)
        );
        assert!(
            remote_file(temp_dir.path(), &remote, ".gitignore").is_some(),
            "the whitelist is still there"
        );
    }

    #[test]
    fn domain_only_syncs_the_clone_that_carries_it() {
        let temp_dir = tempfile::tempdir().unwrap();
        let (personal_remote, shared_remote) = (
            temp_dir
                .path()
                .join("personal-remote")
                .to_string_lossy()
                .into_owned(),
            temp_dir
                .path()
                .join("shared-remote")
                .to_string_lossy()
                .into_owned(),
        );
        let git = isolated_git(temp_dir.path());
        [&personal_remote, &shared_remote]
            .iter()
            .for_each(|remote| {
                git.run_checked(&["init", "-q", "--bare", "-b", REMOTE_BRANCH, remote])
                    .unwrap();
            });
        let machine = Machine::new(temp_dir.path(), "only");
        drop(open(&Domain::Personal.file_path(&machine.home), OpenMode::Create).unwrap());
        machine.add("a shared lesson");
        let configured = SyncSection {
            personal: Some(sync_config(&personal_remote)),
            shared: Some(sync_config(&shared_remote)),
            ..SyncSection::default()
        };
        assert_eq!(
            sync_configured(
                &machine.home,
                &configured,
                Some(Domain::Shared),
                &machine.git_env
            )
            .unwrap(),
            CycleOutcome::Synced
        );
        assert!(remote_file(temp_dir.path(), &shared_remote, "shared.jsonl").is_some());
        assert_eq!(
            remote_commit_count(temp_dir.path(), &personal_remote),
            0,
            "the other clone was left alone"
        );
        assert!(!machine.paths(&personal_remote).state_file().exists());
    }

    #[test]
    fn an_unreadable_last_synced_commit_is_a_union_and_deletes_nothing() {
        let temp_dir = tempfile::tempdir().unwrap();
        let remote = bare_remote(temp_dir.path());
        let (left, right) = (
            Machine::new(temp_dir.path(), "left"),
            Machine::new(temp_dir.path(), "right"),
        );
        let kept_id = left.add("kept on both");
        left.sync(&remote);
        right.sync(&remote);
        let right_only = right.add("right only");
        write_state(
            &right.paths(&remote).state_file(),
            &SyncState {
                last_synced_commit: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
                ..SyncState::default()
            },
        )
        .unwrap();
        assert_eq!(right.sync(&remote), CycleOutcome::Synced);
        left.sync(&remote);
        for machine in [&left, &right] {
            assert!(machine.ids().contains(&kept_id) && machine.ids().contains(&right_only));
        }
    }

    #[test]
    fn an_edit_beats_a_delete_made_on_another_machine() {
        let temp_dir = tempfile::tempdir().unwrap();
        let remote = bare_remote(temp_dir.path());
        let (left, right) = (
            Machine::new(temp_dir.path(), "left"),
            Machine::new(temp_dir.path(), "right"),
        );
        let edited_id = left.add("edited elsewhere");
        let deleted_id = left.add("deleted elsewhere and untouched here");
        left.sync(&remote);
        right.sync(&remote);
        left.delete(&edited_id);
        left.delete(&deleted_id);
        right.set_resolution(&edited_id, "kept alive by an edit");
        left.sync(&remote);
        right.sync(&remote);
        left.sync(&remote);
        assert!(
            left.lesson(&edited_id).is_some() && right.lesson(&edited_id).is_some(),
            "an edit beats a delete"
        );
        assert!(
            left.lesson(&deleted_id).is_none() && right.lesson(&deleted_id).is_none(),
            "an unchanged copy is deleted"
        );
    }

    #[test]
    fn no_database_or_live_queue_file_ever_reaches_the_remote() {
        let temp_dir = tempfile::tempdir().unwrap();
        let remote = bare_remote(temp_dir.path());
        let (left, right) = (
            Machine::new(temp_dir.path(), "left"),
            Machine::new(temp_dir.path(), "right"),
        );
        let lesson_id = left.add("contested");
        left.sync(&remote);
        right.sync(&remote);
        left.set_resolution(&lesson_id, "left");
        right.set_resolution(&lesson_id, "right");
        left.sync(&remote);
        right.sync(&remote);
        let listing: String = isolated_git(temp_dir.path())
            .run_checked(&[
                "--git-dir",
                &remote,
                "ls-tree",
                "-r",
                "--name-only",
                REMOTE_BRANCH,
            ])
            .unwrap();
        let mut mut_names: Vec<&str> = listing.lines().collect();
        mut_names.sort_unstable();
        assert_eq!(
            mut_names,
            vec!["shared.conflicts.jsonl", "shared.jsonl"],
            "only the JSON lines are committed"
        );
        // The control: the same listing would show a database if one were ever added to the clone.
        fs::write(
            left.paths(&remote).clone_dir().join("shared.db"),
            "not a real database",
        )
        .unwrap();
        left.git(&remote)
            .run_checked(&["add", "-f", "shared.db"])
            .unwrap();
        let staged: String = left
            .git(&remote)
            .run_checked(&["diff", "--cached", "--name-only"])
            .unwrap();
        assert!(
            staged.contains("shared.db"),
            "the control can see a leak: {staged}"
        );
    }

    #[test]
    fn rows_of_the_remote_parse_back_into_the_lessons_of_both_machines() {
        let temp_dir = tempfile::tempdir().unwrap();
        let remote = bare_remote(temp_dir.path());
        let machine = Machine::new(temp_dir.path(), "only");
        let lesson_id = machine.add("round trip");
        machine.sync(&remote);
        let text: String = remote_file(temp_dir.path(), &remote, "shared.jsonl").unwrap();
        let rows: Vec<Lesson> = from_jsonl(&text, |row: &Lesson| row.id.clone()).unwrap();
        assert_eq!(rows, vec![machine.lesson(&lesson_id).unwrap()]);
    }

    /// One URL can carry several domains and the clone has ONE remembered BASE. A run of only one domain must leave it
    /// alone: advancing it would make the other domain's newer remote rows look deleted on the next full run.
    #[test]
    fn a_run_of_one_domain_of_a_shared_clone_does_not_delete_the_other_domains_remote_rows() {
        const BOTH: &[Domain] = &[Domain::Shared, Domain::Personal];
        let temp_dir = tempfile::tempdir().unwrap();
        let remote = bare_remote(temp_dir.path());
        let (left, right) = (
            Machine::new(temp_dir.path(), "left"),
            Machine::new(temp_dir.path(), "right"),
        );
        let fn_personal_conn = |machine: &Machine| -> Connection {
            drop(open(&Domain::Personal.file_path(&machine.home), OpenMode::Create).unwrap());
            open(
                &Domain::Personal.file_path(&machine.home),
                OpenMode::Existing,
            )
            .unwrap()
        };
        let fn_sync_domains = |machine: &Machine, domains: &[Domain], whole_clone: bool| {
            let job = CloneJob {
                domains,
                whole_clone,
                ..machine.job(&remote)
            };
            sync_clone(&job, &|_| Ok(()), SOME_NOW).unwrap()
        };
        let (left_personal, right_personal) = (fn_personal_conn(&left), fn_personal_conn(&right));
        fn_sync_domains(&right, BOTH, true);
        let new_row_id: String = insert(
            &left_personal,
            &NewLesson {
                title: "personal row".to_string(),
                problem: "written on the left".to_string(),
                ..NewLesson::default()
            },
        )
        .unwrap();
        fn_sync_domains(&left, BOTH, true);
        fn_sync_domains(&right, ONLY_SHARED, false);
        fn_sync_domains(&right, BOTH, true);
        assert!(
            find_by_id(&right_personal, &new_row_id).unwrap().is_some(),
            "the right machine receives the row"
        );
        assert!(
            remote_file(temp_dir.path(), &remote, "personal.jsonl")
                .unwrap()
                .contains(&new_row_id),
            "the remote still has the row"
        );
    }

    /// A crashed cycle can leave a file staged in the clone. With no remote tip nothing resets the clone, so a later
    /// run of OTHER domains must not commit and push that stale file.
    #[test]
    fn a_file_a_crashed_cycle_left_staged_is_not_pushed_by_a_run_of_other_domains() {
        let temp_dir = tempfile::tempdir().unwrap();
        let remote = bare_remote(temp_dir.path());
        let machine = Machine::new(temp_dir.path(), "only");
        machine.add("shared lesson");
        assert!(
            machine
                .sync_with(&remote, &crash_at(SyncStep::AfterFetch))
                .is_err(),
            "the first cycle makes the clone, then crashes"
        );
        let stale_name: &str = "personal.jsonl";
        fs::write(
            machine.paths(&remote).clone_dir().join(stale_name),
            "stale\n",
        )
        .unwrap();
        machine
            .git(&remote)
            .run_checked(&["add", stale_name])
            .unwrap();
        machine.sync(&remote);
        assert_eq!(
            remote_file(temp_dir.path(), &remote, stale_name),
            None,
            "the stale staged file never reaches the remote"
        );
    }

    // Cycle rows for the access step (the `access_` prefix is what `cargo test access` matches): the guard runs under the lock, before
    // anything is fetched, and a refusal is never remembered.
    /// The work key and the one other key of `work_guard_for_tests`.
    const KEYS_PROBED: usize = 2;

    /// A work remote in ssh form, which the key guard needs; `alias_env_for` points git at a local bare repository for it.
    const WORK_SSH_REMOTE: &str = "git@work.example:o/work.git";

    /// The machine's git environment plus an `insteadOf` that sends `WORK_SSH_REMOTE` to `bare`, so no network is touched.
    fn alias_env_for(machine: &Machine, bare: &str) -> Vec<(String, String)> {
        machine
            .git_env
            .iter()
            .cloned()
            .chain([
                ("GIT_CONFIG_COUNT".to_string(), "1".to_string()),
                (
                    "GIT_CONFIG_KEY_0".to_string(),
                    format!("url.{bare}.insteadOf"),
                ),
                (
                    "GIT_CONFIG_VALUE_0".to_string(),
                    WORK_SSH_REMOTE.to_string(),
                ),
            ])
            .collect()
    }

    /// A job for `WORK_SSH_REMOTE` that runs git under `alias_env`.
    fn work_job<'job>(
        machine: &'job Machine,
        alias_env: &'job [(String, String)],
    ) -> CloneJob<'job> {
        CloneJob {
            normalized_remote: WORK_SSH_REMOTE,
            git_env: alias_env,
            ..machine.job(WORK_SSH_REMOTE)
        }
    }

    fn work_guard_for_tests() -> WorkGuard {
        WorkGuard {
            work_key: PathBuf::from("/k/work"),
            other_keys: vec![("personal".to_string(), PathBuf::from("/k/home"))],
        }
    }

    /// A probe that says `work_answer` for the work key, `home_answer` for the other key, and counts the key probes.
    fn probe_answering<'answers>(
        work_answer: ProbeClass,
        home_answer: ProbeClass,
        calls: &'answers Cell<usize>,
    ) -> impl Fn(&ProbeRequest) -> ProbeClass + 'answers {
        move |request| match request {
            ProbeRequest::WithKey { key, .. } if key == &PathBuf::from("/k/work") => {
                calls.set(calls.get() + 1);
                work_answer
            }
            ProbeRequest::WithKey { .. } => {
                calls.set(calls.get() + 1);
                home_answer
            }
            ProbeRequest::Anonymous { .. } => ProbeClass::Inconclusive,
        }
    }

    #[test]
    fn access_a_refused_work_clone_fails_the_cycle_before_anything_is_fetched() {
        let temp_dir = tempfile::tempdir().unwrap();
        let remote = bare_remote(temp_dir.path());
        let machine = Machine::new(temp_dir.path(), "only");
        let alias_env = alias_env_for(&machine, &remote);
        machine.add("first");
        let calls = Cell::new(0);
        let fn_probe = probe_answering(ProbeClass::Readable, ProbeClass::Readable, &calls);
        let job = CloneJob {
            probe: &fn_probe,
            work_guard: Some(work_guard_for_tests()),
            ..work_job(&machine, &alias_env)
        };
        let fetched = Cell::new(false);
        let outcome = sync_clone(
            &job,
            &|_| {
                fetched.set(true);
                Ok(())
            },
            SOME_NOW,
        );
        let message = format!("{:#}", outcome.unwrap_err());
        assert!(
            message.contains("personal"),
            "the refusal names the key that can read: {message}"
        );
        assert!(!fetched.get(), "no cycle step ran");
        assert_eq!(
            remote_commit_count(temp_dir.path(), &remote),
            0,
            "nothing was pushed"
        );
        assert_eq!(
            read_state(&machine.paths(WORK_SSH_REMOTE).state_file()).consecutive_failures,
            1,
            "the refusal counts as a failed cycle"
        );
    }

    #[test]
    fn access_an_allowed_work_clone_syncs_and_probes_each_key_once() {
        let temp_dir = tempfile::tempdir().unwrap();
        let remote = bare_remote(temp_dir.path());
        let machine = Machine::new(temp_dir.path(), "only");
        let alias_env = alias_env_for(&machine, &remote);
        machine.add("first");
        let calls = Cell::new(0);
        let fn_probe = probe_answering(ProbeClass::Readable, ProbeClass::AuthDenied, &calls);
        let job = CloneJob {
            probe: &fn_probe,
            work_guard: Some(work_guard_for_tests()),
            ..work_job(&machine, &alias_env)
        };
        assert_eq!(
            sync_clone(&job, &|_| Ok(()), SOME_NOW).unwrap(),
            CycleOutcome::Synced
        );
        assert_eq!(
            calls.get(),
            KEYS_PROBED,
            "the work key and the one other key (the anonymous probe is not counted)"
        );
    }

    #[test]
    fn access_a_clone_with_no_work_guard_probes_no_key() {
        let temp_dir = tempfile::tempdir().unwrap();
        let remote = bare_remote(temp_dir.path());
        let machine = Machine::new(temp_dir.path(), "only");
        machine.add("first");
        let calls = Cell::new(0);
        let fn_probe = probe_answering(ProbeClass::AuthDenied, ProbeClass::Readable, &calls);
        let job = CloneJob {
            probe: &fn_probe,
            work_guard: None,
            ..machine.job(&remote)
        };
        assert_eq!(
            sync_clone(&job, &|_| Ok(()), SOME_NOW).unwrap(),
            CycleOutcome::Synced
        );
        assert_eq!(calls.get(), 0, "a shared remote runs no work key probe");
    }

    #[test]
    fn access_no_probe_runs_while_the_lock_is_held() {
        let temp_dir = tempfile::tempdir().unwrap();
        let remote = bare_remote(temp_dir.path());
        let machine = Machine::new(temp_dir.path(), "only");
        let alias_env = alias_env_for(&machine, &remote);
        let lock_path: PathBuf = machine.paths(WORK_SSH_REMOTE).lock_file();
        fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        let held = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        held.lock().unwrap();
        let calls = Cell::new(0);
        let fn_probe = probe_answering(ProbeClass::Readable, ProbeClass::AuthDenied, &calls);
        let job = CloneJob {
            probe: &fn_probe,
            work_guard: Some(work_guard_for_tests()),
            ..work_job(&machine, &alias_env)
        };
        assert_eq!(
            sync_clone(&job, &|_| Ok(()), SOME_NOW).unwrap(),
            CycleOutcome::LockHeld
        );
        assert_eq!(
            calls.get(),
            0,
            "the probes run under the lock, never while another sync holds it"
        );
    }

    #[test]
    fn access_a_refusal_is_not_remembered_as_a_pass() {
        let temp_dir = tempfile::tempdir().unwrap();
        let remote = bare_remote(temp_dir.path());
        let machine = Machine::new(temp_dir.path(), "only");
        let alias_env = alias_env_for(&machine, &remote);
        machine.add("first");
        let refused_calls = Cell::new(0);
        let refusing = probe_answering(ProbeClass::Readable, ProbeClass::Readable, &refused_calls);
        let refused_job = CloneJob {
            probe: &refusing,
            work_guard: Some(work_guard_for_tests()),
            ..work_job(&machine, &alias_env)
        };
        assert!(sync_clone(&refused_job, &|_| Ok(()), SOME_NOW).is_err());
        let allowed_calls = Cell::new(0);
        let allowing =
            probe_answering(ProbeClass::Readable, ProbeClass::AuthDenied, &allowed_calls);
        let allowed_job = CloneJob {
            probe: &allowing,
            work_guard: Some(work_guard_for_tests()),
            ..work_job(&machine, &alias_env)
        };
        assert_eq!(
            sync_clone(&allowed_job, &|_| Ok(()), SOME_NOW).unwrap(),
            CycleOutcome::Synced
        );
        assert_eq!(
            allowed_calls.get(),
            KEYS_PROBED,
            "the next cycle probed again instead of trusting the earlier refusal"
        );
    }

    #[test]
    fn access_a_local_remote_is_never_probed_anonymously_or_stamped() {
        let temp_dir = tempfile::tempdir().unwrap();
        let remote = bare_remote(temp_dir.path());
        let machine = Machine::new(temp_dir.path(), "only");
        machine.add("first");
        let probe_calls = Cell::new(0);
        let fn_probe = |_request: &ProbeRequest| {
            probe_calls.set(probe_calls.get() + 1);
            ProbeClass::Readable // a probe that would stamp a verdict if it were ever asked
        };
        let job = CloneJob {
            probe: &fn_probe,
            ..machine.job(&remote)
        };
        assert_eq!(
            sync_clone(&job, &|_| Ok(()), SOME_NOW).unwrap(),
            CycleOutcome::Synced
        );
        assert_eq!(probe_calls.get(), 0, "no https form, so no probe was asked");
        let state = read_state(&machine.paths(&remote).state_file());
        assert_eq!(
            state.last_access_check_unix_secs, None,
            "and nothing was recorded"
        );
    }

    #[test]
    fn access_a_work_remote_without_ssh_is_refused_unprobed() {
        // The key guard compares ssh keys, and over https no key is ever used, so every key would read the same.
        const HTTPS_REMOTE: &str = "https://example.com/o/work.git";
        let temp_dir = tempfile::tempdir().unwrap();
        let machine = Machine::new(temp_dir.path(), "only");
        machine.add("first");
        let key_calls = Cell::new(0);
        let fn_probe = |request: &ProbeRequest| match request {
            ProbeRequest::WithKey { .. } => {
                key_calls.set(key_calls.get() + 1);
                ProbeClass::Readable
            }
            ProbeRequest::Anonymous { .. } => ProbeClass::Inconclusive,
        };
        let job = CloneJob {
            normalized_remote: HTTPS_REMOTE,
            probe: &fn_probe,
            work_guard: Some(work_guard_for_tests()),
            ..machine.job(HTTPS_REMOTE)
        };
        let message = format!("{:#}", sync_clone(&job, &|_| Ok(()), SOME_NOW).unwrap_err());
        assert!(
            message.contains("the work key guard needs an ssh remote"),
            "the refusal says why: {message}"
        );
        assert_eq!(key_calls.get(), 0, "no key was probed over https");
    }

    #[test]
    fn access_the_cycle_records_the_visibility_once_and_does_not_probe_again_until_due() {
        // An https remote git is told to read from the local bare repository, so the anonymous URL is derivable and no
        // network is touched.
        const HTTPS_REMOTE: &str = "https://example.com/o/r.git";
        let temp_dir = tempfile::tempdir().unwrap();
        let bare = bare_remote(temp_dir.path());
        let machine = Machine::new(temp_dir.path(), "only");
        machine.add("first");
        let rewritten_env: Vec<(String, String)> = machine
            .git_env
            .iter()
            .cloned()
            .chain([
                ("GIT_CONFIG_COUNT".to_string(), "1".to_string()),
                (
                    "GIT_CONFIG_KEY_0".to_string(),
                    format!("url.{bare}.insteadOf"),
                ),
                ("GIT_CONFIG_VALUE_0".to_string(), HTTPS_REMOTE.to_string()),
            ])
            .collect();
        let anonymous_calls = Cell::new(0);
        let fn_probe = |request: &ProbeRequest| match request {
            ProbeRequest::Anonymous { url } if url == HTTPS_REMOTE => {
                anonymous_calls.set(anonymous_calls.get() + 1);
                ProbeClass::Readable
            }
            ProbeRequest::Anonymous { .. } | ProbeRequest::WithKey { .. } => {
                ProbeClass::Inconclusive
            }
        };
        let job = CloneJob {
            normalized_remote: HTTPS_REMOTE,
            probe: &fn_probe,
            git_env: &rewritten_env,
            ..machine.job(HTTPS_REMOTE)
        };
        assert_eq!(
            sync_clone(&job, &|_| Ok(()), SOME_NOW).unwrap(),
            CycleOutcome::Synced
        );
        let state = read_state(&machine.paths(HTTPS_REMOTE).state_file());
        assert_eq!(state.visibility, Visibility::Public);
        assert_eq!(state.last_access_check_unix_secs, Some(SOME_NOW));
        assert_eq!(
            sync_clone(&job, &|_| Ok(()), SOME_NOW + 1).unwrap(),
            CycleOutcome::Synced
        );
        assert_eq!(
            anonymous_calls.get(),
            1,
            "the second cycle is inside the recheck interval, so no probe"
        );
    }
    fn clone_spec(remote: &str, key: &str, domains: Vec<Domain>) -> CloneSpec {
        CloneSpec {
            remote: remote.to_string(),
            normalized_remote: remote.to_string(),
            ssh_key: PathBuf::from(key),
            domains,
        }
    }

    #[test]
    fn access_work_alone_on_its_remote_gets_a_guard_naming_each_other_distinct_key_once() {
        let work_clone = clone_spec("git@h:o/work.git", "/k/work", vec![Domain::Work]);
        let home_clone = clone_spec("git@h:o/home.git", "/k/home", vec![Domain::Personal]);
        let shared_clone = clone_spec("git@h:o/shared.git", "/k/home", vec![Domain::Shared]);
        let same_key_clone = clone_spec("git@h:o/same.git", "/k/work", vec![Domain::Shared]);
        let guard = work_guard_of(
            &work_clone,
            &[work_clone.clone(), home_clone, shared_clone, same_key_clone],
        )
        .unwrap();
        assert_eq!(guard.work_key, PathBuf::from("/k/work"));
        assert_eq!(
            guard.other_keys,
            vec![("personal+shared".to_string(), PathBuf::from("/k/home"))],
            "the work key is not probed against itself and a shared other key is probed once"
        );
    }

    #[test]
    fn access_work_sharing_its_remote_or_absent_has_no_guard() {
        let shared_work = clone_spec(
            "git@h:o/all.git",
            "/k/work",
            vec![Domain::Work, Domain::Shared],
        );
        let home_clone = clone_spec("git@h:o/home.git", "/k/home", vec![Domain::Personal]);
        assert_eq!(
            work_guard_of(&shared_work, &[shared_work.clone(), home_clone.clone()]),
            None,
            "a remote shared with another domain skips the guard"
        );
        assert_eq!(
            work_guard_of(&home_clone, &[shared_work, home_clone.clone()]),
            None,
            "only a work clone is guarded"
        );
    }
    #[test]
    fn access_a_refused_work_clone_still_records_its_visibility() {
        // The visibility probe never depends on the work key guard: a work remote that is public AND refused must still be warned about.
        const HTTPS_REMOTE: &str = "https://example.com/o/work.git";
        let temp_dir = tempfile::tempdir().unwrap();
        let machine = Machine::new(temp_dir.path(), "only");
        machine.add("first");
        let fn_probe = |_request: &ProbeRequest| ProbeClass::Readable; // every key reads, and the remote is anonymously readable
        let job = CloneJob {
            normalized_remote: HTTPS_REMOTE,
            probe: &fn_probe,
            work_guard: Some(work_guard_for_tests()),
            ..machine.job(HTTPS_REMOTE)
        };
        assert!(
            sync_clone(&job, &|_| Ok(()), SOME_NOW).is_err(),
            "every key reads work: refused"
        );
        let state = read_state(&machine.paths(HTTPS_REMOTE).state_file());
        assert_eq!(
            state.visibility,
            Visibility::Public,
            "the visibility was probed and recorded anyway"
        );
        assert_eq!(
            state.consecutive_failures, 1,
            "and the refusal still counts as a failure"
        );
    }
}
