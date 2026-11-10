//! Fails when a tracked Rust file holds a magic-number literal: a number other than 0 or 1 that is
//! not the value of a named `const` or `static`. Rules in prose get forgotten mid-task, by people
//! and by coding agents alike; this check runs on every `cargo test`, whoever wrote the code.
//!
//! Not scanned: comments, string, byte and char literals (so SQL placeholders like `?2` pass),
//! tuple field indexes (`pair.2`), and the value of a `const` or `static` item, which is where a
//! number gets its name. Only files git tracks are scanned, so a new file is checked once it is
//! `git add`ed.

use std::process::Command;

/// The values any code may use unnamed: plain counts and identities. Compared after the type
/// suffix and `_` separators are removed, so `1u8`, `0_usize` and `1.0f64` count too.
const ALLOWED_LITERALS: &[&str] = &["0", "1", "0.0", "1.0"];

/// Type suffixes a number literal may carry, longest first so `u128` is not read as `u1` + `28`.
const NUMBER_SUFFIXES: &[&str] = &[
    "u128", "i128", "usize", "isize", "u64", "i64", "u32", "i32", "u16", "i16", "f64", "f32", "u8",
    "i8",
];

/// Two-character tokens the scanner steps over: a comment opener or closer such as `/*`, an
/// escape such as `\n`, a `b"` or `r"` prefix, and the opening quote plus character of `'x'`.
const TWO_CHAR_TOKEN: usize = 2;
/// A plain char literal such as `'x'`: quote, character, quote.
const PLAIN_CHAR_LITERAL_CHARS: usize = 3;

/// Returns `(line_number, literal)` for every unnamed literal in `source_text`.
fn magic_numbers_in(source_text: &str) -> Vec<(usize, String)> {
    let code_text: String = blank_out_const_values(&blank_out_comments_and_strings(source_text));
    code_text
        .lines()
        .enumerate()
        .flat_map(|(line_index, code_line)| {
            number_literals_in(code_line)
                .into_iter()
                .filter(|literal| !ALLOWED_LITERALS.contains(&normalized_literal(literal).as_str()))
                .map(move |literal| (line_index + 1, literal)) // editors number lines from 1
        })
        .collect()
}

/// The literal without `_` separators and without its type suffix: `1_000u32` -> `1000`.
fn normalized_literal(literal: &str) -> String {
    let without_separators: String = literal
        .chars()
        .filter(|&literal_char| literal_char != '_')
        .collect();
    NUMBER_SUFFIXES
        .iter()
        .find_map(|suffix| without_separators.strip_suffix(suffix))
        .filter(|number_part| !number_part.is_empty())
        .unwrap_or(&without_separators)
        .to_string()
}

