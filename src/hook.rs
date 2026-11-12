//! `lessons hook user-prompt-submit`: the prompt hook that surfaces past lessons for a prompt.
//!
//! It must never get in the way of the prompt. Any failure is reported on stderr and the process
//! still exits 0, because exit code 2 would block the prompt. Hits go to stdout, which the agent
//! adds to the conversation as context; no hits means no output at all.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use rusqlite::Connection;

use crate::db::Domain;
use crate::store::SearchHit;
use crate::{config, db, journal, query, store};

const HEADER: &str = "Relevant past lessons (from lessons db):";

/// The agent replaces hook output over 10,000 characters with a file path and a preview, which
/// the model is not asked to read; stay well under it, dropping whole lessons rather than cutting one.
const MAX_OUTPUT_CHARS: usize = 8_000;

/// A pasted one-line error can be huge; each printed field keeps this many characters.
const MAX_LINE_CHARS: usize = 300;

/// How many more hits than `max_results` to fetch, so a lesson whose context matches the current
/// repo can still be boosted into the top results when its plain score ranks it lower.
const BOOST_POOL_FACTOR: usize = 4;

/// The fields this hook reads. Unknown keys are ignored on purpose: the agent adds fields to
/// the hook payload over time, and a new one must not break every prompt.
#[derive(Debug, Deserialize)]
struct HookInput {
    prompt: String,
    #[serde(default)]
    cwd: Option<PathBuf>,
}

/// Reads the hook payload and returns the text to print, empty when nothing is relevant.
pub fn user_prompt_submit(lessons_home: &Path, hook_stdin: impl Read) -> Result<String> {
    let hook_input: HookInput =
        serde_json::from_reader(hook_stdin).context("cannot parse the hook JSON on stdin")?;
    let hook_config = config::load(lessons_home)?.hook;
    let fts_query: Option<String> = query::fts_query_from_text(&hook_input.prompt);
    let prompt_terms: Vec<String> = query::terms_from_text(&hook_input.prompt);
    journal!(
        "hook: prompt -> {fts_query:?}, domains {:?}",
        hook_config.domains
    );
    let hits: Vec<SearchHit> = match &fts_query {
        Some(fts_query) => {
            let opened = open_readable_domains(lessons_home, &hook_config.domains);
            // The query is an OR of the prompt's words, so it returns the best lessons for ANY prompt; keep
            // only those that share enough with this one (issue 5).
            // The pool is a deliberate bound, a latency and recall trade-off: the search ranks by bm25 (rare words
            // dominate), not by shared words, so a lesson that shares four common words can rank below the pool and be missed.
            // An identifier term ranks high, so the weight-4 path is nearly safe; only the four-ordinary-words path can miss.
            let candidate_hits: Vec<SearchHit> = store::search_all(
                &opened,
                fts_query,
                hook_config.max_results.saturating_mul(BOOST_POOL_FACTOR),
            )?
            .into_iter()
            .filter(|hit| shared_weight(hit, &prompt_terms) >= MIN_SHARED_WEIGHT)
            .collect();
            boost_current_repo(
                candidate_hits,
                hook_input.cwd.as_deref(),
                hook_config.max_results,
            )
        }
        None => Vec::new(),
    };
    journal!("hook: {} hit(s) printed", hits.len());
    Ok(render(&hits))
}

/// What a lesson must share with the prompt to be worth printing: four ordinary words, or one
/// identifier-like term (`not_found_or_no_access`, `src/mcp.rs`, `v1.2.3`). Chat prompts therefore
/// print nothing, and so does a prompt of a few ordinary words, on purpose: with lessons this long a
/// lesson shares two or three common words with almost any prompt. Measured on ten real prompts (issue 5):
/// at 2 or 3 some chat prompts still pulled up to 3 lessons; at 4 all five chat prompts were silent and
/// all five relevant prompts still found their lesson.
const MIN_SHARED_WEIGHT: usize = 4;
/// Weight of a shared identifier-like term: one is enough on its own to reach `MIN_SHARED_WEIGHT`.
const IDENTIFIER_TERM_WEIGHT: usize = 4;
/// Weight of a shared ordinary word: `MIN_SHARED_WEIGHT` of them are needed.
const ORDINARY_TERM_WEIGHT: usize = 1;

