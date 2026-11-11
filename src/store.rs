//! The data access layer: the only module that holds SQL about lessons.
//!
//! Commands, search and the hook go through these types and functions, so adding a column touches
//! a migration in `db.rs` and this file, nothing else.

use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, OptionalExtension, Row, Transaction};
use serde::{Deserialize, Serialize};

use crate::db::Domain;
use crate::journal;

/// The searchable columns, in the order the FTS5 table declares them, each with its bm25 weight.
/// bm25() takes weights by position, so names and weights live together here and the search query
/// is built from this list; a test checks it against the live table so the two cannot drift.
pub const FTS_COLUMNS: &[(&str, f64)] = &[
    ("title", 10.0),
    ("context", 2.0),
    ("problem", 5.0),
    ("root_cause", 3.0),
    ("resolution", 3.0),
    ("failed_tries", 2.0),
    ("tags", 4.0),
];

/// One stored lesson. Its serde field names are the LLM-facing JSON contract: rename nothing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lesson {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    pub title: String,
    pub context: Option<String>,
    pub problem: String,
    pub root_cause: Option<String>,
    pub resolution: Option<String>,
    pub failed_tries: Option<String>,
    pub tags: Option<String>,
    pub env_versions: Option<String>,
    pub superseded_by: Option<String>,
    pub hits: i64,
}

/// A lesson together with the file it was found in, which is what `show` and `search` print.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FoundLesson {
    pub domain: &'static str,
    #[serde(flatten)]
    pub lesson: Lesson,
}

/// The input to `add`, from flags, `--json -` or the `--edit` template. Keys are column names.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)] // a misspelled key in LLM-written JSON fails loudly instead of being dropped
pub struct NewLesson {
    pub title: String,
    pub problem: String,
    pub context: Option<String>,
    pub root_cause: Option<String>,
    pub resolution: Option<String>,
    pub failed_tries: Option<String>,
    pub tags: Option<String>,
    pub env_versions: Option<String>,
}

/// Columns every SELECT reads, by name, so `lesson_from_row` never depends on their order.
const LESSON_SELECT_COLUMNS: &str = "id, created_at, updated_at, title, context, problem, root_cause, \
     resolution, failed_tries, tags, env_versions, superseded_by, hits";

fn lesson_from_row(row: &Row) -> rusqlite::Result<Lesson> {
    Ok(Lesson {
        id: row.get("id")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        title: row.get("title")?,
        context: row.get("context")?,
        problem: row.get("problem")?,
        root_cause: row.get("root_cause")?,
        resolution: row.get("resolution")?,
        failed_tries: row.get("failed_tries")?,
        tags: row.get("tags")?,
        env_versions: row.get("env_versions")?,
        superseded_by: row.get("superseded_by")?,
        hits: row.get("hits")?,
    })
}

/// Blank optional text is stored as NULL, so "no root cause yet" has exactly one representation.
fn non_blank(text: &Option<String>) -> Option<&str> {
    text.as_deref()
        .map(str::trim)
        .filter(|trimmed| !trimmed.is_empty())
}

/// Stores a new lesson and returns its ULID.
pub fn insert(conn: &Connection, new_lesson: &NewLesson) -> Result<String> {
    let title: &str = new_lesson.title.trim();
    let problem: &str = new_lesson.problem.trim();
    if title.is_empty() || problem.is_empty() {
        bail!("a lesson needs a non-empty title and problem");
    }
    // A ULID sorts by creation time and is unique across all three files, so `move` never collides.
    let lesson_id: String = ulid::Ulid::generate().to_string();
    conn.execute(
        "INSERT INTO lesson (id, title, problem, context, root_cause, resolution, failed_tries, tags, env_versions) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        (
            &lesson_id,
            title,
            problem,
            non_blank(&new_lesson.context),
            non_blank(&new_lesson.root_cause),
            non_blank(&new_lesson.resolution),
            non_blank(&new_lesson.failed_tries),
            non_blank(&new_lesson.tags),
            non_blank(&new_lesson.env_versions),
        ),
    )?;
    journal!("insert {lesson_id}: title {title:?}");
    Ok(lesson_id)
}

pub fn find_by_id(conn: &Connection, lesson_id: &str) -> Result<Option<Lesson>> {
    let found: Option<Lesson> = conn
        .query_row(
            &format!("SELECT {LESSON_SELECT_COLUMNS} FROM lesson WHERE id = ?1"),
            [lesson_id],
            lesson_from_row,
        )
        .optional()?;
    Ok(found)
}

/// Looks for `lesson_id` in each open file, in order, and returns the first match.
pub fn find_in_domains(
    opened: &[(Domain, Connection)],
    lesson_id: &str,
) -> Result<Option<FoundLesson>> {
    opened
        .iter()
        .try_fold(None, |first_match, (domain, conn)| match first_match {
            Some(already_found) => Ok(Some(already_found)), // a ULID lives in one file; stop querying once found
            None => find_by_id(conn, lesson_id).map(|found| {
                found.map(|lesson| FoundLesson {
                    domain: domain.as_str(),
                    lesson,
                })
            }),
        })
}

/// The open file that holds `lesson_id`, for commands that change a lesson in place.
pub fn owner_of<'opened>(
    opened: &'opened [(Domain, Connection)],
    lesson_id: &str,
) -> Result<Option<&'opened (Domain, Connection)>> {
    opened
        .iter()
        .try_fold(None, |first_owner, domain_and_conn| match first_owner {
            Some(already_found) => Ok(Some(already_found)),
            None => domain_and_conn
                .1
                .query_row("SELECT 1 FROM lesson WHERE id = ?1", [lesson_id], |_row| {
                    Ok(())
                })
                .optional()
                .map(|found| found.map(|()| domain_and_conn))
                .map_err(anyhow::Error::from), // try_fold needs one error type for both arms
        })
}

/// Changes to an existing lesson. Per field: `None` leaves it as it is, `Some("")` clears it
/// (stores NULL), any other text replaces it. Title and problem can be replaced, never cleared.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LessonPatch {
    pub title: Option<String>,
    pub problem: Option<String>,
    pub context: Option<String>,
    pub root_cause: Option<String>,
    pub resolution: Option<String>,
    pub failed_tries: Option<String>,
    pub tags: Option<String>,
    pub env_versions: Option<String>,
}

