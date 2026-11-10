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

    #[test]
    fn new_lesson_json_refuses_unknown_keys() {
        let parse_result =
            serde_json::from_str::<NewLesson>(r#"{"title":"t","problem":"p","rootcause":"typo"}"#);
        assert!(parse_result.is_err());
    }
}
