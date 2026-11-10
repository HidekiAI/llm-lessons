//! Opening a lessons DB file and keeping its schema current.
//!
//! Each domain (work, personal, shared) is its own SQLite file with the same schema. They are
//! separate files, not one table with a domain column, because a column cannot keep work data off
//! a personal machine or personal data out of work tooling; a file that is simply absent can.

use std::cmp::Ordering;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, TransactionBehavior};

use crate::journal;

/// How long a connection waits on another process's lock before failing with SQLITE_BUSY.
/// WAL lets the hook read while a writer works, but two writers (two `lessons add` runs, or a
/// write racing a migration) still take turns; without a timeout the second fails at once.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The three domains, one DB file each.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")] // config.toml spells domains as the file stems do
pub enum Domain {
    Work,
    Personal,
    Shared,
}

impl Domain {
    pub const ALL: [Domain; 3] = [Domain::Work, Domain::Personal, Domain::Shared];

    /// The name used for the file stem, for the ATTACH schema name, and in config.toml.
    pub fn as_str(self) -> &'static str {
        match self {
            Domain::Work => "work",
            Domain::Personal => "personal",
            Domain::Shared => "shared",
        }
    }

    pub fn file_path(self, lessons_home: &Path) -> PathBuf {
        lessons_home.join(format!("{}.db", self.as_str()))
    }
}

/// The data directory: `LESSONS_HOME` when set and non-empty, else `~/.lessons`.
pub fn lessons_home() -> Result<PathBuf> {
    lessons_home_from(std::env::var_os("LESSONS_HOME"), dirs::home_dir())
}

