# PLAN 2026-11-17: release zips and install.sh (issue #10)

TL;DR: a tag `v<Cargo version>` publishes a GitHub pre-release with two zips that are both run as "unzip, run
`install.sh`". The binary zip (Debian x86_64) carries a prebuilt `bin/lessons`. The source zip is the one GitHub
attaches to every release, and it builds the binary. `install.sh` lists the missing system apps, installs them only when
given `--install-depends`, then hands off to
the existing `scripts/setup.sh`. The release logic (tag check, notes range, pre-release flag) lives in
`scripts/release-meta.sh` so it has `--test` checks with mutations. Detail below.

Current step: R1-R3 committed, reviewed and pushed; R4-R6 workflow committed and reviewed, first run pending. TODO, in order: R4-R6 workflow run green on the temporary branch trigger, then REMOVE that `branches:` trigger before merging; R7 README update (@me 2026-11-19: "make sure to add a todo in the list to update README")

## Requirements

From issue #10 (the ticket):

Quoted from the live ticket (`gh issue view 10 --repo HidekiAI/llm-lessons`, read 2026-11-18):

- T1 "Pushing a git tag (e.g. `v0.1.0`) triggers a build and publishes a GitHub Release with one bundle artifact (e.g.
  `lessons-v0.1.0.tar.gz` containing the binary, MCP entrypoint, skill, plugin, and `setup-from-bundle.sh`)."
- T2 "`setup-from-bundle.sh` installs from the bundle on a machine with no Rust toolchain and no source clone: binary on
  `PATH`, skill copied per agent, MCP registered, DB initialized."
- T3 "`setup-from-bundle.sh --test` runs the same assertions as `scripts/test-debian.sh` against the bundle."
- T4 "Release notes list the version and what changed (Conventional Commits since the last tag)."
- T5 "The bundle ships a binary for **Debian 64-bit only**. ... If you want it on another target, you build it yourself:
  `cargo build --release && scripts/setup.sh`."

Test layer: every check below is a shell `--test` (the unit-level layer for shell). The workflow parts (docker, zip,
`gh release create`) cannot be unit-tested; a `workflow_dispatch` run of the branch exercises them (R4-R6 "Done when").

