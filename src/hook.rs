//! `lessons hook user-prompt-submit`: the Claude Code hook that surfaces past lessons for a prompt.
//!
//! It must never get in the way of the prompt. Any failure is reported on stderr and the process
//! still exits 0, because exit code 2 would block the prompt. Hits go to stdout, which Claude Code
//! adds to the conversation as context; no hits means no output at all.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::store::SearchHit;
use crate::{config, db, journal, query, store};

const HEADER: &str = "Relevant past lessons (from lessons db):";

/// Claude Code replaces hook output over 10,000 characters with a file path and a preview, which
/// Claude is not asked to read; stay well under it, dropping whole lessons rather than cutting one.
const MAX_OUTPUT_CHARS: usize = 8_000;

/// A pasted one-line error can be huge; each printed field keeps this many characters.
const MAX_LINE_CHARS: usize = 300;

/// How many more hits than `max_results` to fetch, so a lesson whose context matches the current
/// repo can still be boosted into the top results when its plain score ranks it lower.
const BOOST_POOL_FACTOR: usize = 4;

/// The fields this hook reads. Unknown keys are ignored on purpose: Claude Code adds fields to
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
    journal!(
        "hook: prompt -> {fts_query:?}, domains {:?}",
        hook_config.domains
    );
    let hits: Vec<SearchHit> = match &fts_query {
        Some(fts_query) => {
            let opened = db::open_readable_domains(lessons_home, &hook_config.domains);
            let candidate_hits: Vec<SearchHit> = store::search_all(
                &opened,
                fts_query,
                hook_config.max_results.saturating_mul(BOOST_POOL_FACTOR),
            )?;
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

/// Compact plain text for Claude: one block per lesson, first lines only, the failed tries kept
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
            hook_output(temp_dir.path(), r#"{"prompt":"registry"}"#).unwrap(),
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
            r#"{"prompt":"registry token","cwd":"/home/someone/src/widget-api"}"#,
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
        let hook_text: String = hook_output(temp_dir.path(), r#"{"prompt":"registry"}"#).unwrap();
        assert_eq!(
            hook_text.matches("\n- [").count() + usize::from(hook_text.starts_with("- [")),
            default_max_results
        );
    }

    #[test]
    fn output_stays_under_the_claude_code_limit_and_long_lines_are_cut() {
        let temp_dir = tempfile::tempdir().unwrap();
        let conn = open(&Domain::Shared.file_path(temp_dir.path()), OpenMode::Create).unwrap();
        let huge_line: String = format!("registry {}", "x".repeat(HUGE_FIELD_CHARS));
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
        let hook_text: String = hook_output(temp_dir.path(), r#"{"prompt":"registry"}"#).unwrap();
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
                problem: "registry token".into(),
                ..NewLesson::default()
            },
        )
        .unwrap();
        add_lesson(
            temp_dir.path(),
            Domain::Shared,
            "small registry lesson",
            None,
        );
        let hook_text: String =
            hook_output(temp_dir.path(), r#"{"prompt":"registry token"}"#).unwrap();
        assert!(hook_text.contains("small registry lesson"), "{hook_text}");
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
        let hook_text: String = hook_output(temp_dir.path(), r#"{"prompt":"registry"}"#).unwrap();
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
