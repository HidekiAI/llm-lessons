# PLAN 2026-11-19: rank search by reminders (issue #19)

TL;DR: one shared ordering (`bypassed` desc, then `last_bypassed_at` desc, then text score) in `store.rs`, used by the
hook, MCP `search_lessons` and a new CLI flag `--by-reminders`. A reminded lesson (bypassed >= 1) joins the candidate
pool whatever its text rank, so it can no longer fall off the limit cut. Plain `lessons search` is unchanged; the shipped
skill tells agents to pass the flag. Detail below.

## TODO (resume here, in order)

Status 2026-11-19: plan only, no code. Branch `feat/search-rank-reminders`. Gate iteration 1 scored 84/100 (pass 90).

1. `git switch feat/search-rank-reminders && git pull origin feat/search-rank-reminders`.
2. Revise this plan with a separate planner agent: apply every item of "Iteration-2 to-do" in the "Plan gate" section
   at the end of this file.
3. Re-run the `plan-ticket-gate` skill on this file (iteration 2; at most 3). Pass is 90/100. Under 90 after
   iteration 3: stop and ask @me.
4. Once it passes: record each gate finding as its own issue (closed, with how it was fixed), and commit the plan.
5. Ask @me whether to code S1 (Rust) with the `coding-assistant` skill or have the assistant write it. Tests come
   first and the assistant writes them.
6. Code S1-S6 in order; full `cargo test` before each commit; self-review must reach a dev-score of 95/100.

## Requirements

From issue #19 (title: "feat(mcp): rank search_lessons by the bypassed count, like the prompt hook") and its second comment:

- T1 "MCP search_lessons ranks by text score only. Give it the hook's relevance filter and order, so lessons agents keep
  forgetting rank first."
- T2 "search_lessons fetches a larger pool, applies the hook's relevance filter, then order_candidates, then cuts to limit."
- T3 "CLI lessons search keeps pure score: a person searching by hand wants the best text match."
- T4 "Should the MCP side also boost the current repo? Should last_bypassed_at (#17) weigh in?" (open questions, answered by D3, D4)
- T5 (comment) "Agents also call the CLI, and the hook reorders only inside its pool, so a much-reminded lesson can still fall
  off." Measured there: bypassed = 3, query "git push" is rank 7 (MCP limit 5 drops it, CLI shows it); query "tests pass"
  is rank 26 (MCP, CLI and the hook pool all drop it).
- T6 (comment) "Should the CLI also rank by bypassed, or take a flag the skill always passes? Should the hook's pool
  grow, or should high-count lessons skip the pool cut?"

Decided with @me 2026-11-19 (ticket-supported; answers T4 to T6):

- D1 "Flag, skill passes it": plain `lessons search` keeps pure text-score ranking for people; a new flag
  (`--by-reminders`) ranks by the bypassed count; the shipped skill (`skills/lessons/SKILL.md`) tells agents to always
  pass it.
- D2 "Reminded skip the cut": in the prompt hook, a lesson with bypassed >= 1 that passes the relevance filter joins the
  candidate pool whatever its text rank; the others stay capped at `max_results x BOOST_POOL_FACTOR`.
- D3 "Yes, newer first": at equal bypassed counts, the lesson with the later `last_bypassed_at` ranks first, then text score.
- D4 "No" repo boost for MCP: MCP `search_lessons` ranks by bypassed, then `last_bypassed_at`, then text score; no
  current-repo boost.

- D5 (@me 2026-11-19, answering Q1) "Bar capped by query size": the CLI flag and MCP use the bar
  `min(MIN_SHARED_WEIGHT, total weight of the query's terms)`, so a 2-word query needs both words; `--raw` skips the
  filter. Long queries stay as strict as the hook.

Question for @me, now answered by D5 (kept for the record):

- Q1 What relevance filter do the CLI flag and MCP use? T1/T2 say "the hook's filter" (4 shared ordinary words or one
  identifier). But T5's own example "git push" has weight 2 and would return nothing under it. Plan default: the same
  `shared_weight` check, with the bar `min(MIN_SHARED_WEIGHT, total weight of the query's terms)`, so a short query must
  be shared whole and a long one needs 4; `--raw` queries skip the filter (no terms to weigh). The hook keeps
  `MIN_SHARED_WEIGHT` unchanged (D2 says "passes the relevance filter").