Decided with @me, 2026-11-17 (folded into #10; @me rewords the ticket):

- D1 One installer flow: the zip ships `install.sh` at its root, and the existing `scripts/setup.sh` stays the step
  that installs the binary, the skill, the plugin, the MCP entries and the DBs. No `setup-from-bundle.sh`.
- D2 A prebuilt binary goes to `~/.local/bin/lessons`; setup warns when that directory is not on `PATH`.
- D3 First tag `v0.1.0`, from `Cargo.toml`'s `0.1.0` (MEASURED: `Cargo.toml:3`); not ready for 1.x, so every `0.x`
  release is a pre-release.
- D4 Issue #9 duplicates #10; closed later with a comment @me words.
- D5 (@me 17:11) "2 kinds of installer, one as binary install, and another as install.sh that will build `cargo build
  --release` (will also install rustc, gh, git, etc if missing)". Answers: runs from the unpacked tree; system packages
  via apt (Debian) or Homebrew (macOS), else list what is missing and stop; no `gh` (MEASURED: `grep -rn '"gh"' src`
  finds no call).
- D6 (@me 17:13) "it is just a zip package which user unzips, runs the install.sh, and it'll install all necessary
  dependant apps". So both kinds are zips holding `install.sh`. Holds with one extra step: GitHub's automatic source zip
  unpacks into a top-level `llm-lessons-<ver>/` (UNVERIFIED, GitHub's documented archive layout), so `install.sh` is not at
  the zip root. For consistency our binary zip also has one top-level directory, `lessons-v<ver>-linux-x86_64/`. The
  README steps for both are "unzip, `cd` into the one directory, `bash install.sh`".
- D7 (@me 17:14) "for Debian, I prefer appImage over brew or other package installation". Asked whether that means
  lessons as an AppImage or the dependencies as AppImages; @me chose neither: keep the zip, no AppImage, apt installs
  only a missing `git` (and the build tools for the source zip).
- D8 The zip ships `LICENSE` (MIT, MEASURED: `LICENSE:1`). @me 2026-11-17: "add license = "MIT" to Cargo.toml too";
  done in this branch (`Cargo.toml:5`).
- D9 (@me 2026-11-19) "have install.sh have `--install-depends` or something, which can then install its dependant apps
  that ISN'T `lessons` specific"; then "go with --install-depends". So `install.sh` installs system apps only when given
  `--install-depends`; without it, a missing app is listed with the rerun command and nothing is installed (no `sudo`
  without consent). This narrows D6: "unzip, run `install.sh`" installs the apps only on the flagged run. Lessons-specific
  steps stay in `setup.sh` (D1).

Premises, each labelled:

- MEASURED: `rusqlite` is `bundled` (`Cargo.toml:12`), so a build needs a C compiler and not OpenSSL.
- MEASURED: `git` is needed only by `lessons sync` (README line 35).
- MEASURED: the MCP entrypoint is the binary itself, `lessons mcp` (`src/main.rs:315`), so the zip has no separate MCP file.
- MEASURED: `REPO_ROOT` is the parent of `scripts/` (`scripts/setup.sh:25`), so in the unpacked zip it is the zip root,
  and `SKILL_SOURCE` / `PLUGIN_SOURCE` (`setup.sh:32,36`) resolve to the zip's `skills/` and `integrations/`.
- MEASURED: the toolchain here is `rustc 1.95.0` (`rustc --version`) and `Cargo.toml:6` has `rust-version = "1.89"`, so the
  build image is pinned `rust:1.95-bookworm`. MEASURED 2026-11-19: the tag exists (`docker pull rust:1.95-bookworm`), its
  `CARGO_HOME` is `/usr/local/cargo` (the workflow's cache path), and it has `git` and `unzip` but no `zip` or `zstd`.
- MEASURED 2026-11-19: glibc 2.36 in `rust:1.95-bookworm` (`docker run --rm rust:1.95-bookworm ldd --version`).
  UNVERIFIED (checked by the smoke job, R5): that the binary runs on Debian 12 and 13 (`debian:bookworm-slim`,
  `debian:trixie-slim`).
- MEASURED: nothing else uses `curl` (`grep -rn curl scripts src` is empty), so only the rustup fetch needs it.
- UNVERIFIED: a fresh `debian:bookworm-slim` has empty apt lists, so `apt-get install` fails without `apt-get update` first.
- MEASURED 2026-11-19: `debian:bookworm-slim` lacks `git`, `cc`, `cargo` and `curl` (a real `bash install.sh` there listed
  all four as missing); R5 still asserts it before installing.
- UNVERIFIED: rustup `--profile minimal` is enough for `cargo install --locked --path` (no rustfmt or clippy are needed,
  and `bundled` needs only `cc`); the source smoke run proves it.
- MEASURED 2026-11-19: `rust:1.95-bookworm` has no `zip` (R4 installs it); UNVERIFIED for `debian:bookworm-slim`, where
  R5 installs `unzip` first, so it does not matter.
- UNVERIFIED: GitHub's automatic "Source code (zip)" keeps `install.sh`'s exec bit; the README says `bash install.sh`, which does not need it.

### Why T3 cannot hold word for word

`scripts/test-debian.sh` runs, in a Debian container: `grep PRETTY_NAME /etc/os-release`, `git --version`, `cargo test -q`
(the whole suite, two tests of which run `git ls-files`). A zip has no source, no `.git` and no Cargo, so `cargo test`
cannot run against it. Mapping each assertion:

| `test-debian.sh` assertion | In the release flow |
|---|---|
| runs on Debian (`os-release`) | smoke job prints `PRETTY_NAME` of `debian:bookworm-slim` and asserts `ID=debian` (R5) |
| `git --version` works | the smoke job asserts `git --version` after `install.sh` installed it (R5) |
| `cargo test -q` passes | CI runs `cargo test --locked` (@me 2026-11-19 tried `--release`; MEASURED: the release build after it still recompiled rusqlite and lessons, so reverted) in the Debian build image BEFORE packaging (R4); the binary tested is the one built |
| `git ls-files` tests | stay in `cargo test` in CI (a git checkout there); not reachable from a zip |
| (new) the bundle itself installs | smoke job: `install.sh`, then `lessons --version`, then plain `scripts/setup.sh --test` with no `LESSONS_BIN`, so its fallback to the zip's `bin/lessons` is what runs (R5) |

The ticket's wording is reworded by @me; this plan keeps its intent: every `test-debian.sh` assertion has a home.

## Steps

| Step | Change | Serves | Done when |
|---|---|---|---|
| R1 | `scripts/setup.sh` `run_setup` (line numbers in this row and in Provenance are from before this change, as blamed; cargo check at line 757, `cargo install` at 765): when `$REPO_ROOT/bin/lessons` is an executable file (`-f` and `-x`: `-x` alone accepts a directory), `install -m 755` it to `~/.local/bin/lessons`, set `lessons_binary` to `~/.local/bin/lessons` (today it is derived from `CARGO_INSTALL_ROOT`/`CARGO_HOME`/`~/.cargo`, lines 766-767), warn when `~/.local/bin` is not on `PATH`, and skip the cargo check; else `cargo install` as today. `run_sync_self_test` (line 959; its `lessons_binary` default is line 960) defaults `LESSONS_BIN` to `bin/lessons` when that exists | T2, D1, D2 | `setup.sh --test` green; new checks red under their mutations |
| R2 | `install.sh` (repo root): checks `git`; when `bin/lessons` is absent also `cc`, and `curl` only when `cargo` is also absent (rustup fetch). Missing ones without `--install-depends` (all of them gated alike: the apt or brew packages and a missing `cargo`, so rustup too): list them, print the same command with the flag added, exit 1, install nothing. Missing ones with it: `apt-get update` then `apt-get install -y` of the missing ones among `git curl ca-certificates build-essential` (with `sudo` unless root) on Debian, `brew install` of the missing ones on macOS; no `cc` on macOS prints `xcode-select --install` and exits 1; other OS lists what is missing and exits 1. No `cargo` and no `bin/lessons`: rustup with `-y --profile minimal`, then `. ~/.cargo/env`. Then `exec scripts/setup.sh` with every argument except `--install-depends`, which `setup.sh` would refuse as unknown (MEASURED: `scripts/setup.sh:1322` prints `setup: unknown option`). `--test` uses fake tools on `PATH` (the shell test layer for D5's install rules) | T2, T5, D1, D5, D6, D7, D9 | `bash install.sh --test` green; each check red under its mutation |
| R3 | `scripts/release-meta.sh` with subcommands `check-tag <tag> <Cargo.toml>`, `notes <tag>`, `prerelease <version>`; checks behind `--test` (repo convention, as `scripts/setup.sh --test`) | T1, T4, D3 | `bash scripts/release-meta.sh --test` green; each check red under its mutation |
| R4 | `.github/workflows/release.yml` build job: pinned `rust:1.95-bookworm`, a cargo cache keyed on `Cargo.lock` only (@me 2026-11-19, from the lenzu workflow), `apt-get update && apt-get install -y zip` (UNVERIFIED zip is preinstalled), `release-meta.sh check-tag`, `cargo test --locked`, `cargo build --release --locked`, zip `lessons-v<ver>-linux-x86_64.zip` with top-level directory `lessons-v<ver>-linux-x86_64/` holding `install.sh`, `bin/lessons`, `scripts/setup.sh`, `skills/`, `integrations/opencode/lessons-hook.js`, `README.md`, `LICENSE`. A `concurrency` group keyed on the tag, `cancel-in-progress: false`: it only serialises a RE-PUSH of the same tag (distinct tags build independently and never touch each other's release); it serves T1 only through that re-push and is the one guard not taken from a ticket line | T1, T3, T5, D8 | `actionlint` if installed, else manual review; the zip listing in the `workflow_dispatch` run shows all seven entries |
| R5 | Smoke job (`needs` build): unzip in `debian:bookworm-slim`, assert `git` absent, `bash install.sh --install-depends --domains shared --no-sync`, then `lessons --version` and `scripts/setup.sh --test` (T3 map above). A second smoke job tests the source path on the SAME tree a release would hold: before a release exists the source tree is `git archive --prefix=llm-lessons-<ver>/ HEAD` of the CI checkout (GitHub's own archive has the same top-level directory), unzipped and run the same way | T2, T3, D6 | green on a `workflow_dispatch` run of the branch; no test tag is pushed |
| R6 | Release job, tags only, with `permissions: contents: write` (the default token cannot create a release): `release-meta.sh notes` into the notes file, `release-meta.sh prerelease` picks `--prerelease`, `gh release create`, binary zip attached | T1, T4, D3 | the `workflow_dispatch` run prints the notes and the flag and skips the release |
| R7 | README "Install" (line 14): the two zips, "unzip, `cd` into the one directory, `bash install.sh`", that `--install-depends` lets it install missing system apps (D9), the Debian-only binary, the `~/.local/bin` note with the `export PATH="$HOME/.local/bin:$PATH"` line setup prints when it is missing, and the note to restart agents after an upgrade (a running MCP keeps the old binary, see this plan's "Race and non-determinism" paragraph) | T5, D2, D6, D9 | grep finds the section |

## Tests

`setup.sh --test` (temp `HOME`, a fake `cargo` on `PATH` that records its calls):

| Check | Serves | Mutation that must turn it red |
|---|---|---|
| with a fake `bin/lessons`, setup installs it to `~/.local/bin` and never calls `cargo` | T2, D2 | delete the `bin/lessons` branch |
| the MCP registration and `init` use `~/.local/bin/lessons`, not the cargo path | T2, D2 | leave `lessons_binary` on the cargo path |
| back-to-back: run setup twice with a fake `bin/lessons`; both exit 0 and `~/.local/bin/lessons` is executable (mode 755) and has the fake's content both times | T2, D2 | `cp -n` (second run skips, a newer binary never lands: use a changed fake on run two); or plain `cp` (keeps the fake's mode 700; MEASURED on macOS BSD `install`: without `-m` it already gives 755, so that mutation cannot go red; GNU `install` UNVERIFIED) |
| without `bin/lessons`, setup still calls `cargo install` | T5 | invert the detection test |
| `~/.local/bin` missing from `PATH` (a lookalike `~/.local/bin2` on it does not count) logs the warning and the `export PATH` line; present, none | D2 | delete the warning; match `$PATH` by substring instead of colon-fenced entries |
| `bin/lessons` a directory, or a regular file without the exec bit: setup calls `cargo install` | T5 | drop the `-f` test; drop the `-x` test |

`install.sh --test` (fake `uname`, `apt-get`, `sudo`, `brew`, `curl` on `PATH`, each logging its arguments). Rows that
install run with `--install-depends`:

| Check | Serves | Mutation that must turn it red |
|---|---|---|
| Debian, `git` missing, no flag: exit 1, names `git` and the rerun command with `--install-depends`, no `apt-get` or `sudo` call | D9 | ignore the flag (always install) |
| macOS, `git` missing, no flag: exit 1, rerun command printed, no `brew` call | D9 | gate only the apt path |
| source, no `cargo`, no flag: exit 1, names `cargo`, rustup never fetched | D9 | gate only the package installs, not rustup |
| all present, no flag: no refusal, `setup.sh` runs | D9, D1 | refuse whenever the flag is absent |
| repeat after refusal: the no-flag refusal, then the same run with `--install-depends`, installs `git` and hands off to `setup.sh` | D9 | the refusal writes a marker or caches the missing list that the second run obeys |
| `--install-depends` is not passed to `setup.sh`, also when it sits between other arguments (`--domains shared --install-depends --no-sync`); every other argument is, in order | D9, D1 | pass `"$@"` unfiltered; or drop only a leading flag (`shift`) |
| Debian, binary zip, `git` missing: installs `git` only, no rustup | D5, D7 | check build tools even with `bin/lessons` |
| Debian, source, `git` and `cc` missing: one `apt-get update`, then one `apt-get install` naming both, in that order (the fake logs the sequence) | D5 | always install every package; drop the `update` |
| Debian, binary zip: `curl` never installed; source with `cargo` present: `curl` never installed | D5 | install `curl` unconditionally |
| Debian as root: no `sudo`; as a user: `sudo` | D5 | always `sudo` |
| macOS, `git` missing: `brew install git`, never `sudo` | D5 | use apt on Darwin |
| macOS, source, no `cc`: prints `xcode-select --install`, exits 1, no build | D5 | drop the `cc` check |
| source, no `cargo`: rustup fetched once; with `cargo` present never | D5 | invert the `cargo` test |
| unknown OS, `git` missing: exit 1, names `git`, installs nothing | D5, T5 | fall through to apt |
| repeat after refusal: the unknown-OS refusal and the macOS-no-cc refusal each run twice; both runs exit 1 with the same message and install nothing | T5, D5 | the first refusal writes a marker file that makes the second run proceed |
| all present: no installs, `setup.sh` gets the same arguments | D1 | drop `"$@"` from the exec |
| second run installs nothing (back-to-back repeat) | D1 | reinstall when present |
| the rerun command quotes an argument with a space and names this install.sh | D9 | print the arguments unquoted |
| macOS, a `git` found on PATH that does not run (the stub without the command line tools) counts as missing | D5 | find git on PATH instead of running it |
| macOS, a `cc` on PATH but no command line tools (`xcode-select -p` fails): refuses | D5 | find `cc` on PATH on macOS |
| no `apt-get` (Linux) or no Homebrew (macOS), only `cargo` missing: rustup alone, no package-manager refusal | D5 | require the package manager even with no packages |
| source, no `cargo` and no `curl`: apt installs `curl ca-certificates`, rustup runs, and `setup.sh` finds the new `cargo` on PATH | D5 | drop `ca-certificates`; drop the `~/.cargo/bin` PATH export |
| a failing install command is journaled with the command and its exit status; `setup.sh` never runs | D5 | run the command without logging its failure |
| Debian, a user with no `sudo`: refuses, calls no `apt-get` | D5 | drop the `sudo` check |
| a failing rustup download is journaled with its exit status; `setup.sh` never runs | D5 | swallow the rustup failure (`|| true`) |
| `CARGO_HOME` set: `setup.sh` finds the cargo rustup put under it | D5 | export `~/.cargo/bin` whatever `CARGO_HOME` says |

`release-meta.sh --test` (a temp git repo with tagged commits; a temp `Cargo.toml`):

| Check | Serves | Mutation that must turn it red |
|---|---|---|
| `check-tag v0.1.0` against version `0.1.0` passes; `v0.1.1` against `0.1.0` and `0.1.0` (no `v`) fail | T1, D3 | always exit 0; or skip the `v` strip |
| `check-tag vX` and `check-tag v` against `0.1.0` fail with the tag named (malformed tag) | T1 | compare only a prefix of the version; or accept an empty version after the strip |
| `check-tag` reads the `[package]` version even when another table with its own `version =` line comes first | T1 | read the first `version =` line in the file, any table |
| `check-tag v0.1.0-rc.1` and `v0.1.00` against `0.1.0` fail: only an exact match passes | T1 | accept any tag that starts with `v<version>` |
| a `[package]` with no version of its own (`version.workspace = true`) refuses every tag, the bare `v` too | T1 | drop the empty-version guard |
| single quotes, an indented `version`, CRLF and a comment after `[package]` are read | T1 | read only `version = "..."` at the start of a line |
| an unreadable `Cargo.toml` is refused and named | T1 | drop the readable-file check |
| repeat after refusal: `check-tag v0.1.1` against `0.1.0` run twice exits non-zero both times with the same message | T1, D3 | the first refusal rewrites the `Cargo.toml` copy or caches the verdict |
| `notes v0.2.0` lists only the subjects after `v0.1.0`, one `- <subject>` per commit, and the version line | T4 | range `HEAD` instead of `<previous>..<tag>` |
| `notes v0.1.0` with no earlier tag lists every commit, exit 0 | T4 | drop the no-previous-tag branch (the lookup fails under `set -e`) |
| `prerelease 0.1.0` prints `true`; `1.0.0` prints `false` | D3 | invert; or always `true` |
| `prerelease 0.1.0-rc.1` prints `true`, `10.0.0` `false`; `9x`, `1` and an empty version are refused | D3 | test only the leading digit |
| running `notes` twice gives identical output (back-to-back repeat) | T4 | stamp the output with `date` |
| `notes` for an unknown tag is refused and named, with no header printed | T4 | drop the commit lookup |
| `notes` in a shallow clone is refused (the previous tag would be missing and the notes too short) | T4 | drop the shallow-clone check |

Workflow-only parts (`docker`, `gh release create`, the zip step) are checked by the `workflow_dispatch` run, not a unit test
(R4, R5, R6 "Done when").

Race and non-determinism: not reachable in the scripts. Interactive `sudo` (from `--install-depends`, non-root, no passwordless sudo) prompts for a password: fine in a terminal, and the CI smoke jobs run as root so never prompt; no extra refusal is planned. Each runs once, by one user, on one machine; no shared row, no timer.
One caveat for the new setup branch: `install` replaces `~/.local/bin/lessons` by unlink and create, so an MCP server already
running keeps its old inode and keeps running the old binary until restarted (UNVERIFIED for this `install` build; `cp` would
instead write into the running file and fail with "Text file busy"). The upgrade note in R7 says to restart agents. The only
external race (the same tag re-pushed) is the `concurrency` group in R4.

## Provenance

- Searched: `git log`/`git blame`, `gh issue` and `gh pr` of this repo, the files under `docs/`. This repo has no Jira or
  Confluence (GitHub issues only). Not searched: chat.
- `scripts/setup.sh:757-764` (cargo check): `8f1d9db4`, HidekiAI-WORK, 2026-11-10 (line 761 `7ce10b81`, same author and date).
  `setup.sh:765` (`cargo install`): `1f17df93`, 2026-11-10. `setup.sh:766-767` (`lessons_binary`): `8f1d9db4`, 2026-11-10.
  `setup.sh:959-960` (`run_sync_self_test`): `6ce4dcf9` 2026-11-11, line 960 amended by `12a31e3b` 2026-11-17. All INHERITED
  by this change (R1 edits them). `scripts/test-debian.sh`: `61ada3e`, HidekiAI-WORK, 2026-11-11. `Cargo.toml:3,12`: `9ded67d8`, 2026-11-10. `Cargo.toml:5` (`license`): `85f79dd`, HidekiAI-WORK, 2026-11-18, NEW in this
  branch (D8). `README.md:14` (`## Install`, R7): `df8e60e7`, HidekiAI-WORK, 2026-11-10, INHERITED.
- PRs: none exist (`gh pr list --state all` is empty); every change was merged to `trunk` locally.
- Issues: #10 (this ticket), #9 (its duplicate). No design docs beyond `docs/PLAN-2026-11-10-sync.md`, whose agent-install
  steps (R14, S8b) are what `setup.sh` already does.

## Plan gate

- 2026-11-17 iteration 1: 76/100 (Traceability 27, Coverage 19, Minimality 12, Grounding 7, Provenance 11). No CRITICAL.
  To fix in iteration 2: Serves label on every step and test row; map `test-debian.sh` assertions; testable form for tag,
  notes range and pre-release logic; label premises; `lessons_binary` and `REPO_ROOT`; fix line refs; provenance classes.
- 2026-11-18: paused for a new high-priority issue; resumed at iteration 2.
- iteration 2: revised for the iteration-1 findings (date 2026-11-18)
- 2026-11-18 iteration 2: 87/100 (Traceability 32, Coverage 20 capped by UNCOVERED, Minimality 12, Grounding 9, Provenance 14).
- iteration 3: revised for the iteration-2 findings (2026-11-18)
- 2026-11-18 iteration 3: 91/100 PASS (Traceability 32, Coverage 23, Minimality 13, Grounding 9, Provenance 14). No CRITICAL,
  no UNCOVERED. Refinements applied after scoring: `--dry-run` dropped from R2 (no ticket line); R4 concurrency marked as the
  one non-ticket guard; malformed-tag `check-tag` row added; blame for `Cargo.toml:5` and `README.md:14` added; R7 restart-agents note traced to D2.
- 2026-11-19 iteration 4 (amendment D9, `--install-depends`, commit `a789b3f`): 90/100 PASS (Traceability 33, Coverage 23,
  Minimality 13, Grounding 9, Provenance 12). No CRITICAL, no UNCOVERED. Refinements applied: TL;DR names the flag; R2
  gates rustup with the packages; no-flag rows for macOS, no `cargo` and all-present; flag stripped mid-arguments; the
  `setup.sh` unknown-option claim and the `install` mode claim labelled with where they were measured.
