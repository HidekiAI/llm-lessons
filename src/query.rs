//! Turning free text into a safe FTS5 MATCH expression.
//!
//! Raw text, above all a whole prompt handed over by the hook, holds characters and words that
//! FTS5 reads as syntax: `"` opens a phrase, `:` names a column, `-` and `NOT` negate, `*` asks
//! for a prefix, `(` groups, `AND`/`OR`/`NEAR` combine. Passed straight to MATCH, such text either
//! errors or quietly means something else. Here every kept word is double quoted, so FTS5 only
//! ever reads it as text.

/// Upper bound on terms, so a long prompt cannot become a slow query.
const MAX_TERMS: usize = 16;

/// Shorter terms match almost everything and only add noise.
const MIN_TERM_CHARS: usize = 2;

/// Characters that stay inside a word, because they hold identifiers, paths, versions and flags
/// together (`not_found_or_no_access`, `example-org/widget-api`, `v1.2.3`). FTS5's tokenizer still
/// splits on them inside the quotes, which turns such a word into an exact phrase match.
const JOINER_CHARS: &[char] = &['_', '-', '.', '/'];

/// Words too common to help ranking. Compared in lower case.
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "from", "has", "have", "if",
    "in", "into", "is", "it", "its", "me", "my", "no", "not", "of", "on", "or", "so", "that",
    "the", "then", "there", "this", "to", "was", "were", "with",
];