## Premises

- MEASURED: hook ordering is `order_candidates` (`src/hook.rs:164-190`): partition by current repo, then
  `sort_by_key(Reverse(bypassed))` per group (stable, so score order breaks ties), `take(max_results)`. Repo boost stays (D4 only removes it from MCP).
- MEASURED: hook pool is `store::search_all(opened, fts, max_results.saturating_mul(BOOST_POOL_FACTOR))` then
  `shared_weight >= MIN_SHARED_WEIGHT` (`src/hook.rs:53-75`; `BOOST_POOL_FACTOR = 4`, `hook.rs:24`; `MIN_SHARED_WEIGHT = 4`, `hook.rs:82`).
- MEASURED: `store::search_all` runs `store::search` per file (`ORDER BY score LIMIT ?2`, `store.rs:425-430`), sorts by
  bm25 `total_cmp` ascending (lower is better), truncates to `limit` (`store.rs:453-466`). It returns hits in score order.
- MEASURED: MCP `search_lessons` calls `store::search_all(..., arguments.limit.unwrap_or(mcp_config.max_results))`
  with no ordering step (`src/mcp.rs:344-365`); its schema has `query`, `limit`, `domains`, `raw` (`mcp.rs:217-224`, `266`).
- MEASURED: CLI `run_search` calls `store::search_all(&opened, fts_query, limit)` (`src/main.rs:730-766`);
  `SEARCH_DEFAULT_LIMIT = 10` (`main.rs:24`); `Search` args at `main.rs:81-95`.
