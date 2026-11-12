//! The files of one sync clone (plan step S4a): where they live, the JSON lines the clone commits, the live conflict
//! queue beside it, and the state file. Every write goes through a temp file and a rename, so a crash leaves the old
//! file or the new one, never half of one. Only the sync cycle and `lessons resolve` write these, each holding the
//! clone lock, so two writers never race on one file.

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::fs;
use std::io::{ErrorKind, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::access::Visibility;
use crate::db::Domain;
use crate::journal;

/// The directory under `$LESSONS_HOME` that holds every clone and the files beside them.
pub const SYNC_DIR_NAME: &str = "sync";
/// `<domain>.jsonl`: one domain's lessons, committed in the clone.
pub const LESSONS_FILE_SUFFIX: &str = ".jsonl";
/// `<domain>.conflicts.jsonl`: one domain's conflict queue, committed in the clone and kept live beside it.
pub const QUEUE_FILE_SUFFIX: &str = ".conflicts.jsonl";
/// `<slug>.lock`: the clone lock, beside the clone.
pub const LOCK_FILE_SUFFIX: &str = ".lock";
/// `<slug>.state`: what the clone remembers between cycles, beside the clone.
pub const STATE_FILE_SUFFIX: &str = ".state";
/// `<file>.tmp`: where `write_atomically` writes before the rename.
pub const TEMP_FILE_SUFFIX: &str = ".tmp";
/// Permissions of every file `write_atomically` writes: the owner only. The lessons may belong to an employer, and a
/// file follows the process umask otherwise, which on a shared host is usually readable by every user.
#[cfg(unix)]
pub const PRIVATE_FILE_MODE: u32 = 0o600;

/// Longest slug in bytes, far below the 255-byte file-name limit, so `.state`, `.lock` and the
/// `.<domain>.conflicts.jsonl` suffix still fit.
const MAX_SLUG_BYTES: usize = 200;
/// FNV-1a, 64 bit, starting value. Its result is fixed by its definition, unlike `DefaultHasher`, which may change
/// between Rust releases and would rename every clone of a long remote.
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a_64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(FNV_OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)
    })
}

/// The slug of the empty remote. No other input gives `_` alone, because every escape is `_` plus two hex digits.
const EMPTY_REMOTE_SLUG: &str = "_";

/// A normalized remote as one path component, made only of `a-z`, `0-9`, `-` and `_`. Letters and digits stay,
/// lower-cased because the default macOS disk ignores case, and so does `-`; every other byte, `.` and `_` included,
/// becomes `_` and two hex digits. With no `.` in it, a slug can never equal another remote's `<slug>.state`,
/// `<slug>.lock` or `<slug>.<domain>.conflicts.jsonl`, and is never `.`, `..` or hidden. Past `MAX_SLUG_BYTES` the
/// slug keeps its head and ends in `-` and the 16-digit hash of the whole, so two long remotes that share a head still
/// differ.
pub fn remote_slug(normalized_remote: &str) -> String {
    // Every byte outside letters, digits and - is encoded, underscore included, so two remotes never share a slug.
    let encoded: String = normalized_remote
        .bytes()
        .map(|byte| {
            let is_kept: bool = byte.is_ascii_alphanumeric() || byte == b'-';
            if is_kept {
                char::from(byte.to_ascii_lowercase()).to_string()
            } else {
                format!("_{byte:02x}")
            }
        })
        .collect();
    // The slug is ASCII only, so cutting at a byte offset never splits a character.
    match encoded
        .get(..MAX_SLUG_BYTES)
        .filter(|_| encoded.len() > MAX_SLUG_BYTES)
    {
        Some(head) => format!("{head}-{:016x}", fnv1a_64(encoded.as_bytes())),
        None if encoded.is_empty() => EMPTY_REMOTE_SLUG.to_string(),
        None => encoded,
    }
}

