# llm-lessons
> "...my sympathy got the better of my judgment, and I said: 'Isn't it a shame that with the tremendous amount of work you have done you haven't been able to get any results?' Edison turned on me like a flash, and with a smile replied: 'Results! Why, man, I have gotten a lot of results! I know several thousand things that won't work.'"
>
> Walter S. Mallory, quoted in Frank Lewis Dyer and Thomas Commerford Martin, *Edison: His Life and Inventions*, Vol. 2, Ch. 24 "Edison's Method in Inventing," pp. 615 to 616 (New York: Harper & Brothers, 1910). Full text: [Project Gutenberg #820](https://www.gutenberg.org/ebooks/820).

Your coding assistant fixed a similar pattern of bugs last week. Today it walks straight into it again: the rules you drilled into it and the mistakes it already made vanished with the last session. `lessons` gives it a memory that outlives the context window. Every problem goes into a small local database with its root cause, the fix, and the attempts that failed, so the next session, on any agent (opencode, Claude Code, anything that runs a shell command or speaks MCP) and any model, looks there during the planning stage, before implementation. Lessons sync across your machines through a git repo you own, so what you learn at work and at home stays learned.

It is only as good as what you feed it: record what each session learned, and every post-mortem, in the same session, every time. An empty or stale database teaches nothing.

Correct approaches change; mistakes do not. A fix that was right ten years ago may be superseded tomorrow, but an anti-pattern is still an anti-pattern. `lessons` is a record of failures, not a catalogue of best practices: it keeps each mistake beside what fixed it, and a new way of doing something right never makes an old mistake correct.

`lessons` is a small CLI that records each problem: what went wrong, why, what fixed it, and what was tried that did NOT work. A human or an LLM can then search it before debugging the same thing again. The failed attempts are kept on purpose: they are what saves a repeat of the same dead end.

## Install

### From a release

Each release on the [Releases page](https://github.com/HidekiAI/llm-lessons/releases) has two zips. Either way: unzip,
`cd` into the one directory it holds, run `install.sh`. Each zip carries this README, so the steps travel with it.

- `lessons-v<version>-linux-x86_64.zip`: a prebuilt binary, for **Debian x86_64 (64-bit) only**.
- "Source code (zip)": every other platform; it builds `lessons` with cargo.

```sh
unzip lessons-v0.1.0-linux-x86_64.zip
cd lessons-v0.1.0-linux-x86_64
bash install.sh --domains work,shared --agents opencode,claude
```

`install.sh` first checks the apps lessons needs: `git`, and for the source zip also a C compiler and `cargo` (with `curl` to fetch Rust when `cargo` is missing). If one is missing it lists them and installs nothing; rerun with `--install-depends` to let it install them with `apt-get` (through `sudo` unless you are root) or Homebrew, and Rust with rustup. On macOS the C compiler comes from `xcode-select --install`, which you run yourself. It then passes every other argument to `scripts/setup.sh`, described below.

The binary zip installs `lessons` to `~/.local/bin/lessons`. If `~/.local/bin` is not on your PATH, setup prints the line to add to your shell config: `export PATH="$HOME/.local/bin:$PATH"`. To upgrade, run `install.sh` from the newer zip, then restart your agents: an MCP server that is already running keeps the old binary until it restarts. The upgrading note below (DB migration) applies to a release too.

### From a clone

```sh
scripts/setup.sh --domains work,shared --agents opencode,claude   # all of the below, plus MCP registration and the `lessons` skill
```

Or by hand:

```sh
cargo install --path .        # puts `lessons` in ~/.cargo/bin
lessons init                  # creates the DB files (--domain work|personal|shared for one)
```

Upgrading: a newer `lessons` may migrate the `.db` files the first time it opens them, and an older `lessons` then refuses them ("newer than this lessons binary supports"). So install the new build at once for everything on that machine that runs `lessons` (the hook, the MCP server, your shell). Upgrade your other machines soon too: an older `lessons` ignores a field it does not know (such as `bypassed`) in synced lessons and writes them back without it, until a newer machine syncs again.

`setup.sh` requires `--domains`, so it never creates a file this machine should not hold. `--dry-run` shows the steps without running them.

For each agent in `--agents` that is installed, `setup.sh` also copies the shipped skill (`skills/lessons/SKILL.md`) into that agent's skills directory: `~/.claude/skills/lessons/` for Claude Code, and `$XDG_CONFIG_HOME/opencode/skills/lessons/` (default `~/.config/opencode/skills/lessons/`) for opencode. It never overwrites an existing `SKILL.md`, and it leaves the skill alone when the skill directory or any directory above it, up to your home directory, is a symlink (a symlink usually points into another repository, such as a dotfiles repo, and a copy would put a file there). A symlink above your home directory is not looked at. `--no-skills` skips the step (and the plugin below). The skill teaches an agent to search the lessons before a task and to record one after a fix, through the `lessons` CLI, so it works without the MCP server too.

For opencode, `setup.sh` also copies the hook plugin `integrations/opencode/lessons-hook.js` into `$XDG_CONFIG_HOME/opencode/plugins/` (default `~/.config/opencode/plugins/`), with the same guards (never overwrites, leaves a symlinked directory alone; then link the file there yourself). Before each message the plugin runs `lessons hook user-prompt-submit` and adds the lessons it prints to the message; it stays silent when nothing is relevant or `lessons` is missing. Test it with `node --test integrations/opencode/lessons-hook.test.js`.

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
lessons bypassed <id>                           # you had to remind the agent of it (alias: remind)
lessons retire <id> --by <newer-id>             # hide from search
lessons move <id> --to shared
lessons sync                                    # --domain shared: only that domain's repo
```

`lessons bypassed <id>` counts the times you had to remind an agent of a lesson it was already given ("I already told you ..."). Only you decide: an agent runs it when you ask, never on its own, and no hook bumps it. An agent that catches its own slip against a lesson tells you (the id and what slipped), so you can still count a slip you missed. `show` prints the count next to the hits, and the JSON and MCP output carry it as `bypassed`. Each bump also records when it happened, as `last_bypassed_at` (UTC); `show` adds `(last <time>)` once a lesson has been bumped, so "10 times, last one 3 months ago" reads differently from "10 times, last one today". A lesson you keep having to remind the agent of is also a hint that the session's context has grown too long and should be compacted or restarted.

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

`lessons mcp` serves two tools over stdio to any MCP client: `search_lessons` and `record_lesson`. Two only, on purpose: every tool's schema is loaded into the agent's context. Both refuse domains not listed in `[mcp] domains`, and their schemas offer only the listed ones, each once (no tool at all if the list is empty). A `config.toml` that does not load makes `tools/list` fail with a JSON-RPC error that carries the parse message.

```sh
opencode mcp add --global lessons -- lessons mcp
claude mcp add --scope user lessons -- lessons mcp
```

### Optional: prompt hook

Both agents can search on every prompt: `lessons hook user-prompt-submit` prints matching lessons, which the agent adds as context, prints nothing when none match, and always exits 0, so it never blocks a prompt. A lesson matches only when it shares at least four ordinary words with the prompt, or one identifier-like term (`not_found_or_no_access`, `src/mcp.rs`, `v1.2.3`), as whole words (no stemming, so `failing` does not match `failed`); so chat prompts and prompts of one ordinary word print nothing. Matching lessons are printed in this order: those whose context names the current directory first, then the ones with the highest `bypassed` count, then by search score; the count only reorders lessons that already match, it never adds one. Claude Code runs it from a hook entry (below); opencode runs it from the plugin that `scripts/setup.sh --agents opencode` installs (see Install).

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

## Troubleshooting

- **opencode shows the lessons plugin as failed** (`Plugin must export a default definition...`, or it keeps failing after you fixed the file): the opencode background service keeps the plugin it loaded when it started and does not re-read a changed file. Run `opencode service restart`, then open a new session and check `/plugins` and `/mcp`. The log entry named by the error reference is in `~/.local/share/opencode/log/opencode.log`.
- **The hook prints nothing in either agent**: expected for chat prompts, see the matching rule above. Check that `lessons` is on the PATH the agent sees, or use its absolute path.
- **`lessons sync` fails with `Permission denied (publickey)` or "Could not read from remote repository"**: the `ssh_key` named in `[sync.<domain>]` (or the default key git falls back to) is not authorized for that remote. Diagnose with `lessons sync --domain <domain> --verbose` (it prints the exact `git ls-remote` command and stderr), then check the key against the remote: `ssh -i <ssh_key> -o IdentitiesOnly=yes -T git@github.com` must print `Hi <user>! You've successfully authenticated`, and `GIT_SSH_COMMAND="ssh -i <ssh_key>" git ls-remote <remote>` must list the branches. Common causes: the key is simply not registered on the repo (add it as a deploy key or collaborator), the `ssh_key` is a `.pub` whose private half only an agent holds and the agent is not running, or the private key has a passphrase that sync cannot supply (a sync never prompts). Domains that share a remote must share its `ssh_key` on one machine: `lessons` refuses a config where two such domains name different keys (`[sync.work] and [sync.shared] use the same remote ... with different ssh_key values`), so change them together (setup asks "Keep sync for <domain> as-is? (y/n)"; or edit the config).

## FAQ

### How do I share my sqlite db lessons so that lessons learned at work can be applied to home and vice-versa?

Sync them through a git repo (GitHub, GitLab, any host) with `lessons sync`. The `.db` files never leave your machines: each sync writes their lessons as text, one JSON line per lesson (`work.jsonl`, `personal.jsonl`, `shared.jsonl`), merges those with the repo's copy, and updates the local `.db` from the result.

> [!CAUTION]
> Keep any repo that holds **work** lessons **private**: your employer's rules on its data apply, and anything pushed stays in the history, and in forks and archives, even after you delete it. `shared` and `personal` are your call: if you have nothing to hide, a public repo lets others learn from your lessons. A repo's visibility is set in its Settings, under Danger Zone. While a synced repo is public, every `lessons` command but `init` prints a warning, and a `lessons sync` that includes `work` refuses to run against it. A work-only repo must be an ssh remote (`git@host:path`): an https or local remote is refused, because the key guard compares ssh keys.

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

In a terminal, `setup.sh` then asks, per domain, for the repo URL (use the real host name, `git@github.com:...`, not a `~/.ssh/config` alias, which sync does not read; Enter on the first question skips syncing that domain; after that the previous URL is offered, Enter reuses it so one repo can serve all three, and `-` skips the domain), the SSH key for it (from the keys in `~/.ssh`; domains sharing a repo share its key), `live` (meant to sync after each change; not built yet, run `lessons sync`) or `on-demand` (sync when reading), and `pull_every_minutes` (default 10). If `~/.lessons/config.toml` already has a `[sync.<domain>]` section, it shows the current remote/key and asks **Keep sync for <domain> as-is? (y/n)**; answering `n` re-prompts and rewrites that section. `scripts/setup.sh --debug` logs `DEBUG:` lines to stderr alongside the normal log. Sync commits use your global git name and email, so `git config --global user.name` and `user.email` must be set (without them every sync fails with git's "Author identity unknown"). It writes the answers to `~/.lessons/config.toml`, your private copy (never commit it), creating it if needed and otherwise only adding the missing `[sync.<domain>]` sections. To write the file by hand instead, start from [`docs/config.sample.toml`](docs/config.sample.toml).

Without a terminal (a script, CI), pass the answers as flags instead; they apply to every domain given: `scripts/setup.sh --domains work,personal,shared --remote git@github.com:<you>/<private-repo>.git --ssh-key ~/.ssh/id_ed25519` (add `--update on-demand` or `--pull-every-minutes N` to change the defaults, or `--no-sync` to skip sync). The new config is checked by loading it with `lessons search`, and put back as it was if it does not load.

After that, run `lessons sync` by hand (running it by itself in the background, as `update` says, is not built yet). `lessons` makes and manages its own clone under `~/.lessons/sync/`.

`<slug>`: the clone for a remote lives at `~/.lessons/sync/<slug>/`, where `<slug>` is the normalized remote encoded byte by byte: letters and digits are kept (lowercased), `-` is kept, and every other byte becomes `_` plus its hex code. Normalization trims the remote, drops http(s) credentials, lowercases the scheme and host but not the path, removes a trailing `/` and `.git`, and rewrites `ssh://user@host/path` without a port in scp form. So `git@github.com:HidekiAI/llm-lessons-dbs.git` clones to `~/.lessons/sync/git_40github_2ecom_3ahidekiai_2fllm-lessons-dbs/`. Domains that share a remote share the clone ("one clone has one key"); domains with different remotes each get their own. An encoded name longer than 200 bytes is cut there and suffixed with `-` and 16 hex digits. `ls ~/.lessons/sync/` lists every clone, and `--verbose` logs `config: [sync.<domain>] -> clone of <remote>` per configured domain, where `<remote>` is that normalized, credential-free clone key.

Prefer an ssh remote. A token written into an https URL is kept in plain text in `~/.lessons/sync/<slug>/.git/config` and shows in the process list while git runs; `lessons` never prints it.

How the merge works:

- Each lesson is merged field by field against the last synced version, so edits to different fields on two machines are both kept.
- When both machines changed the same field differently, sync keeps one value and queues the other; nothing is dropped. `lessons resolve` then asks you per field: keep one, keep the other, combine both (tagged `needs-reword`, for you to reword later), edit, or skip to keep it queued.
- A sync never prompts (it is meant to run in the background), so a passphrase or an unknown-host question makes it fail, not wait.

When `work` shares a repo with `personal` or `shared`, sync notes once per sync that work data is readable by everyone with access to that repo.

### Why do you need 3 different databases?

- **The leak.** One database with a `domain` column keeps every lesson on every machine, so a work lesson (a server name, a customer, a ticket) would sit on your home machine.
- **The missing file.** If you split machines and repos (Scenario A, with a work-only repo): a machine that never has `work.db` and never clones the work repo cannot expose it. Your home machine holds `personal.db` and `shared.db`; your work machine holds `work.db` and `shared.db`. Scenario B (all three on every machine) gives this up.
- **The sync.** Each file syncs to its own repo (keep `work` private), all three share one, or `work` gets its own repo and `personal` and `shared` share another. With one shared repo, everyone who can read it can read all three, so split `work` into a repo of its own if anyone else can read the shared one (see [how to share lessons](#how-do-i-share-my-sqlite-db-lessons-so-that-lessons-learned-at-work-can-be-applied-to-home-and-vice-versa)).
- **The search.** `lessons search --in personal,shared` reads only the files you name.

### Why does git store JSON lines, and never the `.db` itself?

An SQLite file is binary: a sequence of fixed-size pages holding B-trees. One new lesson can rewrite bytes in several pages, so git sees an opaque blob. It cannot show a readable diff, git cannot merge two machines' versions (one side has to be picked), and a commit can store a whole new copy, so the history can grow by the size of the database each time.

So each sync turns every lesson into one JSON line (`<domain>.jsonl`) and commits that text. The merge itself never runs in git: `lessons sync` merges in-process, field by field, three ways (against the version both machines last agreed on), then commits the result on top of the repo's latest commit, so every push is a plain fast-forward and there are no merge commits. The local `.db` is then updated with the merged rows.

**Is rebuilding SQLite from text not wasted CPU?** Only sync reads the JSON lines, and sync is its own step. Every command reads and writes the SQLite file directly, so no search, add or prompt hook parses a line. A cycle writes only the rows that changed (an unchanged lesson is not rewritten, and `hits` is a value, not a sum), so its cost follows your edits, not the size of the database. A full build from the lines happens once, on a machine that has no `.db` yet, which is how a fresh machine gets its lessons. The text is also cheap at this size: a thousand lessons is about a megabyte, and a line parses in microseconds. Once the background trigger lands (`update = "live"`), a burst of searches still costs one pull, because a read pulls at most once every `pull_every_minutes` (10 by default). What the text buys is what a binary cannot give: a readable diff per lesson, a per-field merge between machines, and history that grows by the lessons you changed rather than by another copy of the whole database. One realistic lesson is about 1 KB, so a gigabyte would be a million lessons; if a database ever does reach tens of megabytes, the fix is to shard the file by year, not to go back to a binary.

What pays off is syncing often (`update = "live"`, once background sync is built; until then run `lessons sync` often): the less each machine changes between syncs, the smaller each commit, the sooner a lesson shows up on your other machines, and the rarer a real conflict, which only happens when two machines change the same field of the same lesson differently.

**Why not CSV, or a `sqlite3` dump?** Because neither is loadable *and* mergeable, and the file has to be both:

| Format | `sqlite3` can load it | One lesson per line | Named fields, so a per-field merge |
| --- | --- | --- | --- |
| `<domain>.jsonl` (ours) | no, `lessons sync` rebuilds the database from it | yes | yes |
| `sqlite3 db .dump` (SQL) | yes, it is SQL | yes | no: `INSERT INTO lesson VALUES (...)` is positional, so one added column silently misplaces every old line, and one lesson is still one line, so two machines editing different fields of it conflict |
| `.mode json` (one array) | no, `sqlite3` has no JSON importer | no: the whole file is one line, so any change rewrites it | no |
| CSV (`.import`) | yes, CSV or ASCII only, never JSON (checked on 3.51.2) | only until a field holds a newline, which quoted log text in `problem` does | no: `.import` cannot spell NULL, so `None` and `''` collapse and a machine manufactures a change and a conflict |

A CSV import also does not produce a database `lessons` will use: `.import` creates a table of TEXT columns with no search index, no triggers and `user_version = 0`, so the schema rebuild refuses it. The supported restore is `lessons sync`, which rebuilds schema, index and triggers from the code and fills them from the JSON lines.

CSV is still a fine place to *export* to, for a human with a spreadsheet; that belongs in an `export` command, not in the stored format. Today one `jq` gets you there: `jq -r '[.id,.title,.problem] | @csv' shared.jsonl`.

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

### Does `lessons sync` pull the latest JSONL files automatically?

Yes in effect, but not with a literal `git pull`. Each sync cycle runs `git fetch origin` (no throttle on the explicit command), reads the remote `<domain>.jsonl` at the fetched tip, three-way merges it into your local database field by field, then resets its clone to the remote tip, commits and pushes a fast-forward. So running `lessons sync` is all you need: there is no reason to `cd ~/.lessons/sync/<slug>/ && git pull` yourself.

`pull_every_minutes` (default 10) does not gate the explicit `lessons sync` command. It belongs to the automatic background sync, which is not built yet; until then, run `lessons sync` by hand.

### What happens when I change a sync remote? (planned)

A changed `remote` in `[sync.<domain>]` gets a new clone slug under `~/.lessons/sync/`. Today that new slug starts from an empty BASE, so deletions stop propagating. The planned recovery (not yet shipped) makes the next `lessons sync` adopt the previous clone's last synced commit when that commit exists in the new remote, so the three-way merge still propagates deletions and keeps one-sided edits out of the conflict queue. When the commit is not in the new remote, sync uses the empty-BASE union; no lesson is lost. `--verbose` logs which commit was adopted and from which slug, and reports when it falls back to the empty-BASE union.

The old clone is left in place. To clean up, run `lessons --verbose search`; its `config: [sync.<domain>] -> clone of ...` lines name the remotes still in use. Keep the clone for each listed remote (its slug is that remote encoded as described above); back up and remove every other `<slug>/` directory and `<slug>.*` file under `~/.lessons/sync/`.

### How do I import my existing SQLite lessons into another machine whose old sync repos no longer exist?

Copy the `.db` file, point the config at a new repo, then run `lessons sync`. There is no `import` command, and none is needed: the `.db` is the data, and the repo only carries a text copy of it.

1. **Copy the files** of the domains that machine should hold (`work.db`, `personal.db`, `shared.db`) into its `~/.lessons/`, e.g. `scp ~/.lessons/personal.db ~/.lessons/shared.db newhost:~/.lessons/`. Run no `lessons` command on either side while you copy. SQLite runs these files in WAL mode, so also copy any `<name>.db-wal` and `<name>.db-shm` beside each file, or the newest lessons are left behind.
2. **Point the config at a repo that exists.** In `~/.lessons/config.toml` on the new machine, replace the dead `remote` of each `[sync.<domain>]` with a new private repo (an empty one is fine) and check `ssh_key`. Keep a work repo private.
3. **Run `lessons sync`.** A new remote gets a new `<slug>` clone under `~/.lessons/sync/`, so there is no earlier synced version to compare against, and the first sync is a union: every local lesson is written to the repo as `<domain>.jsonl` and pushed. Nothing is dropped.
4. **Check the count:** `lessons list --limit 1000 | wc -l` before and after the sync, on both machines.

Then delete the old `~/.lessons/sync/<slug>/` clones of the dead remotes. Nothing reads them once the config no longer names them.

**The new machine already has lessons of its own? Two `.db` files are never merged directly**: there is no command for it, and copying one over the other replaces it. The only merge is `lessons sync` through a repo, so put both sets of lessons through the same repo:

1. On the new machine, sync its own `.db` to the new repo as in step 3 above.
2. Give the old `.db` a temporary home of its own, with a config naming the SAME repo and key: `mkdir /tmp/old-lessons`, copy the old `shared.db` (plus its `-wal` and `-shm`) and a `config.toml` with only `[sync.shared]` into it.
3. Run `LESSONS_HOME=/tmp/old-lessons lessons sync --domain shared`. This pushes the old lessons into the same `shared.jsonl`, merged with what step 1 pushed.
4. Run `lessons sync` on the new machine. It pulls the old lessons into its `shared.db`.
5. Check the counts with `lessons list --limit 1000 | wc -l`, then delete `/tmp/old-lessons`.

Every lesson has its own ULID, so two databases that grew apart share no ids and the merge is a plain union. The same id on both sides with different text in one field cannot be told apart without a base: sync keeps one value and queues the other, and `lessons resolve` asks you per field. Do not replay the old lessons with `lessons add --json -`: `add` always makes a new id, so any lesson both sides already hold is duplicated.

**Will the same problem, recorded on both machines, collide?** Not as a conflict: the two lessons have different ids, sync compares ids and never the title or text (checked in the source, not by running a merged pair), so both survive and `lessons search` returns both. After the merge, search the recurring topics (`lessons search "<error text>"`) and run `lessons retire <old-id> --by <kept-id>` on each pair: it hides the duplicate from search and deletes nothing. There is no automatic duplicate finder yet; a `lessons dedupe` that finds near-duplicate pairs and merges them is requested in [#35](https://github.com/HidekiAI/llm-lessons/issues/35).

**Why not `INSERT` the rows with `sqlite3`?** A hand-made database skips the FTS5 search index, the triggers and the schema version that `lessons` creates, so the schema check refuses it or search finds nothing (see the CSV row in [Why does git store JSON lines](#why-does-git-store-json-lines-and-never-the-db-itself)).

### Why are there no extra indexes in the SQLite files?

Every query that runs often already has one:

| Query | Index it uses |
|---|---|
| `show`, `hit`, `bypassed`, `update`, `retire`, `move`, sync deletes (`WHERE id = ?`) | the unique index SQLite builds for `id TEXT NOT NULL UNIQUE` |
| sync export (`ORDER BY id`) | the same unique index |
| `search` and the prompt hook (`MATCH`, ranked by score) | the FTS5 full-text index `lesson_fts` |
| the search join back to the lesson row | `seq INTEGER PRIMARY KEY`, the row id itself |

Two reads touch every row. The sync export has to, since it writes them all, in the unique index's `id` order. `lessons list` filters tags with a text search inside the `tags` column, which no index can speed up, then sorts by `created_at`. A lessons file holds hundreds to a few thousand short rows, and that scan has not shown up as slow; it has not been measured either. The counters (`hits`, `bypassed`, `last_bypassed_at`) are never used to filter rows, so they need no index. Add an index on `created_at` only if `list` becomes measurably slow: every extra index also slows each write a little and grows the file.

### Won't the prompt get bigger if the entire sqlite rows were cached?

No: no rows are cached in the prompt, and the DB never enters it whole. The DB stays on disk, and each way in returns only a few matching lessons:

| Path | What reaches the agent | Bound |
|---|---|---|
| Prompt hook (`lessons hook user-prompt-submit`) | lessons that pass the relevance filter (four shared ordinary words, or one identifier-like term), one block each: the first line of its title, problem, root cause, fix and failed tries | `max_results` lessons (default 5), each line cut at 300 characters, the whole output cut at 8,000 characters |
| MCP `search_lessons` | the matching lessons as JSON, every field in full, only when the agent calls the tool | `limit` lessons, default `max_results` (5); no character cap |
| CLI `lessons search` | matching lessons, every field in full, only when run | `--limit` lessons, default 10; no character cap |

Only the hook adds text to the prompt on its own, and a prompt that matches nothing gets nothing added (see the prompt hook section for why chat prompts and one-word prompts match nothing). Its size depends on its caps, not on how many lessons the files hold: 50 lessons or 50,000 add at most 8,000 characters, about 2,000 tokens at the common rule of thumb of 4 characters per token (an estimate; tokenizers differ). MCP and the CLI cap the number of lessons, not their length, so a lesson with a long fix returns the whole fix; the agent sees that output only when it asked for it.

## Contributing

`cargo test` also enforces two repo rules: no DB file can be committed (`tests/gitignore.rs`), and no magic-number literal outside a named `const` (`tests/no_magic_numbers.rs`). `scripts/setup.sh --test` checks the setup script.

## Phase 2 (not built yet)

- Semantic search with sqlite-vec embeddings.
- `lessons dedupe`: find near-duplicate lessons and merge each confirmed pair into one row ([#35](https://github.com/HidekiAI/llm-lessons/issues/35)).