- MEASURED: `Lesson` carries `bypassed: i64` and `last_bypassed_at: Option<String>` (set by #12 and #17, shipped; `hook.rs:666-667`).
  The stamp is fixed-width UTC ISO text, so text order is time order (`docs/PLAN-2026-11-18-last-bypassed-at.md`, premise 1).
  `Option<String>` orders `None < Some`, so a descending compare puts a stamped lesson before an unstamped one and the later stamp first.
- MEASURED: the hook test `more_bypassed_ranks_first_and_equal_counts_keep_score_order` (`hook.rs:701`) gives the
  tied pair `last_bypassed_at` values opposite to score and asserts score order. D3 reverses that rule, so this test is
  CHANGED by S2 (its expected order flips for the tied pair); it was the T6 guard of #17 ("ranking does not change yet").
- MEASURED: tests are in-source `#[cfg(test)] mod tests` (`hook.rs:246`, `mcp.rs`, `store.rs:617`, `main.rs:822`) plus
  integration files in `tests/` (`cli.rs` runs the real binary). `tests/no_magic_numbers.rs` fails on any non-0/1 literal outside a
  named `const`, so every number in new code and tests is a `const`.
- MEASURED: `scripts/setup.sh` never overwrites an existing `SKILL.md` (README line 51). An existing install keeps the old skill text until the owner updates it (see follow-ups).
- MEASURED: `gh pr list --repo HidekiAI/llm-lessons --state all` is empty (see Provenance).
- UNVERIFIED: how many lessons have bypassed >= 1 in real use (ticket says two lessons have 3, #34). The reminded set
  is small by nature (counted only on a manual user action), so fetching all matching reminded rows without a cap is
  bounded in practice. Measure with `lessons list --format json | jq '[.[]|select(.bypassed>=1)]|length'` before shipping.
- UNVERIFIED: that the T5 ranks (7 and 26) still hold on the owner's live DBs; they are the ticket's numbers, not
  re-run here. The tests use fixtures instead.
- UNVERIFIED: whether `lessons search` JSON output order change under the flag breaks a consumer; none found by
  `grep -n "search" scripts/ skills/` (only the skill text and README mention it).

## Design

One ordering, one pool, three callers.

```rust
// store.rs (new, shared). Stable sort: equal keys keep the incoming score order.
pub fn sort_by_reminders(mut_hits: &mut [SearchHit]) {
    mut_hits.sort_by(|left_hit, right_hit| {
        right_hit.lesson.bypassed.cmp(&left_hit.lesson.bypassed)
            .then_with(|| right_hit.lesson.last_bypassed_at.cmp(&left_hit.lesson.last_bypassed_at))
    });
}

// store.rs (new). Top `score_pool` by score, plus every matching reminded lesson, deduplicated, score-ordered.
pub fn reminder_pool(opened: &[(Domain, Connection)], fts_query: &str, score_pool: usize) -> Result<Vec<SearchHit>>;
```

- `search` gets a private `only_reminded: bool` (adds `AND l.bypassed >= 1`, no `LIMIT` when true) so the SQL stays one
  statement body; `search_all` keeps its signature and callers (`store.rs:407-466`).
- `reminder_pool` = `search_all(score_pool)` chained with the reminded-only search of every file, deduplicated by
  `(domain, id)`, then `total_cmp` on score (the same order `search_all` returns).
- `hook::search_by_reminders(opened, search_text, raw, limit)` (new, `pub(crate)`, in `hook.rs` because that file owns
  `shared_weight` and the relevance constants): `reminder_pool(limit x BOOST_POOL_FACTOR)`, relevance filter (D5), `sort_by_reminders`, `truncate(limit)`.
  Used by CLI `--by-reminders` and MCP. A move of the relevance code to its own module is a follow-up, not this ticket.
- Hook: `user_prompt_submit` swaps its `search_all` call for `reminder_pool` (D2); `order_candidates`'s inner
  `fn_most_bypassed_first` becomes `store::sort_by_reminders` (D3). Repo partition, filter and `take` unchanged.
- Why not use `order_candidates` for MCP (T2 names it): it carries the repo boost, which D4 forbids for MCP.
  The shared piece is the sort inside it, so hook and MCP cannot drift apart.

## Steps

Each step is one commit; "Done when" is a command.

| Step | Change | Serves | Done when |
|---|---|---|---|
| S1 | `src/store.rs`: `sort_by_reminders`; `search` gains the reminded-only mode; `reminder_pool`; unit tests below | D2, D3, T6 | `cargo test store::` exits 0; each S1 mutation in Tests goes red |
| S2 | `src/hook.rs`: pool via `reminder_pool`; `order_candidates` calls `sort_by_reminders`; change `hook.rs:701` test per D3; add the pool-cut test | D2, D3 | `cargo test hook::` exits 0; mutations red |
| S3 | `src/hook.rs`: `search_by_reminders` (`pub(crate)`, `shared_weight` stays private); `src/mcp.rs:356`: `search_lessons` calls it; no repo boost | T1, T2, D4 | `cargo test mcp::` exits 0; mutations red |
| S4 | `src/main.rs`: `Search` gets `#[arg(long)] by_reminders: bool` (help: "Rank by how often the user had to remind the agent, then newest reminder; for agents"); `run_search` calls `hook::search_by_reminders` when set, else the unchanged `search_all` path. Relevance bar per D5 | D1, D5, T3, T6 | `cargo test --test cli` exits 0; mutations red |
| S5 | `skills/lessons/SKILL.md` "Before a task: search" step 1 and 2: command becomes `lessons search --by-reminders "<text>"`; one sentence why ("lessons you were reminded of rank first") | D1 | `grep -c -- '--by-reminders' skills/lessons/SKILL.md` prints at least 1; skill test green |
| S6 | `README.md`: the CLI example block (line 76), the hook ordering paragraph (line 124: reminded lessons now also join the pool past the cut), the agent-rules sample (line 108) and the FAQ table rows (lines 269-270) gain the flag and the new bounds | D1, D2, D4 | `grep -n -- '--by-reminders' README.md` lists the four sites |

Bounds after the change (for the FAQ): hook and MCP still return at most `max_results`/`limit` lessons; the pool is
`limit x 4` by score plus every matching reminded lesson, so the output stays capped by the same `take`/`truncate`.

## Tests

In-source unit tests (the repo's pattern, `#[cfg(test)]`); the CLI and skill checks are integration/in-source in the
files named. Fixed stamp strings (`EARLIER = "2026-01-01T00:00:00Z"`, `LATER = "2026-06-01T00:00:00Z"`) and named
counts (`FEW_REMINDERS`, `MORE_REMINDERS`), because `no_magic_numbers` rejects literals.

| Check | Serves | Mutation that must turn it red |
|---|---|---|
| store: `sort_by_reminders` puts counts 3, 1, 0 first in that order whatever the incoming score order | D3, D1 | sort ascending; drop the bypassed key |
| store: equal counts, `LATER` before `EARLIER` before `None`, whatever the incoming order | D3 | compare `last_bypassed_at` ascending; drop the second key; `None` first |
| store: equal count and equal stamp keep incoming score order (stable) | D3 | use `sort_unstable_by` with a tiebreak on id |
| store: `reminder_pool` with `score_pool` 1 over 3 matching lessons where the reminded one has the worst score: returns it plus the best; duplicates collapsed when the reminded one is also in the top | D2, T5 | drop the reminded-only half; drop the dedup (shows twice) |
| store: `reminder_pool` skips a retired (`superseded_by`) reminded lesson | D2 | drop `superseded_by IS NULL` from the reminded query |
| hook: lesson with bypassed 3 at score rank beyond `max_results x BOOST_POOL_FACTOR` and sharing 4 words with the prompt is printed | D2 | revert S2's pool to `search_all` |
| hook: same lesson sharing only 2 words is NOT printed (filter still applies; extends `hook.rs:521`) | D2 | skip the filter for `bypassed >= 1` |
| hook: `hook.rs:701` updated: tied pair now ordered `LATER` first against score order | D3 | remove the stamp key |
| hook: current-repo lesson still beats a higher count (`hook.rs:740`, unchanged) | D4 scope | drop the partition |
| mcp: `search_lessons` returns a bypassed-3 lesson ranked below `limit` by score, first (extends `mcp.rs:643`); two reminded lessons with equal counts come back newest stamp first | T1, D3, D4 | call `search_all` (old path); drop the stamp key |
| mcp: a lesson whose context names the current directory gets no boost: with `cwd` irrelevant to MCP, an unreminded lesson never outranks a reminded one | D4 | add a repo partition to `search_by_reminders` |
| mcp: weak match with count 9 sharing one word of a 4-word query is excluded (D5) | T2, D5 | skip the filter |
| mcp: query "git push"-shaped (2 terms) still finds a reminded lesson sharing both | T5, D5 | use fixed `MIN_SHARED_WEIGHT` as the bar |
| cli (`tests/cli.rs`): without the flag, a reminded lesson stays in text-score order and an out-of-limit one stays absent (`search --limit 1`) | T3, D1 | make `run_search` always call the reminder path |
| cli: with `--by-reminders --limit 1` the reminded lesson is the one printed, in `--format json` too | D1, T6 | ignore the flag |
| cli: `--by-reminders --raw` runs (no filter) and returns the reminded lesson first | D1 | apply the filter to raw queries |
| skill: `include_str!("../skills/lessons/SKILL.md")` contains `lessons search --by-reminders` (in `main.rs` tests, a tracked file per falsifiability rule 4) | D1 | delete the flag from SKILL.md |

Test classes (testing.md):

- Race: not forced. A search is read-only SQL on separate connections; a concurrent `bump_bypassed` only changes
  the count between two reads, and each file is read in one statement, so a hit is never half-updated. The only
  cross-statement window is the top-N query then the reminded query seeing different counts; the result is still a
  valid ordering of rows that existed. Not deterministic to force, so not tested; stated here instead.
- Back-to-back repeat: the same search twice returns the same list (a test in `tests/cli.rs` runs it twice and compares
  JSON ids), and a bump between two runs moves the lesson up (repeat after the blocking condition changes).
- Non-determinism: no clock or randomness in the code under test; stamps are fixed strings; ties are broken by the
  stable sort over score, which is deterministic for equal bm25 only if the incoming order is; tests use distinct scores.
- Layer: ordering and pool are unit (store, hook, mcp: no wire needed); flag wiring and skill text are the CLI layer
  (the real binary in `tests/cli.rs`). No E2E beyond that: the repo has none.
- Falsifiability: every row above names its mutation; run each, check stderr for compile errors first (a void experiment).

## Provenance

- Lines changed and blame (all HidekiAI-WORK, all INHERITED, `git blame --date=short`):
  `hook.rs:53-75` `e8bed74` and `e0b8bff` 2026-11-10/12, `96156b4` 2026-11-18 (the `bypassed` ordering);
  `hook.rs:160-190` `e8bed74` 2026-11-10, `96156b4` 2026-11-18; `mcp.rs:343-365` `6190ca5` and `8f1d9db` 2026-11-10;
  `main.rs:730-766` `0f7c1f5` 2026-11-10, `main.rs:24` `497ce75` 2026-11-10; `store.rs:453-466` `0f7c1f5` 2026-11-10;
  `skills/lessons/SKILL.md` step lines `8e7c99e` 2026-11-12 (latest edits `6b30a01`, `7aa0442`, `265a081` 2026-11-18).
- Ticket history: #12 (bypassed counter and hook ranking, shipped `7aa0442`), #17 (`last_bypassed_at`, shipped `24126ee`;
  it said "Ranking does not use it yet: see #19"), #34 (session-start hook; says "#19 ranks by bypassed, but only
  among lessons that already matched"; out of scope here, it reuses `sort_by_reminders` later).
- PRs: none (`gh pr list --repo HidekiAI/llm-lessons --state all` empty).
- Not searched: chat outside the ticket; the owner's live DB contents. This repo has no Jira or Confluence.

## Possible follow-ups (not this ticket)

- Move `shared_weight` and the relevance constants out of `hook.rs` into a shared module.
- Update installed skills (`setup.sh` never overwrites `SKILL.md`), e.g. an update subcommand or a note in the README.
- Make the plain CLI flag the default later, if the owner wants (D1 says no for now).
- #34 session-start hook reusing `sort_by_reminders`.

## Plan gate

Current step: plan gate iteration 1 scored 84/100 (pass 90), so the plan is NOT approved and no code is written.
Next session: a separate planner applies the iteration-2 to-do below, then the gate re-scores (up to iteration 3).
Still under 90 after iteration 3: stop, do not code, and ask @me how to resolve it.
@me 2026-11-19: "not coding today; just place all the docs in repos, I may continue working from home".

- 2026-11-19 iteration 1: 84/100 (Traceability 31/35, Coverage 21/25, Minimality 12/15, Grounding 7/10, Provenance
  13/15; lost 16). No CRITICAL, no UNCOVERED cap. Q1 was open during scoring; now answered by D5.

Iteration-2 to-do, from the evaluator's findings:

- Fix wrong MEASURED line refs: `BOOST_POOL_FACTOR` is `hook.rs:31` (not 24), `MIN_SHARED_WEIGHT` is `hook.rs:87`
  (not 82); re-check every other ref.
- Cite D5 for the relevance bar and the `--raw` skip (lost Traceability), and mark which ticket line or decision each
  README site in S6 serves, or move the README beyond D1/D2/D4 to a follow-up.
- Replace the vacuous MCP "no repo boost" test (MCP has no cwd, so its mutation cannot go red) with: an unreminded lesson
  with a better score and a matching context never outranks a reminded one; mutation: call `order_candidates`.
- Every "Done when" checks that tests actually ran (a nonzero count; lesson 01M78AZ468CS5ENJM66ZWA1VC7), and the plan
  runs the full `cargo test` before each commit (lesson 01M78HCG174YC31WS5J2T2EE44).
- Say explicitly that repeat-after-refusal does not apply (no refusal path), instead of substituting the bump test.
- Race: state the two-query window inside `reminder_pool` as an accepted design decision, or force it at unit level.
- Bound the reminded-only query (a LIMIT or cap), or record the measured count. MEASURED 2026-11-19: 4 lessons have
  bypassed >= 1 (`lessons list --limit 1000 --format json | jq` filtered on `bypassed >= 1`).
- S5: the skill-text test needs a new `include_str!` of `skills/lessons/SKILL.md` (none exists in `src/` today); check
  the path from the test's file.
- Provenance: add `8f1d9db` to the `hook.rs:53-75` blame, and blame the README and SKILL.md lines S5/S6 change.
- After the plan passes: record each gate finding as its own issue (the `plan-ticket-gate` Persist rule).