/// `<domain>.jsonl`, the name inside the clone and the path `git show <commit>:<name>` reads.
pub fn lessons_file_name(domain: Domain) -> String {
    format!("{}{LESSONS_FILE_SUFFIX}", domain.as_str())
}

/// `<domain>.conflicts.jsonl`, the name inside the clone and the path `git show <commit>:<name>` reads.
pub fn queue_file_name(domain: Domain) -> String {
    format!("{}{QUEUE_FILE_SUFFIX}", domain.as_str())
}

/// Where one clone and its files live: `sync/<slug>/` is the git clone; `<slug>.lock`, `<slug>.state` and the live
/// queue `<slug>.<domain>.conflicts.jsonl` sit beside it, outside the clone, so resetting the clone never undoes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClonePaths {
    sync_dir: PathBuf,
    slug: String,
}

impl ClonePaths {
    pub fn new(lessons_home: &Path, normalized_remote: &str) -> ClonePaths {
        ClonePaths {
            sync_dir: lessons_home.join(SYNC_DIR_NAME),
            slug: remote_slug(normalized_remote),
        }
    }

    pub fn clone_dir(&self) -> PathBuf {
        self.sync_dir.join(&self.slug)
    }

    pub fn lock_file(&self) -> PathBuf {
        self.sync_dir
            .join(format!("{}{LOCK_FILE_SUFFIX}", self.slug))
    }

    pub fn state_file(&self) -> PathBuf {
        self.sync_dir
            .join(format!("{}{STATE_FILE_SUFFIX}", self.slug))
    }

    pub fn live_queue(&self, domain: Domain) -> PathBuf {
        self.sync_dir
            .join(format!("{}.{}", self.slug, queue_file_name(domain)))
    }
}

/// One JSON object per line, ordered by `fn_sort_key`, each line ending in `\n`; no rows is the empty string. Sorting
/// here, not in the caller, keeps the committed file's order (and so its diffs) the same on every machine.
pub fn to_jsonl<Row: Serialize, SortKey: Ord>(
    rows: &[Row],
    fn_sort_key: impl Fn(&Row) -> SortKey,
) -> Result<String> {
    // The input index breaks ties, so rows with equal keys keep their order and none is dropped.
    let ordered: BTreeMap<(SortKey, usize), &Row> = rows
        .iter()
        .enumerate()
        .map(|(row_index, row)| ((fn_sort_key(row), row_index), row))
        .collect();
    ordered
        .values()
        .map(|row| serde_json::to_string(row).map(|line| line + "\n"))
        .collect::<serde_json::Result<String>>()
        .context("to_jsonl: a row did not serialize")
}

/// Parses one JSON object per non-blank line. A line that does not parse is an error naming its line number, and a
/// second row with the same `fn_unique_key` is an error naming the key: the merge indexes rows by that key, so a
/// duplicate would silently keep only one of them.
pub fn from_jsonl<Row: DeserializeOwned, UniqueKey: Ord + Debug>(
    text: &str,
    fn_unique_key: impl Fn(&Row) -> UniqueKey,
) -> Result<Vec<Row>> {
    // A duplicate key is refused, not merged: the merge indexes by key and would silently keep only one row.
    let rows: Vec<Row> = text
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(line_index, line)| {
            serde_json::from_str::<Row>(line)
                .with_context(|| format!("line {}: not a valid JSON row", line_index + 1))
        })
        .collect::<Result<Vec<Row>>>()?;
    let unique_keys: Vec<UniqueKey> = rows.iter().map(&fn_unique_key).collect(); // each key computed once
    // A key whose last row is not this row appears again later: that is a duplicate.
    let last_index_by_key: BTreeMap<&UniqueKey, usize> = unique_keys
        .iter()
        .enumerate()
        .map(|(row_index, unique_key)| (unique_key, row_index))
        .collect();
    let duplicated_key: Option<&UniqueKey> = unique_keys
        .iter()
        .enumerate()
        .find(|(row_index, unique_key)| last_index_by_key.get(*unique_key) != Some(row_index))
        .map(|(_, unique_key)| unique_key);
    match duplicated_key {
        Some(unique_key) => bail!("duplicate row for {unique_key:?}"),
        None => Ok(rows),
    }
}

