#!/usr/bin/env bash
# The release workflow's decisions, kept here so they have tests: does the tag match Cargo.toml, what are the release
# notes, and is the release a pre-release.
#
#   scripts/release-meta.sh check-tag <tag> <Cargo.toml>   # exit 0 when the tag is exactly v<package version>
#   scripts/release-meta.sh notes <tag>                    # release notes: the version, then one line per commit
#   scripts/release-meta.sh prerelease <version>           # prints true for a 0.x version, else false
#   scripts/release-meta.sh --test                         # run this script's own checks; touches only temp dirs
#
# Each step is logged to stderr; results go to stdout. Written for bash 3.2 too (the macOS default).
set -euo pipefail

unset GREP_OPTIONS GREP_COLOR GREP_COLORS
grep() { command grep --color=never "$@"; }

SCRIPT_PATH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
readonly SCRIPT_PATH # absolute: --test runs it from inside a temp repo
readonly EXIT_FAILURE=1
readonly EXIT_USAGE=2

log_step() {
    printf '[%s] release-meta: %s\n' "$(date -u +%H:%M:%SZ)" "$*" >&2
}

# The version under [package] only: a [dependencies.<name>] table also has a `version =` line.
package_version() {
    awk '/^\[/ { in_package = ($0 == "[package]") } in_package && /^version *=/ { gsub(/^version *= *"|".*$/, ""); print; exit }' "$1"
}

check_tag() {
    local tag_name="$1" cargo_file="$2" package_version_text
    package_version_text="$(package_version "$cargo_file")"
    [[ -n "$package_version_text" ]] || { log_step "ERROR: no [package] version in $cargo_file" && return "$EXIT_FAILURE"; }
    case "$tag_name" in
        "v$package_version_text") log_step "check-tag: tag $tag_name matches $cargo_file version $package_version_text" ;;
        *)
            log_step "ERROR: tag $tag_name does not match $cargo_file version $package_version_text (expected v$package_version_text)"
            return "$EXIT_FAILURE"
            ;;
    esac
}

# The version line, a blank line, then one "- <subject>" per commit since the previous tag (every commit for the
# first tag), newest first.
release_notes() {
    local tag_name="$1" previous_tag
    previous_tag="$(git describe --tags --abbrev=0 "$tag_name^" 2>/dev/null || true)" # empty: this is the first tag
    log_step "notes: $tag_name since ${previous_tag:-the first commit}"
    printf 'lessons %s\n\n' "$tag_name"
    git log --format='- %s' "${previous_tag:+$previous_tag..}$tag_name"
}

is_prerelease() {
    local version_text="$1"
    case "$version_text" in
        0.*) echo true ;; # every 0.x release is a pre-release until 1.0
        [1-9]*) echo false ;;
        *) log_step "ERROR: not a version: '$version_text'" && return "$EXIT_FAILURE" ;;
    esac
}

