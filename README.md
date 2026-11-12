# llm-lessons
> "...my sympathy got the better of my judgment, and I said: 'Isn't it a shame that with the tremendous amount of work you have done you haven't been able to get any results?' Edison turned on me like a flash, and with a smile replied: 'Results! Why, man, I have gotten a lot of results! I know several thousand things that won't work.'"
>
> Walter S. Mallory, quoted in Frank Lewis Dyer and Thomas Commerford Martin, *Edison: His Life and Inventions*, Vol. 2, Ch. 24 "Edison's Method in Inventing," pp. 615 to 616 (New York: Harper & Brothers, 1910). Full text: [Project Gutenberg #820](https://www.gutenberg.org/ebooks/820).

Your coding assistant fixed a similar pattern of bugs last week. Today it walks straight into it again: the rules you drilled into it and the mistakes it already made vanished with the last session. `lessons` gives it a memory that outlives the context window. Every problem goes into a small local database with its root cause, the fix, and the attempts that failed, so the next session, on any agent (opencode, Claude Code, anything that runs a shell command or speaks MCP) and any model, looks there during the planning stage, before implementation. Lessons sync across your machines through a git repo you own, so what you learn at work and at home stays learned.

It is only as good as what you feed it: record what each session learned, and every post-mortem, in the same session, every time. An empty or stale database teaches nothing.

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

`lessons sync` runs the `git` command, so git must be installed and on your PATH. Without it, sync fails with an error; every other `lessons` command still works. On Windows, use WSL2 and keep `~/.lessons` inside the Linux filesystem.

## Data: three files, never in git

| File | Holds |
| --- | --- |
| `work.db` | work problems; may belong to your employer |
| `personal.db` | home and personal problems |
| `shared.db` | generic lessons, useful anywhere |

They live in `~/.lessons/` (on Windows, inside WSL2), or wherever `LESSONS_HOME` points. They are separate files, not one table with a column, because only a missing file can keep work data off a personal machine and personal data out of work tooling. Three files is the design, not an accident: [why](#why-do-you-need-3-different-databases). Create only the ones a machine should hold, e.g. `lessons init --domain work` on a work laptop. Commands skip missing files.

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
lessons sync                                    # --domain shared: only that domain's repo
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

Sync them through a git repo (GitHub, GitLab, any host) with `lessons sync`. The `.db` files never leave your machines: each sync writes their lessons as text, one JSON line per lesson (`work.jsonl`, `personal.jsonl`, `shared.jsonl`), merges those with the repo's copy, and updates the local `.db` from the result.

> [!CAUTION]
> Keep any repo that holds **work** lessons **private**: your employer's rules on its data apply, and anything pushed stays in the history, and in forks and archives, even after you delete it. `shared` and `personal` are your call: if you have nothing to hide, a public repo lets others learn from your lessons. A repo's visibility is set in its Settings, under Danger Zone. While a synced repo is public, every `lessons` command prints a warning.

**Before you start**, create 1, 2 or 3 repos dedicated to the lessons DBs (private unless you chose otherwise above), not a gist or a cloud-drive folder (see [Why a git repo, and not a gist or a cloud drive?](#why-a-git-repo-and-not-a-gist-or-a-cloud-drive)).

**Scenario A: separate work and personal accounts.** Create the repo(s) with your personal account and add your work account as a collaborator (Settings > Collaborators), so both can push. Either one repo for all three DBs, or a work-only repo plus one for `personal` and `shared`; with a work-only repo, sync also checks that no other key on the machine can read it. Then, on each machine:

```sh
scripts/setup.sh --domains work,shared        # the work laptop: no personal.db there
scripts/setup.sh --domains personal,shared    # a home machine
```

**Scenario B: one account for work and personal.** One private repo is enough. On each machine:

```sh
scripts/setup.sh --domains work,personal,shared
```

In a terminal, `setup.sh` then asks, per domain, for the repo URL (use the real host name, `git@github.com:...`, not a `~/.ssh/config` alias, which sync does not read; Enter on the first question skips syncing that domain; after that the previous URL is offered, Enter reuses it so one repo can serve all three, and `-` skips the domain), the SSH key for it (from the keys in `~/.ssh`; domains sharing a repo share its key), `live` (meant to sync after each change; not built yet, run `lessons sync`) or `on-demand` (sync when reading), and `pull_every_minutes` (default 10). Sync commits use your global git name and email, so `git config --global user.name` and `user.email` must be set (without them every sync fails with git's "Author identity unknown"). It writes the answers to `~/.lessons/config.toml`, your private copy (never commit it), creating it if needed and otherwise only adding the missing `[sync.<domain>]` sections. To write the file by hand instead, start from [`docs/config.sample.toml`](docs/config.sample.toml).

Without a terminal (a script, CI), pass the answers as flags instead; they apply to every domain given: `scripts/setup.sh --domains work,personal,shared --remote git@github.com:<you>/<private-repo>.git --ssh-key ~/.ssh/id_ed25519` (add `--update on-demand` or `--pull-every-minutes N` to change the defaults, or `--no-sync` to skip sync). The new config is checked by loading it with `lessons search`, and put back as it was if it does not load.

After that, run `lessons sync` by hand (running it by itself in the background, as `update` says, is not built yet). `lessons` makes and manages its own clone under `~/.lessons/sync/`.

Prefer an ssh remote. A token written into an https URL is kept in plain text in `~/.lessons/sync/<slug>/.git/config` and shows in the process list while git runs; `lessons` never prints it.

How the merge works:

- Each lesson is merged field by field against the last synced version, so edits to different fields on two machines are both kept.
- When both machines changed the same field differently, sync keeps one value and queues the other; nothing is dropped. `lessons resolve` then asks you per field: keep one, keep the other, combine both (tagged `needs-reword`, for you to reword later), edit, or skip to keep it queued.
- A sync never prompts (it is meant to run in the background), so a passphrase or an unknown-host question makes it fail, not wait.

When `work` shares a repo with `personal` or `shared`, sync notes once per sync that work data is readable by everyone with access to that repo.

### Why do you need 3 different databases?

- **The leak.** One database with a `domain` column keeps every lesson on every machine, so a work lesson (a server name, a customer, a ticket) would sit on your home machine.
- **The missing file.** If you split machines and repos (Scenario A, with a work-only repo): a machine that never has `work.db` and never clones the work repo cannot expose it. Your home machine holds `personal.db` and `shared.db`; your work machine holds `work.db` and `shared.db`. Scenario B (all three on every machine) gives this up.
- **The sync.** Each file syncs to its own repo (keep `work` private), or all three share one. With one shared repo, everyone who can read it can read all three, so split `work` into a repo of its own if anyone else can read the shared one (see [how to share lessons](#how-do-i-share-my-sqlite-db-lessons-so-that-lessons-learned-at-work-can-be-applied-to-home-and-vice-versa)).
- **The search.** `lessons search --in personal,shared` reads only the files you name.

### Why does git store JSON lines, and never the `.db` itself?

An SQLite file is binary: a sequence of fixed-size pages holding B-trees. One new lesson can rewrite bytes in several pages, so git sees an opaque blob. It cannot show a readable diff, git cannot merge two machines' versions (one side has to be picked), and a commit can store a whole new copy, so the history can grow by the size of the database each time.

So each sync turns every lesson into one JSON line (`<domain>.jsonl`) and commits that text. The merge itself never runs in git: `lessons sync` merges in-process, field by field, three ways (against the version both machines last agreed on), then commits the result on top of the repo's latest commit, so every push is a plain fast-forward and there are no merge commits. The local `.db` is rebuilt from the merged result.

What pays off is syncing often (`update = "live"`, once background sync is built; until then run `lessons sync` often): the less each machine changes between syncs, the smaller each commit, the sooner a lesson shows up on your other machines, and the rarer a real conflict, which only happens when two machines change the same field of the same lesson differently.

### Why a git repo, and not a gist or a cloud drive?

Because only a regular git repo takes pushes from all your accounts and gives `lessons sync` a shared history to merge against. A cloud drive does not merge; a gist is owner-only.

Do not point `LESSONS_HOME` at a cloud-drive folder (OneDrive, Google Drive, Dropbox). A cloud drive syncs whole files. Two machines change the same `.db`. On a conflict the drive keeps one side, or leaves a conflicted copy that nothing merges. It can also copy a `.db` in the middle of a write, or apart from its `-wal` file, which can corrupt it.

A gist is a git repo, but only its owner can push. It cannot have collaborators. A secret gist is readable by anyone who has its URL.

With a git repo, `lessons sync` merges the JSON lines per lesson and per field. A real conflict only happens when two machines change the same field of the same lesson. Then nothing is dropped: `lessons resolve` asks you.

A repo that holds work lessons stays private because they may belong to your employer, and anything pushed stays in the history.

### Why does lessons merge on its own when git can merge?

It looks like duplicated work, because lessons also compares two versions against a common base, keeps conflicts for later, writes its files through a temp file and a rename, and stores lessons as sorted lines of text. Those are git's tools too.

But git merges lines of text, and one lesson is one line of JSON. Two machines that edit different fields of one lesson would conflict in git, and git would stop and wait for a person. Sync is meant to run in the background and cannot wait.

So lessons merges each lesson field by field, against the version both machines last agreed on, and keeps both edits. If both machines changed the same field, it keeps one value, queues the other, and asks you later with `lessons resolve`.

Everything else is git: lessons only fetches, commits on top of the remote tip, and pushes. It never runs `git merge` or `git pull`.

## Contributing

`cargo test` also enforces two repo rules: no DB file can be committed (`tests/gitignore.rs`), and no magic-number literal outside a named `const` (`tests/no_magic_numbers.rs`). `scripts/setup.sh --test` checks the setup script.

## Phase 2 (not built yet)

- Semantic search with sqlite-vec embeddings.
