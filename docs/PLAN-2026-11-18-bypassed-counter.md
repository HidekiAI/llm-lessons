# PLAN 2026-11-18: bypassed counter (issue #12)

TL;DR: add a per-lesson `bypassed` counter, the twin of `hits`. `lessons bypassed <id>` (alias `remind`) adds one
when @me had to remind the agent of a rule it already had. It syncs by max, survives `move`, shows in `show` and JSON,
and the hook ranks by it (repo boost, then `bypassed` descending, then score). Nothing bumps it automatically. Detail below.

## Requirements

From issue #12 (title: "feat: add a reminders counter to lessons in all three DBs") and its two comments:

- T1 Each lesson in all three DBs (`work`, `personal`, `shared`) carries a reminder counter, default 0.
- T2 The counter changes ONLY when the user caught the agent ("I already mentioned this once", "I thought we agreed
  that"); a rule the agent simply followed does not count. "no hook or agent auto-bump, it's only when user gets frustrated".
- T3 Rationale: the count is a context-size signal (a session that needs reminding should be compacted or restarted)
  and keeps the reader reading generated output.
- T4 "The count appears in show text and in the JSON and MCP output" (the clause the judge cites for output).

Decided with @me, 2026-11-18 (count as ticket comments):

- D1 "column: 'bypassed', but both `lessons bypassed|remind <id>` (because remind is better for syntax, but bypassed is
  better for what it really DID)". So column `bypassed INTEGER NOT NULL DEFAULT 0`, subcommand `bypassed`, clap alias `remind`.
- D2 The agent runs it only when the user explicitly asks to count a reminder, naming the id; never on its own guess.
  The skill says so. No MCP tool bumps it (MEASURED: `src/mcp.rs` has no `bump_hits` call, `grep bump_hits src/mcp.rs` is empty).
- D3 Ranking in this change: hook order is repo boost, then `bypassed` descending, then FTS score, only among
  candidates that already passed `MIN_SHARED_WEIGHT` (`src/hook.rs:66`). A stable sort keeps score order within equal counts.
- D4 The assistant writes the code; no `TODO(human)` blocks.

Premises, MEASURED unless marked:

- `Lesson` derives `Deserialize` with no `deny_unknown_fields` (`src/store.rs:29-30`; the deny is on `NewLesson`, line 56).
  So an old binary reads a new `.jsonl` line, ignores `bypassed`, and drops it on its next rewrite. Known limit: the newer
  host restores it by max-merge at its next sync, unless every host that held the larger count also lost it (UNVERIFIED, not worth guarding).
- `#[serde(default)]` on the new field is what lets a pre-change `.jsonl` line (no key) parse as 0.
- `MIGRATIONS` has one entry (`src/db.rs:185`); `migrate` applies missing entries in one IMMEDIATE transaction (`db.rs:211-250`).
- All three domain files migrate via the shared `open` (`db.rs:74`, which runs `migrate`; `open_target` `db.rs:120-129` and
  `open_existing_domains` `db.rs:134-143` both call it): MEASURED by reading; no test opens all three.
- `lesson_au` fires only `UPDATE OF` the seven indexed columns (`db.rs:308`), so a new column never reindexes.
- Increment is one statement (`UPDATE ... SET x = x + 1 ... RETURNING`, as `store.rs:244`); `busy_timeout` is 5 s (`db.rs:20,90`).
- Accepted limit (D3): `search_all` cuts candidates to `max_results * BOOST_POOL_FACTOR` (`hook.rs:30`, used at `hook.rs:63`)
  by score BEFORE the `bypassed` ordering, so a high-`bypassed` lesson outside that pool is never promoted. MEASURED; accepted,
  because D3 orders only candidates that already passed the filter.
- A one-sided lesson (ours-only or theirs-only) is cloned whole by `merge_lessons` (`sync.rs:291-293`), so it keeps the count today;
  the test pins that against a later field-by-field rewrite.

