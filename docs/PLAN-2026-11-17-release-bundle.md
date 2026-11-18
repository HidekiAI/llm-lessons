# PLAN 2026-11-17: release zips and install.sh (issue #10)

TL;DR: a tag `v<Cargo version>` publishes a GitHub pre-release with two zips that are both run as "unzip, run
`install.sh`". The binary zip (Debian x86_64) carries a prebuilt `bin/lessons`. The source zip is the one GitHub
attaches to every release, and it builds the binary. `install.sh` installs only the missing apps, then hands off to
the existing `scripts/setup.sh`. Detail below.

Current step: plan gate iteration 2 (paused 2026-11-18)

## Requirements

From issue #10 (the ticket):

- T1 A version tag (e.g. `v0.1.0`) triggers a build and publishes a GitHub Release with one bundle: the binary, the
  MCP entrypoint, the skill, the opencode plugin, and the installer.
- T2 The installer works with no Rust toolchain and no source clone: binary on `PATH`, skill per agent, MCP
  registered, DB initialized.
- T3 The installer's `--test` runs the install assertions against the bundle.
- T4 Release notes list the version and the Conventional Commits since the last tag.
- T5 Debian 64-bit only; other platforms build from source (`cargo build --release && scripts/setup.sh`).

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
  dependant apps". So both kinds are zips with `install.sh` at the root.
- D7 (@me 17:14) "for Debian, I prefer appImage over brew or other package installation". Asked whether that means
  lessons as an AppImage or the dependencies as AppImages; @me chose neither: keep the zip, no AppImage, apt installs
  only a missing `git` (and the build tools for the source zip).
- D8 The zip ships `LICENSE` (MIT, MEASURED: `LICENSE:1`). @me 2026-11-17: "add license = "MIT" to Cargo.toml too";
  done in this branch (`Cargo.toml` `[package]`).

Prerequisites, MEASURED from `Cargo.toml`: `rusqlite` uses `bundled`, so a build needs a C compiler and not OpenSSL.
`git` is needed only by `lessons sync` (MEASURED: README "Install", line 33).

Deviations, stated for the gate: the MCP entrypoint is the binary itself (`lessons mcp`, MEASURED: `src/main.rs:311`),
so the zip has no separate MCP file. `cargo test` cannot run without the source, so CI runs it before packaging (T3).
The source zip is GitHub's automatic "Source code (zip)" of the tag; it already holds `install.sh` (UNVERIFIED: that
GitHub's archive keeps the exec bit; the README says `bash install.sh`, which does not need it).

## Steps

| Step | Change | Done when |
|---|---|---|
| R1 | `scripts/setup.sh` `run_setup` (line 765): when `$REPO_ROOT/bin/lessons` is executable, `install -m 755` it to `~/.local/bin/lessons` and use that path, skipping the `cargo` check; else `cargo install` as today. `run_sync_self_test` (line 960) defaults `LESSONS_BIN` to `bin/lessons` when it exists | `setup.sh --test` green; new checks go red under their mutations |
| R2 | `install.sh` (repo root, D5, D6): checks `git`; when `bin/lessons` is absent also `curl` and `cc`. Missing ones: `apt-get install -y git curl ca-certificates build-essential` (with `sudo` unless root) on Debian, `brew install git curl` on macOS; no `cc` on macOS prints `xcode-select --install` and exits 1; other OS lists what is missing and exits 1. No `cargo` and no `bin/lessons`: rustup with `-y --profile minimal`, then `. ~/.cargo/env`. Then `exec scripts/setup.sh "$@"`, whose `cargo install --locked --path` is the release build. `--dry-run` prints and runs nothing; `--test` uses fake tools on `PATH` | `bash install.sh --test` green; each check red under its mutation |
| R3 | `.github/workflows/release.yml`, build job: pinned `rust:1.NN-bookworm` (glibc 2.36, runs on Debian 12 and 13), tag == `Cargo.toml` version, `cargo test --locked`, `cargo build --release --locked`, zip `lessons-v<ver>-linux-x86_64.zip` holding `install.sh`, `bin/lessons`, `scripts/setup.sh`, `skills/`, `integrations/opencode/lessons-hook.js`, `README.md`, `LICENSE` | `actionlint` if installed, else manual review |
| R4 | Smoke job: unzip in `debian:bookworm-slim` (no Rust, no git), `bash install.sh --domains shared --no-sync`, then `~/.local/bin/lessons --version` and `scripts/setup.sh --test` | green on a `workflow_dispatch` run of the branch; no test tag is pushed |
| R5 | Release job, tags only: notes from `git log --format='- %s' <previous tag>..<tag>` (all commits with no previous tag), `gh release create --prerelease` for `0.x`, binary zip attached | the `workflow_dispatch` run prints the notes and skips the release |
| R6 | README "Install": the two zips, `bash install.sh`, the Debian-only binary, the `~/.local/bin` note | grep finds the section |