/// Builds `"term1" OR "term2" ...` from free text, or `None` when no usable term is left.
pub fn fts_query_from_text(text: &str) -> Option<String> {
    let fn_is_word_char =
        |character: char| character.is_alphanumeric() || JOINER_CHARS.contains(&character);
    let fn_is_kept_term =
        |term: &str| term.chars().count() >= MIN_TERM_CHARS && !STOPWORDS.contains(&term);

    let candidate_terms = text
        .split(|character: char| !fn_is_word_char(character))
        .map(|word| word.trim_matches(JOINER_CHARS).to_lowercase()) // "--flag" -> "flag"; lower case only for de-duplication, FTS5 already ignores case
        .filter(|term| fn_is_kept_term(term));

    // A plain loop, not a fold: it stops reading the text as soon as MAX_TERMS distinct terms are
    // found, and the `contains` scan stays cheap because the list never exceeds MAX_TERMS.
    let mut mut_unique_terms: Vec<String> = Vec::with_capacity(MAX_TERMS);
    for term in candidate_terms {
        if mut_unique_terms.len() == MAX_TERMS {
            break;
        }
        if !mut_unique_terms.contains(&term) {
            mut_unique_terms.push(term);
        }
    }

    (!mut_unique_terms.is_empty()).then(|| {
        mut_unique_terms
            .iter()
            .map(|term| format!("\"{}\"", term.replace('"', "\"\""))) // the splitter already drops `"`; escaping stays in case the word rules change
            .collect::<Vec<String>>()
            .join(" OR ")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{OpenMode, open};
    use rusqlite::Connection;

    fn search_db() -> (tempfile::TempDir, Connection) {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let conn: Connection =
            open(&temp_dir.path().join("shared.db"), OpenMode::Create).expect("open");
        (temp_dir, conn)
    }

    fn insert_problem(conn: &Connection, lesson_id: &str, problem: &str) {
        conn.execute(
            "INSERT INTO lesson (id, title, problem) VALUES (?1, 'test lesson', ?2)",
            (lesson_id, problem),
        )
        .expect("insert");
    }

    fn match_count(conn: &Connection, fts_query: &str) -> rusqlite::Result<i64> {
        conn.query_row(
            "SELECT count(*) FROM lesson_fts WHERE lesson_fts MATCH ?1",
            [fts_query],
            |row| row.get(0),
        )
    }

    #[test]
    fn splits_on_punctuation_and_quotes_each_term() {
        assert_eq!(
            fts_query_from_text("pull_image: not found").as_deref(),
            Some(r#""pull_image" OR "found""#)
        );
    }

    #[test]
    fn trims_flag_dashes_and_brackets_from_word_ends() {
        assert_eq!(
            fts_query_from_text("--flag (foo)").as_deref(),
            Some(r#""flag" OR "foo""#)
        );
    }

    #[test]
    fn keeps_identifiers_paths_and_versions_whole() {
        assert_eq!(
            fts_query_from_text("example-org/widget-api v1.2.3").as_deref(),
            Some(r#""example-org/widget-api" OR "v1.2.3""#)
        );
    }

    #[test]
    fn drops_stopwords_and_single_characters() {
        assert_eq!(
            fts_query_from_text("can you fix the a").as_deref(),
            Some(r#""can" OR "you" OR "fix""#)
        );
    }

    #[test]
    fn drops_repeats_regardless_of_case() {
        assert_eq!(
            fts_query_from_text("Error error ERROR linker").as_deref(),
            Some(r#""error" OR "linker""#)
        );
    }

    #[test]
    fn caps_the_number_of_terms() {
        // A few words past the cap, so the cap has something to drop.
        const EXTRA_WORDS_PAST_THE_CAP: usize = 4;
        let many_words: String = (0..MAX_TERMS + EXTRA_WORDS_PAST_THE_CAP)
            .map(|word_index| format!("word{word_index} "))
            .collect();
        let fts_query: String = fts_query_from_text(&many_words).expect("query");
        assert_eq!(fts_query.matches(" OR ").count() + 1, MAX_TERMS);
        assert!(
            fts_query.ends_with(&format!("\"word{}\"", MAX_TERMS - 1)),
            "kept the first terms, got {fts_query}"
        );
    }

    #[test]
    fn returns_none_when_nothing_usable_is_left() {
        ["", "   ", "the a of", "!!! --- ..."]
            .iter()
            .for_each(|empty_text| {
                assert_eq!(
                    fts_query_from_text(empty_text),
                    None,
                    "input {empty_text:?}"
                )
            });
    }

    #[test]
    fn hostile_text_never_breaks_match() {
        let (_temp_dir, conn) = search_db();
        insert_problem(&conn, "01ID", "linker failed on bar");

        // Proof these inputs really are hostile: passed raw, each one makes MATCH fail.
        let raw_breaking_inputs: &[&str] = &[r#"say "hi"#, "foo AND (bar", "nosuchcolumn:foo"];
        raw_breaking_inputs.iter().for_each(|raw_input| {
            assert!(
                match_count(&conn, raw_input).is_err(),
                "raw {raw_input:?} unexpectedly worked"
            )
        });

        let hostile_inputs: Vec<&str> = raw_breaking_inputs
            .iter()
            .copied()
            .chain([
                "NOT linker",
                "-flag",
                "prefix*",
                "NEAR(a b)",
                r#"ab"cd"ef"#,
                "^start",
                r#"col:"x" OR"#,
                // Joined words keep `-` `.` `/`, which FTS5 cannot parse unquoted: these are what need the quotes.
                "example-org/widget-api v1.2.3",
                "--some-flag src/db.rs",
                "why does `cargo build` fail with error[E0277]: the trait bound `usize: FromSql` is not satisfied?",
            ])
            .collect();
        hostile_inputs.iter().for_each(|hostile_input| {
            let fts_query: String = fts_query_from_text(hostile_input).expect("some term survives");
            assert!(
                match_count(&conn, &fts_query).is_ok(),
                "{hostile_input:?} -> {fts_query:?} broke MATCH"
            );
        });
    }

    #[test]
    fn an_underscore_identifier_matches_as_an_exact_phrase() {
        // An exact error-code search: it must find the exact error code, and not a lesson that
        // merely contains the same words in another order.
        let (_temp_dir, conn) = search_db();
        insert_problem(
            &conn,
            "01EXACT",
            "pull_image: not_found_or_no_access for example-org/widget-api",
        );
        insert_problem(
            &conn,
            "01LOOSE",
            "file not found, and no access was granted",
        );
        let fts_query: String = fts_query_from_text("not_found_or_no_access").expect("query");
        assert_eq!(match_count(&conn, &fts_query).unwrap(), 1);
    }
}