/// Applies `patch` and bumps `updated_at`. Returns false when no lesson has `lesson_id`.
pub fn update(conn: &Connection, lesson_id: &str, patch: &LessonPatch) -> Result<bool> {
    let fn_is_blank =
        |field: &Option<String>| field.as_deref().is_some_and(|text| text.trim().is_empty());
    let fields_named: &[&Option<String>] = &[
        &patch.title,
        &patch.problem,
        &patch.context,
        &patch.root_cause,
        &patch.resolution,
        &patch.failed_tries,
        &patch.tags,
        &patch.env_versions,
    ];
    if fields_named.iter().all(|field| field.is_none()) {
        bail!("nothing to update: pass at least one field flag");
    }
    if fn_is_blank(&patch.title) || fn_is_blank(&patch.problem) {
        bail!("title and problem cannot be cleared, only replaced");
    }
    // One static statement for every patch: a NULL parameter keeps the column (CASE), an empty
    // one clears it (NULLIF), so no SQL is assembled from strings at run time.
    let fn_trimmed = |field: &Option<String>| field.as_deref().map(str::trim).map(str::to_string);
    let changed_rows: usize = conn.execute(
        "UPDATE lesson SET \
             title        = CASE WHEN ?2 IS NULL THEN title        ELSE ?2 END, \
             problem      = CASE WHEN ?3 IS NULL THEN problem      ELSE ?3 END, \
             context      = CASE WHEN ?4 IS NULL THEN context      ELSE NULLIF(?4, '') END, \
             root_cause   = CASE WHEN ?5 IS NULL THEN root_cause   ELSE NULLIF(?5, '') END, \
             resolution   = CASE WHEN ?6 IS NULL THEN resolution   ELSE NULLIF(?6, '') END, \
             failed_tries = CASE WHEN ?7 IS NULL THEN failed_tries ELSE NULLIF(?7, '') END, \
             tags         = CASE WHEN ?8 IS NULL THEN tags         ELSE NULLIF(?8, '') END, \
             env_versions = CASE WHEN ?9 IS NULL THEN env_versions ELSE NULLIF(?9, '') END, \
             updated_at   = strftime('%Y-%m-%dT%H:%M:%SZ','now') \
         WHERE id = ?1",
        (
            lesson_id,
            fn_trimmed(&patch.title),
            fn_trimmed(&patch.problem),
            fn_trimmed(&patch.context),
            fn_trimmed(&patch.root_cause),
            fn_trimmed(&patch.resolution),
            fn_trimmed(&patch.failed_tries),
            fn_trimmed(&patch.tags),
            fn_trimmed(&patch.env_versions),
        ),
    )?;
    journal!("update {lesson_id}: {changed_rows} row(s) changed");
    Ok(changed_rows == 1)
}

/// Adds one to `hits` and returns the new count, or `None` for an unknown id. Leaves
/// `updated_at` alone: a hit records use, not an edit.
pub fn bump_hits(conn: &Connection, lesson_id: &str) -> Result<Option<i64>> {
    let new_hits: Option<i64> = conn
        .query_row(
            "UPDATE lesson SET hits = hits + 1 WHERE id = ?1 RETURNING hits",
            [lesson_id],
            |row| row.get(0),
        )
        .optional()?;
    journal!("hit {lesson_id}: hits now {new_hits:?}");
    Ok(new_hits)
}

/// Marks `lesson_id` as replaced by `superseded_by_id`, which hides it from search. Returns false
/// for an unknown id. The caller checks that the replacement exists: it may live in another file.
pub fn retire(conn: &Connection, lesson_id: &str, superseded_by_id: &str) -> Result<bool> {
    let changed_rows: usize = conn.execute(
        "UPDATE lesson SET superseded_by = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') \
         WHERE id = ?1",
        (lesson_id, superseded_by_id),
    )?;
    journal!("retire {lesson_id}: superseded by {superseded_by_id}, {changed_rows} row(s) changed");
    Ok(changed_rows == 1)
}

/// Writes `lesson` into `target` with its id, timestamps and counters, creating or refreshing it.
/// An upsert rather than INSERT OR IGNORE, so a re-run after a crash copies the source's current
/// state instead of keeping a stale copy; ON CONFLICT DO UPDATE fires the update trigger, so the
/// index stays in step (REPLACE would skip the delete trigger and desync it).
fn copy_into(target: &Connection, lesson: &Lesson) -> Result<()> {
    target.execute(
        "INSERT INTO lesson (id, created_at, updated_at, title, context, problem, root_cause, resolution, \
                             failed_tries, tags, env_versions, superseded_by, hits) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13) \
         ON CONFLICT(id) DO UPDATE SET \
             created_at = excluded.created_at, updated_at = excluded.updated_at, title = excluded.title, \
             context = excluded.context, problem = excluded.problem, root_cause = excluded.root_cause, \
             resolution = excluded.resolution, failed_tries = excluded.failed_tries, tags = excluded.tags, \
             env_versions = excluded.env_versions, superseded_by = excluded.superseded_by, hits = excluded.hits",
        rusqlite::params![
            lesson.id,
            lesson.created_at,
            lesson.updated_at,
            lesson.title,
            lesson.context,
            lesson.problem,
            lesson.root_cause,
            lesson.resolution,
            lesson.failed_tries,
            lesson.tags,
            lesson.env_versions,
            lesson.superseded_by,
            lesson.hits,
        ],
    )?;
    Ok(())
}

/// How often `move_lesson` re-copies a lesson that keeps changing while it is being moved.
const MOVE_ATTEMPTS: u32 = 3;

/// What `delete_if_unchanged` found in the source once it held the write lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceDelete {
    Deleted,
    /// The row was edited after it was copied, so the copy is stale and was not trusted.
    Changed,
    /// Nothing to delete: a concurrent move already finished it.
    AlreadyGone,
}

/// Deletes the source row only if it still equals `copied`. IMMEDIATE blocks other writers on
/// the source, so the row cannot change between this comparison and the delete.
fn delete_if_unchanged(source: &mut Connection, copied: &Lesson) -> Result<SourceDelete> {
    let transaction = source.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    match find_by_id(&transaction, &copied.id)? {
        Some(current) if current == *copied => {
            transaction.execute("DELETE FROM lesson WHERE id = ?1", [&copied.id])?;
            transaction.commit()?;
            Ok(SourceDelete::Deleted)
        }
        Some(_changed) => Ok(SourceDelete::Changed), // dropping the transaction rolls back; nothing was written
        None => Ok(SourceDelete::AlreadyGone),
    }
}