## Tests

`setup.sh --test` (temp `HOME`, a fake `cargo` on `PATH` that records its calls):

| Check | Mutation that must turn it red |
|---|---|
| with a fake `bin/lessons`, setup installs it to `~/.local/bin` and never calls `cargo` | delete the `bin/lessons` branch |
| without `bin/lessons`, setup still calls `cargo install` | invert the detection test |
| `~/.local/bin` missing from `PATH` logs the warning | delete the warning |

`install.sh --test` (fake `uname`, `apt-get`, `sudo`, `brew`, `curl` on `PATH`, each logging its arguments):

| Check | Mutation that must turn it red |
|---|---|
| Debian, binary zip, `git` missing: installs `git` only, no rustup | check build tools even with `bin/lessons` |
| Debian, source, `git` and `cc` missing: one `apt-get install` naming both | always install every package |
| Debian as root: no `sudo`; as a user: `sudo` | always `sudo` |
| macOS, `git` missing: `brew install git`, never `sudo` | use apt on Darwin |
| macOS, source, no `cc`: prints `xcode-select --install`, exits 1, no build | drop the `cc` check |
| source, no `cargo`: rustup fetched once; with `cargo` present never | invert the `cargo` test |
| unknown OS, `git` missing: exit 1, names `git`, installs nothing | fall through to apt |
| all present: no installs, `setup.sh` gets the same arguments | drop `"$@"` from the exec |
| second run installs nothing (back-to-back repeat) | reinstall when present |

Race and non-determinism: not reachable. Both scripts run once, by one user, on one machine; no shared row, no timer.

## Provenance

- `scripts/setup.sh:757-767` (cargo check and install): `8f1d9db`, `1f17df9`, `7ce10b8`, HidekiAI-WORK, 2026-11-10.
  `scripts/setup.sh:960`: `12a31e3`, 2026-11-17. `scripts/test-debian.sh`: `61ada3e`, 2026-11-11.
- PRs: none exist (MEASURED: `gh pr list --state all` is empty); every change was merged to `trunk` locally.
- Issues: #10 (this ticket), #9 (its duplicate). Searched no other source; there are no design docs beyond
  `docs/PLAN-2026-11-10-sync.md`, whose agent-install steps (R14, S8b) are what `setup.sh` already does.

## Plan gate

- 2026-11-17 iteration 1: 76/100 (Traceability 27, Coverage 19, Minimality 12, Grounding 7, Provenance 11). No CRITICAL.
  To fix in iteration 2 (by a separate planner agent): cite a T or D label on every step and test row; map
  `scripts/test-debian.sh`'s assertions to the zip's checks or state why they differ; give the tag-equals-version,
  previous-tag notes range and `0.x` pre-release logic a testable form with mutations; label the glibc 2.36,
  `debian:bookworm-slim` and rustup `--profile minimal` premises; say R1 sets `lessons_binary` to `~/.local/bin/lessons`
  and that `REPO_ROOT` is the zip root; fix line refs (`run_sync_self_test` is `setup.sh:959`, `Command::Mcp` is
  `src/main.rs:310`); name the provenance classes not searched (Jira, Confluence, chat).
- 2026-11-18: paused for a new high-priority issue; resume at iteration 2.