/// Whether `term` occurs in `text` as a whole word: the characters on both sides are not part of a
/// word (`show` is not found in `shown`, `ready` not in `already`). Both are lower case. Two consequences, kept on
/// purpose: the search stems words (porter) and folds accents, but this check does not, so a lesson that says `failed`
/// matches the search for `failing` yet shares no whole word with it; and a joiner (`-`, `.`, `/`) ends a word on the
/// lesson side, so the term `foo` is found in `foo-bar` (a term that itself holds a joiner must match it exactly).
fn contains_whole_term(text: &str, term: &str) -> bool {
    let fn_is_word_char = |character: char| character.is_alphanumeric() || character == '_';
    text.match_indices(term).any(|(start, matched)| {
        let before_ok: bool = text[..start]
            .chars()
            .next_back()
            .is_none_or(|character| !fn_is_word_char(character));
        let after_ok: bool = text[start + matched.len()..]
            .chars()
            .next()
            .is_none_or(|character| !fn_is_word_char(character));
        before_ok && after_ok
    })
}

/// The summed weight of the prompt terms that appear anywhere in the lesson's text.
fn shared_weight(hit: &SearchHit, prompt_terms: &[String]) -> usize {
    let lesson = &hit.lesson;
    let lesson_text: String = [
        Some(lesson.title.as_str()),
        Some(lesson.problem.as_str()),
        lesson.context.as_deref(),
        lesson.root_cause.as_deref(),
        lesson.resolution.as_deref(),
        lesson.failed_tries.as_deref(),
        lesson.tags.as_deref(),
        lesson.env_versions.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<&str>>()
    .join(" ")
    .to_lowercase();
    prompt_terms
        .iter()
        .filter(|term| contains_whole_term(&lesson_text, term))
        .map(|term| {
            if query::is_identifier_like(term) {
                IDENTIFIER_TERM_WEIGHT
            } else {
                ORDINARY_TERM_WEIGHT
            }
        })
        .sum()
}

/// Opens each existing file of `domains`, skipping one that fails to open (corrupt, locked, no
/// permission) with a note on stderr: in the hook, one bad file must not silence the good ones.
fn open_readable_domains(lessons_home: &Path, domains: &[Domain]) -> Vec<(Domain, Connection)> {
    domains
        .iter()
        .filter(|domain| domain.file_path(lessons_home).exists())
        .filter_map(|&domain| {
            match db::open(&domain.file_path(lessons_home), db::OpenMode::Existing) {
                Ok(conn) => Some((domain, conn)),
                Err(open_error) => {
                    eprintln!("lessons hook: skipping {}: {open_error:#}", domain.as_str());
                    None
                }
            }
        })
        .collect()
}

/// Moves hits whose `context` names the current directory to the front, keeping score order
/// within each group, then cuts to `max_results`.
fn boost_current_repo(
    candidate_hits: Vec<SearchHit>,
    cwd: Option<&Path>,
    max_results: usize,
) -> Vec<SearchHit> {
    let repo_name: Option<String> = cwd
        .and_then(Path::file_name)
        .map(|dir_name| dir_name.to_string_lossy().to_lowercase());
    let fn_is_current_repo = |hit: &SearchHit| match (&repo_name, &hit.lesson.context) {
        (Some(repo_name), Some(lesson_context)) => {
            lesson_context.to_lowercase().contains(repo_name.as_str())
        }
        (None, _) | (_, None) => false,
    };
    let (mut mut_boosted, other_hits): (Vec<SearchHit>, Vec<SearchHit>) =
        candidate_hits.into_iter().partition(fn_is_current_repo);
    mut_boosted.extend(other_hits);
    mut_boosted.truncate(max_results);
    mut_boosted
}

/// Compact plain text for the agent: one block per lesson, first lines only, the failed tries kept
/// because they are what saves a repeat of the same dead end.
fn render(hits: &[SearchHit]) -> String {
    let fn_first_line = |text: &str| -> String {
        let first_line: &str = text.lines().next().unwrap_or("").trim();
        if first_line.chars().count() > MAX_LINE_CHARS {
            format!(
                "{}...",
                first_line.chars().take(MAX_LINE_CHARS).collect::<String>()
            )
        } else {
            first_line.to_string()
        }
    };
    let fn_optional_line = |label: &str, value: &Option<String>| {
        value.as_deref().map_or(String::new(), |text| {
            format!("  {label}: {}\n", fn_first_line(text))
        })
    };
    let lesson_blocks: Vec<String> = hits
        .iter()
        .map(|hit| {
            format!(
                "- [{}] {} (id {})\n  problem: {}\n{}{}{}",
                hit.domain,
                fn_first_line(&hit.lesson.title),
                hit.lesson.id,
                fn_first_line(&hit.lesson.problem),
                fn_optional_line("root cause", &hit.lesson.root_cause),
                fn_optional_line("fix", &hit.lesson.resolution),
                fn_optional_line("failed", &hit.lesson.failed_tries),
            )
        })
        .collect();
    // Keep whole blocks while they fit, best-scored first. A block that does not fit is skipped,
    // not a stop: one oversized lesson must not hide the smaller ones ranked after it.
    let (_chars_used, kept_blocks): (usize, String) = lesson_blocks.iter().fold(
        (HEADER.len() + 1, String::new()),
        |(chars_used, kept_blocks), lesson_block| {
            let block_chars: usize = lesson_block.chars().count();
            if chars_used + block_chars <= MAX_OUTPUT_CHARS {
                (chars_used + block_chars, kept_blocks + lesson_block)
            } else {
                (chars_used, kept_blocks)
            }
        },
    );
    if kept_blocks.is_empty() {
        String::new()
    } else {
        format!("{HEADER}\n{kept_blocks}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A title longer than MAX_LINE_CHARS, so the per-line cut must apply.
    const OVERSIZED_TITLE_CHARS: usize = MAX_LINE_CHARS + 100;
    /// Enough pasted-log lines to exceed MAX_OUTPUT_CHARS many times over.
    const PASTED_LOG_LINES: usize = 20_000;
    /// How many lessons past the result cap a test inserts, so the cap has something to cut.
    const EXTRA_LESSONS_PAST_THE_CAP: usize = 3;
    /// A field far longer than MAX_LINE_CHARS.
    const HUGE_FIELD_CHARS: usize = MAX_LINE_CHARS * 16;
    /// Enough maximal lessons that their blocks together overflow MAX_OUTPUT_CHARS.
    const MANY_LESSONS: usize = 40;
    /// `search_hit` fills problem, root cause, fix and failed with the same text: four long lines.
    const LONG_FIELDS_PER_LARGE_BLOCK: usize = 4;
    use crate::db::{Domain, OpenMode, open};
    use crate::store::{NewLesson, insert};

    fn add_lesson(
        lessons_home: &Path,
        domain: Domain,
        title: &str,
        context: Option<&str>,
    ) -> String {
        let conn = open(&domain.file_path(lessons_home), OpenMode::Create).unwrap();
        insert(
            &conn,
            &NewLesson {
                title: title.into(),
                problem: "docker pull failed with not_found_or_no_access".into(),
                context: context.map(str::to_string),
                ..NewLesson::default()
            },
        )
        .unwrap()
    }

    fn hook_output(lessons_home: &Path, hook_json: &str) -> Result<String> {
        user_prompt_submit(lessons_home, hook_json.as_bytes())
    }

    /// A lesson with the given title, problem and fix, in shared.db.
    fn add_full_lesson(lessons_home: &Path, title: &str, problem: &str, resolution: &str) {
        let conn = open(&Domain::Shared.file_path(lessons_home), OpenMode::Create).unwrap();
        insert(
            &conn,
            &NewLesson {
                title: title.into(),
                problem: problem.into(),
                resolution: Some(resolution.into()),
                ..NewLesson::default()
            },
        )
        .unwrap();
    }

    /// A home with two lessons whose words (file, time, review, push) also turn up in everyday chat.
    fn home_with_everyday_words_in_lessons() -> tempfile::TempDir {
        let temp_dir = tempfile::tempdir().unwrap();
        add_full_lesson(
            temp_dir.path(),
            "A perl substitution with the pipe delimiter corrupts a source file",
            "perl s with a pipe in the pattern mangled the file at the start of the file; time was lost",
            "Use another delimiter, or the editor tool; review the diff before the push",
        );
        add_full_lesson(
            temp_dir.path(),
            "Registry pull fails with not_found_or_no_access",
            "docker pull failed with not_found_or_no_access",
            "Log in again",
        );
        temp_dir
    }

    /// Issue 5: the hook used to return the best lessons for ANY prompt, because the query is an OR of
    /// the prompt's words and nothing said "no good match". A lesson must now share four ordinary words
    /// with the prompt, or one identifier-like term.
    #[test]
    fn everyday_prompts_print_nothing_when_a_lesson_shares_only_a_few_ordinary_words() {
        let temp_dir = home_with_everyday_words_in_lessons();
        let prompts: &[&str] = &[
            "what time is it",
            "ok thanks, go ahead",
            "show me the file",
            "push it now",
            "start the day",
        ];
        prompts.iter().for_each(|prompt| {
            let hook_json: String = serde_json::json!({ "prompt": prompt }).to_string();
            assert_eq!(
                hook_output(temp_dir.path(), &hook_json).unwrap(),
                "",
                "{prompt}"
            );
        });
    }

    #[test]
    fn a_prompt_sharing_four_ordinary_words_prints_the_lesson() {
        let temp_dir = home_with_everyday_words_in_lessons();
        let hook_text: String = hook_output(
            temp_dir.path(),
            r#"{"prompt":"a perl substitution with the pipe delimiter broke my file"}"#,
        )
        .unwrap();
        assert!(hook_text.contains("A perl substitution"), "{hook_text}");
        // Three shared words are one short of the bar: pins the cutoff at four.
        assert_eq!(
            hook_output(temp_dir.path(), r#"{"prompt":"perl substitution file"}"#).unwrap(),
            ""
        );
    }

    #[test]
    fn one_identifier_like_term_is_enough() {
        let temp_dir = home_with_everyday_words_in_lessons();
        let hook_text: String = hook_output(
            temp_dir.path(),
            r#"{"prompt":"error: not_found_or_no_access"}"#,
        )
        .unwrap();
        assert!(hook_text.contains("Registry pull fails"), "{hook_text}");
    }

    /// A prompt word must match a whole word of the lesson: `show` is not found in `shown`, nor `ready` in
    /// `already`. Here the lesson holds one exact word (`prompt`, so the search returns it) and only
    /// pieces of the other four.
    #[test]
    fn a_prompt_word_inside_a_longer_lesson_word_does_not_count() {
        let temp_dir = tempfile::tempdir().unwrap();
        add_full_lesson(
            temp_dir.path(),
            "Hook prompt notes",
            "shown already reviewed pushes preview reorder newsfeed",
            "none",
        );
        assert_eq!(
            hook_output(
                temp_dir.path(),
                r#"{"prompt":"prompt show ready review push"}"#
            )
            .unwrap(),
            ""
        );
        // The same for the tail of a longer word: view in preview, order in reorder, feed in newsfeed.
        assert_eq!(
            hook_output(temp_dir.path(), r#"{"prompt":"prompt view order feed"}"#).unwrap(),
            ""
        );
        // The control: the same words as whole words do count.
        let hook_text: String = hook_output(
            temp_dir.path(),
            r#"{"prompt":"prompt shown already reviewed pushes"}"#,
        )
        .unwrap();
        assert!(hook_text.contains("Hook prompt notes"), "{hook_text}");
    }

    /// `_` and non-ASCII letters belong to a word: `not_found` is not found in `not_found_or_no_access`, and
    /// `caf` is not found in the accented word that starts with it. (The exact word `recipe` makes the search return the lesson.)
    #[test]
    fn underscore_and_non_ascii_letters_keep_a_word_whole() {
        let temp_dir = tempfile::tempdir().unwrap();
        // Escapes, not literals, keep the source ASCII.
        let accented_title: &str = "caf\u{e9} cr\u{e8}me br\u{fb}l\u{e9}e recipe";
        add_full_lesson(
            temp_dir.path(),
            accented_title,
            "docker failed with not_found_or_no_access",
            "none",
        );
        assert_eq!(
            hook_output(temp_dir.path(), r#"{"prompt":"not_found"}"#).unwrap(),
            ""
        );
        assert_eq!(
            hook_output(temp_dir.path(), r#"{"prompt":"recipe caf cr br"}"#).unwrap(),
            ""
        );
        // The control: the whole words do count.
        let accented_prompt: String = serde_json::json!({ "prompt": accented_title }).to_string();
        let hook_text: String = hook_output(temp_dir.path(), &accented_prompt).unwrap();
        assert!(hook_text.contains("caf\u{e9} cr\u{e8}me"), "{hook_text}");
    }

    /// The price of the cutoff, kept on purpose: a prompt that is one ordinary word prints nothing.
    #[test]
    fn a_prompt_of_one_ordinary_word_prints_nothing() {
        let temp_dir = home_with_everyday_words_in_lessons();
        assert_eq!(
            hook_output(temp_dir.path(), r#"{"prompt":"perl"}"#).unwrap(),
            ""
        );
    }

    #[test]
    fn prints_nothing_when_no_lesson_matches() {
        let temp_dir = tempfile::tempdir().unwrap();
        add_lesson(temp_dir.path(), Domain::Shared, "registry token", None);
        assert_eq!(
            hook_output(temp_dir.path(), r#"{"prompt":"bake a cake"}"#).unwrap(),
            ""
        );
        assert_eq!(
            hook_output(temp_dir.path(), r#"{"prompt":"the a of"}"#).unwrap(),
            ""
        );
    }

    #[test]
    fn prints_the_header_and_ignores_unknown_payload_keys() {
        let temp_dir = tempfile::tempdir().unwrap();
        add_lesson(temp_dir.path(), Domain::Shared, "registry token", None);
        let hook_text: String = hook_output(
            temp_dir.path(),
            r#"{"session_id":"s1","hook_event_name":"UserPromptSubmit","prompt":"why not_found_or_no_access?","future_key":1}"#,
        )
        .unwrap();
        assert!(hook_text.starts_with(HEADER), "{hook_text}");
        assert!(
            hook_text.contains("- [shared] registry token"),
            "{hook_text}"
        );
    }

    #[test]
    fn never_reads_personal_by_default() {
        let temp_dir = tempfile::tempdir().unwrap();
        add_lesson(
            temp_dir.path(),
            Domain::Personal,
            "private registry note",
            None,
        );
        assert_eq!(
            hook_output(
                temp_dir.path(),
                r#"{"prompt":"private registry note not_found_or_no_access"}"#
            )
            .unwrap(),
            ""
        );
    }

    #[test]
    fn a_lesson_from_the_current_repo_is_moved_to_the_front() {
        let temp_dir = tempfile::tempdir().unwrap();
        // Same text, so without the boost the order is decided by score and insertion, not repo.
        add_lesson(
            temp_dir.path(),
            Domain::Shared,
            "registry token elsewhere",
            Some("other-repo"),
        );
        add_lesson(
            temp_dir.path(),
            Domain::Work,
            "registry token here",
            Some("widget-api"),
        );
        let hook_text: String = hook_output(
            temp_dir.path(),
            r#"{"prompt":"registry token not_found_or_no_access","cwd":"/home/someone/src/widget-api"}"#,
        )
        .unwrap();
        let first_lesson_line: &str = hook_text.lines().nth(1).unwrap_or("");
        assert!(
            first_lesson_line.contains("registry token here"),
            "{hook_text}"
        );
    }

    #[test]
    fn max_results_caps_the_output() {
        let temp_dir = tempfile::tempdir().unwrap();
        let default_max_results: usize = config::HookConfig::default().max_results;
        (0..default_max_results + EXTRA_LESSONS_PAST_THE_CAP).for_each(|lesson_index| {
            add_lesson(
                temp_dir.path(),
                Domain::Shared,
                &format!("registry token {lesson_index}"),
                None,
            );
        });
        let hook_text: String = hook_output(
            temp_dir.path(),
            r#"{"prompt":"registry token not_found_or_no_access"}"#,
        )
        .unwrap();
        assert_eq!(
            hook_text.matches("\n- [").count() + usize::from(hook_text.starts_with("- [")),
            default_max_results
        );
    }

    #[test]
    fn output_stays_under_the_hook_output_limit_and_long_lines_are_cut() {
        let temp_dir = tempfile::tempdir().unwrap();
        let conn = open(&Domain::Shared.file_path(temp_dir.path()), OpenMode::Create).unwrap();
        let huge_line: String = format!(
            "registry token not_found_or_no_access {}",
            "x".repeat(HUGE_FIELD_CHARS)
        );
        (0..MANY_LESSONS).for_each(|lesson_index| {
            insert(
                &conn,
                &NewLesson {
                    title: format!(
                        "registry {lesson_index} {}",
                        "t".repeat(OVERSIZED_TITLE_CHARS)
                    ),
                    problem: huge_line.clone(),
                    resolution: Some(huge_line.clone()),
                    failed_tries: Some(huge_line.clone()),
                    ..NewLesson::default()
                },
            )
            .unwrap();
        });
        std::fs::write(
            temp_dir.path().join("config.toml"),
            format!("[hook]\nmax_results = {MANY_LESSONS}\n"),
        )
        .unwrap();
        let hook_text: String = hook_output(
            temp_dir.path(),
            r#"{"prompt":"registry token not_found_or_no_access"}"#,
        )
        .unwrap();
        assert!(hook_text.starts_with(HEADER));
        assert!(
            hook_text.chars().count() <= MAX_OUTPUT_CHARS,
            "{} chars",
            hook_text.chars().count()
        );
        assert!(
            hook_text
                .lines()
                .all(|line| !line.contains(&"x".repeat(MAX_LINE_CHARS + 1)))
        );
        assert!(hook_text.ends_with('\n'), "no lesson block is cut in half");
    }

    #[test]
    fn one_oversized_lesson_does_not_hide_the_others() {
        let temp_dir = tempfile::tempdir().unwrap();
        let conn = open(&Domain::Shared.file_path(temp_dir.path()), OpenMode::Create).unwrap();
        // A huge multi-line title, matching the prompt in its strongest column, so it ranks first.
        let huge_title: String = format!(
            "registry token\n{}",
            "line of pasted log\n".repeat(PASTED_LOG_LINES)
        );
        insert(
            &conn,
            &NewLesson {
                title: huge_title,
                problem: "registry token not_found_or_no_access".into(),
                ..NewLesson::default()
            },
        )
        .unwrap();
        add_lesson(
            temp_dir.path(),
            Domain::Shared,
            "small registry token lesson",
            None,
        );
        let hook_text: String = hook_output(
            temp_dir.path(),
            r#"{"prompt":"registry token not_found_or_no_access"}"#,
        )
        .unwrap();
        assert!(
            hook_text.contains("small registry token lesson"),
            "{hook_text}"
        );
        assert!(hook_text.chars().count() <= MAX_OUTPUT_CHARS);
        assert!(
            !hook_text.contains("line of pasted log"),
            "only the first line of a title is printed"
        );
    }

    fn search_hit(title: &str, field_text: &str) -> SearchHit {
        SearchHit {
            domain: "shared",
            score: -1.0,
            lesson: crate::store::Lesson {
                id: "01TESTID".into(),
                created_at: "2000-01-01T00:00:00Z".into(),
                updated_at: "2000-01-01T00:00:00Z".into(),
                title: title.into(),
                context: None,
                problem: field_text.into(),
                root_cause: Some(field_text.into()),
                resolution: Some(field_text.into()),
                failed_tries: Some(field_text.into()),
                tags: None,
                env_versions: None,
                superseded_by: None,
                hits: 0,
            },
        }
    }

    /// A hit whose only populated optional field is the one `set_field` fills, so a test sees which field is read.
    fn weighted_hit(set_field: fn(&mut crate::store::Lesson, String), text: &str) -> SearchHit {
        let mut mut_hit: SearchHit = search_hit("unrelated title", "unrelated words");
        mut_hit.lesson.root_cause = None;
        mut_hit.lesson.resolution = None;
        mut_hit.lesson.failed_tries = None;
        set_field(&mut mut_hit.lesson, text.to_string());
        mut_hit
    }

    #[test]
    fn shared_weight_ignores_the_case_of_the_lesson_text() {
        let prompt_terms: Vec<String> = ["perl", "pipe", "substitution", "failed"]
            .map(String::from)
            .to_vec();
        let hit: SearchHit = weighted_hit(
            |lesson, text| lesson.title = text,
            "Perl PIPE Substitution Failed",
        );
        assert_eq!(shared_weight(&hit, &prompt_terms), MIN_SHARED_WEIGHT);
    }

    #[test]
    fn shared_weight_reads_every_lesson_field() {
        let prompt_terms: Vec<String> = ["alpha", "bravo", "charlie", "delta", "zulu_9"]
            .map(String::from)
            .to_vec();
        let shared_text: &str = "alpha bravo charlie delta zulu_9";
        let expected_weight: usize = MIN_SHARED_WEIGHT + IDENTIFIER_TERM_WEIGHT;
        type SetField = fn(&mut crate::store::Lesson, String);
        let field_setters: &[(&str, SetField)] = &[
            ("title", |lesson, text| lesson.title = text),
            ("problem", |lesson, text| lesson.problem = text),
            ("context", |lesson, text| lesson.context = Some(text)),
            ("root_cause", |lesson, text| lesson.root_cause = Some(text)),
            ("resolution", |lesson, text| lesson.resolution = Some(text)),
            ("failed_tries", |lesson, text| {
                lesson.failed_tries = Some(text)
            }),
            ("tags", |lesson, text| lesson.tags = Some(text)),
            ("env_versions", |lesson, text| {
                lesson.env_versions = Some(text)
            }),
        ];
        field_setters.iter().for_each(|(field_name, set_field)| {
            let hit: SearchHit = weighted_hit(*set_field, shared_text);
            assert_eq!(
                shared_weight(&hit, &prompt_terms),
                expected_weight,
                "{field_name}"
            );
        });
    }

    #[test]
    fn render_skips_a_block_that_no_longer_fits_but_keeps_smaller_ones_after_it() {
        // Each large block is about 1,300 chars after the per-line caps, so a run of them overflows
        // MAX_OUTPUT_CHARS; the small block ranked last must still be printed.
        let long_field: String = "y".repeat(MAX_LINE_CHARS);
        let large_hit_count: usize =
            MAX_OUTPUT_CHARS / (LONG_FIELDS_PER_LARGE_BLOCK * MAX_LINE_CHARS) + 1;
        let ranked_hits: Vec<SearchHit> = (0..large_hit_count)
            .map(|_| search_hit("large", &long_field))
            .chain(std::iter::once(search_hit("small last hit", "short")))
            .collect();
        let rendered: String = render(&ranked_hits);
        assert!(rendered.chars().count() <= MAX_OUTPUT_CHARS);
        assert!(
            rendered.matches("] large (").count() < large_hit_count,
            "the run must overflow"
        );
        assert!(rendered.contains("small last hit"), "{rendered}");
    }

    #[test]
    fn render_prints_only_the_first_line_of_a_title() {
        let rendered: String =
            render(&[search_hit("first title line\nsecond title line", "short")]);
        assert!(rendered.contains("first title line"), "{rendered}");
        assert!(!rendered.contains("second title line"), "{rendered}");
    }

    #[test]
    fn a_corrupt_file_is_skipped_and_the_others_still_answer() {
        let temp_dir = tempfile::tempdir().unwrap();
        add_lesson(temp_dir.path(), Domain::Shared, "registry token", None);
        std::fs::write(
            Domain::Work.file_path(temp_dir.path()),
            "this is not a SQLite file at all",
        )
        .unwrap();
        let hook_text: String = hook_output(
            temp_dir.path(),
            r#"{"prompt":"registry token not_found_or_no_access"}"#,
        )
        .unwrap();
        assert!(
            hook_text.contains("- [shared] registry token"),
            "{hook_text}"
        );
    }

    #[test]
    fn bad_stdin_is_an_error_the_caller_reports() {
        let temp_dir = tempfile::tempdir().unwrap();
        assert!(hook_output(temp_dir.path(), "not json").is_err());
        assert!(
            hook_output(temp_dir.path(), r#"{"cwd":"/tmp"}"#).is_err(),
            "prompt is required"
        );
    }
}
