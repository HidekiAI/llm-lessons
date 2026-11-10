//! The data access layer: the only module that holds SQL about lessons.
//!
//! Commands, search and the hook go through these types and functions, so adding a column touches
//! a migration in `db.rs` and this file, nothing else.

use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, Row};
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
#[derive(Debug, Clone, PartialEq, Serialize)]
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
    let fields_named: [&Option<String>; 8] = [
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{OpenMode, open};

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
        let (_temp_dir, conn) = temp_conn();
        let lesson_id: String = insert(&conn, &minimal_lesson()).unwrap();
        backdate(&conn, &lesson_id);
        assert_eq!(bump_hits(&conn, &lesson_id).unwrap(), Some(1));
        assert_eq!(bump_hits(&conn, &lesson_id).unwrap(), Some(2));
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
        fn_insert_tagged("gitXhub sshd");
        let fn_listed_ids = |tag: Option<&str>| -> Vec<String> {
            list(&conn, 10, tag)
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
        assert_eq!(fn_listed_ids(None).len(), 3);
        assert_eq!(list(&conn, 2, None).unwrap().len(), 2);
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
        search(conn, Domain::Shared, fts_query, 10)
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
        fn_insert(&work_conn, "linker error", "build broke", None);
        fn_insert(&work_conn, "unrelated", "printer jam", None);
        fn_insert(&shared_conn, "other", "it broke", Some("linker"));
        fn_insert(&shared_conn, "another", "linker broke once", None);
        fn_insert(&shared_conn, "unrelated", "printer jam", None);
        let opened: Vec<(Domain, Connection)> =
            vec![(Domain::Work, work_conn), (Domain::Shared, shared_conn)];
        let all_hits: Vec<SearchHit> = search_all(&opened, "\"linker\"", 10).unwrap();
        assert_eq!(all_hits.len(), 3);
        assert_eq!(all_hits[0].domain, "work", "the title match ranks first");
        assert!(
            all_hits
                .windows(2)
                .all(|pair| pair[0].score <= pair[1].score),
            "sorted best first"
        );
        assert_eq!(search_all(&opened, "\"linker\"", 2).unwrap().len(), 2);
    }

    #[test]
    fn new_lesson_json_refuses_unknown_keys() {
        let parse_result =
            serde_json::from_str::<NewLesson>(r#"{"title":"t","problem":"p","rootcause":"typo"}"#);
        assert!(parse_result.is_err());
    }
}