/// Pure half of `lessons_home`. The inputs are arguments so tests never mutate the process
/// environment, which is unsafe in Rust 2024 and races between parallel tests.
fn lessons_home_from(env_override: Option<OsString>, home_dir: Option<PathBuf>) -> Result<PathBuf> {
    match env_override.filter(|override_dir| !override_dir.is_empty()) {
        Some(override_dir) => Ok(PathBuf::from(override_dir)),
        None => home_dir
            .map(|home| home.join(".lessons"))
            .context("cannot find the home directory; set LESSONS_HOME"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenMode {
    /// `lessons init`: create the file, and its directory, when missing.
    Create,
    /// Every other command. A missing file is an error the caller decides on (search skips it,
    /// `--to` reports it); it is never created as a side effect of a typo or a fresh machine.
    Existing,
}

/// Opens `path`, sets the per-connection pragmas, and migrates the schema to the latest version.
pub fn open(path: &Path, mode: OpenMode) -> Result<Connection> {
    let open_flags: OpenFlags = match mode {
        OpenMode::Create => {
            if let Some(parent_dir) = path.parent() {
                std::fs::create_dir_all(parent_dir)
                    .with_context(|| format!("cannot create directory {}", parent_dir.display()))?;
            }
            OpenFlags::default()
        }
        OpenMode::Existing => OpenFlags::default().difference(OpenFlags::SQLITE_OPEN_CREATE),
    };
    let mut mut_conn: Connection = Connection::open_with_flags(path, open_flags)
        .with_context(|| format!("cannot open {} ({mode:?})", path.display()))?;
    journal!("open {}: opened ({mode:?})", path.display());

    // The timeout goes first: switching to WAL and migrating both take locks.
    mut_conn.busy_timeout(BUSY_TIMEOUT)?;

    // WAL mode is stored in the file, so only a file not yet in it is switched: the switch needs
    // an exclusive lock, and readers (search, the hook) must not queue behind a writer for it.
    // SQLite opens lazily: this first read is where a corrupt or non-SQLite file fails, so it names the file.
    let current_journal_mode: String = mut_conn
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .with_context(|| format!("cannot read {}", path.display()))?;
    // journal_mode answers with the mode it actually got. A filesystem that cannot do WAL (some
    // network mounts) keeps the old mode; the file still works, readers just block on writers.
    let journal_mode: String = if current_journal_mode.eq_ignore_ascii_case("wal") {
        current_journal_mode
    } else {
        switch_to_wal(&mut_conn)
            .with_context(|| format!("cannot set WAL mode on {}", path.display()))?
    };
    if !journal_mode.eq_ignore_ascii_case("wal") {
        eprintln!(
            "lessons: WARNING: {} is in journal mode {journal_mode}, not WAL; readers will wait on writers",
            path.display()
        );
    }
    mut_conn.pragma_update(None, "foreign_keys", true)?;

    migrate(&mut mut_conn, path)?;
    Ok(mut_conn)
}

/// Opens the file a write targets (`add --to`). A missing file is an error that names the fix,
/// because the user chose this domain explicitly; it is never created as a side effect.
pub fn open_target(lessons_home: &Path, domain: Domain) -> Result<Connection> {
    let db_path: PathBuf = domain.file_path(lessons_home);
    if !db_path.exists() {
        bail!(
            "{} does not exist; run `lessons init --domain {}` first",
            db_path.display(),
            domain.as_str()
        );
    }
    open(&db_path, OpenMode::Existing)
}

/// Opens each requested domain whose file exists and skips the rest: a machine may hold only some
/// of the three files on purpose (no personal.db on a work laptop).
pub fn open_existing_domains(
    lessons_home: &Path,
    domains: &[Domain],
) -> Result<Vec<(Domain, Connection)>> {
    domains
        .iter()
        .filter_map(|&domain| {
            let db_path: PathBuf = domain.file_path(lessons_home);
            if db_path.exists() {
                Some(open(&db_path, OpenMode::Existing).map(|conn| (domain, conn)))
            } else {
                journal!(
                    "skip {}: {} does not exist",
                    domain.as_str(),
                    db_path.display()
                );
                None
            }
        })
        .collect()
}

/// Pause between attempts to switch a contended file into WAL mode.
const WAL_RETRY_PAUSE: Duration = Duration::from_millis(10);

/// Sets WAL mode and returns the mode SQLite actually chose.
///
/// The busy timeout does not cover this step. Switching modes upgrades a shared lock to an
/// exclusive one, and when two connections attempt that at once (several first opens of a fresh
/// file), waiting would deadlock, so SQLite returns SQLITE_BUSY at once instead of calling the busy
/// handler; the documented answer is to retry. Retries stop once BUSY_TIMEOUT has been spent.
fn switch_to_wal(conn: &Connection) -> rusqlite::Result<String> {
    let attempt_count: u128 = BUSY_TIMEOUT.as_millis() / WAL_RETRY_PAUSE.as_millis();
    let fn_try_switch =
        || conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get::<_, String>(0));
    (1..=attempt_count)
        .find_map(|attempt| match fn_try_switch() {
            Err(rusqlite::Error::SqliteFailure(failure, _))
                if failure.code == rusqlite::ErrorCode::DatabaseBusy && attempt < attempt_count =>
            {
                journal!("open: WAL switch busy, retry {attempt} of {attempt_count}");
                std::thread::sleep(WAL_RETRY_PAUSE);
                None
            }
            last_outcome => Some(last_outcome),
        })
        .expect("attempt_count is at least 1, and its last attempt always yields an outcome")
}

/// Schema migrations, in order: entry N (1-based) moves a file from `user_version` N-1 to N.
/// Append new entries; never edit one that has shipped, files in the wild already ran it.
const MIGRATIONS: &[&str] = &[SCHEMA_V1];

fn migrate(conn: &mut Connection, path: &Path) -> Result<()> {
    // Fast path without any lock: a file already at the latest version needs nothing, and taking
    // the write lock here would make every read-only command (search, the hook) wait on writers.
    let unlocked_version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if unlocked_version == MIGRATIONS.len() as i64 {
        journal!(
            "open {}: schema at version {unlocked_version}, nothing to migrate",
            path.display()
        );
        return Ok(());
    }
    // IMMEDIATE takes the write lock BEFORE the version is read, so two processes opening a fresh
    // file at the same moment cannot both read 0 and both run CREATE TABLE; the second waits, then
    // reads the version the first one committed.
    let transaction = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .with_context(|| {
            format!(
                "cannot take the write lock on {} to migrate",
                path.display()
            )
        })?;
    let current_version: i64 =
        transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let latest_version: i64 = MIGRATIONS.len() as i64; // a handful of migrations, nowhere near i64::MAX

    match current_version.cmp(&latest_version) {
        Ordering::Equal => {
            journal!(
                "open {}: schema at version {current_version}, nothing to migrate",
                path.display()
            );
            Ok(()) // nothing written, so dropping the transaction (a rollback) is correct
        }
        Ordering::Greater => bail!(
            "{} has schema version {current_version}, newer than this lessons binary supports \
             ({latest_version}); upgrade lessons",
            path.display()
        ),
        Ordering::Less => {
            // user_version is a signed 32-bit field, so a hand-edited file could hold a negative value.
            let migrations_already_applied: usize =
                usize::try_from(current_version).with_context(|| {
                    format!(
                        "{} has invalid schema version {current_version}",
                        path.display()
                    )
                })?;
            MIGRATIONS
                .iter()
                .enumerate()
                .skip(migrations_already_applied)
                .try_for_each(|(migration_index, migration_sql)| {
                    journal!(
                        "open {}: applying migration {}",
                        path.display(),
                        migration_index + 1
                    );
                    transaction.execute_batch(migration_sql)
                })
                .with_context(|| format!("migration of {} failed", path.display()))?;
            transaction.pragma_update(None, "user_version", latest_version)?;
            transaction.commit()?;
            journal!(
                "open {}: migrated schema {current_version} -> {latest_version}",
                path.display()
            );
            Ok(())
        }
    }
}

// journal_mode and foreign_keys are set per connection in `open`, and user_version by `migrate`,
// not here: this text runs inside a transaction, and SQLite refuses to change into WAL mode
// from within one.
const SCHEMA_V1: &str = r#"
CREATE TABLE lesson (
    -- Explicit INTEGER PRIMARY KEY on purpose: the FTS5 external-content table keys on rowid,
    -- and VACUUM may renumber the implicit rowid of tables that lack an INTEGER PRIMARY KEY,
    -- which would silently desync the index. See https://www.sqlite.org/lang_vacuum.html
    seq           INTEGER PRIMARY KEY,

    -- Public id: ULID text, unique across ALL three files, so `move` between files never collides.
    id            TEXT NOT NULL UNIQUE,

    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    updated_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    title         TEXT NOT NULL,   -- one-line summary
    context       TEXT,            -- where: repo name, 'car', 'home network', 'taxes', ...
    problem       TEXT NOT NULL,   -- what went wrong; paste exact error text when there is one
    root_cause    TEXT,            -- may be unknown at first; fill in later
    resolution    TEXT,            -- what worked
    failed_tries  TEXT,            -- what did NOT work (the Edison column)
    tags          TEXT,            -- space separated
    env_versions  TEXT,            -- versions / dates that let a reader judge staleness
    superseded_by TEXT,            -- id of the newer lesson (may live in another file, so no FK)
    hits          INTEGER NOT NULL DEFAULT 0   -- bumped when a retrieved lesson actually helped
);

CREATE VIRTUAL TABLE lesson_fts USING fts5(
    title, context, problem, root_cause, resolution, failed_tries, tags,
    content = 'lesson',
    content_rowid = 'seq',
    tokenize = 'porter unicode61'  -- stemming: "failing" matches "failed"
);

-- Sync triggers: pattern from https://www.sqlite.org/fts5.html#external_content_tables
CREATE TRIGGER lesson_ai AFTER INSERT ON lesson BEGIN
    INSERT INTO lesson_fts(rowid, title, context, problem, root_cause, resolution, failed_tries, tags)
    VALUES (new.seq, new.title, new.context, new.problem, new.root_cause, new.resolution,
            new.failed_tries, new.tags);
END;

CREATE TRIGGER lesson_ad AFTER DELETE ON lesson BEGIN
    INSERT INTO lesson_fts(lesson_fts, rowid, title, context, problem, root_cause, resolution,
                           failed_tries, tags)
    VALUES ('delete', old.seq, old.title, old.context, old.problem, old.root_cause,
            old.resolution, old.failed_tries, old.tags);
END;

-- Only reindex when indexed columns change, so bumping `hits` stays cheap.
CREATE TRIGGER lesson_au AFTER UPDATE OF title, context, problem, root_cause, resolution,
                                         failed_tries, tags ON lesson BEGIN
    INSERT INTO lesson_fts(lesson_fts, rowid, title, context, problem, root_cause, resolution,
                           failed_tries, tags)
    VALUES ('delete', old.seq, old.title, old.context, old.problem, old.root_cause,
            old.resolution, old.failed_tries, old.tags);
    INSERT INTO lesson_fts(rowid, title, context, problem, root_cause, resolution, failed_tries, tags)
    VALUES (new.seq, new.title, new.context, new.problem, new.root_cause, new.resolution,
            new.failed_tries, new.tags);
END;
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// A DB path inside a fresh temp dir; the dir must outlive the test, so it is returned too.
    fn temp_db_path() -> (tempfile::TempDir, PathBuf) {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let db_path: PathBuf = temp_dir.path().join("nested").join("work.db");
        (temp_dir, db_path)
    }

    fn insert_lesson(conn: &Connection, lesson_id: &str, title: &str) {
        conn.execute(
            "INSERT INTO lesson (id, title, problem) VALUES (?1, ?2, 'cargo build failed')",
            (lesson_id, title),
        )
        .expect("insert lesson");
    }

    fn match_count(conn: &Connection, fts_query: &str) -> i64 {
        conn.query_row(
            "SELECT count(*) FROM lesson_fts WHERE lesson_fts MATCH ?1",
            [fts_query],
            |row| row.get(0),
        )
        .expect("match query")
    }

    #[test]
    fn create_migrates_a_fresh_file_to_the_latest_version_in_wal_mode() {
        let (_temp_dir, db_path) = temp_db_path();
        let conn: Connection = open(&db_path, OpenMode::Create).expect("open");
        let schema_version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        let journal_mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(schema_version, MIGRATIONS.len() as i64);
        assert_eq!(journal_mode, "wal");
    }

    #[test]
    fn reopening_a_current_file_is_a_no_op() {
        let (_temp_dir, db_path) = temp_db_path();
        let first_conn: Connection = open(&db_path, OpenMode::Create).expect("first open");
        insert_lesson(&first_conn, "01ID", "kept across reopen");
        drop(first_conn);
        let second_conn: Connection = open(&db_path, OpenMode::Existing).expect("second open");
        assert_eq!(match_count(&second_conn, "kept"), 1);
    }

    #[test]
    fn concurrent_first_opens_of_one_fresh_file_all_succeed() {
        // Enough simultaneous first opens to make the WAL-switch race show up within a few runs.
        const CONCURRENT_OPENERS: usize = 8;
        // Each thread has its own connection, as separate processes would.
        let (_temp_dir, db_path) = temp_db_path();
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let open_results: Vec<Result<Connection>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..CONCURRENT_OPENERS)
                .map(|_| scope.spawn(|| open(&db_path, OpenMode::Create)))
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("thread panicked"))
                .collect()
        });
        open_results.iter().for_each(|open_result| {
            assert!(
                open_result.is_ok(),
                "open failed: {:#}",
                open_result.as_ref().unwrap_err()
            )
        });
    }

    #[test]
    fn opening_a_current_file_does_not_wait_for_a_writer() {
        // Far below BUSY_TIMEOUT, far above a lock-free open (milliseconds).
        const NO_WAIT_LIMIT: Duration = Duration::from_secs(1);
        let (_temp_dir, db_path) = temp_db_path();
        drop(open(&db_path, OpenMode::Create).expect("create"));
        let mut mut_writer_conn: Connection = Connection::open(&db_path).unwrap();
        let writer_transaction = mut_writer_conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap(); // holds the write lock for the rest of the test
        let open_started = std::time::Instant::now();
        let reader_conn: Connection =
            open(&db_path, OpenMode::Existing).expect("open while a writer holds the lock");
        assert!(
            open_started.elapsed() < NO_WAIT_LIMIT,
            "open waited {:?} on the writer; BUSY_TIMEOUT is {BUSY_TIMEOUT:?}",
            open_started.elapsed()
        );
        let lesson_count: i64 = reader_conn
            .query_row("SELECT count(*) FROM lesson", [], |row| row.get(0))
            .unwrap();
        assert_eq!(lesson_count, 0);
        drop(writer_transaction);
    }

    #[test]
    fn open_existing_domains_skips_missing_files() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        open(&Domain::Shared.file_path(temp_dir.path()), OpenMode::Create).expect("create shared");
        let opened: Vec<Domain> =
            open_existing_domains(temp_dir.path(), &[Domain::Work, Domain::Shared])
                .expect("open")
                .into_iter()
                .map(|(domain, _conn)| domain)
                .collect();
        assert_eq!(opened, vec![Domain::Shared]);
        assert!(!Domain::Work.file_path(temp_dir.path()).exists());
    }

    #[test]
    fn open_target_names_init_when_the_file_is_missing() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let open_error: String = format!(
            "{:#}",
            open_target(temp_dir.path(), Domain::Work).unwrap_err()
        );
        assert!(
            open_error.contains("lessons init --domain work"),
            "unexpected error: {open_error}"
        );
    }

    #[test]
    fn existing_mode_refuses_a_missing_file_and_does_not_create_it() {
        let (_temp_dir, db_path) = temp_db_path();
        assert!(open(&db_path, OpenMode::Existing).is_err());
        assert!(!db_path.exists());
    }

    #[test]
    fn a_schema_newer_than_this_binary_is_refused() {
        let (_temp_dir, db_path) = temp_db_path();
        let conn: Connection = open(&db_path, OpenMode::Create).expect("open");
        conn.pragma_update(None, "user_version", MIGRATIONS.len() as i64 + 1)
            .unwrap(); // one version past what this binary knows
        drop(conn);
        let open_error: String = format!("{:#}", open(&db_path, OpenMode::Existing).unwrap_err());
        assert!(
            open_error.contains("newer"),
            "unexpected error: {open_error}"
        );
    }

    #[test]
    fn fts5_is_compiled_in_and_the_insert_trigger_indexes_with_stemming() {
        let (_temp_dir, db_path) = temp_db_path();
        let conn: Connection = open(&db_path, OpenMode::Create).expect("open");
        insert_lesson(&conn, "01ID", "linker error");
        // The stored problem says "failed"; the porter tokenizer must match "failing" to it.
        assert_eq!(match_count(&conn, "failing"), 1);
    }

    #[test]
    fn the_delete_trigger_removes_the_row_from_the_index() {
        let (_temp_dir, db_path) = temp_db_path();
        let conn: Connection = open(&db_path, OpenMode::Create).expect("open");
        insert_lesson(&conn, "01ID", "linker error");
        conn.execute("DELETE FROM lesson WHERE id = '01ID'", [])
            .unwrap();
        assert_eq!(match_count(&conn, "linker"), 0);
    }

    #[test]
    fn editing_an_indexed_column_reindexes_but_bumping_hits_does_not() {
        let (_temp_dir, db_path) = temp_db_path();
        let conn: Connection = open(&db_path, OpenMode::Create).expect("open");
        insert_lesson(&conn, "01ID", "linker error");

        // total_changes counts rows written by triggers too, so an UPDATE that fires lesson_au
        // moves it by more than the one row the UPDATE itself changed.
        let fn_changes_made_by = |update_sql: &str| -> u64 {
            let changes_before: u64 = conn.total_changes();
            conn.execute(update_sql, []).expect("update");
            conn.total_changes() - changes_before
        };
        assert_eq!(
            fn_changes_made_by("UPDATE lesson SET hits = hits + 1 WHERE id = '01ID'"),
            1
        );
        assert!(
            fn_changes_made_by("UPDATE lesson SET title = 'loader error' WHERE id = '01ID'") > 1
        );
        assert_eq!(match_count(&conn, "loader"), 1);
        assert_eq!(match_count(&conn, "linker"), 0);
    }

    #[test]
    fn lessons_home_prefers_a_non_empty_override_then_falls_back_to_home() {
        let home_dir = Some(PathBuf::from("/home/someone"));
        assert_eq!(
            lessons_home_from(Some("/data/lessons".into()), home_dir.clone()).unwrap(),
            PathBuf::from("/data/lessons")
        );
        assert_eq!(
            lessons_home_from(Some("".into()), home_dir.clone()).unwrap(),
            PathBuf::from("/home/someone/.lessons")
        );
        assert_eq!(
            lessons_home_from(None, home_dir).unwrap(),
            PathBuf::from("/home/someone/.lessons")
        );
        assert!(lessons_home_from(None, None).is_err());
    }
}