fn is_identifier_char(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

/// Replaces comments and string, byte and char literals with spaces, keeping every newline so
/// line numbers still match the original file.
fn blank_out_comments_and_strings(source_text: &str) -> String {
    let source_chars: Vec<char> = source_text.chars().collect();
    let fn_blank = |character: char| if character == '\n' { '\n' } else { ' ' };
    let mut mut_code_text: String = String::with_capacity(source_text.len());
    let mut mut_char_index: usize = 0;
    while mut_char_index < source_chars.len() {
        let current_char: char = source_chars[mut_char_index];
        let next_char: Option<char> = source_chars.get(mut_char_index + 1).copied();
        let follows_identifier: bool =
            mut_char_index > 0 && is_identifier_char(source_chars[mut_char_index - 1]);
        let span_end: Option<usize> = match (current_char, next_char) {
            ('/', Some('/')) => Some(
                source_chars[mut_char_index..]
                    .iter()
                    .position(|&scanned_char| scanned_char == '\n')
                    .map_or(source_chars.len(), |offset| mut_char_index + offset),
            ),
            ('/', Some('*')) => Some(end_of_block_comment(&source_chars, mut_char_index)),
            ('"', _) => Some(end_of_quoted(&source_chars, mut_char_index)),
            ('b', Some('"')) if !follows_identifier => {
                Some(end_of_quoted(&source_chars, mut_char_index + 1))
            }
            ('b', Some('\'')) if !follows_identifier => {
                end_of_char_literal(&source_chars, mut_char_index + 1)
            }
            ('b', Some('r'))
                if !follows_identifier
                    && is_raw_string_start(&source_chars, mut_char_index + 1) =>
            {
                Some(end_of_raw_string(&source_chars, mut_char_index + 1))
            }
            ('r', Some('#' | '"'))
                if !follows_identifier && is_raw_string_start(&source_chars, mut_char_index) =>
            {
                Some(end_of_raw_string(&source_chars, mut_char_index))
            }
            ('\'', _) => end_of_char_literal(&source_chars, mut_char_index),
            _ => None,
        };
        match span_end {
            Some(span_end) => {
                let blanked_span: String = source_chars[mut_char_index..span_end]
                    .iter()
                    .map(|&span_char| fn_blank(span_char))
                    .collect();
                mut_code_text.push_str(&blanked_span);
                mut_char_index = span_end;
            }
            None => {
                mut_code_text.push(current_char); // ordinary code, or a lifetime such as 'a
                mut_char_index += 1;
            }
        }
    }
    mut_code_text
}

/// Index just past the `*/` that closes the block comment opened at `opener_index`. Rust block
/// comments nest, so depth is counted.
fn end_of_block_comment(source_chars: &[char], opener_index: usize) -> usize {
    let mut mut_depth: usize = 0;
    let mut mut_scan_index: usize = opener_index;
    while mut_scan_index < source_chars.len() {
        match (
            source_chars[mut_scan_index],
            source_chars.get(mut_scan_index + 1),
        ) {
            ('/', Some('*')) => {
                mut_depth += 1;
                mut_scan_index += TWO_CHAR_TOKEN;
            }
            ('*', Some('/')) => {
                mut_depth -= 1;
                mut_scan_index += TWO_CHAR_TOKEN;
                if mut_depth == 0 {
                    return mut_scan_index;
                }
            }
            _ => mut_scan_index += 1,
        }
    }
    source_chars.len()
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

/// True when `r_index` holds the `r` of `r"` or `r#...#"`.
fn is_raw_string_start(source_chars: &[char], r_index: usize) -> bool {
    let hash_count: usize = source_chars[r_index + 1..]
        .iter()
        .take_while(|&&scanned_char| scanned_char == '#')
        .count();
    source_chars.get(r_index) == Some(&'r')
        && source_chars.get(r_index + 1 + hash_count) == Some(&'"')
}

/// Index just past the end of the raw string whose `r` is at `r_index`: a quote followed by as
/// many `#` as opened it.
fn end_of_raw_string(source_chars: &[char], r_index: usize) -> usize {
    let hash_count: usize = source_chars[r_index + 1..]
        .iter()
        .take_while(|&&scanned_char| scanned_char == '#')
        .count();
    let closing: Vec<char> = std::iter::once('"')
        .chain(std::iter::repeat_n('#', hash_count))
        .collect();
    let body_start: usize = r_index + hash_count + TWO_CHAR_TOKEN;
    (body_start..source_chars.len())
        .find(|&candidate_index| source_chars[candidate_index..].starts_with(&closing))
        .map_or(source_chars.len(), |closing_index| {
            closing_index + closing.len()
        })
}

/// Index just past a char literal (`'x'`, `'\n'`, `'\''`, `'"'`), or None for a lifetime (`'a`).
fn end_of_char_literal(source_chars: &[char], quote_index: usize) -> Option<usize> {
    match source_chars.get(quote_index + 1) {
        Some('\\') => (quote_index + TWO_CHAR_TOKEN + 1..source_chars.len())
            .find(|&candidate_index| source_chars[candidate_index] == '\'')
            .map(|closing_index| closing_index + 1),
        Some(_) if source_chars.get(quote_index + TWO_CHAR_TOKEN) == Some(&'\'') => {
            Some(quote_index + PLAIN_CHAR_LITERAL_CHARS)
        }
        Some(_) | None => None,
    }
}

/// Blanks the value of every `const NAME` / `static NAME` item: from the keyword to the `;` that
/// ends the item at bracket depth 0, so an inner `;` or code after the item on the same line is
/// handled. `const fn` and `'static` are not items and are left alone.
fn blank_out_const_values(code_text: &str) -> String {
    let code_chars: Vec<char> = code_text.chars().collect();
    let fn_item_starts_at = |keyword_index: usize| -> bool {
        let preceded_by_word_char: bool = keyword_index > 0
            && (is_identifier_char(code_chars[keyword_index - 1])
                || code_chars[keyword_index - 1] == '\'');
        let after_keyword: Option<usize> = ["const ", "static "].iter().find_map(|keyword| {
            let keyword_chars: Vec<char> = keyword.chars().collect();
            code_chars[keyword_index..]
                .starts_with(&keyword_chars)
                .then_some(keyword_index + keyword_chars.len())
        });
        let item_name_is_uppercase: bool = after_keyword.is_some_and(|name_search_start| {
            code_chars[name_search_start..]
                .iter()
                .find(|name_char| !name_char.is_whitespace())
                .is_some_and(char::is_ascii_uppercase)
        });
        !preceded_by_word_char && item_name_is_uppercase
    };
    let mut mut_blanked: Vec<char> = code_chars.clone();
    let mut mut_scan_index: usize = 0;
    while mut_scan_index < code_chars.len() {
        if fn_item_starts_at(mut_scan_index) {
            let item_end: usize = end_of_item(&code_chars, mut_scan_index);
            mut_blanked[mut_scan_index..item_end]
                .iter_mut()
                .filter(|item_char| **item_char != '\n')
                .for_each(|item_char| *item_char = ' ');
            mut_scan_index = item_end;
        } else {
            mut_scan_index += 1;
        }
    }
    mut_blanked.into_iter().collect()
}

/// Index just past the `;` ending the item that starts at `item_start`, at bracket depth 0.
fn end_of_item(code_chars: &[char], item_start: usize) -> usize {
    let mut mut_depth: i64 = 0;
    (item_start..code_chars.len())
        .find(|&scan_index| {
            match code_chars[scan_index] {
                '(' | '[' | '{' => mut_depth += 1,
                ')' | ']' | '}' => mut_depth -= 1,
                _ => {}
            }
            mut_depth == 0 && code_chars[scan_index] == ';'
        })
        .map_or(code_chars.len(), |semicolon_index| semicolon_index + 1)
}

/// Number literals in one line of code, with comments, strings and const values already blanked.
/// A literal is a digit not preceded by an identifier character, plus everything after it that can
/// belong to it. A digit run right after `ident.` or `).` is a tuple field index, not a literal.
fn number_literals_in(code_line: &str) -> Vec<String> {
    let line_chars: Vec<char> = code_line.chars().collect();
    let mut mut_literals: Vec<String> = Vec::new();
    let mut mut_char_index: usize = 0;
    while mut_char_index < line_chars.len() {
        let current_char: char = line_chars[mut_char_index];
        let previous_char: Option<char> = mut_char_index
            .checked_sub(1)
            .map(|previous_index| line_chars[previous_index]);
        let before_previous: Option<char> = mut_char_index
            .checked_sub(TWO_CHAR_TOKEN)
            .map(|before_index| line_chars[before_index]);
        let starts_number: bool =
            current_char.is_ascii_digit() && !previous_char.is_some_and(is_identifier_char);
        let is_field_index: bool = previous_char == Some('.')
            && before_previous
                .is_some_and(|before_char| is_identifier_char(before_char) || before_char == ')');
        if starts_number {
            let literal_end: usize = (mut_char_index..line_chars.len())
                .find(|&scan_index| {
                    let scanned_char: char = line_chars[scan_index];
                    // A '.' belongs to the literal only when a digit follows (10.5, not 0..8 or 1.max()).
                    let is_decimal_point: bool = scanned_char == '.'
                        && !is_field_index
                        && line_chars
                            .get(scan_index + 1)
                            .is_some_and(char::is_ascii_digit);
                    !(is_identifier_char(scanned_char) || is_decimal_point)
                })
                .unwrap_or(line_chars.len());
            if !is_field_index {
                mut_literals.push(line_chars[mut_char_index..literal_end].iter().collect());
            }
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
            .unwrap_or_else(|read_error| panic!("cannot scan {relative_path}: {read_error}")); // an unreadable file must fail, not pass as empty
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
const BLOCK_VALUE: usize = { let partial = 2; partial * 2 }; let after_const = 7;
fn retry<'a>(pair: (u8, u8, u8), name: &'static str) -> &'a str {
    let wait_ms = 250; // flagged
    let offset = 0; let step = 1; let small = 1u8; let zero = 0_usize; let unit = 1.0f64;
    let range_end = 0..8; // flagged: 8
    let third = pair.2; let nested = (pair.0, pair.1).1;
    let sql = "UPDATE t SET a = ?2 WHERE b = 42"; // inside a string: allowed
    let raw = r#"LIMIT 99"#; let bytes = br"55"; let byte_text = b"66"; let byte_char = b'9';
    let quote_char = '"';
    /* 500 in a block comment /* nested 600 */ still a comment 700 */
    let ratio = 2.5;
    let big = 1_000u32;
}
"##;
    let flagged: Vec<String> = magic_numbers_in(sample_source)
        .into_iter()
        .map(|(_line_number, literal)| literal)
        .collect();
    assert_eq!(flagged, vec!["7", "250", "8", "2.5", "1_000u32"]);
}