/// Moves `lesson_id` from `source` to `target`. Returns false when `source` does not hold it.
///
/// Two commits, one per file: a transaction cannot span two SQLite files atomically in WAL mode.
/// The copy commits first, so a crash in between leaves a copy in both files, never in neither;
/// re-running the move then refreshes the copy and finishes the delete.
///
/// Known limit: two moves of the same lesson to DIFFERENT targets at the same moment can both copy
/// before either deletes, leaving a copy in each target. Running `move <id> --to <one of them>`
/// afterwards merges them back into one, because it takes the other copy as its source.
pub fn move_lesson(source: &mut Connection, target: &Connection, lesson_id: &str) -> Result<bool> {
    for attempt in 1..=MOVE_ATTEMPTS {
        let copied: Lesson = match find_by_id(source, lesson_id)? {
            Some(lesson) => lesson,
            None => return Ok(false),
        };
        copy_into(target, &copied)?;
        if find_by_id(target, lesson_id)?.as_ref() != Some(&copied) {
            bail!(
                "the copy of {lesson_id} did not land in the target; the source is left untouched"
            );
        }
        match delete_if_unchanged(source, &copied)? {
            SourceDelete::Deleted => {
                journal!("move {lesson_id}: copied and deleted from source (attempt {attempt})");
                return Ok(true);
            }
            SourceDelete::Changed => {
                journal!(
                    "move {lesson_id}: source changed during the move, copying again (attempt {attempt})"
                );
            }
            SourceDelete::AlreadyGone => {
                journal!(
                    "move {lesson_id}: source row already gone (a concurrent move finished it)"
                );
                return Ok(true);
            }
        }
    }
    bail!(
        "{lesson_id} kept changing during the move ({MOVE_ATTEMPTS} attempts); both files hold a copy, run move again"
    )
}

/// A search result: the lesson, its file, and its bm25 score (lower ranks higher).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchHit {
    pub domain: &'static str,
    pub score: f64,
    #[serde(flatten)]
    pub lesson: Lesson,
}

/// The best `limit` live lessons of one file for an FTS5 `fts_query`, best first.
pub fn search(
    conn: &Connection,
    domain: Domain,
    fts_query: &str,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    // Both tables have columns named title, problem, ...; qualify and alias each, so the join is
    // unambiguous and `lesson_from_row` still reads them by their plain names.
    let qualified_columns: String = LESSON_SELECT_COLUMNS
        .split(',')
        .map(|column_name| format!("l.{0} AS {0}", column_name.trim()))
        .collect::<Vec<String>>()
        .join(", ");
    let bm25_weights: String = FTS_COLUMNS
        .iter()
        .map(|(_column_name, weight)| format!("{weight:?}")) // Debug keeps the ".0", so SQLite reads a real, not an integer
        .collect::<Vec<String>>()
        .join(", ");
    let mut mut_statement = conn.prepare(&format!(
        "SELECT {qualified_columns}, bm25(lesson_fts, {bm25_weights}) AS score \
         FROM lesson_fts JOIN lesson l ON l.seq = lesson_fts.rowid \
         WHERE lesson_fts MATCH ?1 AND l.superseded_by IS NULL \
         ORDER BY score LIMIT ?2"
    ))?;
    let hits: Vec<SearchHit> = mut_statement
        .query_map(
            (fts_query, i64::try_from(limit).unwrap_or(i64::MAX)),
            |row| {
                Ok(SearchHit {
                    domain: domain.as_str(),
                    score: row.get("score")?,
                    lesson: lesson_from_row(row)?,
                })
            },
        )?
        .collect::<rusqlite::Result<Vec<SearchHit>>>()?;
    journal!(
        "search {}: {} hit(s) for {fts_query}",
        domain.as_str(),
        hits.len()
    );
    Ok(hits)
}

/// Searches every open file and merges the results by score. Scores from different files are not
/// strictly comparable (each file has its own word statistics) but rank well enough for a top N.
pub fn search_all(
    opened: &[(Domain, Connection)],
    fts_query: &str,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    let per_file: Vec<Vec<SearchHit>> = opened
        .iter()
        .map(|(domain, conn)| search(conn, *domain, fts_query, limit))
        .collect::<Result<Vec<Vec<SearchHit>>>>()?;
    let mut mut_best: Vec<SearchHit> = per_file.into_iter().flatten().collect();
    mut_best.sort_by(|left_hit, right_hit| left_hit.score.total_cmp(&right_hit.score));
    mut_best.truncate(limit);
    Ok(mut_best)
}

/// The newest `limit` lessons of one file, optionally only those carrying `tag` as a whole tag.
pub fn list(conn: &Connection, limit: usize, tag: Option<&str>) -> Result<Vec<Lesson>> {
    // instr, not LIKE: `_` and `%` are wildcards in LIKE, and `_` is common in tags.
    let mut mut_statement = conn.prepare(&format!(
        "SELECT {LESSON_SELECT_COLUMNS} FROM lesson \
         WHERE ?1 IS NULL OR instr(' ' || lower(ifnull(tags, '')) || ' ', ' ' || lower(?1) || ' ') > 0 \
         ORDER BY created_at DESC, seq DESC LIMIT ?2"
    ))?;
    let lessons: Vec<Lesson> = mut_statement
        .query_map(
            (tag, i64::try_from(limit).unwrap_or(i64::MAX)),
            lesson_from_row,
        )? // a limit beyond i64 means "all"
        .collect::<rusqlite::Result<Vec<Lesson>>>()?;
    Ok(lessons)
}

/// Every lesson in the file, retired ones included, sorted by id: OURS for a sync. The sync reads it inside its
/// write transaction, so no local write lands between this read and `apply_merged`.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "called by the sync cycle (plan step S4)")
)]
pub fn all_lessons(conn: &Connection) -> Result<Vec<Lesson>> {
    // ORDER BY id, so OURS is sorted the same way the merge and the JSON lines are.
    let mut mut_statement = conn.prepare(&format!(
        "SELECT {LESSON_SELECT_COLUMNS} FROM lesson ORDER BY id"
    ))?;
    let lessons: Vec<Lesson> = mut_statement
        .query_map([], lesson_from_row)?
        .collect::<rusqlite::Result<Vec<Lesson>>>()?;
    journal!("all_lessons: {} row(s)", lessons.len());
    Ok(lessons)
}

/// What `apply_merged` wrote, for the sync journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppliedCounts {
    pub inserted: usize,
    pub updated: usize,
    pub deleted: usize,
}

/// The writes that turn `ours` into `merged`.
struct PlannedWrites<'rows> {
    inserts: Vec<&'rows Lesson>,
    updates: Vec<&'rows Lesson>,
    deletes: Vec<&'rows str>,
}