## Steps

| Step | Change | Serves | Done when |
|---|---|---|---|
| S1 | `src/db.rs`: `SCHEMA_V2 = "ALTER TABLE lesson ADD COLUMN bypassed INTEGER NOT NULL DEFAULT 0;"`, appended to `MIGRATIONS` (never edit V1) | T1 | `cargo test db::` green |
| S2 | `src/store.rs`: `Lesson.bypassed: i64` with `#[serde(default)]`; add to `LESSON_SELECT_COLUMNS`, `lesson_from_row`, and the `copy_into` column list, VALUES and ON CONFLICT (so `move` and sync apply keep it) | T1, T4 | `cargo build` |
| S3 | `store::bump_bypassed(conn, id) -> Result<Option<i64>>`, clone of `bump_hits`: `UPDATE lesson SET bypassed = bypassed + 1 WHERE id = ?1 RETURNING bypassed`, no `updated_at` change, `journal!("bypassed {id}: now {n:?}")` | T2, D1 | unit tests below |
| S4 | `src/sync.rs`: `merge_both_present` sets `bypassed: ours.bypassed.max(theirs.bypassed)`; `fields_differ` untouched (already ignores non-merged fields); fix doc comments naming `hits` | T1 | sync tests below |
| S5 | `src/main.rs`: `Command::Bypassed { id }` with `#[command(alias = "remind")]` (D1), `run_bypassed` cloned from `run_hit`, prints `bypassed {id} [domain]: N bypassed`; unknown id fails like `hit` (`lesson_not_found`); `show` text adds `, {} bypassed` after hits (`main.rs:551`) | T1, T2, T4, D1 | CLI tests below |
| S6 | `src/hook.rs`: in `boost_current_repo` (line 163) add a stable `sort_by_key(\|hit\| Reverse(hit.lesson.bypassed))` on each partition, before the `extend`/`truncate`; rename the doc comment to the three-level order | D3 | hook tests below |
| S7 | Fix every `Lesson { .. }` literal that now needs the field (`hook.rs:626`, `sync.rs:517`, `store.rs:1101,1160,1221,1371,1389`); no MCP code change: `search` is serde of `SearchHit` (`mcp.rs:343-365`), so JSON gets the field by S2 | T1, T4 | `cargo test` compiles |
| S8 | `README.md` (next to `lessons hit`, line 58) and `skills/lessons/SKILL.md` (step 4, line 18): command, alias, and D2 wording: run only when the user asks to count a reminder, naming the id | D2, D1, T2 | grep finds both |

## Tests

Counts are named constants (`BUMP_COUNT`, `SYNC_BYPASSED`, `MORE_BYPASSED`, `FEW_BYPASSED`), like `SYNC_HITS` (`store.rs:1083`).
Layer: unit for store, sync, hook, db and mcp (in-source, as the repo does); one CLI test for the wire.

