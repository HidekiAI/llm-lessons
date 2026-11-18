# PLAN 2026-11-18: last_bypassed_at (issue #17)

TL;DR: add `last_bypassed_at TEXT` (NULL until the first bump) beside `bypassed` from #12. `bump_bypassed` sets it to
now in the same UPDATE; `updated_at` stays untouched; sync keeps the later of the two; `show`, JSON and MCP carry it;
hook ranking does not change. Nothing else moves. Detail below.

## Requirements

From issue #17 (title: "feat: record when a lesson was last bypassed"):

- T1 "One new column, last_bypassed_at TEXT, NULL until the first bump."
- T2 "lessons bypassed sets it to now in the same UPDATE as the count."
- T3 "updated_at stays untouched. A reminder is not an edit."
- T4 "Sync merges it as the later of the two times, like the count's max."
- T5 "show, the JSON and the MCP output carry it."
- T6 "Hook ranking does not change yet." Not in scope: "a full history of every bump."

Decided with @me (count as ticket comments):

- D1 (2026-11-18) Chose "Add last_bypassed_at" over a full bump history and over "counter is enough": column
  `last_bypassed_at TEXT`, NULL until the first bump, ranking unchanged.
- D2 These commits stack on `feat/last-bypassed-at` after `6b30a01` (the #12 docs commit) and ship together with #12.
- D3 (@me 2026-11-18, chose "Only when set" over "Always, 'never' if unset") `show` text prints the time only when
  set, as ` (last <ts>)` after the bypassed count; a NULL prints nothing extra.

## Premises

- MEASURED: stamp format is `strftime('%Y-%m-%dT%H:%M:%SZ','now')` (`src/db.rs:272-273`, `src/store.rs:225`); fixed
  width, UTC, so text order equals time order. The new UPDATE uses the same expression.
- MEASURED: `MIGRATIONS = [SCHEMA_V1, SCHEMA_V2]` (`src/db.rs:185`); V2 is one `ALTER TABLE` (`db.rs:321`). A V3 appends.
- MEASURED: `lesson_au` fires only `UPDATE OF` the seven indexed columns (`db.rs:308`), so the new column never reindexes.
- MEASURED: `Option<String>::max` in Rust orders `None < Some`, so `ours.last_bypassed_at.clone().max(theirs...)` keeps a
  set value over NULL and the later of two set values. Same idiom as `updated_at` (`sync.rs:243`). A test pins it.
- MEASURED: `Lesson` has `#[serde(default)]` precedent on `bypassed` (`store.rs:44-46`); `Option` also defaults, so
  old `.jsonl` lines parse to `None` (the attribute is kept for symmetry and a test pins it).
- MEASURED: the one-sided sync path clones the whole row (`sync.rs` `(None, Some(added_row), None)` arm), so it keeps the field.
- MEASURED: `fields_differ` compares `MERGED_FIELDS` only (`sync.rs:178`); the new field is not in it, so it is ignored.
- MEASURED: MCP `search` is serde of `SearchHit`; JSON gets the field with no MCP code change (as in #12).
- Fixtures needing `last_bypassed_at: None`: `hook.rs:666,680`, `sync.rs:522,744,783,913` (the last three may use `..base`;
  compiler decides), `store.rs:808,1159,1281`, `sync/files.rs:312`, `sync/cycle.rs:302` (MEASURED by `grep 'bypassed: '`).
- MEASURED: `Lesson` derives `Deserialize` with no `deny_unknown_fields` (`src/store.rs:29-30`; the deny is on `NewLesson`,
  `store.rs:59`), so an older binary reads a `.jsonl` line with the unknown `last_bypassed_at` key, ignores it, and drops
  it on its next rewrite (same limit as #12's `bypassed`); a newer host restores it at its next sync by later-of-two.
- MEASURED: `skills/lessons/SKILL.md:20-23` describes the command and when to run it, not what `show` prints, so nothing
  it states changes; no edit to it.
- Clock: `strftime('now')` has 1 s resolution, so two bumps in one second store equal stamps. Accepted: the field means
  "the last second a reminder happened", not an ordering of bumps (T6: no history).

## Steps

| Step | Change | Serves |
|---|---|---|
| S1 | `src/db.rs`: `SCHEMA_V3 = "ALTER TABLE lesson ADD COLUMN last_bypassed_at TEXT;"`, appended to `MIGRATIONS` (V1, V2 untouched) | T1 |
| S2 | `src/store.rs`: `Lesson.last_bypassed_at: Option<String>` with `#[serde(default)]`; add to `LESSON_SELECT_COLUMNS`, `lesson_from_row`, `copy_into` (column list, VALUES, ON CONFLICT) so `move` and sync apply keep it | T1, T5 |
| S3 | `bump_bypassed`: `SET bypassed = bypassed + 1, last_bypassed_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')` in the one UPDATE; `updated_at` not set; extend doc comment and journal line with the stamp | T2, T3 |
| S4 | `src/sync.rs`: `merge_both_present` adds `last_bypassed_at: ours.last_bypassed_at.clone().max(theirs.last_bypassed_at.clone())`; `fields_differ` untouched; fix the doc comments at `sync.rs:26,177,232` to name it | T4 |
| S5 | `src/main.rs`: in `lesson_as_text` (`main.rs` ~531-556), the ` (last <ts>)` text is appended after the `{} bypassed` text in the format string (`main.rs:555`), only when `Some` (D3); `run_bypassed` output unchanged | T5, D3 |
| S6 | Add `last_bypassed_at: None` to the fixtures listed above (compile fix only; the new field is required by S2) | T1, T5 |
| S7 | `README.md:67` one clause ("and when it was last bumped, as `last_bypassed_at`"). `skills/lessons/SKILL.md` dropped (premise above) | T5 |

## Tests

Fixed stamp strings in tests (`EARLIER = "2026-01-01T00:00:00Z"`, `LATER = "2026-06-01T00:00:00Z"`); never wall-clock equality.

| Check | Serves | Mutation that must turn it red |
|---|---|---|
| db: a v2 file (`SCHEMA_V1` + `SCHEMA_V2`, `user_version` 2, one row with `bypassed` = 3) opens, gains the column as NULL, the row keeps `bypassed` = 3, version equals `MIGRATIONS.len()` | T1 | drop S1's entry; make V3 `DROP`/recreate the column or add `DEFAULT` to a non-NULL value |
| db: bumping `last_bypassed_at` changes exactly 1 row (extend the `db.rs:538` test) | T1 | add `last_bypassed_at` to the `lesson_au` `UPDATE OF` list (`db.rs:308`), as #12 guarded for `bypassed` |
| store: fresh lesson has `None`; after a bump it is `Some`, matches `^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ$`, and is `>=` a `strftime` time read from the same connection just before the bump; `updated_at` (backdated) unchanged | T1, T2, T3 | omit it from the UPDATE; also set `updated_at`; write a non-ISO format |
| store: second bump keeps count 2 and a stamp `>=` the first (equal allowed, 1 s clock) | T2 | stamp only on the first bump (`COALESCE`) with a backdated stored value |
| store: `move_lesson` keeps a set stamp in the target | T1 | omit it from `copy_into` |
| store: JSON line without the key parses to `None`; with it round-trips; `null` round-trips | T1, T5 | remove the field's serde handling |
| sync: both set: `LATER` wins in both argument orders; one `None`, one set: set wins in both orders; both `None` stays `None` | T4 | take `ours` only; use `min` |
| sync: one-sided (ours-only, theirs-only) keeps a set stamp through `merge_lessons` | T4 | rebuild the one-sided row with `None` |
| sync back-to-back: the same MERGED applied twice writes nothing the second time; a stamp-only difference is "unchanged" in `fields_differ` (extend the test near `sync.rs:912`) | T4 | add a `MergedField` variant for it plus its `field_value` / `with_field` arms and a `MERGED_FIELDS` entry (compiles, then goes red) |
| mcp: `search_lessons` JSON for a bumped lesson has `"last_bypassed_at"` as a string; for a fresh one it is `null` | T5 | drop it from `LESSON_SELECT_COLUMNS`; `#[serde(skip)]` |
| cli: `show` text has ` (last <ts>)` after a bump and no `last` before; `show --format json` has the key (null then string) | T5, D3 | omit the S5 line |
| hook: `more_bypassed_ranks_first_and_equal_counts_keep_score_order` (`hook.rs:700`): give the equal-count hits distinct `last_bypassed_at` values, in an order opposite to score; they must still come out in score order | T6 | add `last_bypassed_at` to the sort key in `order_candidates` (`hook.rs:178-179`); the tie order flips and it goes red |

Race: not forced. The stamp and count share one `UPDATE` statement, executed under SQLite's write lock (`busy_timeout` 5 s,
`db.rs:20,90`), so a count and its stamp can never come from different bumps. Forcing an overlap is not deterministic;
the back-to-back bump test is the check. Gap: it would not catch a later split into two statements; the code comment at
`bump_bypassed` states why it is one. Back-to-back repeat: covered (two bumps; sync applied twice). Repeat after refusal:
unknown id returns `None` and stays refused. Non-determinism: the only clock input is `strftime('now')`; tests assert
format and `>=` a pre-bump read, never a literal or equality; merge tests use fixed strings.

## Provenance

- Twin: `bypassed` added in `cbc892e` (store, db), `a07fbed` (sync), `76245bc` (CLI), `7aa0442` (review fixes), all
  HidekiAI-WORK, 2026-11-18, unpushed on this branch; `bump_bypassed` `store.rs:257-271` NEW in `cbc892e`; `updated_at`
  max idiom `sync.rs:243` INHERITED (`95dee12`-era). The plan only adds beside them.
- Blame of the lines this changes (all HidekiAI-WORK): `copy_into` `store.rs:289-300` `51a69f2` 2026-11-10, with the
  `bypassed` column lines `cbc892e` 2026-11-18; `lesson_from_row` `store.rs:85-92` `5eee77f` 2026-11-10, `bypassed` line
  `cbc892e`; `merge_both_present` `sync.rs:238-246` `95dee12` 2026-11-11, `bypassed` line `a07fbed` 2026-11-18. All INHERITED;
  the plan adds one line beside each.
- Not searched: chat (any design talk outside the ticket). This repo has no Jira or Confluence.
- PRs: none (`gh pr list --state all --repo HidekiAI/llm-lessons` empty, as measured for #12).
- Issues: #17 (this); builds on #12. This repo has no Jira; every ticket is a GitHub issue.

## Possible follow-ups (not this ticket)

- Rank the hook by recency of `last_bypassed_at` (T6 defers it; the ticket says "not yet").
- A decay or "stale reminder" hint when the stamp is months old.
- Full per-bump history (explicitly out of scope).

## Plan gate

- 2026-11-18 iteration 1: 91/100 (Traceability 33, Coverage 24, Minimality 14, Grounding 10, Provenance 10). Passed at
  MIN_SCORE 90, no CRITICAL, nothing UNCOVERED. Fixes applied: (1) the `fields_differ` mutation now adds a `MergedField`
  variant with its arms; (2) the db trigger mutation is specific (`lesson_au` `UPDATE OF` list); (3) the named hook test
  and sort-key mutation for T6; (4) SKILL.md edit resolved: dropped, it states nothing this changes; (5) blame for
  `copy_into`, `lesson_from_row`, `merge_both_present` and the classes not searched; (6) migration test keeps a
  `bypassed` > 0 row; (7) old-binary limit relabelled MEASURED with `store.rs:29-30,59`; (8) S5 names `lesson_as_text`
  and the format string; (9) S6 and S7 carry T tags.

## Progress

- 2026-11-18: S1, S2, S3 and S6 (migration V3, the field, the stamp in `bump_bypassed`, fixtures) done; every planned db
  and store mutation went red. S4 (sync) done. S5 (show text), the MCP, CLI and hook rank tests, and S7 (README) done; every mutation red. Next: independent self-review.
- 2026-11-18 12:29, added by @me mid-task: "can we at least mention on README.md (FAQ part?) about what or why we don't
  need to index the sqlite". Done as its own docs commit on this branch.
- 2026-11-18 12:36, added by @me mid-task: one more FAQ, "Won't the prompt get bigger if the entire sqlite rows were
  cached?". Its own docs commit on this branch.