/// Rows of `merged` missing from `ours` are inserts, rows that differ are updates, ids of `ours` missing from
/// `merged` are deletes. A row equal on both sides is not written, so applying the same MERGED twice writes nothing.
fn planned_writes<'rows>(ours: &'rows [Lesson], merged: &'rows [Lesson]) -> PlannedWrites<'rows> {
    // Values are compared, so an unchanged row is never rewritten and a repeated apply writes nothing.
    let ours_by_id: BTreeMap<&str, &Lesson> =
        ours.iter().map(|row| (row.id.as_str(), row)).collect();
    let merged_ids: BTreeSet<&str> = merged.iter().map(|row| row.id.as_str()).collect();
    let (inserts, kept): (Vec<&Lesson>, Vec<&Lesson>) = merged
        .iter()
        .partition(|row| !ours_by_id.contains_key(row.id.as_str()));
    let updates: Vec<&Lesson> = kept
        .into_iter()
        .filter(|row| ours_by_id.get(row.id.as_str()) != Some(row))
        .collect();
    let deletes: Vec<&str> = ours_by_id
        .keys()
        .copied()
        .filter(|lesson_id| !merged_ids.contains(lesson_id))
        .collect();
    PlannedWrites {
        inserts,
        updates,
        deletes,
    }
}

/// Ids listed per kind in the apply journal line; past it only the count is logged, so a first sync of a large file
/// stays one readable line.
const JOURNAL_ID_LIMIT: usize = 20;

/// "<count> [first ids]" for the apply journal line, with " and more" once past `JOURNAL_ID_LIMIT`.
fn id_summary(lesson_ids: &[&str]) -> String {
    let listed: &[&str] = lesson_ids.get(..JOURNAL_ID_LIMIT).unwrap_or(lesson_ids);
    let more: &str = lesson_ids
        .get(JOURNAL_ID_LIMIT..)
        .filter(|unlisted| !unlisted.is_empty())
        .map_or("", |_unlisted| " and more");
    format!("{} {listed:?}{more}", lesson_ids.len())
}