/// The file's text, or the empty string when it does not exist yet (a domain's first sync). Any other read error
/// (permissions, a directory in its place) is returned, never read as "no lessons".
pub fn read_text_or_empty(path: &Path) -> Result<String> {
    // Only a missing file is empty; any other read error must not be read as no lessons.
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(read_error) if read_error.kind() == ErrorKind::NotFound => Ok(String::new()),
        Err(read_error) => Err(read_error).with_context(|| format!("read {}", path.display())),
    }
}

/// Writes `text` to `path` through `<path>.tmp` in the same directory, flushed to disk, then renamed over `path`, so a
/// crash leaves the old file or the new one. Creates the directory if needed. One fixed temp name is enough: the
/// clone lock admits one writer at a time.
pub fn write_atomically(path: &Path, text: &str) -> Result<()> {
    // Temp file in the same directory, synced, then renamed, so a crash leaves the old file or the new one.
    let parent_dir: &Path = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    let file_name: &str = path
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("{} has no UTF-8 file name", path.display()))?;
    let temp_path: PathBuf = parent_dir.join(format!("{file_name}{TEMP_FILE_SUFFIX}"));
    fs::create_dir_all(parent_dir).with_context(|| format!("create {}", parent_dir.display()))?;
    let mut mut_temp_file =
        fs::File::create(&temp_path).with_context(|| format!("create {}", temp_path.display()))?;
    // Set on the open file, not only at creation: a temp file left by a crash keeps the mode it was created with.
    #[cfg(unix)]
    mut_temp_file
        .set_permissions(fs::Permissions::from_mode(PRIVATE_FILE_MODE))
        .with_context(|| format!("restrict {} to its owner", temp_path.display()))?;
    mut_temp_file
        .write_all(text.as_bytes())
        .and_then(|()| mut_temp_file.sync_all())
        .with_context(|| format!("write {}", temp_path.display()))?;
    fs::rename(&temp_path, path)
        .with_context(|| format!("rename {} to {}", temp_path.display(), path.display()))?;
    journal!("write {}: {} byte(s)", path.display(), text.len());
    Ok(())
}

/// What a clone remembers between cycles. Every field has a default, so a file written before a field existed still
/// reads, and unknown fields (from a newer version) are ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncState {
    /// The commit both sides last agreed on: the BASE of the next merge. Written only after a push, or when no push was
    /// needed.
    pub last_synced_commit: Option<String>,
    /// When the clone last fetched, in seconds since the Unix epoch, for the pull throttle.
    pub last_pull_unix_secs: Option<u64>,
    /// Failed cycles in a row, for the backoff; a successful cycle resets it.
    pub consecutive_failures: u32,
    /// What the last visibility probe found for the remote (R13): `public` makes every command warn.
    pub visibility: Visibility,
    /// When the visibility was last probed conclusively, in seconds since the Unix epoch, for the recheck interval.
    pub last_access_check_unix_secs: Option<u64>,
}

/// The state file, or the default when it is missing, empty, corrupt or unreadable. The default has no
/// `last_synced_commit`, which the cycle treats as an empty BASE: a union that deletes nothing. Losing the state costs
/// a re-merge, never a lesson, so a bad state file is logged rather than stopping sync. The next write replaces a
/// corrupt or empty file; a path that cannot be read (a directory in its place) is not replaced, and that write's
/// error surfaces to the caller.
pub fn read_state(path: &Path) -> SyncState {
    // A bad state file costs a re-merge from an empty BASE, never a lesson, so it is logged and the next write
    // replaces it.
    let fn_start_empty = |why: &str| {
        journal!(
            "state {}: {why}, starting from an empty BASE",
            path.display()
        );
        SyncState::default()
    };
    match read_text_or_empty(path) {
        Ok(state_text) if state_text.trim().is_empty() => fn_start_empty("no state yet"),
        Ok(state_text) => serde_json::from_str(&state_text).unwrap_or_else(|parse_error| {
            fn_start_empty(&format!("corrupt state ({parse_error})"))
        }),
        Err(read_error) => fn_start_empty(&format!("unreadable state ({read_error:#})")),
    }
}

