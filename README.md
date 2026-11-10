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

It works with any coding agent and any model provider: opencode with OpenRouter, Claude Code, or anything that can run a shell command or speak MCP. Nothing in `lessons` depends on the model. Add a line like this to the agent's instruction file (`AGENTS.md` for opencode, `CLAUDE.md` for Claude Code):

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

### Optional, Claude Code only: prompt hook

opencode gets the same lessons through the MCP server or the `AGENTS.md` line above. Claude Code can additionally search on every prompt: `lessons hook user-prompt-submit` prints matching lessons, which Claude Code adds as context, prints nothing when none match, and always exits 0, so it never blocks a prompt.

<details>
<summary>Hook setup for <code>~/.claude/settings.json</code></summary>

Use the absolute path to `lessons` if it is not on the PATH the hook sees.

```json
{
  "hooks": {
    "UserPromptSubmit": [
      { "hooks": [{ "type": "command", "command": "lessons hook user-prompt-submit", "timeout": 10 }] }
    ]
  }
}
```

</details>

## FAQ

### How do I share my sqlite db lessons so that lessons learned at work can be applied to home and vice-versa?

Sync them through a **private git repo** (GitHub, GitLab, any host) with `lessons sync`. The `.db` files never leave your machines: each sync writes their lessons as text, one JSON line per lesson (`work.jsonl`, `personal.jsonl`, `shared.jsonl`), merges those with the repo's copy, and updates the local `.db` from the result.

1. Create one private repo for the lessons, e.g. `<you>/<private-repo>`. If your work and personal GitHub accounts are separate, add the other account as a collaborator, so both can push. A gist does not work for this: only its owner can push, it cannot have collaborators, and a secret gist is readable by anyone who has its URL.
2. Copy [`docs/config.sample.toml`](docs/config.sample.toml) to `~/.lessons/config.toml` (your private copy; never commit it) and fill in one `[sync.<domain>]` section per database this machine should sync: the repo's `remote`, the `ssh_key` for it, and `update = "live"` (sync after each change) or `"on-demand"` (sync when reading, at most every `pull_every_minutes`). All three may name the same repo.
3. Run `lessons sync`, or let `update` trigger it in the background. `lessons` makes and manages its own clone under `~/.lessons/sync/`.

How the merge works:

- Each lesson is merged field by field against the last synced version, so edits to different fields on two machines are both kept.
- When both machines changed the same field differently, sync keeps one value and queues the other; nothing is dropped. `lessons resolve` then asks you per field: keep one, keep the other, combine both (tagged `needs-reword`, for you to reword later), or edit, or skip it to keep it queued.
- A sync never prompts: it runs in the background, so a passphrase or an unknown-host question makes it fail, not wait.

Guards:

- A public remote is refused for every database.
- When `work` has a repo of its own, sync refuses it if any other configured key can read that repo. With all three in one repo, it warns once per sync that work data is readable by everyone with access to that repo; check your employer's rules on its data before syncing `work` into a personal account's repo.

Do not point `LESSONS_HOME` at a cloud-drive folder (OneDrive, Google Drive, Dropbox). A sync client can copy a `.db` mid-write, or apart from its `-wal` file, which can corrupt it, and on a conflict it silently keeps one copy.

## Contributing

`cargo test` also enforces two repo rules: no DB file can be committed (`tests/gitignore.rs`), and no magic-number literal outside a named `const` (`tests/no_magic_numbers.rs`). `scripts/setup.sh --test` checks the setup script.

## Phase 2 (not built yet)

- Semantic search with sqlite-vec embeddings.