/// Makes the file hold exactly `merged`, given that it held `ours` when the caller's transaction began. It takes the
/// transaction, not a connection, so it cannot run outside one; the caller opens it with
/// `TransactionBehavior::Immediate`, because a deferred one lets a local writer slip in between reading OURS and this
/// apply. Inserts and updates go through `copy_into`, whose upsert keeps the search index in step; deletes fire the
/// index's delete trigger.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "called by the sync cycle (plan step S4)")
)]
pub fn apply_merged(
    transaction: &Transaction,
    ours: &[Lesson],
    merged: &[Lesson],
) -> Result<AppliedCounts> {
    // Upserts through copy_into, so the search index follows every insert and update.
    let planned: PlannedWrites = planned_writes(ours, merged);
    planned
        .inserts
        .iter()
        .chain(&planned.updates)
        .try_for_each(|lesson| {
            copy_into(transaction, lesson)
                .with_context(|| format!("apply_merged: upsert of {} failed", lesson.id))
        })?;
    planned
        .deletes
        .iter()
        .try_for_each(|lesson_id| -> Result<()> {
            transaction
                .execute("DELETE FROM lesson WHERE id = ?1", [lesson_id])
                .with_context(|| format!("apply_merged: delete of {lesson_id} failed"))?;
            Ok(())
        })?;
    let (inserted_ids, updated_ids): (Vec<&str>, Vec<&str>) = (
        planned.inserts.iter().map(|row| row.id.as_str()).collect(),
        planned.updates.iter().map(|row| row.id.as_str()).collect(),
    );
    journal!(
        "apply_merged: inserted {}, updated {}, deleted {}",
        id_summary(&inserted_ids),
        id_summary(&updated_ids),
        id_summary(&planned.deletes)
    );
    Ok(AppliedCounts {
        inserted: planned.inserts.len(),
        updated: planned.updates.len(),
        deleted: planned.deletes.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{OpenMode, open};

    /// A search or list limit above any row count a test inserts, so nothing is cut.
    const LIMIT_ABOVE_ROW_COUNT: usize = 10;
    /// A limit below the row count a test inserts, so the cut is visible.
    const LIMIT_BELOW_ROW_COUNT: usize = 2;

    fn temp_conn() -> (tempfile::TempDir, Connection) {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let conn: Connection =
            open(&temp_dir.path().join("shared.db"), OpenMode::Create).expect("open");
        (temp_dir, conn)
    }

    fn minimal_lesson() -> NewLesson {
        NewLesson {
            title: "linker error".into(),
            problem: "cargo build failed".into(),
            ..NewLesson::default()
        }
    }

    #[test]
    fn fts_columns_match_the_live_index_in_order() {
        let (_temp_dir, conn) = temp_conn();
        let mut mut_statement = conn
            .prepare("SELECT name FROM pragma_table_info('lesson_fts')")
            .unwrap();
        let live_columns: Vec<String> = mut_statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<String>>>()
            .unwrap();
        let declared_columns: Vec<&str> = FTS_COLUMNS
            .iter()
            .map(|(column_name, _weight)| *column_name)
            .collect();
        assert_eq!(live_columns, declared_columns);
    }

    #[test]
    fn insert_then_find_round_trips_and_stores_blank_optionals_as_null() {
        let (_temp_dir, conn) = temp_conn();
        let new_lesson = NewLesson {
            title: "  linker error  ".into(),
            root_cause: Some("missing -lssl".into()),
            context: Some("   ".into()),
            ..minimal_lesson()
        };
        let lesson_id: String = insert(&conn, &new_lesson).unwrap();
        let stored: Lesson = find_by_id(&conn, &lesson_id).unwrap().expect("stored");
        assert_eq!(stored.title, "linker error");
        assert_eq!(stored.root_cause.as_deref(), Some("missing -lssl"));
        assert_eq!(stored.context, None);
        assert_eq!(stored.hits, 0);
        assert_eq!(stored.created_at, stored.updated_at);
    }

    #[test]
    fn insert_refuses_a_blank_title_or_problem() {
        let (_temp_dir, conn) = temp_conn();
        assert!(
            insert(
                &conn,
                &NewLesson {
                    title: " ".into(),
                    ..minimal_lesson()
                }
            )
            .is_err()
        );
        assert!(
            insert(
                &conn,
                &NewLesson {
                    problem: "".into(),
                    ..minimal_lesson()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn find_by_id_returns_none_for_an_unknown_id() {
        let (_temp_dir, conn) = temp_conn();
        assert_eq!(find_by_id(&conn, "01NOSUCHID").unwrap(), None);
    }

    /// Backdates a lesson's timestamps, since `updated_at` only has one-second resolution.
    fn backdate(conn: &Connection, lesson_id: &str) {
        conn.execute(
            "UPDATE lesson SET created_at = '2000-01-01T00:00:00Z', updated_at = '2000-01-01T00:00:00Z' WHERE id = ?1",
            [lesson_id],
        )
        .unwrap();
    }

    #[test]
    fn update_replaces_named_fields_clears_empty_ones_and_keeps_the_rest() {
        let (_temp_dir, conn) = temp_conn();
        let lesson_id: String = insert(
            &conn,
            &NewLesson {
                context: Some("old context".into()),
                tags: Some("rust".into()),
                ..minimal_lesson()
            },
        )
        .unwrap();
        backdate(&conn, &lesson_id);
        let patch = LessonPatch {
            title: Some("loader error".into()),
            context: Some("".into()),
            ..LessonPatch::default()
        };
        assert!(update(&conn, &lesson_id, &patch).unwrap());
        let stored: Lesson = find_by_id(&conn, &lesson_id).unwrap().expect("stored");
        assert_eq!(stored.title, "loader error");
        assert_eq!(stored.context, None, "an empty string clears the field");
        assert_eq!(
            stored.tags.as_deref(),
            Some("rust"),
            "an unnamed field is unchanged"
        );
        assert_eq!(stored.problem, "cargo build failed");
        assert_ne!(stored.updated_at, "2000-01-01T00:00:00Z");
        assert_eq!(stored.created_at, "2000-01-01T00:00:00Z");
    }

    #[test]
    fn update_refuses_an_empty_patch_and_clearing_title_or_problem() {
        let (_temp_dir, conn) = temp_conn();
        let lesson_id: String = insert(&conn, &minimal_lesson()).unwrap();
        assert!(update(&conn, &lesson_id, &LessonPatch::default()).is_err());
        let clear_title = LessonPatch {
            title: Some(" ".into()),
            ..LessonPatch::default()
        };
        assert!(update(&conn, &lesson_id, &clear_title).is_err());
    }

    #[test]
    fn update_hit_and_retire_report_an_unknown_id() {
        let (_temp_dir, conn) = temp_conn();
        let patch = LessonPatch {
            title: Some("t".into()),
            ..LessonPatch::default()
        };
        assert!(!update(&conn, "01NOSUCHID", &patch).unwrap());
        assert_eq!(bump_hits(&conn, "01NOSUCHID").unwrap(), None);
        assert!(!retire(&conn, "01NOSUCHID", "01OTHER").unwrap());
    }

    #[test]
    fn bump_hits_counts_up_and_leaves_updated_at_alone() {
        const BUMP_COUNT: i64 = 2; // more than one, so a counter stuck at 1 fails
        let (_temp_dir, conn) = temp_conn();
        let lesson_id: String = insert(&conn, &minimal_lesson()).unwrap();
        backdate(&conn, &lesson_id);
        // Each bump must report the running total.
        (1..=BUMP_COUNT).for_each(|expected_hits| {
            assert_eq!(bump_hits(&conn, &lesson_id).unwrap(), Some(expected_hits));
        });
        let stored: Lesson = find_by_id(&conn, &lesson_id).unwrap().expect("stored");
        assert_eq!(stored.updated_at, "2000-01-01T00:00:00Z");
    }

    #[test]
    fn retire_records_the_replacement_and_bumps_updated_at() {
        let (_temp_dir, conn) = temp_conn();
        let lesson_id: String = insert(&conn, &minimal_lesson()).unwrap();
        backdate(&conn, &lesson_id);
        assert!(retire(&conn, &lesson_id, "01NEWER").unwrap());
        let stored: Lesson = find_by_id(&conn, &lesson_id).unwrap().expect("stored");
        assert_eq!(stored.superseded_by.as_deref(), Some("01NEWER"));
        assert_ne!(stored.updated_at, "2000-01-01T00:00:00Z");
    }

    #[test]
    fn list_filters_by_whole_tag_and_treats_underscore_literally() {
        let (_temp_dir, conn) = temp_conn();
        let fn_insert_tagged = |tags: &str| {
            insert(
                &conn,
                &NewLesson {
                    tags: Some(tags.into()),
                    ..minimal_lesson()
                },
            )
            .unwrap()
        };
        let ssh_id: String = fn_insert_tagged("widget-app SSH");
        let underscore_id: String = fn_insert_tagged("git_hub");
        let decoy_id: String = fn_insert_tagged("gitXhub sshd");
        let inserted_count: usize = [&ssh_id, &underscore_id, &decoy_id].len();
        let fn_listed_ids = |tag: Option<&str>| -> Vec<String> {
            list(&conn, LIMIT_ABOVE_ROW_COUNT, tag)
                .unwrap()
                .into_iter()
                .map(|lesson| lesson.id)
                .collect()
        };
        assert_eq!(
            fn_listed_ids(Some("ssh")),
            vec![ssh_id],
            "whole tag, any case; `sshd` is not `ssh`"
        );
        assert_eq!(
            fn_listed_ids(Some("git_hub")),
            vec![underscore_id],
            "`_` must not match `X`"
        );
        assert_eq!(fn_listed_ids(None).len(), inserted_count);
        assert_eq!(
            list(&conn, LIMIT_BELOW_ROW_COUNT, None).unwrap().len(),
            LIMIT_BELOW_ROW_COUNT
        );
    }

    #[test]
    fn owner_of_finds_the_file_holding_the_id() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let work_conn: Connection =
            open(&temp_dir.path().join("work.db"), OpenMode::Create).unwrap();
        let shared_conn: Connection =
            open(&temp_dir.path().join("shared.db"), OpenMode::Create).unwrap();
        let lesson_id: String = insert(&shared_conn, &minimal_lesson()).unwrap();
        let opened: Vec<(Domain, Connection)> =
            vec![(Domain::Work, work_conn), (Domain::Shared, shared_conn)];
        assert_eq!(
            owner_of(&opened, &lesson_id)
                .unwrap()
                .map(|(domain, _conn)| *domain),
            Some(Domain::Shared)
        );
        assert!(owner_of(&opened, "01NOSUCHID").unwrap().is_none());
    }

    fn search_ids(conn: &Connection, fts_query: &str) -> Vec<String> {
        search(conn, Domain::Shared, fts_query, LIMIT_ABOVE_ROW_COUNT)
            .unwrap()
            .into_iter()
            .map(|hit| hit.lesson.id)
            .collect()
    }

    #[test]
    fn search_ranks_a_title_match_above_a_problem_match() {
        let (_temp_dir, conn) = temp_conn();
        let problem_id: String = insert(
            &conn,
            &NewLesson {
                title: "unrelated".into(),
                problem: "segfault in the loader".into(),
                ..NewLesson::default()
            },
        )
        .unwrap();
        let title_id: String = insert(
            &conn,
            &NewLesson {
                title: "segfault".into(),
                problem: "it crashed in the loader".into(),
                ..NewLesson::default()
            },
        )
        .unwrap();
        assert_eq!(
            search_ids(&conn, "\"segfault\""),
            vec![title_id, problem_id]
        );
    }

    #[test]
    fn search_hides_retired_lessons() {
        let (_temp_dir, conn) = temp_conn();
        let lesson_id: String = insert(&conn, &minimal_lesson()).unwrap();
        assert_eq!(search_ids(&conn, "\"linker\""), vec![lesson_id.clone()]);
        retire(&conn, &lesson_id, "01NEWER").unwrap();
        assert!(search_ids(&conn, "\"linker\"").is_empty());
    }

    #[test]
    fn search_sees_an_update_and_forgets_the_old_text() {
        let (_temp_dir, conn) = temp_conn();
        let lesson_id: String = insert(&conn, &minimal_lesson()).unwrap();
        let patch = LessonPatch {
            title: Some("loader error".into()),
            ..LessonPatch::default()
        };
        update(&conn, &lesson_id, &patch).unwrap();
        assert_eq!(search_ids(&conn, "\"loader\""), vec![lesson_id]);
        assert!(search_ids(&conn, "\"linker\"").is_empty());
    }

    #[test]
    fn search_all_merges_files_by_score_and_cuts_to_the_limit() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let work_conn: Connection =
            open(&temp_dir.path().join("work.db"), OpenMode::Create).unwrap();
        let shared_conn: Connection =
            open(&temp_dir.path().join("shared.db"), OpenMode::Create).unwrap();
        // Distinct match strengths (title, tags, problem) so the scores differ, plus a non-matching
        // lesson per file so the term is not in every document (bm25 flattens such terms).
        let fn_insert = |conn: &Connection, title: &str, problem: &str, tags: Option<&str>| {
            insert(
                conn,
                &NewLesson {
                    title: title.into(),
                    problem: problem.into(),
                    tags: tags.map(str::to_string),
                    ..NewLesson::default()
                },
            )
            .unwrap()
        };
        let matching_ids: Vec<String> = vec![
            fn_insert(&work_conn, "linker error", "build broke", None),
            fn_insert(&shared_conn, "other", "it broke", Some("linker")),
            fn_insert(&shared_conn, "another", "linker broke once", None),
        ];
        fn_insert(&work_conn, "unrelated", "printer jam", None);
        fn_insert(&shared_conn, "unrelated", "printer jam", None);
        let matching_lesson_count: usize = matching_ids.len();
        let opened: Vec<(Domain, Connection)> =
            vec![(Domain::Work, work_conn), (Domain::Shared, shared_conn)];
        let all_hits: Vec<SearchHit> =
            search_all(&opened, "\"linker\"", LIMIT_ABOVE_ROW_COUNT).unwrap();
        assert_eq!(all_hits.len(), matching_lesson_count);
        assert_eq!(all_hits[0].domain, "work", "the title match ranks first");
        assert!(
            all_hits
                .iter()
                .zip(all_hits.iter().skip(1))
                .all(|(better_hit, next_hit)| better_hit.score <= next_hit.score),
            "sorted best first"
        );
        assert_eq!(
            search_all(&opened, "\"linker\"", LIMIT_BELOW_ROW_COUNT)
                .unwrap()
                .len(),
            LIMIT_BELOW_ROW_COUNT
        );
    }

    fn work_and_shared() -> (tempfile::TempDir, Connection, Connection) {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let work_conn: Connection =
            open(&temp_dir.path().join("work.db"), OpenMode::Create).unwrap();
        let shared_conn: Connection =
            open(&temp_dir.path().join("shared.db"), OpenMode::Create).unwrap();
        (temp_dir, work_conn, shared_conn)
    }

    #[test]
    fn move_lesson_keeps_every_field_and_reindexes_in_the_target() {
        let (_temp_dir, mut work_conn, shared_conn) = work_and_shared();
        let lesson_id: String = insert(&work_conn, &minimal_lesson()).unwrap();
        bump_hits(&work_conn, &lesson_id).unwrap();
        let before_move: Lesson = find_by_id(&work_conn, &lesson_id).unwrap().unwrap();

        assert!(move_lesson(&mut work_conn, &shared_conn, &lesson_id).unwrap());
        assert_eq!(
            find_by_id(&shared_conn, &lesson_id).unwrap(),
            Some(before_move)
        );
        assert_eq!(find_by_id(&work_conn, &lesson_id).unwrap(), None);
        assert_eq!(
            search(
                &shared_conn,
                Domain::Shared,
                "\"linker\"",
                LIMIT_ABOVE_ROW_COUNT
            )
            .unwrap()
            .len(),
            1
        );
        assert!(
            search(
                &work_conn,
                Domain::Work,
                "\"linker\"",
                LIMIT_ABOVE_ROW_COUNT
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn move_lesson_finishes_after_a_crash_between_copy_and_delete() {
        let (_temp_dir, mut work_conn, shared_conn) = work_and_shared();
        let lesson_id: String = insert(&work_conn, &minimal_lesson()).unwrap();
        // The crash: the copy committed, the delete never ran. Then the source was edited.
        copy_into(
            &shared_conn,
            &find_by_id(&work_conn, &lesson_id).unwrap().unwrap(),
        )
        .unwrap();
        let patch = LessonPatch {
            title: Some("edited after the crash".into()),
            ..LessonPatch::default()
        };
        update(&work_conn, &lesson_id, &patch).unwrap();

        assert!(move_lesson(&mut work_conn, &shared_conn, &lesson_id).unwrap());
        let moved: Lesson = find_by_id(&shared_conn, &lesson_id).unwrap().unwrap();
        assert_eq!(
            moved.title, "edited after the crash",
            "the retry copies the source's current state"
        );
        assert_eq!(find_by_id(&work_conn, &lesson_id).unwrap(), None);
        let target_rows: i64 = shared_conn
            .query_row("SELECT count(*) FROM lesson", [], |row| row.get(0))
            .unwrap();
        assert_eq!(target_rows, 1, "no duplicate in the target");
        assert_eq!(
            search(
                &shared_conn,
                Domain::Shared,
                "\"crash\"",
                LIMIT_ABOVE_ROW_COUNT
            )
            .unwrap()
            .len(),
            1,
            "the index followed the refresh"
        );
        assert!(
            search(
                &shared_conn,
                Domain::Shared,
                "\"linker\"",
                LIMIT_ABOVE_ROW_COUNT
            )
            .unwrap()
            .is_empty(),
            "and dropped the old title"
        );
    }

    #[test]
    fn move_lesson_reports_an_id_the_source_does_not_hold() {
        let (_temp_dir, mut work_conn, shared_conn) = work_and_shared();
        assert!(!move_lesson(&mut work_conn, &shared_conn, "01NOSUCHID").unwrap());
    }

    #[test]
    fn delete_if_unchanged_deletes_only_the_row_that_was_copied() {
        let (_temp_dir, mut work_conn, _shared_conn) = work_and_shared();
        let lesson_id: String = insert(&work_conn, &minimal_lesson()).unwrap();
        let copied: Lesson = find_by_id(&work_conn, &lesson_id).unwrap().unwrap();

        // Edited after the copy: the stale copy must not cost the source its newer text.
        bump_hits(&work_conn, &lesson_id).unwrap();
        assert_eq!(
            delete_if_unchanged(&mut work_conn, &copied).unwrap(),
            SourceDelete::Changed
        );
        assert!(
            find_by_id(&work_conn, &lesson_id).unwrap().is_some(),
            "the changed row is kept"
        );

        let fresh_copy: Lesson = find_by_id(&work_conn, &lesson_id).unwrap().unwrap();
        assert_eq!(
            delete_if_unchanged(&mut work_conn, &fresh_copy).unwrap(),
            SourceDelete::Deleted
        );
        assert_eq!(find_by_id(&work_conn, &lesson_id).unwrap(), None);
        assert_eq!(
            delete_if_unchanged(&mut work_conn, &fresh_copy).unwrap(),
            SourceDelete::AlreadyGone
        );
    }

    #[test]
    fn new_lesson_json_refuses_unknown_keys() {
        let parse_result =
            serde_json::from_str::<NewLesson>(r#"{"title":"t","problem":"p","rootcause":"typo"}"#);
        assert!(parse_result.is_err());
    }

    // all_lessons and apply_merged (plan step S3). Race: a local writer against the sync's transaction, forced in both
    // orders by holding the transaction, never by timing. Back-to-back: applying the same MERGED twice writes nothing.
    const ID_A: &str = "01KAAAAAAAAAAAAAAAAAAAAAAA";
    const ID_B: &str = "01KBBBBBBBBBBBBBBBBBBBBBBB";
    const ID_C: &str = "01KCCCCCCCCCCCCCCCCCCCCCCC";
    const ID_D: &str = "01KDDDDDDDDDDDDDDDDDDDDDDD";
    const SYNC_HITS: i64 = 7;
    /// How long the second writer waits for the lock: long enough to be a real attempt, short so the test is fast.
    const WRITER_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(50);

    /// A lesson with every optional field set and a word only it holds (`marker`), so a dropped field or a stale
    /// search index shows.
    fn full_lesson(lesson_id: &str, marker: &str) -> Lesson {
        Lesson {
            id: lesson_id.to_string(),
            created_at: "2026-11-01T00:00:00Z".to_string(),
            updated_at: "2026-11-02T00:00:00Z".to_string(),
            title: format!("title {marker}"),
            context: Some("context".to_string()),
            problem: "problem".to_string(),
            root_cause: Some("root cause".to_string()),
            resolution: Some("resolution".to_string()),
            failed_tries: Some("failed tries".to_string()),
            tags: Some("rust".to_string()),
            env_versions: Some("rustc 1.95".to_string()),
            superseded_by: None,
            hits: 0,
        }
    }

    /// A file holding lessons A, B and C, written in the order C, A, B so an id sort is visible.
    fn three_lessons() -> (tempfile::TempDir, Connection) {
        let (temp_dir, conn) = temp_conn();
        [
            full_lesson(ID_C, "charlie"),
            full_lesson(ID_A, "alpha"),
            full_lesson(ID_B, "bravo"),
        ]
        .iter()
        .for_each(|lesson| copy_into(&conn, lesson).unwrap());
        (temp_dir, conn)
    }

    fn apply_in_transaction(conn: &mut Connection, merged: &[Lesson]) -> AppliedCounts {
        let transaction = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let ours: Vec<Lesson> = all_lessons(&transaction).unwrap();
        let counts: AppliedCounts = apply_merged(&transaction, &ours, merged).unwrap();
        transaction.commit().unwrap();
        counts
    }

    #[test]
    fn all_lessons_returns_every_row_sorted_by_id_retired_included() {
        let (_temp_dir, conn) = three_lessons();
        retire(&conn, ID_A, ID_B).unwrap();
        let read_back: Vec<Lesson> = all_lessons(&conn).unwrap();
        assert_eq!(
            read_back
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec![ID_A, ID_B, ID_C]
        );
        assert_eq!(
            read_back[0].superseded_by.as_deref(),
            Some(ID_B),
            "a retired lesson is still synced"
        );
        assert_eq!(
            read_back[1],
            full_lesson(ID_B, "bravo"),
            "every field read back"
        );
        let json_round_trip: Vec<Lesson> = read_back
            .iter()
            .map(|row| serde_json::from_str(&serde_json::to_string(row).unwrap()).unwrap())
            .collect();
        assert_eq!(json_round_trip, read_back);
    }

    #[test]
    fn apply_merged_inserts_updates_deletes_and_skips_unchanged_rows() {
        let (_temp_dir, mut mut_conn) = three_lessons();
        let edited_a = Lesson {
            resolution: Some("rewritten".to_string()),
            hits: SYNC_HITS,
            ..full_lesson(ID_A, "alpha")
        };
        let merged: Vec<Lesson> = vec![
            edited_a,
            full_lesson(ID_C, "charlie"),
            full_lesson(ID_D, "delta"),
        ]; // B dropped
        let counts: AppliedCounts = apply_in_transaction(&mut mut_conn, &merged);
        assert_eq!(
            counts,
            AppliedCounts {
                inserted: 1,
                updated: 1,
                deleted: 1
            }
        );
        assert_eq!(all_lessons(&mut_conn).unwrap(), merged);
    }

    #[test]
    fn apply_merged_keeps_the_search_index_in_step() {
        let (_temp_dir, mut mut_conn) = three_lessons();
        let retitled_a = Lesson {
            title: "title echo".to_string(),
            ..full_lesson(ID_A, "alpha")
        };
        apply_in_transaction(
            &mut mut_conn,
            &[
                retitled_a,
                full_lesson(ID_C, "charlie"),
                full_lesson(ID_D, "delta"),
            ],
        );
        assert_eq!(
            search_ids(&mut_conn, "delta"),
            vec![ID_D],
            "inserted row is indexed"
        );
        assert_eq!(
            search_ids(&mut_conn, "echo"),
            vec![ID_A],
            "updated row is re-indexed"
        );
        assert!(
            search_ids(&mut_conn, "alpha").is_empty(),
            "old text is gone from the index"
        );
        assert!(
            search_ids(&mut_conn, "bravo").is_empty(),
            "deleted row is gone from the index"
        );
    }

    /// A back-to-back sync: the second apply of the same MERGED writes nothing, and `hits` stays a value, not a sum.
    #[test]
    fn applying_the_same_merged_twice_writes_nothing_the_second_time() {
        let (_temp_dir, mut mut_conn) = three_lessons();
        let merged: Vec<Lesson> = vec![
            Lesson {
                hits: SYNC_HITS,
                ..full_lesson(ID_A, "alpha")
            },
            full_lesson(ID_D, "delta"),
        ];
        apply_in_transaction(&mut mut_conn, &merged);
        let second: AppliedCounts = apply_in_transaction(&mut mut_conn, &merged);
        assert_eq!(
            second,
            AppliedCounts {
                inserted: 0,
                updated: 0,
                deleted: 0
            }
        );
        assert_eq!(all_lessons(&mut_conn).unwrap(), merged);
    }

    #[test]
    fn id_summary_lists_up_to_the_limit_and_marks_the_rest() {
        let many_ids: Vec<String> = (0..=JOURNAL_ID_LIMIT)
            .map(|index| format!("id{index}"))
            .collect();
        let many_refs: Vec<&str> = many_ids.iter().map(String::as_str).collect();
        let at_limit: &[&str] = &many_refs[..JOURNAL_ID_LIMIT];
        assert_eq!(id_summary(&[]), "0 []");
        assert_eq!(id_summary(&[ID_A]), format!("1 [\"{ID_A}\"]"));
        assert!(
            !id_summary(at_limit).ends_with(" and more"),
            "exactly the limit lists every id"
        );
        let last_listed: String = format!("\"id{}\"", JOURNAL_ID_LIMIT - 1);
        assert!(
            id_summary(at_limit).contains(&last_listed),
            "the last id within the limit is listed"
        );
        assert!(
            id_summary(&many_refs).contains(&last_listed),
            "past the limit, the first ids are still listed"
        );
        let over_limit: String = id_summary(&many_refs);
        assert!(
            over_limit.starts_with(&format!("{} ", JOURNAL_ID_LIMIT + 1)),
            "{over_limit}"
        );
        assert!(
            over_limit.ends_with(" and more")
                && !over_limit.contains(&format!("\"id{JOURNAL_ID_LIMIT}\"")),
            "{over_limit}"
        );
    }

    /// A failed write names its lesson id (forced by a trigger that aborts it), so a failed sync can be retraced.
    #[test]
    fn a_failed_upsert_or_delete_names_its_lesson_id() {
        let (_temp_dir, mut mut_conn) = three_lessons();
        mut_conn
            .execute_batch(
                "CREATE TRIGGER refuse_insert BEFORE INSERT ON lesson BEGIN SELECT RAISE(ABORT, 'forced'); END;\n                 CREATE TRIGGER refuse_delete BEFORE DELETE ON lesson BEGIN SELECT RAISE(ABORT, 'forced'); END;",
            )
            .unwrap();
        let fn_apply_error = |conn: &mut Connection, merged: &[Lesson]| -> String {
            let transaction = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            let ours: Vec<Lesson> = all_lessons(&transaction).unwrap();
            format!(
                "{:#}",
                apply_merged(&transaction, &ours, merged).unwrap_err()
            )
        };
        let all_three: Vec<Lesson> = all_lessons(&mut_conn).unwrap();
        let with_new_d: Vec<Lesson> = all_three
            .iter()
            .cloned()
            .chain([full_lesson(ID_D, "delta")])
            .collect();
        assert!(
            fn_apply_error(&mut mut_conn, &with_new_d)
                .contains(&format!("upsert of {ID_D} failed"))
        );
        assert!(
            fn_apply_error(&mut mut_conn, &all_three[1..])
                .contains(&format!("delete of {ID_A} failed"))
        );
    }

    /// A transaction dropped before its commit (a crash between the queue write and the DB commit) changes nothing.
    #[test]
    fn an_uncommitted_apply_changes_nothing() {
        let (_temp_dir, mut mut_conn) = three_lessons();
        let before: Vec<Lesson> = all_lessons(&mut_conn).unwrap();
        let transaction = mut_conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        apply_merged(&transaction, &before, &[full_lesson(ID_D, "delta")]).unwrap();
        drop(transaction);
        assert_eq!(all_lessons(&mut_conn).unwrap(), before);
    }

    /// Both orders of a local write against the sync: while the sync's transaction is open the writer is refused (it
    /// would wait on `busy_timeout`, never slip in between the read and the apply); after the commit it lands; and a
    /// write made before the transaction is in OURS.
    #[test]
    fn a_local_write_never_lands_between_the_read_and_the_apply() {
        let (temp_dir, mut mut_conn) = three_lessons();
        let writer_conn: Connection = Connection::open(temp_dir.path().join("shared.db")).unwrap();
        writer_conn.busy_timeout(WRITER_BUSY_TIMEOUT).unwrap();

        copy_into(&writer_conn, &full_lesson(ID_D, "delta")).unwrap(); // writer first
        let transaction = mut_conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let ours: Vec<Lesson> = all_lessons(&transaction).unwrap();
        assert!(
            ours.iter().any(|row| row.id == ID_D),
            "a write before the transaction is in OURS"
        );

        let blocked = copy_into(
            &writer_conn,
            &Lesson {
                hits: SYNC_HITS,
                ..full_lesson(ID_A, "alpha")
            },
        ); // sync first
        assert!(
            blocked.is_err(),
            "a writer cannot write while the sync holds the transaction"
        );
        apply_merged(&transaction, &ours, &ours).unwrap();
        transaction.commit().unwrap();
        copy_into(
            &writer_conn,
            &Lesson {
                hits: SYNC_HITS,
                ..full_lesson(ID_A, "alpha")
            },
        )
        .unwrap();
        assert_eq!(
            find_by_id(&mut_conn, ID_A).unwrap().map(|row| row.hits),
            Some(SYNC_HITS),
            "it lands after the commit"
        );
    }
}