pub fn write_state(path: &Path, state: &SyncState) -> Result<()> {
    let state_text: String =
        serde_json::to_string_pretty(state).context("serialize the sync state")? + "\n";
    write_atomically(path, &state_text)
}

// Unit tests only, in temp dirs. Race: none at this layer; every writer of these files holds the clone lock (S4c
// tests the lock). Back-to-back: writing the same file twice is pinned by
// `writing_twice_leaves_the_last_text_and_no_temp_file`.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Lesson;
    use crate::sync::{Conflict, MergedField, Version};

    const ID_A: &str = "01KAAAAAAAAAAAAAAAAAAAAAAA";
    const ID_B: &str = "01KBBBBBBBBBBBBBBBBBBBBBBB";
    const ID_C: &str = "01KCCCCCCCCCCCCCCCCCCCCCCC";
    const SOME_COMMIT: &str = "3f2a9c1e8d7b6a5f4e3d2c1b0a9f8e7d6c5b4a39";
    const SOME_PULL_SECS: u64 = 1_762_000_000;
    const SOME_FAILURES: u32 = 3;
    /// The published FNV-1a 64-bit test vectors (Fowler, Noll and Vo), so a change to the hash is caught.
    const FNV1A_64_OF_A: u64 = 0xaf63_dc4c_8601_ec8c;
    const FNV1A_64_OF_FOOBAR: u64 = 0x8594_4171_f739_67e8;
    /// What a hashed slug adds to its head: `-` and the 16 hex digits of the hash.
    const HASH_SUFFIX_BYTES: usize = 17;

    fn lesson(lesson_id: &str) -> Lesson {
        Lesson {
            id: lesson_id.to_string(),
            created_at: "2026-11-01T00:00:00Z".to_string(),
            updated_at: "2026-11-02T00:00:00Z".to_string(),
            title: format!("title of {lesson_id}"),
            context: Some("context".to_string()),
            problem: "problem".to_string(),
            root_cause: None,
            resolution: Some("resolution".to_string()),
            failed_tries: None,
            tags: Some("rust".to_string()),
            env_versions: None,
            superseded_by: None,
            hits: 0,
        }
    }

    fn lesson_key(row: &Lesson) -> String {
        row.id.clone()
    }

    fn group_key(group: &Conflict) -> (String, MergedField) {
        (group.id.clone(), group.field)
    }

    #[test]
    fn remote_slug_keeps_safe_characters_and_encodes_the_rest() {
        assert_eq!(
            remote_slug("git@github.com:owner/repo"),
            "git_40github_2ecom_3aowner_2frepo"
        );
        assert_eq!(remote_slug("a_b"), "a_5fb", "an underscore is encoded");
        assert_eq!(remote_slug(".."), "_2e_2e", "never the parent directory");
        assert_eq!(
            remote_slug(""),
            EMPTY_REMOTE_SLUG,
            "the empty remote still has a name"
        );
        assert!(
            ["a.b", "x/.y", "..", ".hidden", "v1.2"]
                .iter()
                .all(|remote| !remote_slug(remote).contains('.')),
            "a slug has no dot, so it cannot equal a sibling file such as <slug>.state"
        );
    }

    #[test]
    fn remote_slug_ignores_case_so_one_directory_serves_one_remote() {
        assert_eq!(
            remote_slug("git@GitHub.com:Owner/Repo"),
            remote_slug("git@github.com:owner/repo")
        );
    }

    #[test]
    fn fnv1a_64_matches_the_published_test_vectors() {
        assert_eq!(fnv1a_64(b""), FNV_OFFSET_BASIS);
        assert_eq!(fnv1a_64(b"a"), FNV1A_64_OF_A);
        assert_eq!(fnv1a_64(b"foobar"), FNV1A_64_OF_FOOBAR);
    }

    #[test]
    fn a_long_remote_gets_a_bounded_slug_that_still_tells_remotes_apart() {
        let shared_head: String = "a/".repeat(MAX_SLUG_BYTES);
        let first_slug: String = remote_slug(&format!("{shared_head}one"));
        let second_slug: String = remote_slug(&format!("{shared_head}two"));
        assert_ne!(first_slug, second_slug, "same head, different tail");
        assert!(
            first_slug.len() <= MAX_SLUG_BYTES + HASH_SUFFIX_BYTES,
            "{}",
            first_slug.len()
        );
        assert_eq!(
            first_slug,
            remote_slug(&format!("{shared_head}one")),
            "stable across calls"
        );
        let exactly_at_limit: String = "x".repeat(MAX_SLUG_BYTES);
        assert_eq!(
            remote_slug(&exactly_at_limit),
            exactly_at_limit,
            "at the limit nothing is cut or hashed"
        );
        assert_eq!(
            remote_slug(&format!("{exactly_at_limit}x")).len(),
            MAX_SLUG_BYTES + HASH_SUFFIX_BYTES
        );
    }

    #[test]
    fn remote_slug_never_maps_two_remotes_to_one_slug() {
        let lookalikes: &[&str] = &["a/b", "a_2fb", "a-b", "a.b", "a_b", "", "_"];
        let slugs: std::collections::BTreeSet<String> = lookalikes
            .iter()
            .map(|remote| remote_slug(remote))
            .collect();
        assert_eq!(slugs.len(), lookalikes.len(), "{slugs:?}");
    }

    /// The pin test for the on-disk layout: these names are a contract with clones made by earlier versions.
    #[test]
    fn clone_paths_layout_is_pinned() {
        let lessons_home: &Path = Path::new("/lessons-home");
        let paths = ClonePaths::new(lessons_home, "x/y");
        let sync_dir: PathBuf = lessons_home.join("sync");
        assert_eq!(paths.clone_dir(), sync_dir.join("x_2fy"));
        assert_eq!(paths.lock_file(), sync_dir.join("x_2fy.lock"));
        assert_eq!(paths.state_file(), sync_dir.join("x_2fy.state"));
        assert_eq!(
            paths.live_queue(Domain::Shared),
            sync_dir.join("x_2fy.shared.conflicts.jsonl")
        );
    }

    #[test]
    fn to_jsonl_writes_one_sorted_line_per_row() {
        let text: String =
            to_jsonl(&[lesson(ID_C), lesson(ID_A), lesson(ID_B)], lesson_key).unwrap();
        let ids: Vec<String> = text
            .lines()
            .map(|line| serde_json::from_str::<Lesson>(line).unwrap().id)
            .collect();
        assert_eq!(ids, vec![ID_A, ID_B, ID_C]);
        assert!(text.ends_with('\n'));
        assert_eq!(to_jsonl::<Lesson, String>(&[], lesson_key).unwrap(), "");
    }

    #[test]
    fn lessons_and_conflicts_round_trip_through_jsonl() {
        let lessons: Vec<Lesson> = vec![lesson(ID_A), lesson(ID_B)];
        let text: String = to_jsonl(&lessons, lesson_key).unwrap();
        assert_eq!(from_jsonl(&text, lesson_key).unwrap(), lessons);
        let groups: Vec<Conflict> = vec![Conflict {
            id: ID_A.to_string(),
            field: MergedField::Resolution,
            base: None,
            versions: vec![Version {
                value: Some("one".to_string()),
                updated_at: "2026-11-02T00:00:00Z".to_string(),
            }],
        }];
        let queue_text: String = to_jsonl(&groups, group_key).unwrap();
        assert_eq!(from_jsonl(&queue_text, group_key).unwrap(), groups);
    }

    #[test]
    fn from_jsonl_skips_blank_lines_and_reads_crlf() {
        let text: String = format!("\n{}\r\n\n", serde_json::to_string(&lesson(ID_A)).unwrap());
        assert_eq!(from_jsonl(&text, lesson_key).unwrap(), vec![lesson(ID_A)]);
    }

    #[test]
    fn from_jsonl_names_the_line_that_does_not_parse() {
        let text: String = format!(
            "{}\nnot json\n",
            serde_json::to_string(&lesson(ID_A)).unwrap()
        );
        let parse_error: String = format!("{:#}", from_jsonl(&text, lesson_key).unwrap_err());
        assert!(parse_error.contains("line 2"), "{parse_error}");
    }

    #[test]
    fn from_jsonl_refuses_three_copies_of_a_key() {
        let three_copies: String = to_jsonl(
            &[lesson(ID_A), lesson(ID_A), lesson(ID_B), lesson(ID_A)],
            lesson_key,
        )
        .unwrap();
        let three_error: String =
            format!("{:#}", from_jsonl(&three_copies, lesson_key).unwrap_err());
        assert!(
            three_error.contains(ID_A) && !three_error.contains(ID_B),
            "{three_error}"
        );
    }

    #[test]
    fn from_jsonl_refuses_a_duplicate_key_and_names_it() {
        let text: String =
            to_jsonl(&[lesson(ID_A), lesson(ID_B), lesson(ID_A)], lesson_key).unwrap();
        let duplicate_error: String = format!("{:#}", from_jsonl(&text, lesson_key).unwrap_err());
        assert!(
            duplicate_error.contains(ID_A) && !duplicate_error.contains(ID_B),
            "{duplicate_error}"
        );
    }

    #[test]
    fn read_text_or_empty_reads_a_missing_file_as_empty_and_fails_on_a_directory() {
        let temp_dir = tempfile::tempdir().unwrap();
        assert_eq!(
            read_text_or_empty(&temp_dir.path().join("missing.jsonl")).unwrap(),
            ""
        );
        assert!(
            read_text_or_empty(temp_dir.path()).is_err(),
            "a directory is not an empty file"
        );
        fs::write(temp_dir.path().join("present.jsonl"), "text").unwrap();
        assert_eq!(
            read_text_or_empty(&temp_dir.path().join("present.jsonl")).unwrap(),
            "text"
        );
    }

    #[test]
    fn no_clone_directory_equals_another_remotes_state_lock_or_queue_file() {
        let lessons_home: &Path = Path::new("/lessons-home");
        let owner = ClonePaths::new(lessons_home, "a");
        let lookalike = ClonePaths::new(lessons_home, "a.state");
        let lock_lookalike = ClonePaths::new(lessons_home, "a.lock");
        let queue_lookalike = ClonePaths::new(lessons_home, "a.shared.conflicts.jsonl");
        assert_ne!(lookalike.clone_dir(), owner.state_file());
        assert_ne!(lock_lookalike.clone_dir(), owner.lock_file());
        assert_ne!(
            queue_lookalike.clone_dir(),
            owner.live_queue(Domain::Shared)
        );
    }

    #[test]
    fn write_atomically_refuses_a_directory_in_the_files_place() {
        let temp_dir = tempfile::tempdir().unwrap();
        let target_path: PathBuf = temp_dir.path().join("work.jsonl");
        fs::create_dir(&target_path).unwrap();
        let write_error: String = format!(
            "{:#}",
            write_atomically(&target_path, "text\n").unwrap_err()
        );
        assert!(write_error.contains("rename"), "{write_error}");
        assert!(target_path.is_dir(), "the directory is untouched");
    }

    /// Every file written is the owner's alone, even when a crash left a world-readable temp file of the same name.
    #[cfg(unix)]
    #[test]
    fn write_atomically_leaves_only_the_owner_able_to_read_the_file() {
        const OPEN_MODE: u32 = 0o644;
        const PERMISSION_BITS: u32 = 0o777;
        let temp_dir = tempfile::tempdir().unwrap();
        let target_path: PathBuf = temp_dir.path().join("work.jsonl");
        let stale_temp: PathBuf = temp_dir.path().join("work.jsonl.tmp");
        fs::write(&stale_temp, "left by a crash").unwrap();
        fs::set_permissions(&stale_temp, fs::Permissions::from_mode(OPEN_MODE)).unwrap();
        write_atomically(&target_path, "private\n").unwrap();
        let written_mode: u32 =
            fs::metadata(&target_path).unwrap().permissions().mode() & PERMISSION_BITS;
        assert_eq!(written_mode, PRIVATE_FILE_MODE);
    }

    #[test]
    fn write_atomically_creates_the_directory_and_replaces_the_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let target_path: PathBuf = temp_dir.path().join("sync").join("work.jsonl");
        write_atomically(&target_path, "first\n").unwrap();
        assert_eq!(fs::read_to_string(&target_path).unwrap(), "first\n");
        let stale_temp: PathBuf = temp_dir.path().join("sync").join("work.jsonl.tmp");
        fs::write(&stale_temp, "half written by a crash").unwrap();
        write_atomically(&target_path, "second\n").unwrap();
        assert_eq!(
            fs::read_to_string(&target_path).unwrap(),
            "second\n",
            "a stale temp file never wins"
        );
        assert!(!stale_temp.exists(), "the temp file is renamed away");
    }

    /// Back-to-back: the same write twice leaves the last text and no temp file.
    #[test]
    fn writing_twice_leaves_the_last_text_and_no_temp_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let target_path: PathBuf = temp_dir.path().join("shared.jsonl");
        write_atomically(&target_path, "same\n").unwrap();
        write_atomically(&target_path, "same\n").unwrap();
        assert_eq!(fs::read_to_string(&target_path).unwrap(), "same\n");
        assert_eq!(fs::read_dir(temp_dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn state_round_trips_and_a_missing_empty_or_corrupt_file_reads_as_the_default() {
        let temp_dir = tempfile::tempdir().unwrap();
        let state_path: PathBuf = temp_dir.path().join("x.state");
        assert_eq!(read_state(&state_path), SyncState::default(), "missing");
        let state = SyncState {
            last_synced_commit: Some(SOME_COMMIT.to_string()),
            last_pull_unix_secs: Some(SOME_PULL_SECS),
            consecutive_failures: SOME_FAILURES,
            ..SyncState::default()
        };
        write_state(&state_path, &state).unwrap();
        assert_eq!(read_state(&state_path), state, "round trip");
        fs::write(&state_path, "").unwrap();
        assert_eq!(read_state(&state_path), SyncState::default(), "empty");
        fs::write(&state_path, "{ not json").unwrap();
        assert_eq!(read_state(&state_path), SyncState::default(), "corrupt");
    }

    #[test]
    fn an_unreadable_state_path_reads_as_the_default() {
        let temp_dir = tempfile::tempdir().unwrap();
        assert_eq!(
            read_state(temp_dir.path()),
            SyncState::default(),
            "a directory is not a state file"
        );
    }

    #[test]
    fn state_ignores_unknown_fields_and_defaults_missing_ones() {
        let temp_dir = tempfile::tempdir().unwrap();
        let state_path: PathBuf = temp_dir.path().join("x.state");
        fs::write(
            &state_path,
            format!("{{\"last_synced_commit\": \"{SOME_COMMIT}\", \"added_later\": true}}"),
        )
        .unwrap();
        assert_eq!(
            read_state(&state_path),
            SyncState {
                last_synced_commit: Some(SOME_COMMIT.to_string()),
                ..SyncState::default()
            }
        );
    }
}
