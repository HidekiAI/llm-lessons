# llm-lessons
Lessons learned from each|every sessions based on trials, errors, failures, and building postmortem style database, so that LLMs can query the lessons db to make sure mistakes|err are not repeated

`lessons` is a small CLI that records each problem: what went wrong, why, what fixed it, and what was tried that did NOT work. A human or an LLM can then search it before debugging the same thing again. The failed attempts are kept on purpose: they are what saves a repeat of the same dead end.

## Install

```sh
scripts/setup.sh --domains work,shared --agents opencode,claude   # all of the below, plus MCP registration
```

Or by hand:

```sh
cargo install --path .        # puts `lessons` in ~/.cargo/bin
lessons init                  # creates the DB files (--domain work|personal|shared for one)
```

`setup.sh` requires `--domains`, so it never creates a file this machine should not hold. `--dry-run` shows the steps without running them.

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

[mcp]
domains = ["work", "shared"]   # the only files MCP tools may search or record into
max_results = 5
```

## Using it from a coding agent

Any agent that can run a shell command can use it (opencode, Claude Code, others). Add a line like this to the agent's instruction file (`AGENTS.md`, `CLAUDE.md`, ...):

```
On an unfamiliar error, run `lessons search '<error text>' --format json` before debugging; after solving something non-trivial, propose a `lessons add --json -` entry for me to approve.
```

`--format json` output and `add --json -` input are the stable contract for agents.

### MCP server

`lessons mcp` serves two tools over stdio to any MCP client: `search_lessons` and `record_lesson`. Two only, on purpose: every tool's schema is loaded into the agent's context. Both refuse domains not listed in `[mcp] domains`.

```sh
opencode mcp add --global lessons -- lessons mcp
claude mcp add --scope user lessons -- lessons mcp
```

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

## FAQ

### How do I share my sqlite db lessons so that lessons learned at work can be applied to home and vice-versa?

Put a lesson that applies anywhere in `shared.db` (`lessons add --to shared`, or `lessons move <id> --to shared`), then sync that one file through a **private GitHub repo** that both of your accounts can push to.

A gist does not work when your personal and work GitHub accounts are separate: only the gist's owner can push to it, a gist cannot have collaborators, and a secret gist is readable by anyone who has its URL.

1. With your personal account, create a private repo, e.g. `lessons-data`.
2. In its Settings > Collaborators, invite your work account and accept the invite.
3. On each machine, clone it and point `LESSONS_HOME` at the clone:

   ```sh
   git clone git@github.com:<personal-account>/lessons-data.git ~/lessons-data
   export LESSONS_HOME=~/lessons-data
   ```

4. Commit `.gitignore` in that repo as a whitelist, so only `shared.db` can ever be pushed (never `work.db`, `personal.db`, a `-wal` file or `config.toml`):

   ```gitignore
   *
   !.gitignore
   !shared.db
   ```

5. Pull before you start, and commit and push `shared.db` after adding lessons. Do both while no `lessons` process is running (close agent sessions): a `shared.db-wal` file beside it means one still is, and the file is not complete yet.

`shared.db` is binary, so git cannot merge it. If both machines added lessons since the last push, the second push is rejected and one side's copy must win; re-add the other side's lessons by hand. Phase 2's export and import (one JSON line per lesson) will make this mergeable.

Do not point `LESSONS_HOME` at a cloud-drive folder (OneDrive, Google Drive, Dropbox). A sync client can copy the file mid-write, or apart from its `-wal` file, which can corrupt it, and on a conflict it silently keeps one copy. Git rejects the second push instead, so nothing is lost without you seeing it.

Never put `work.db` in a personal account's repo: your employer's rules on its data apply.

## Contributing

`cargo test` also enforces two repo rules: no DB file can be committed (`tests/gitignore.rs`), and no magic-number literal outside a named `const` (`tests/no_magic_numbers.rs`). `scripts/setup.sh --test` checks the setup script.

## Phase 2 (not built yet)

- Semantic search with sqlite-vec embeddings.
- Sync between machines: `lessons export` / `import` to one JSON line per lesson, for a private data repo (never `work.db` in a personal repo).
