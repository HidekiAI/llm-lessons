# PLAN 2026-11-26: prime-directive lessons always returned (issue #37)

TL;DR: add one per-row `prime_directive` flag, set and cleared only by `lessons update`, and return those lessons in all three read paths (prompt hook, CLI search, MCP search) before keyword hits and outside every limit and character cap. No sentinel, no new table, no config knob. Detail below.

This document is the execution ledger for [issue #37](https://github.com/HidekiAI/llm-lessons/issues/37) and is PLANNED only: per @me, two decision comments are posted on the issue ([the 2026-11-26 decision comment](https://github.com/HidekiAI/llm-lessons/issues/37#issuecomment-7197080507) and [the sync decision comment](https://github.com/HidekiAI/llm-lessons/issues/37#issuecomment-7197223200)), but no code, no branch and no code write has been made for it. MEASURED: `gh issue view 37 -R HidekiAI/llm-lessons --json comments` returns 2 after posting. All source paths below are measured in `/usr/src/github/mine/hidekiai/llm-lessons`. This plan replaces the issue's suggested `always_required` name with `prime_directive`, per the decision comment cited below.

## Requirements

From issue #37 (`gh issue view 37 -R HidekiAI/llm-lessons`, 2026-11-26; OPEN, `Difficulty: L`; MEASURED `comments: 0` at the pre-posting read):

- R1 "A marked lesson appears in every search result."
- R2 "It appears when the query shares no words with it."
- R3 "It appears in addition to the `--limit` keyword hits."
- R4 "It respects the domains the caller asked for."
- R5 "It stays absent when the lesson is retired."
- Ticket problem statements: hook keyword gate `src/hook.rs:67` and `src/hook.rs:87`; CLI cut `src/main.rs:752` and `src/store.rs:464`; MCP cut `src/mcp.rs:361`.
- Ticket open questions resolved by the decisions below: no count against `--limit`; yes even with no searchable words; setter is `lessons update` only, not `lessons add`.

Decisions recorded in [the 2026-11-26 decision comment on issue #37](https://github.com/HidekiAI/llm-lessons/issues/37#issuecomment-7197080507), authoritative over the issue's options:

- D1 Name the flag `prime_directive`, not `always_required`. Serves the ticket TL;DR line recommending `always_required` and [the decision comment](https://github.com/HidekiAI/llm-lessons/issues/37#issuecomment-7197080507): "prime directive is singular, or in some cases, less than handful (2 or 3 at most?)". Hence one boolean per row, not a separate ranking or a new table.
- D2 All three read paths inject the marked lessons: prompt hook (`user-prompt-submit`), MCP `search_lessons`, CLI `lessons search`.
- D3 No `bypassed = -1` sentinel and no separate `inquired` counter. Inquiries and reminders both bump the existing `bypassed`, which already stamps `last_bypassed_at` in the same UPDATE. Serves the ticket's option 1 (`bypassed = -1`) and [the decision comment](https://github.com/HidekiAI/llm-lessons/issues/37#issuecomment-7197080507) rejecting it.
- D4 Set/clear only with `lessons update <id> --prime-directive` / `--no-prime-directive`. `lessons add` is untouched (answers the issue's setter question).
- D5 Marked lessons are prepended before keyword hits, are exempt from `--limit`, `max_results`, and the hook's `MAX_OUTPUT_CHARS` cap; retired lessons stay excluded; requested domains are respected.
- D6 @me-authorized docs/rules scope: an inquiry about an existing lesson and a self-noticed slip both get a proposed bump naming the lesson id; silence on @me's next message counts as yes; an explicit no prevents it. This applies to `skills/lessons/SKILL.md` and the global agent rules, and is not a scope growth of issue #37's code work. Source: [the 2026-11-26 decision comment on issue #37](https://github.com/HidekiAI/llm-lessons/issues/37#issuecomment-7197080507), not the issue body.
- D7 No config cap and no new MCP tool field: the set is expected to stay tiny (D1), so a knob nothing reads is not added. Serves [the decision comment](https://github.com/HidekiAI/llm-lessons/issues/37#issuecomment-7197080507) and D1's "handful" wording.

## Standing constraints

Obeyed by every step; they are not re-decided per step.

- Work happens on a new branch, never on `trunk`/`main`/`master`; commits are Conventional Commits; ask once before any push, then push by explicit remote name and merge non-squash (`git merge --ff-only`). No code write or pull-request write is part of this planning task. The two decision comments are its only GitHub writes (MEASURED: `gh issue view 37 -R HidekiAI/llm-lessons --json comments` returns 2, as on line 5). @me separately decided to commit this gated document on a docs branch, push it and merge it back; that planning-artifact commit is recorded in the Plan gate section and is not a code change.
- Preplanning is done: the design below names the concrete functions and types it extends and reconciles them with their documented intent. A step that finds a conflict with those contracts stops and records it instead of widening scope.
- A step fixes only its own objective. Out-of-scope discoveries go to the follow-up list at the bottom, not into the diff.
- A step finishes with a mechanical check (exit code, count, or diff) and persists its own result in its `## Step N result` block before the next step starts. Result blocks are append-only.
- Rust unit tests stay in-source (`#[cfg(test)] mod tests`); CLI and MCP/hook cross-process cases stay in `tests/cli.rs`, `src/mcp.rs`'s test module, and `src/hook.rs`'s test module, matching the existing layout. No new test framework.
- New named constants must keep `cargo test --test no_magic_numbers` green (MEASURED: `tests/no_magic_numbers.rs:1-14`, command run 2026-11-26: `2 passed; 0 failed`).
- Every block of logic touched gets a gist comment above it; every persisted-artifact or multi-step flow keeps journal-grade log lines with inputs and outcomes, as these modules already do (`db.rs:87,114,240-253`, `store.rs:128,242,256,273,285,443`, `hook.rs:49,77`).
- The session checkpoint carries one pointer line, `Current step: S<N> — <objective>`, never a second progress table.

## Premises

Every factual claim about this repo is labelled. Scan this whole document before running the plan gate: any new claim added later must carry a label too.

- MEASURED: the issue text quoted under Requirements and its metadata come from `gh issue view 37 -R HidekiAI/llm-lessons` (2026-11-26); `comments: 0` at that read, before the decision comment was posted. MEASURED: after posting, `gh issue view 37 -R HidekiAI/llm-lessons --json comments` returns 2, at https://github.com/HidekiAI/llm-lessons/issues/37#issuecomment-7197080507 and https://github.com/HidekiAI/llm-lessons/issues/37#issuecomment-7197223200.
- MEASURED: `MIGRATIONS = &[SCHEMA_V1, SCHEMA_V2, SCHEMA_V3]` (`src/db.rs:185`); `migrate` takes an IMMEDIATE transaction, applies missing entries in order and sets `user_version` once (`src/db.rs:187-257`); V3 is one `ALTER TABLE` (`src/db.rs:325-328`). V4 appends; V1-V3 are never edited.
- MEASURED: `lesson_au` fires only `UPDATE OF` the seven FTS columns (`src/db.rs:307-317`), with the same deliberate reason stated for `bypassed` and `last_bypassed_at` (`src/db.rs:320,325`). A flag UPDATE therefore does not reindex.
- MEASURED: `Lesson` is serde `Serialize, Deserialize` with the LLM-facing JSON names and `#[serde(default)]` on `bypassed` for old `.jsonl` lines (`src/store.rs:28-49`). A bool with `#[serde(default)]` follows the same precedent; an old line without the key reads `false`.
- MEASURED: rusqlite 0.41.1 (`Cargo.lock:459-461`) implements `FromSql for bool` as `i64 != 0` (`~/.cargo/registry/src/*/rusqlite-0.41.1/src/types/from_sql.rs:194-199`) and `ToSql for bool` (`to_sql.rs:105,215`), so `INTEGER NOT NULL DEFAULT 0` maps to `bool` with no hand-written converter.
- MEASURED: the DAO owns every SELECT column list: `LESSON_SELECT_COLUMNS` (`src/store.rs:74-75`), `lesson_from_row` (`src/store.rs:77-95`), `update`/`LessonPatch` (`src/store.rs:181-244`), `bump_bypassed` (`src/store.rs:260-275`), `copy_into` used by `move` and sync apply (`src/store.rs:289-323`), `search`/`search_all` (`src/store.rs:406-466`). Adding a column touches these and no command-level SQL.
- MEASURED: `search_all` sorts by bm25 score and truncates to `limit` (`src/store.rs:462-465`); injection must happen at the callers, after that cut, to satisfy R3.
- MEASURED: the hook builds an FTS query and returns `Vec::new()` when there are no usable words (`src/hook.rs:47-76`); `MIN_SHARED_WEIGHT` is 4 with an identifier weighted 4 and an ordinary word 1 (`src/hook.rs:81-91`); `order_candidates` partitions by current repo, sorts by `bypassed`, then `.take(max_results)` (`src/hook.rs:162-189`); `render` keeps whole blocks only while they fit `MAX_OUTPUT_CHARS = 8_000` (`src/hook.rs:20-31,191-243`). R2 needs marked lessons to bypass all three gates.
- MEASURED: CLI `--limit` defaults to `SEARCH_DEFAULT_LIMIT = 10` (`src/main.rs:22-24,88-89`) and is applied in `run_search` (`src/main.rs:730-767`); MCP `limit` defaults to `mcp_config.max_results` and is applied in `search_lessons` (`src/mcp.rs:343-365`); MCP refuses domains outside `[mcp] domains` (`src/mcp.rs:328-340`), so injecting from the already-opened domain list preserves R4.
- MEASURED: `Command::Update` flattens `FieldArgs` (`src/main.rs:69-74,177-237`), `FieldArgs::into_patch` builds `LessonPatch` (`src/main.rs:206-218`), `run_update` calls `store::update` (`src/main.rs:670-679`), and `lesson_as_text` prints a fixed list of optional fields plus the counters (`src/main.rs:531-563`). The new flags belong on `Command::Update`, not on the shared `FieldArgs`, to keep `add` untouched (D4).
- MEASURED: `MergedField` is an exhaustive enum with append-at-end ordering (`src/sync.rs:26-55`), with exhaustive `field_value` and `with_field` arms (`src/sync.rs:98-156`), `fields_differ` ignoring counters (`src/sync.rs:178-185`) and `merge_both_present` handling counters separately (`src/sync.rs:234-267`). A new flag is a merged field, not a counter, so a local clear can propagate on sync — DECIDED (2026-11-26) in [the sync decision comment](https://github.com/HidekiAI/llm-lessons/issues/37#issuecomment-7197223200), not an open question.
- MEASURED: `Lesson { ... }` literals are spread over `src/store.rs` (26), `src/sync.rs` (42), `src/hook.rs` (7), `src/sync/cycle.rs` (4), `src/main.rs` (2), `src/sync/files.rs` (2), `src/sync/run.rs` (2), `src/mcp.rs` (1), `tests/` (0), from `grep -rn -E 'Lesson \{' --include='*.rs' src tests` on 2026-11-26. The compiler finds every one after the struct grows; fixtures without a base row get `prime_directive: false`, fixtures with `..base` need no edit.
- MEASURED: baseline `cargo test --bin lessons -- --list | grep -c ': test$'` is 282 tests (`db` 14, `store` 33, `sync` 163, `hook` 22, `mcp` 17), measured 2026-11-26. Final counts are recorded, not predicted.
- MEASURED: `skills/lessons/SKILL.md:20-23` in this repo still says to run `bypassed` only when asked and omits the self-notice exception; the installed global skill has the exception (`~/scripts/LLM/skills/lessons/SKILL.md:20-25`) and the global rules have it at `~/scripts/LLM/AGENTS.global.md:342-347` and `~/scripts/LLM/rules/default.md:427-432`. D6 extends both to the inquiry case.
- MEASURED: `gh pr list --state all -R HidekiAI/llm-lessons` printed no PRs on 2026-11-26; `gh issue list -R HidekiAI/llm-lessons --state all --limit 60` lists #37 and related issues named in Provenance.
- UNVERIFIED: D1's "handful" does not state a numeric cap, so none is introduced (D7). The design assumes marked rows stay few; if that changes, the scan ceiling in the Technical design is the upgrade trigger.
- DECIDED (2026-11-26, [the sync decision comment](https://github.com/HidekiAI/llm-lessons/issues/37#issuecomment-7197223200)): an explicit clear on one machine propagates on the next sync. Implementation: `prime_directive` goes in `MergedField` (three-way, both sides' explicit changes win, deterministic tie-break by the existing `newer_or_larger`), because a plain OR would make a local clear impossible to propagate. No question remains open.
- UNVERIFIED: no measurement of the host's claimed 10,000-character hook-output replacement (README:124) was made on any agent; the cap bypass in D5 is a deliberate ceiling, documented with a `ponytail:` comment as required.
- UNVERIFIED: install state of the hook, MCP server and CLI on @me's machines; only their source paths were checked. This plan changes all three code paths regardless of which are installed.

## Technical design

Every factual statement in this section is MEASURED at the file:line cited beside it or in Premises; statements about the new code are proposals. Anything inferred rather than measured is marked UNVERIFIED in place.

### Column and migration

Append exactly one migration; do not edit V1-V3:

```rust
/// Not an indexed column, so `lesson_au` does not fire when the mark changes, as for `bypassed`.
const SCHEMA_V4: &str = r#"
ALTER TABLE lesson ADD COLUMN prime_directive INTEGER NOT NULL DEFAULT 0;
"#;

const MIGRATIONS: &[&str] = &[SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4];
```

The stored value is 0/1 through rusqlite's bool conversion (Premises). No partial index: marked retrieval scans the small `lesson` table, exactly as `list` scans for tags today (`src/store.rs:468-474`, README:309 names the same trade-off). `ponytail:` ceiling: if that scan becomes measurably slow at a few thousand rows, add `CREATE INDEX lesson_prime_directive ON lesson(prime_directive) WHERE prime_directive = 1` as a new migration, not in V4.

### Data structures and ownership

- `src/db.rs` owns the column; `src/store.rs` owns all SQL that reads or writes it (Premises). No command builds SQL.
- `Lesson.prime_directive: bool` with `#[serde(default)]` keeps old `.jsonl` lines and JSON without the key valid.
- `LessonPatch.prime_directive: Option<bool>`: `None` leaves the flag as-is, `Some(true)`/`Some(false)` sets or clears. It joins the "nothing named" guard in `store::update` and one extra `CASE` arm in the existing static UPDATE:

```rust
// One static statement: a NULL parameter keeps the column, a bool replaces it.
prime_directive = CASE WHEN ?10 IS NULL THEN prime_directive ELSE ?10 END,
```

- `Command::Update` gains two conflicting bool flags; the patch value is one drilled expression:

```rust
let prime_directive_patch: Option<bool> = match (prime_directive, no_prime_directive) {
    (true, _) => Some(true),
    (false, true) => Some(false),
    (false, false) => None,
};
```

- `SearchHit` is reused unchanged for injected rows. A named, finite score keeps serde_json from writing `null` for a non-finite float:

```rust
/// Injected rows are prepended, never sorted against bm25 scores; finite so JSON stays a number.
pub const PRIME_DIRECTIVE_SCORE: f64 = 0.0;
```

### Retrieval and combination algorithm

New DAO function in `store.rs`, called by all three paths with the same `opened` list they already build:

```rust
// Standing lessons: no FTS, no keyword gate, no caller limit; retired rows stay hidden.
pub fn prime_directive_hits(opened: &[(Domain, Connection)]) -> Result<Vec<SearchHit>> {
    opened
        .iter()
        .map(|(domain, conn)| {
            let mut mut_statement = conn.prepare(&format!(
                "SELECT {qualified_columns}, ?1 AS score FROM lesson l \
                 WHERE l.prime_directive = 1 AND l.superseded_by IS NULL \
                 ORDER BY l.created_at, l.seq"
            ))?;
            mut_statement
                .query_map([PRIME_DIRECTIVE_SCORE], |row| Ok(SearchHit {
                    domain: domain.as_str(),
                    score: row.get("score")?,
                    lesson: lesson_from_row(row)?,
                }))?
                .collect::<rusqlite::Result<Vec<SearchHit>>>()
                .map_err(anyhow::Error::from)
        })
        .collect::<Result<Vec<Vec<SearchHit>>>>()
        .map(|per_file| per_file.into_iter().flatten().collect())
}

// One expression: marked rows first, keyword rows that are not already marked after them.
pub fn with_prime_directive(marked: Vec<SearchHit>, keyword: Vec<SearchHit>) -> Vec<SearchHit> {
    let marked_ids: BTreeSet<&str> = marked.iter().map(|hit| hit.lesson.id.as_str()).collect();
    marked
        .into_iter()
        .chain(
            keyword
                .into_iter()
                .filter(|hit| !marked_ids.contains(hit.lesson.id.as_str())),
        )
        .collect()
}
```

`qualified_columns` is the same alias list `search` builds at `src/store.rs:415-419`; reuse that construction rather than a second literal. Deterministic order comes from `opened` (caller domain order), then `created_at, seq`; no clock or randomness.

Call-site changes, each after the existing limit/cut:

- `run_search` (`src/main.rs:749-766`): open domains once, compute `marked`, compute keyword hits via the existing `search_all` (also in the `fts_query = None` branch), then print `with_prime_directive(marked, keyword)`.
- `search_lessons` (`src/mcp.rs:355-365`): same, after `refuse_unlisted` and with the already-opened domain list; no schema change.
- `user_prompt_submit` (`src/hook.rs:43-79`): open readable domains once before the `match`, compute `marked`; filter keyword candidates as today, run `order_candidates(...).take(max_results)`, then prepend `marked`. `render` gains a `marked_count: usize` parameter and keeps the first `marked_count` blocks regardless of the character budget:

```rust
// Marked blocks bypass the cap; keyword blocks keep the whole-block fold.
if block_index < marked_count || chars_used + block_chars <= MAX_OUTPUT_CHARS { ... }
```

`ponytail:` the cap bypass is global, not per-lesson; its ceiling is the agent's replacement threshold, and a per-marked budget is the upgrade path if marked text ever approaches it.

Complexity: with `M` marked rows and `K` keyword hits (`K <= --limit` or `max_results`), the added work is `O(D * M)` for the scans plus `O(M + K)` for dedupe; FTS ranking stays unchanged at `O(log N + K)`. Memory is `O(M + K)`. UNVERIFIED: the bound `D <= 3` files is the count of the opened-domain list, not a measured constant; the algorithm is correct for any `D`.

### Sync

Add `MergedField::PrimeDirective` after `SupersededBy` (append, so the conflict-file sort order of existing entries is unchanged, `src/sync.rs:29-30`) and grow `MERGED_FIELDS` to 10 entries. Named wire tokens on the type that owns them, with a pin test:

```rust
pub const PRIME_DIRECTIVE_SET: &str = "1";
pub const PRIME_DIRECTIVE_CLEAR: &str = "0";
```

- `field_value`: `Some(if lesson.prime_directive { PRIME_DIRECTIVE_SET } else { PRIME_DIRECTIVE_CLEAR })`.
- `with_field`: `prime_directive: value.as_deref() == Some(PRIME_DIRECTIVE_SET)`; an unknown token fails closed to false.
- `fields_differ` now includes the flag, so a flag-only edit is an edit and sync apply writes it; there is no separate counter rule to edit.
- DECIDED (2026-11-26, [the sync decision comment](https://github.com/HidekiAI/llm-lessons/issues/37#issuecomment-7197223200)): an explicit clear propagates on next sync through this three-way merge; no sticky OR.
- UNVERIFIED (derived from the two-valued domain, pinned by the S7 test rather than run yet): two differently changed sides always coincide, so `merge_field` settles without a queue entry in those cases, and a both-sides-unchanged row keeps the flag.

### Docs and rules

- Repo `README.md`: command list (~line 76-82), the `bypassed` paragraph (~87), the hook paragraph (~124), the search-surface table (~318-319), and the no-index FAQ (~309) gain one clause each: marked lessons are returned first by CLI, MCP and hook, outside limits; the flag is set/cleared with `lessons update`.
- Repo `skills/lessons/SKILL.md:12-23`: say marked lessons appear on every search; align the `bypassed` rule with the installed global skill and D6 (inquiry or self-noticed slip: propose the bump with the lesson id, silence next message = yes, explicit no stops it).
- Global rules (`~/scripts/LLM/AGENTS.global.md:336-347`, `~/scripts/LLM/rules/default.md:421-432`, `~/scripts/LLM/skills/lessons/SKILL.md:20-25`): add the inquiry case beside the existing self-correction exception. This is a separate git repository; its commit is separate from the code commits.

## STEP LEDGER

| step | status | evidence | fingerprints |
| --- | --- | --- | --- |
| S0 | PLANNED | - | - |
| S1 | PLANNED | - | - |
| S2 | PLANNED | - | - |
| S3 | PLANNED | - | - |
| S4 | PLANNED | - | - |
| S5 | PLANNED | - | - |
| S6 | PLANNED | - | - |
| S7 | PLANNED | - | - |
| S8 | PLANNED | - | - |
| S9 | PLANNED | - | - |

`fingerprints` record the commit subject plus short hash for each completed step; `evidence` records the exact commands and their exit codes/counts. Do not predict either field.

## Step S0 result — fixture: ledger and baseline

- Serves ticket R1-R5 (the whole expected result); the ledger is the record of those five lines, not a new behaviour.
- `Read first`: this plan; `git -C /usr/src/github/mine/hidekiai/llm-lessons status --short`; `git -C ... ls-files docs/PLAN-2026-11-26-prime-directive.md`.
- `Do`: use the gated plan already committed as `docs/PLAN-2026-11-26-prime-directive.md` on a docs branch by the planning task as the execution ledger, and write `Current step: S1 — schema and DAO plumbing` in the session checkpoint. Do not copy or re-create the plan (minimality refinement of the 59/100 gate). UNVERIFIED at revision time: that commit is not yet present.
- `Done when`: `git ls-files docs/PLAN-2026-11-26-prime-directive.md` prints the path; `git status --short` is clean; the checkpoint pointer is written.
- `Expected numbers`: baseline binary test count 282 (MEASURED above); `cargo test --test no_magic_numbers` 2 passed; record both in S0 result.
- `STOP if`: the ledger file is missing or the working tree is dirty at start — stage and commit pre-existing owned work first, or stop and ask; never include someone else's untracked files.

## Step S1 result — schema, field and fixtures (D1)

- `Read first`: `src/db.rs:183-257,307-328,341-531`; `src/store.rs:28-95,289-323`; the Premises on rusqlite bool conversion.
- `Do`: append `SCHEMA_V4` and extend `MIGRATIONS`; add `Lesson.prime_directive: bool` with `#[serde(default)]`, add the column to `LESSON_SELECT_COLUMNS`, `lesson_from_row` and `copy_into`; add `prime_directive: false` to `Lesson` fixtures without a base row. Leave `lesson_au` and V1-V3 untouched.
- `Done when`: `cargo test --bin lessons -- --list | grep -c ': test$'` is greater than 282; the named db/store/move tests in the test matrix pass; `cargo test --test no_magic_numbers` exits 0.
- `Expected numbers`: `MIGRATIONS.len() == 4`; a V3 file opens with the new column at 0 and keeps its old `bypassed` count; bumping the flag changes exactly 1 row plus zero trigger rows.
- `STOP if`: a fixture cannot be given a value without changing behaviour, or the trigger fires on the flag UPDATE — stop and record, do not widen `lesson_au`.

## Step S2 result — set/clear, patch SQL and show (D4)

- `Read first`: `src/main.rs:69-74,177-237,531-563,670-679`; `src/store.rs:181-244`.
- `Do`: add the two conflicting flags to `Command::Update`, build `Option<bool>` as in the design, extend `LessonPatch` and the "nothing named" guard and static UPDATE, add a `prime directive` line to `lesson_as_text` only when true, and extend the journal line in `store::update` with the flag value.
- `Done when`: the CLI E2E cases in the test matrix pass; `lessons add --prime-directive` exits non-zero with a clap usage error; `cargo test --test no_magic_numbers` exits 0.
- `Expected numbers`: two flags, one patch value, one extra CASE arm; no new subcommand; `add` argument count unchanged.
- `STOP if`: clap reports an ambiguous flag or `store::update` would need dynamic SQL — stop; the statement stays static.

## Step S3 result — standing retrieval in the DAO (R1-R5)

- `Read first`: `src/store.rs:397-466`; the design snippet above.
- `Do`: add `PRIME_DIRECTIVE_SCORE`, `prime_directive_hits` and `with_prime_directive` in `store.rs`, reusing the column-alias construction from `search`; add the unit tests from the matrix.
- `Done when`: those unit tests pass and each named mutation reddens its assertion; no keyword FTS query is issued by `prime_directive_hits`.
- `Expected numbers`: one query per opened file; zero hits for a retired marked row; one hit for a marked row regardless of query text.
- `STOP if`: the helper needs its own SQL column list — reuse `LESSON_SELECT_COLUMNS`; if that is impossible, stop and record the divergence.

## Step S4 result — CLI search (R1, R3, R4, R5)

- `Read first`: `src/main.rs:730-767`; `tests/cli.rs:516-628`.
- `Do`: open domains once, inject marked hits after the existing `search_all` call in both `fts_query` branches, journal the marked count and the final hit count.
- `Done when`: the CLI E2E cases in the matrix pass, including no-searchable-words text and `--in` filtering.
- `Expected numbers`: marked hits are extra to `--limit`, not counted in it; with limit 1 and 2 keyword matches plus 1 marked, output has 2 hits.
- `STOP if`: a marked lesson appears twice or a retired one appears — that is R1/R5, stop and fix in the DAO helper, not at the call site.

## Step S5 result — MCP search (R1, R3, R4, R5)

- `Read first`: `src/mcp.rs:215-229,328-365,600-694`; `src/main.rs:759-765` for the shared JSON shape.
- `Do`: inject after `search_all` in `search_lessons`, from the same opened domains; journal counts; do not change the tool schema or `refuse_unlisted`.
- `Done when`: the MCP cases in the matrix pass; `tools/list` still returns exactly `["search_lessons", "record_lesson"]`.
- `Expected numbers`: JSON hit count equals keyword hits plus distinct marked hits in the allowed domains; score of an injected hit is a number, not `null`.
- `STOP if`: a domain outside `[mcp] domains` is read or an injected hit changes the JSON shape — stop.

## Step S6 result — prompt hook (R2, D5)

- `Read first`: `src/hook.rs:42-79,162-243,283-285,490-719`.
- `Do`: open readable domains once, compute marked hits before the `match`, inject after `order_candidates(...).take(max_results)`, pass `marked_count` to `render`, and keep marked blocks outside the character fold. Add the `ponytail:` ceiling comment.
- `Done when`: the hook cases in the matrix pass; a prompt of only stopwords/punctuation still prints marked lessons; an ordinary chat prompt with no marked lessons still prints nothing.
- `Expected numbers`: marked hits printed regardless of `MIN_SHARED_WEIGHT`, `max_results` and `MAX_OUTPUT_CHARS`; keyword hits still cut to `max_results`.
- `STOP if`: hook output exceeds the agent's replacement threshold in a test, or a keyword hit is no longer filtered — stop and record the trade-off.

## Step S7 result — sync field and wire tokens

- `Read first`: `src/sync.rs:24-55,98-156,178-185,234-267,750-856`; the design snippet above.
- `Do`: append `MergedField::PrimeDirective`, grow `MERGED_FIELDS`, add the two named tokens and their exhaustive arms, and add the sync tests from the matrix.
- `Done when`: those tests pass in both argument orders; a flag-only change propagates through apply and a second apply writes nothing.
- `Expected numbers`: 10 `MERGED_FIELDS`; no new conflict entry for a two-valued field changed differently on both sides; round-trip token equals `PRIME_DIRECTIVE_SET`/`PRIME_DIRECTIVE_CLEAR`.
- `STOP if`: an older `.jsonl` line cannot be read, or the conflict-file sort order of existing fields changes — stop before rewriting history.

## Step S8 result — docs and rules (D6)

- `Read first`: `README.md:76-124,309-319`; `skills/lessons/SKILL.md:12-23`; the three global rule paths in the design.
- Traceability: the repo README/SKILL half serves ticket R1 ("A marked lesson appears in every search result"); the global-rules half serves [the 2026-11-26 decision comment on issue #37](https://github.com/HidekiAI/llm-lessons/issues/37#issuecomment-7197080507) (D6).
- `Do`: apply the clauses under Docs and rules; commit the `~/scripts/LLM` change separately from the repo change.
- `Done when`: `grep -rn 'prime directive' README.md skills/lessons/SKILL.md` finds the new clauses; `grep -rn 'silence counts as consent' ~/scripts/LLM/AGENTS.global.md ~/scripts/LLM/rules/default.md ~/scripts/LLM/skills/lessons/SKILL.md` finds the inquiry wording; no code changed in this step.
- `Expected numbers`: one docs commit in `llm-lessons`, one rules commit in `~/scripts` (branch there first), zero source files touched.
- `STOP if`: the global rules repo is on its default branch — branch there before editing; do not mix the commits.

## Step S9 result — fixture: full gate and handoff

- `Read first`: the STEP LEDGER and every `## Step N result` block in the repo ledger.
- Traceability: serves ticket R1-R5 (verifies each expected-result line) and the Plan gate MIN_SCORE 85 set by @me; no new scope.
- `Do`: run the full suite, the no-magic-number guard, an independent self-review of the whole diff, fill in every ledger row's evidence/fingerprints, set the checkpoint to `nothing in flight`, and name the shipped artifacts.
- `Done when`: full suite exits 0 with the measured test count recorded; no self-review finding is left unfixed; the ledger shows no PLANNED row.
- `Expected numbers`: binary tests at least 282 plus the new named cases (exact final number measured here, never predicted earlier); no-magic-number guard 2 passed.
- `STOP if`: any mutation listed in the test matrix failed to redden its test — a green suite is not evidence until it is shown able to fail.

## Tests

Each row names the clause or decision it guards, the layer that can force it, the assertion and the mutation that must turn it red. "Unit" means in-source in the named module; "E2E" means the real binary or JSON-RPC exchange.

| Step | Clause / decision | Layer | Assertion | Mutation that must turn it red |
| --- | --- | --- | --- | --- |
| S1 | R1, D1 | Unit, `db.rs` | A V3 file (`SCHEMA_V1`+`V2`+`V3`, one row with `bypassed` = 3 and a stamp) opens at `MIGRATIONS.len()` with `prime_directive` 0 and both old values intact | Drop `SCHEMA_V4`, or give the column a `DEFAULT 1` |
| S1 | D1, no reindex | Unit, `db.rs` | `UPDATE lesson SET prime_directive = 1` changes total_changes by exactly the one row, unlike an indexed-column edit (`db.rs:566-580` idiom) | Add `prime_directive` to the `lesson_au` `UPDATE OF` list |
| S1 | R1, D1 | Unit, `store.rs` | Old JSONL-like struct without the key reads `false`; both bool states survive `copy_into` and `move` | Drop the column from `LESSON_SELECT_COLUMNS` or from `copy_into`'s ON CONFLICT clause |
| S2 | D4 | E2E, `tests/cli.rs` | `update --prime-directive` then `show --format json` has `true`; `--no-prime-directive` has `false`; a mark-surviving field edit keeps it; `update` with no flags still fails | Remove the flag's CASE arm, or bind `Some(false)` for both flags |
| S2 | D4 | E2E, `tests/cli.rs` | `add --prime-directive` exits non-zero with a usage error; `show` text prints `prime directive` only when true | Move the flags into shared `FieldArgs` (add starts accepting them) |
| S2 | D4, repeat after refusal | E2E, `tests/cli.rs` | `update` with no flags exits non-zero before a valid set and again after that set; the refusal does not undo the valid set | Remove the "nothing named" guard so a no-flag `update` succeeds; the assertion that it exits non-zero goes red |
| S3 | R2, R4 | Unit, `store.rs` | `prime_directive_hits` returns the marked row for empty, stopword-only and hostile queries, and returns none from a domain not in `opened`; retired marked row is absent | Reuse `search` with an FTS query, or drop `superseded_by IS NULL` |
| S3 | R1, R3 | Unit, `store.rs` | `with_prime_directive` puts the marked hit first and returns one copy when the same lesson is also a keyword hit; keyword count above an arbitrary limit stays intact before injection | Drop the dedupe filter, or truncate after combining |
| S4 | R1, R3 | E2E, `tests/cli.rs` | With `--limit 1`, two keyword matches and one marked non-matching lesson, `search --format json` has both hits and the marked one first | Apply `--limit` after injection, or sort the combined list by score |
| S4 | R4 | E2E, `tests/cli.rs` | A marked lesson in `work` is absent from `search --in shared` and present in `--in work,shared` | Open all domains instead of the caller's list |
| S4 | R5 | E2E, `tests/cli.rs` | A retired marked lesson is absent from `lessons search` for a query matching no other lesson | Drop `superseded_by IS NULL` in `prime_directive_hits`; the retired marked hit appears |
| S5 | R1, R3 | E2E, `src/mcp.rs` tests | `search_lessons` JSON contains an injected hit beyond `limit`; its `score` is a JSON number; `tools/list` is unchanged | Cut after injection, or use a non-finite score constant |
| S5 | R4 | E2E, `src/mcp.rs` tests | A marked lesson in a domain outside `[mcp] domains` never appears, and an explicit unlisted `domains` still fails as before | Bypass `refuse_unlisted` for injected rows |
| S6 | R2, D5 | Unit/E2E, `hook.rs` | Marked lesson prints for a prompt of `the a of` and for `!!!`; an unmarked lesson with no shared words stays silent | Keep the `fts_query = None` branch returning `Vec::new()`, or filter marked rows by `shared_weight` |
| S6 | D5, caps | Unit/E2E, `hook.rs` | Marked rows survive `max_results` = 1 and a long-block render budget; keyword rows still cut at 1 and still obey whole-block fitting | Leave `order_candidates(...).take()` or the `render` fold applied to marked rows |
| S6 | R5 | Unit/E2E, `hook.rs` | Retiring the marked lesson makes it disappear from hook output | Drop `superseded_by IS NULL` in `prime_directive_hits` |
| S7 | R1, sync | Unit, `sync.rs` | Both argument orders: a marked side wins over an unmarked side; a clear on one side against an unchanged marked BASE yields clear; one-sided rows keep their flag; a second identical apply writes nothing | Use OR, `ours` only, or add a `MergedField` variant without its `field_value`/`with_field` arms |
| S7 | persisted token | Unit, `sync.rs` | `field_value` returns the literal `PRIME_DIRECTIVE_SET`/`PRIME_DIRECTIVE_CLEAR` and `with_field` round-trips them; an unknown token fails closed | Rename a token constant at its definition only; the pin must go red |
| S8 | D6 | Check only | `grep` clauses above; docs carry no executable behaviour | Delete a clause; the grep goes empty |

Race, back-to-back and non-determinism answers (required by the testing rules, stated up front):

- Race: MEASURED: the repo already guards the sync read/apply interleaving at `src/store.rs:1496` (`a_local_write_never_lands_between_the_read_and_the_apply`, doc comment `src/store.rs:1492-1494`: while the sync transaction is open the local writer is refused and never slips between the read and the apply); S7 inherits that guard. MEASURED: the mark is one row written by one static `UPDATE` under SQLite's write lock (`busy_timeout` 5 s, `db.rs:20,90`), and retrieval only reads; no check-then-act across collections exists here. A forced-interleaving gate would test SQLite, not this change, so no new race test is added. Layer: none; reason stated.
- Back-to-back repeats: S2 repeats set/clear twice; S7 applies the same merge twice and asserts the second write is empty. Both forced deterministically, no timing.
- Repeat after refusal: the dedicated S2 repeat-after-refusal row pins `update` with no flags staying refused before and after a valid set, with a mutation that makes the no-flag `update` succeed and the assertion go red; a retired marked lesson stays absent before and after another search (S4 R5 row, S6 R5 case).
- Non-determinism: `prime_directive_hits` orders by `created_at, seq` and caller domain order; bm25 keyword order is unchanged; no timing or random pick is introduced. The `0.0` score is a pinned constant, not a measured value.
- Falsifiability: every row above has a named mutation that must redden it; a mutation that only breaks parsing is void and must be re-run after matching the file's real indentation.

## Provenance

- Blame measured 2026-11-26 for the lines this change touches, all `HidekiAI-WORK`: `MIGRATIONS`/`migrate` `db.rs:183-187` in `4816385` (2026-11-10, "feat(db): open DB files and migrate the schema") and `f0ccf88` (2026-11-18); V3 `db.rs:320-329` in `cbc892e`/`f0ccf88` (2026-11-18); `Lesson`/columns `store.rs:28-50,73-95` in `5eee77f` (2026-11-10), `95dee12` (2026-11-11), `cbc892e`/`f0ccf88` (2026-11-18), `24126ee` (2026-11-18); `update` `store.rs:181-244` in `497ce75`/`dc67b26` (2026-11-10); `bump_bypassed` `store.rs:260-275` in `cbc892e`/`f0ccf88`; `copy_into` `store.rs:289-323` in `51a69f2` (2026-11-10) plus the 2026-11-18 counter lines; `search_all` `store.rs:406-466` in `0f7c1f5` (2026-11-10); hook gate/order/render `hook.rs:42-79,162-243` in `e8bed74` (2026-11-10), `8f1d9db4` (2026-11-12), `96156b4` (2026-11-18); MCP search `mcp.rs:343-366` in the measured blame set. All are INHERITED; this plan adds beside them and edits no shipped migration.
- Summaries of those commits were read with `git show -s --format='%h %an %ad %s'`; HEAD at measurement time was `9cee336` (2026-11-23, docs(readme): add a sample LLM interaction) and `git status --short` was empty.
- SEARCHED: PR threads — `gh pr list --state all -R HidekiAI/llm-lessons` (2026-11-26) returned zero PRs, so there is no PR thread to read; MEASURED zero, structural rather than an omission.
- Issues: #37 (this, always-included lessons). Related: [#12](https://github.com/HidekiAI/llm-lessons/issues/12) (bypassed counter, closed), [#17](https://github.com/HidekiAI/llm-lessons/issues/17) (last_bypassed_at, closed), [#19](https://github.com/HidekiAI/llm-lessons/issues/19) (MCP ranking by bypassed, open), [#34](https://github.com/HidekiAI/llm-lessons/issues/34) (session-start most-reminded, open). Their bodies/comments were read with `gh issue view <n> -R HidekiAI/llm-lessons` and supply the precedent for the counter and the "same UPDATE" rule; none of them implements this flag.
- Decisions: D1-D7 come from [the 2026-11-26 decision comment on issue #37](https://github.com/HidekiAI/llm-lessons/issues/37#issuecomment-7197080507), not from the issue body; the sync-clear decision comes from [the second decision comment](https://github.com/HidekiAI/llm-lessons/issues/37#issuecomment-7197223200); D6 is additionally grounded in `~/scripts/LLM/AGENTS.global.md:342-347`.
- Source classes not searched: chat outside this conversation, Jira and Confluence (this repo has neither), other repositories' issue trackers, and the installed hook/MCP configurations on any host. No claim above depends on them; the UNVERIFIED labels cover what does.

## Possible follow-ups (not this ticket)

- #19 (MCP ranking by `bypassed`) — ranking of keyword hits is untouched here.
- #34 (session-start most-reminded lessons) — a different trigger; R1 covers search results only.
- The separate `memory` table from issue option 3 — only if the standing set becomes editable as one document rather than a handful of rows.
- A configurable cap for marked lessons, or a partial index on the flag — only if D1's "handful" is exceeded or the scan is measurably slow.
- A full history of reminder inquiries — D3 deliberately reuses the existing `bypassed` counter.

## Plan gate

- DECIDED (2026-11-26, @me): MIN_SCORE = 85. Reason: MEASURED zero PRs in the repo (`gh pr list --state all -R HidekiAI/llm-lessons`) make the provenance dimension structurally 0/15; no provenance points are invented to clear a higher bar.
- Iteration 0 (2026-11-26): MEASURED from the independent evaluator: 59/100, `BLOCKED`. CRITICAL item, quoted from the iteration-0 plan: "D6 @me-authorized docs/rules scope: an inquiry about an existing lesson and a self-noticed slip both get a proposed bump naming the lesson id; silence on @me's next message counts as yes; an explicit no prevents it. This applies to `skills/lessons/SKILL.md` and the global agent rules, and is not a scope growth of issue #37's code work." -> no line in issue #37 then supported bumps or global-rule edits. Resolved by the 2026-11-26 decision comment on issue #37.
- Iteration 0 dimension breakdown from the independent evaluator: Traceability 30/35, Coverage 23/25, Minimality 6/15 (D6 global-rules edits, S8 step and its grep test, plus the S0 plan copy), Grounding 9/10, Provenance 0/15 (structural zero PRs).
- Iteration 0 refinements applied, all eleven: (1) D6 and the global-rules half of S8 cite the first decision comment, and the opening line records that comment and no code write; (2) MIN_SCORE 85 with the zero-PR reason; (3) S7 sync semantics DECIDED as explicit-clear propagation through the three-way `MergedField` merge, open question removed; (4) S0 no longer copies the plan into the repo; (5) traceability citations added to D1, D3, D7, S0, S9 and the README/SKILL half of S8; (6) dedicated S2 repeat-after-refusal row with a mutation that makes the no-flag `update` succeed, and the paragraph points at that row; (7) S4 CLI R5 E2E row for a retired marked lesson; (8) complexity bound `D <= 3` labelled UNVERIFIED; (9) race dismissal cites the verified sync guard at `src/store.rs:1496` inherited by S7; (10) PR threads marked SEARCHED with the zero-PR command, and the decision comment named as the source of D1-D7; (11) iteration 0 score, CRITICAL item, losses and every refinement recorded here, with iteration 1 left for the evaluator.
- Findings placement: @me decided to keep these findings in this plan section rather than open one issue per finding. Why: that is @me's decision for this planning pass (no further reason supplied; recorded as a user decision, not inferred).
- Planning-artifact commit: @me decided to commit this gated plan as `docs/PLAN-2026-11-26-prime-directive.md` on a new `docs/prime-directive-plan` branch, push it by explicit remote name, fast-forward merge it into `trunk` and delete only the local working branch. This is a documentation commit, not part of the implementation steps S0-S9.
- S7 sync semantics: DECIDED (2026-11-26, [the sync decision comment](https://github.com/HidekiAI/llm-lessons/issues/37#issuecomment-7197223200)) — an explicit clear on one machine propagates on next sync; no question remains for @me.
- Iteration 1 (2026-11-26): MEASURED from the independent evaluator: 81/100, `BLOCKED` (below MIN_SCORE 85). CRITICAL: none. UNCOVERED: none. Dimension findings: Traceability 31/35 (the sync decision was cited to a comment with no sync sentence), Coverage 25/25, Minimality 15/15, Grounding 10/10, Provenance 0/15 (structural zero PRs). Refinements applied: the sync decision is recorded in the second issue comment and cited at lines 56, 62, 184 and 351; iteration 0's CRITICAL quote and Minimality loss are corrected above. Lesson `01M874FTHDG04PXHWBZ1354Q91` was violated by the earlier DECIDED label and is now followed.
- Iteration 2 (2026-11-26): MEASURED from the independent evaluator: 84/100, `BLOCKED` (below MIN_SCORE 85). CRITICAL: none. UNCOVERED: none. Dimension findings: Traceability 35/35, Coverage 25/25, Minimality 15/15, Grounding 9/10 (line 33 still said "the one decision comment" while line 5 measured two), Provenance 0/15. Refinement applied: line 33 now says two decision comments are the only GitHub writes, quotes the `comments` = 2 measurement, and records @me's separate docs-branch commit decision.
- Iteration 3 (2026-11-26): MEASURED from the independent evaluator: 85/100, `PASS` at @me's MIN_SCORE 85. CRITICAL: none. UNCOVERED: none. Dimension scores: Traceability 35/35, Coverage 25/25, Minimality 15/15, Grounding 10/10, Provenance 0/15 (structural zero PRs, the maximum reachable). Non-blocking tidy applied: the second decision comment is named in Provenance. The score does not show that zero PRs make 85 the ceiling, or that implementation code has been written; none has.
- Check before the gate: every factual premise in this file carries `MEASURED` with a file:line or command, or `UNVERIFIED`; every test row carries a clause/decision, a layer, an assertion and a mutation; no follow-up item has leaked into a step.
