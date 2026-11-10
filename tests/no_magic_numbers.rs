//! Fails when a tracked Rust file holds a magic-number literal: a number other than 0 or 1 that is
//! not the value of a named `const` or `static`. Rules in prose get forgotten mid-task, by people
//! and by coding agents alike; this check runs on every `cargo test`, whoever wrote the code.
//!
//! Not scanned: comments, string literals (so SQL placeholders like `?2` pass), and the body of a
//! `const` or `static` item, which is where a value gets its name.

use std::process::Command;

/// Two-character tokens the scanner steps over: a comment opener such as `/*`, an escape such as
/// `\n`, the `r"` that opens a raw string, and the opening quote plus character of `'x'`.
const TWO_CHAR_TOKEN: usize = 2;
/// A plain char literal such as `'x'`: quote, character, quote.
const PLAIN_CHAR_LITERAL_CHARS: usize = 3;

/// The literals any code may use unnamed: plain counts and identities.
const ALLOWED_LITERALS: &[&str] = &["0", "1", "0.0", "1.0"];

/// Returns `(line_number, literal)` for every unnamed literal in `source_text`.
fn magic_numbers_in(source_text: &str) -> Vec<(usize, String)> {
    let code_text: String = blank_out_comments_and_strings(source_text);
    let mut mut_inside_const_item: bool = false;
    let mut mut_found: Vec<(usize, String)> = Vec::new();
    for (line_index, code_line) in code_text.lines().enumerate() {
        let trimmed_line: &str = code_line.trim_start();
        let starts_const_item: bool = ["const ", "pub const ", "static ", "pub static "]
            .iter()
            .any(|item_prefix| trimmed_line.starts_with(item_prefix))
            && trimmed_line
                .split_whitespace()
                .find(|word| !["pub", "const", "static"].contains(word))
                .is_some_and(|item_name| {
                    item_name
                        .chars()
                        .next()
                        .is_some_and(|first_char| first_char.is_ascii_uppercase())
                });
        if starts_const_item {
            mut_inside_const_item = true;
        }
        if !mut_inside_const_item {
            mut_found.extend(
                number_literals_in(code_line)
                    .into_iter()
                    .filter(|literal| !ALLOWED_LITERALS.contains(&literal.as_str()))
                    .map(|literal| (line_index + 1, literal)), // editors number lines from 1
            );
        }
        if mut_inside_const_item && code_line.trim_end().ends_with(';') {
            mut_inside_const_item = false; // the item's value ends at its semicolon
        }
    }
    mut_found
}

/// Replaces comments, string literals and char literals with spaces, keeping every newline so
/// line numbers still match the original file.
fn blank_out_comments_and_strings(source_text: &str) -> String {
    let source_chars: Vec<char> = source_text.chars().collect();
    let fn_blank = |character: char| if character == '\n' { '\n' } else { ' ' };
    let mut mut_code_text: String = String::with_capacity(source_text.len());
    let mut mut_char_index: usize = 0;
    while mut_char_index < source_chars.len() {
        let current_char: char = source_chars[mut_char_index];
        let next_char: Option<char> = source_chars.get(mut_char_index + 1).copied();
        let span_end: usize = match (current_char, next_char) {
            ('/', Some('/')) => source_chars[mut_char_index..]
                .iter()
                .position(|&scanned_char| scanned_char == '\n')
                .map_or(source_chars.len(), |offset| mut_char_index + offset),
            ('/', Some('*')) => {
                find_after(&source_chars, mut_char_index + TWO_CHAR_TOKEN, &['*', '/'])
            }
            ('"', _) => end_of_quoted(&source_chars, mut_char_index),
            ('r', Some('#' | '"')) if is_raw_string_start(&source_chars, mut_char_index) => {
                end_of_raw_string(&source_chars, mut_char_index)
            }
            ('\'', _) => {
                end_of_char_literal(&source_chars, mut_char_index).unwrap_or(mut_char_index + 1)
            }
            _ => {
                mut_code_text.push(current_char);
                mut_char_index += 1;
                continue;
            }
        };
        let blanked_span: String = source_chars[mut_char_index..span_end]
            .iter()
            .map(|&span_char| fn_blank(span_char))
            .collect();
        mut_code_text.push_str(&blanked_span);
        mut_char_index = span_end;
    }
    mut_code_text
}

/// Index just past the first occurrence of `closing` at or after `search_from`.
fn find_after(source_chars: &[char], search_from: usize, closing: &[char]) -> usize {
    (search_from..source_chars.len())
        .find(|&candidate_index| source_chars[candidate_index..].starts_with(closing))
        .map_or(source_chars.len(), |closing_index| {
            closing_index + closing.len()
        })
}

/// Index just past the closing quote of the `"..."` starting at `quote_index`, honouring `\` escapes.
fn end_of_quoted(source_chars: &[char], quote_index: usize) -> usize {
    let mut mut_scan_index: usize = quote_index + 1;
    while mut_scan_index < source_chars.len() {
        match source_chars[mut_scan_index] {
            '\\' => mut_scan_index += TWO_CHAR_TOKEN, // skip the escaped character, quotes included
            '"' => return mut_scan_index + 1,
            _ => mut_scan_index += 1,
        }
    }
    source_chars.len()
}