# The checks, run by --test: a temp git repo with tagged commits and a temp Cargo.toml.
run_self_test() {
    local test_root repo_dir cargo_file first_status second_status first_message failures=0
    test_root="$(cd "$(mktemp -d "${TMPDIR:-/tmp}/lessons-release-meta-test.XXXXXX")" && pwd)"
    # shellcheck disable=SC2064 # expand test_root now: it is local and gone when the trap runs
    trap "rm -rf '$test_root'" RETURN
    repo_dir="$test_root/repo"
    cargo_file="$test_root/Cargo.toml"
    printf '[dependencies.serde]\nversion = "9.9.9"\n\n[package]\nname = "lessons"\nversion = "0.1.0"\n' >"$cargo_file" # a table before [package] with its own version line

    check_equal() {
        local check_name="$1" expected_text="$2" actual_text="$3"
        if [[ "$expected_text" == "$actual_text" ]]; then
            printf 'PASS %s\n' "$check_name"
        else
            printf 'FAIL %s\n  expected: %s\n  actual:   %s\n' "$check_name" "$expected_text" "$actual_text"
            failures=$((failures + 1))
        fi
    }
    fn_git() { # git in the temp repo, with no user hooks, signing or identity from this machine
        git -C "$repo_dir" -c core.hooksPath=/dev/null -c commit.gpgsign=false -c tag.gpgsign=false \
            -c user.name=release-test -c user.email=release-test@example.invalid "$@"
    }
    fn_commit() { # subject, then an optional tag for that commit
        fn_git commit -q --allow-empty -m "$1"
        [[ -z "${2:-}" ]] || fn_git tag "$2"
    }
    fn_status() { # runs this script's subcommand in the temp repo; prints its exit status, output to output.txt
        (cd "$repo_dir" && bash "$SCRIPT_PATH" "$@") >"$test_root/output.txt" 2>&1 && echo 0 || echo $?
    }

    mkdir -p "$repo_dir"
    git -C "$repo_dir" init -q
    fn_commit "feat: first" v0.1.0
    fn_commit "fix: second"
    fn_commit "feat: third" v0.2.0
    fn_commit "docs: after the tag"

    # check-tag
    check_equal "check-tag: v0.1.0 matches version 0.1.0 (the [dependencies] version is not read)" "0" \
        "$(fn_status check-tag v0.1.0 "$cargo_file")"
    check_equal "check-tag: v0.1.1 and 0.1.0 (no v) do not match 0.1.0" "1 1" \
        "$(fn_status check-tag v0.1.1 "$cargo_file") $(fn_status check-tag 0.1.0 "$cargo_file")"
    first_status="$(fn_status check-tag vX "$cargo_file")"
    check_equal "check-tag: a malformed tag (vX, v) is refused and named" "1 1 1" \
        "$first_status $(grep -c 'tag vX does not match' "$test_root/output.txt") $(fn_status check-tag v "$cargo_file")"
    first_status="$(fn_status check-tag v0.1.1 "$cargo_file")"
    first_message="$(grep -o 'tag v0.1.1 .*' "$test_root/output.txt" || true)"
    second_status="$(fn_status check-tag v0.1.1 "$cargo_file")"
    check_equal "check-tag: a refusal repeated gives the same refusal, and Cargo.toml is untouched" "1 1 same 1" \
        "$first_status $second_status $([[ "$first_message" == "$(grep -o 'tag v0.1.1 .*' "$test_root/output.txt")" ]] && echo same || echo differs) $(grep -c '^version = "0.1.0"$' "$cargo_file")"

    # notes
    check_equal "notes v0.2.0: the version line, then only the commits after v0.1.0, newest first" \
        "0|lessons v0.2.0||- feat: third|- fix: second" \
        "$(fn_status notes v0.2.0)|$(grep -v '^\[' "$test_root/output.txt" | tr '\n' '|' | sed 's/|$//')"
    check_equal "notes v0.1.0, the first tag: every commit up to it" "0|lessons v0.1.0||- feat: first" \
        "$(fn_status notes v0.1.0)|$(grep -v '^\[' "$test_root/output.txt" | tr '\n' '|' | sed 's/|$//')"
    fn_status notes v0.2.0 >/dev/null
    first_message="$(cat "$test_root/output.txt")"
    fn_status notes v0.2.0 >/dev/null
    check_equal "notes run twice give identical text (back-to-back repeat)" "same" \
        "$([[ "$(grep -v '^\[' <<<"$first_message")" == "$(grep -v '^\[' "$test_root/output.txt")" ]] && echo same || echo differs)"

    # prerelease
    check_equal "prerelease: 0.1.0 is a pre-release, 1.0.0 is not" "true false" \
        "$(fn_status prerelease 0.1.0 >/dev/null; grep -v '^\[' "$test_root/output.txt") $(fn_status prerelease 1.0.0 >/dev/null; grep -v '^\[' "$test_root/output.txt")"

    if [[ "$failures" -eq 0 ]]; then
        printf 'release-meta self-test: all checks passed\n'
    else
        printf 'release-meta self-test: %s check(s) failed\n' "$failures"
        exit "$EXIT_FAILURE"
    fi
}

main() {
    case "${1:-}" in
        check-tag) check_tag "${2:-}" "${3:-}" ;;
        notes) release_notes "${2:-}" ;;
        prerelease) is_prerelease "${2:-}" ;;
        --test) run_self_test ;;
        *)
            printf 'usage: release-meta.sh check-tag <tag> <Cargo.toml> | notes <tag> | prerelease <version> | --test\n' >&2
            exit "$EXIT_USAGE"
            ;;
    esac
}

main "$@"