| Check (layer) | Serves | Mutation that must turn it red |
|---|---|---|
| db: a v1 file (made with `SCHEMA_V1`, `user_version` 1, one row) opens, gains the column, row reads 0, version equals `MIGRATIONS.len()` | T1 | drop S1's entry |
| db: bumping `bypassed` changes exactly 1 row (no reindex), extending the `hits` test at `db.rs:490` | T1 | make the trigger fire on all columns |
| store: `bump_bypassed` returns 1, 2 for `BUMP_COUNT` bumps, leaves a backdated `updated_at`, leaves `hits` at 0; unknown id gives `None` | T2, D1 | bump `hits` instead; set `updated_at` |
| store: `move_lesson` keeps a non-zero count in the target | T1 | omit it from `copy_into` |
| store: a JSON line without `bypassed` parses to 0; a line with it round-trips | T1 | remove `#[serde(default)]` |
| sync: both present, larger count wins in both argument orders; `fields_differ`/unchanged ignores a bypassed-only difference | T1 | use `ours` only; add it to `MERGED_FIELDS` |
| sync one-sided: a lesson only in ours, and one only in theirs (`sync.rs:291`), each keeps a non-zero `SYNC_BYPASSED` through `merge_lessons` | T1 | rebuild the one-sided row with `bypassed: 0` |
| sync back-to-back: applying the same MERGED twice writes nothing the second time and the count stays `SYNC_BYPASSED`, not twice it | T1 | sum the two sides |
| hook: three or more candidates with equal counts keep score order, and a group with higher count ranks first; repo match still beats a higher count; a low-`bypassed` candidate below `MIN_SHARED_WEIGHT` is never promoted | D3 | reverse the tie order; sort ascending among ties; sort before the relevance filter; drop the repo-first partition |
| mcp: `search_lessons` JSON for a lesson bumped `BUMP_COUNT` times carries `"bypassed"` equal to it (mirror the search test near `mcp.rs:634`) | T4 | drop `bypassed` from `LESSON_SELECT_COLUMNS` or add `#[serde(skip)]` to the field |
| cli: `bypassed <id>` then `remind <id>` prints `2 bypassed`; `show --format json` has `"bypassed": 2`; text `show` has `2 bypassed`; unknown id exits non-zero with the same message as `hit` | T4, T2, D1 | drop the alias; skip the JSON field |

Race: not forced. Two processes bumping one id run the single `UPDATE ... bypassed + 1` statement, which SQLite executes
under its write lock, waiting up to `busy_timeout` (5 s) instead of failing, so no lost update is reachable (T2 count stays exact).
A test forcing the overlap cannot be made deterministic without hooking SQLite; the back-to-back bump test above is the check.
Known gap: it would not catch a later rewrite into read-then-write; the code comment at `bump_bypassed` states why it is one statement.
Back-to-back repeat: covered (2 bumps give 2; sync applied twice keeps the value). Repeat after refusal: an unknown id
refuses and stays refused; nothing clears that cause. Non-determinism: none (no clock or random input; `updated_at` is backdated).

## Provenance

- Twin code: `hits` field `src/store.rs:43` and `bump_hits` 239-250 (`5eee77f`, `dc67b26`, HidekiAI-WORK, 2026-11-10);
  `sync.rs:241` (`95dee12`, 2026-11-11); `db.rs:185,283,307` (`4816385`, 2026-11-10); `run_hit` `main.rs:672-679`
  (`dc67b26`); `boost_current_repo` `hook.rs:161-180` (`e8bed74`). All INHERITED; the plan only adds beside them.
- PRs: none (MEASURED: `gh pr list --state all --repo HidekiAI/llm-lessons` is empty).
- Issues: #12 (this). #11 (retire without a replacement id) also edits `store.rs` (`retire`, line 253), a different
  function, so only a trivial merge conflict if both land. Others (#10 release bundle) do not touch these files.
- Not searched: Jira, Confluence, chat, any design doc beyond `docs/`.

## Possible follow-ups (not this ticket)

- A `--reset` or decrement for a miscounted bump (no ticket line asks for one).
- A surfaced "bypassed >= N, consider compacting" hint (T3 rationale, but the ticket sets no threshold).
- `lessons list --sort bypassed` (not requested).

## Plan gate

- 2026-11-18 iteration 1: 91/100 (Traceability 32, Coverage 23, Minimality 15, Grounding 9, Provenance 12). Passed at
  MIN_SCORE 90, no CRITICAL. Refinements applied: (1) an MCP `search_lessons` test row tagged T4; (2) a sync one-sided
  (ours-only, theirs-only) row with a mutation; (3) the hook mutation changed to "reverse the tie order" and the test
  given several equal-count ties; (4) the `search_all` pool truncation (`hook.rs:30,63`) labelled an accepted limit.
  Also: a T or D tag on every test row and on the docs step, and the all-three-domains migration via `open` labelled MEASURED.
