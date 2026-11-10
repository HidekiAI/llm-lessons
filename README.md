# llm-lessons
Lessons learned from each|every sessions based on trials, errors, failures, and building postmortem style database, so that LLMs can query the lessons db to make sure mistakes|err are not repeated

`lessons` is a small CLI that records each problem: what went wrong, why, what fixed it, and what was tried that did NOT work. A human or an LLM can then search it before debugging the same thing again. The failed attempts are kept on purpose: they are what saves a repeat of the same dead end.

## Install

```sh
cargo install --path .        # puts `lessons` in ~/.cargo/bin
lessons init                  # creates the DB files
```

## Data: three files, never in git

| File | Holds |
| --- | --- |
| `work.db` | work problems; may belong to your employer |
| `personal.db` | home and personal problems |
| `shared.db` | generic lessons, useful anywhere |

They live in `~/.lessons/` (Windows: `%USERPROFILE%\.lessons\`), or wherever `LESSONS_HOME` points. They are separate files, not one table with a column, because only a missing file can keep work data off a personal machine and personal data out of work tooling. Create only the ones a machine should hold, e.g. `lessons init --domain work` on a work laptop. Commands skip missing files.

The DB files are private data. `.gitignore` excludes every SQLite file form, and `tests/gitignore.rs` fails if a rule is removed. `git add -f` still gets past both, so never force-add a DB file: a pushed file stays retrievable from history even after it is deleted.

## Usage

```sh
lessons add --to shared --title "..." --problem "exact error text" \
    --root-cause "..." --resolution "..." --failed "what did not work" --tags "rust sqlite"
lessons add --to work --json - < lesson.json    # one lesson as JSON, for LLMs
lessons add --to work --edit                    # fill a template in $EDITOR
lessons search "pasted error text"              # --in work,shared  --raw  --format json
lessons show <id>                               # --format json
lessons list --tag rust                         # --in, --limit
lessons update <id> --resolution "..."          # an empty value clears a field
lessons hit <id>                                # this lesson helped
lessons retire <id> --by <newer-id>             # hide from search
lessons move <id> --to shared
```

`--verbose` (or `LESSONS_LOG=1`) logs each step to stderr. `$LESSONS_HOME/config.toml` is optional:

```toml
default_search = ["work", "personal", "shared"]   # search without --in

[hook]
domains = ["work", "shared"]   # personal is left out on purpose
max_results = 5
```

## Using it from a coding agent

Any agent that can run a shell command can use it (opencode, Claude Code, others). Add a line like this to the agent's instruction file (`AGENTS.md`, `CLAUDE.md`, ...):

```
On an unfamiliar error, run `lessons search '<error text>' --format json` before debugging; after solving something non-trivial, propose a `lessons add --json -` entry for me to approve.
```

`--format json` output and `add --json -` input are the stable contract for agents.

### Optional: Claude Code prompt hook

Claude Code can also run the search on every prompt. Add to `~/.claude/settings.json`; use the absolute path to `lessons` if it is not on the PATH the hook sees.

```json
{
  "hooks": {
    "UserPromptSubmit": [
      { "hooks": [{ "type": "command", "command": "lessons hook user-prompt-submit", "timeout": 10 }] }
    ]
  }
}
```

On each prompt the hook prints matching lessons, which Claude Code adds as context, and prints nothing when none match. It always exits 0, so it never blocks a prompt.

## Contributing

`cargo test` also enforces two repo rules: no DB file can be committed (`tests/gitignore.rs`), and no magic-number literal outside a named `const` (`tests/no_magic_numbers.rs`).

## Phase 2 (not built yet)

- MCP server exposing `search_lessons` and `record_lesson`, for any MCP-capable agent.
- Semantic search with sqlite-vec embeddings.
- Sync between machines: `lessons export` / `import` to one JSON line per lesson, for a private data repo (never `work.db` in a personal repo).