/// True for `r"` or `r#...#"` that is not the tail of an identifier such as `for` or `ptr`.
fn is_raw_string_start(source_chars: &[char], r_index: usize) -> bool {
    let follows_identifier: bool = r_index > 0
        && (source_chars[r_index - 1].is_alphanumeric() || source_chars[r_index - 1] == '_');
    let hash_count: usize = source_chars[r_index + 1..]
        .iter()
        .take_while(|&&scanned_char| scanned_char == '#')
        .count();
    !follows_identifier && source_chars.get(r_index + 1 + hash_count) == Some(&'"')
}

/// Index just past the end of the raw string starting at `r_index`: a quote followed by as many
/// `#` as opened it.
fn end_of_raw_string(source_chars: &[char], r_index: usize) -> usize {
    let hash_count: usize = source_chars[r_index + 1..]
        .iter()
        .take_while(|&&scanned_char| scanned_char == '#')
        .count();
    let closing: Vec<char> = std::iter::once('"')
        .chain(std::iter::repeat_n('#', hash_count))
        .collect();
    find_after(
        source_chars,
        r_index + hash_count + TWO_CHAR_TOKEN,
        &closing,
    )
}

/// Index just past a char literal (`'x'`, `'\n'`, `'\''`, `'"'`), or None for a lifetime (`'a`).
fn end_of_char_literal(source_chars: &[char], quote_index: usize) -> Option<usize> {
    match source_chars.get(quote_index + 1) {
        Some('\\') => (quote_index + TWO_CHAR_TOKEN..source_chars.len())
            .find(|&candidate_index| {
                source_chars[candidate_index] == '\''
                    && candidate_index > quote_index + TWO_CHAR_TOKEN
            })
            .map(|closing_index| closing_index + 1),
        Some(_) if source_chars.get(quote_index + TWO_CHAR_TOKEN) == Some(&'\'') => {
            Some(quote_index + PLAIN_CHAR_LITERAL_CHARS)
        }
        Some(_) | None => None,
    }
}

/// Number literals in one line of code (comments and strings already blanked). A literal is a
/// digit not preceded by an identifier character, plus everything after it that can belong to it.
fn number_literals_in(code_line: &str) -> Vec<String> {
    let line_chars: Vec<char> = code_line.chars().collect();
    let fn_is_identifier_char = |character: char| character.is_alphanumeric() || character == '_';
    let mut mut_literals: Vec<String> = Vec::new();
    let mut mut_char_index: usize = 0;
    while mut_char_index < line_chars.len() {
        let starts_literal: bool = line_chars[mut_char_index].is_ascii_digit()
            && (mut_char_index == 0 || !fn_is_identifier_char(line_chars[mut_char_index - 1]));
        if starts_literal {
            let literal_end: usize = (mut_char_index..line_chars.len())
                .find(|&scan_index| {
                    let scanned_char: char = line_chars[scan_index];
                    // A '.' belongs to the literal only when a digit follows (10.5, not 0..8 or 1.max()).
                    let is_decimal_point: bool = scanned_char == '.'
                        && line_chars
                            .get(scan_index + 1)
                            .is_some_and(char::is_ascii_digit);
                    !(fn_is_identifier_char(scanned_char) || is_decimal_point)
                })
                .unwrap_or(line_chars.len());
            mut_literals.push(line_chars[mut_char_index..literal_end].iter().collect());
            mut_char_index = literal_end;
        } else {
            mut_char_index += 1;
        }
    }
    mut_literals
}

#[test]
fn no_tracked_rust_file_holds_a_magic_number() {
    let listing = Command::new("git")
        .args(["ls-files", "*.rs"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run git ls-files");
    let tracked_files: String = String::from_utf8_lossy(&listing.stdout).into_owned();
    assert!(
        !tracked_files.trim().is_empty(),
        "git ls-files found no Rust files to scan"
    );
    let violations: Vec<String> = tracked_files
        .lines()
        .flat_map(|relative_path| {
            let source_text: String = std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative_path),
            )
            .unwrap_or_default(); // a file listed but deleted in the working tree has nothing to scan
            magic_numbers_in(&source_text)
                .into_iter()
                .map(move |(line_number, literal)| {
                    format!("{relative_path}:{line_number}: {literal}")
                })
        })
        .collect();
    assert!(
        violations.is_empty(),
        "magic numbers; give each a named const, or derive it from the data:\n{}",
        violations.join("\n")
    );
}

#[test]
fn the_scanner_flags_unnamed_literals_and_nothing_else() {
    let sample_source: &str = r##"
const RETRY_LIMIT: u32 = 3; // named here: allowed
pub const WEIGHTS: &[(&str, f64)] = &[
    ("title", 10.0),
];
fn retry() {
    let wait_ms = 250; // flagged
    let offset = 0; let step = 1; // 0 and 1 are allowed
    let range_end = 0..8; // flagged: 8
    let sql = "UPDATE t SET a = ?2 WHERE b = 42"; // inside a string: allowed
    let raw = r#"LIMIT 99"#;
    let quote_char = '"'; let tuple_field = pair.1;
    /* 500 in a block comment */
    let ratio = 2.5;
}
"##;
    let flagged: Vec<String> = magic_numbers_in(sample_source)
        .into_iter()
        .map(|(_line_number, literal)| literal)
        .collect();
    assert_eq!(flagged, vec!["250", "8", "2.5"]);
}
