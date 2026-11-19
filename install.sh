#!/usr/bin/env bash
# Checks the system apps lessons needs, installs them only when asked, then hands off to scripts/setup.sh.
# Run it from the unpacked release zip (binary or source) or from a clone:
#
#   bash install.sh [--install-depends] <scripts/setup.sh arguments, e.g. --domains work,shared>
#   bash install.sh --test        # run this script's own checks; touches only temp dirs
#
# Without --install-depends a missing app is listed with the command to rerun, and nothing is installed (no sudo
# without consent). lessons itself (the binary, DBs, MCP entries, skills, sync) is set up by scripts/setup.sh.
# Each step is logged to stderr and to $LOG_FILE. Written for bash 3.2 too (the macOS default).
set -euo pipefail

unset GREP_OPTIONS GREP_COLOR GREP_COLORS
grep() { command grep --color=never "$@"; }

INSTALL_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly INSTALL_ROOT
readonly INSTALL_DEPENDS_FLAG="--install-depends"
readonly EXIT_FAILURE=1
# A per-user log directory, as scripts/setup.sh uses: a fixed name in a shared /tmp could be pre-planted as a symlink.
readonly LOG_FILE="${XDG_STATE_HOME:-$HOME/.local/state}/lessons/install.log"

log_step() {
    mkdir -p "${LOG_FILE%/*}"
    printf '[%s] %s\n' "$(date -u +%H:%M:%SZ)" "$*" | tee -a "$LOG_FILE" >&2
}

has_tool() {
    command -v "$1" >/dev/null 2>&1
}

has_compiler() {
    case "$1" in
        Darwin) xcode-select -p >/dev/null 2>&1 ;; # /usr/bin/cc is a stub that exists even without the command line tools
        *) has_tool cc ;;
    esac
}

# The apps this tree needs that are missing, one per line: git always (lessons sync runs it); a tree without a
# prebuilt bin/lessons also needs a C compiler and cargo to build, and curl only to fetch rustup when cargo is missing.
missing_tools() {
    local os_name="$1"
    has_tool git || echo git
    if [[ ! -f "$INSTALL_ROOT/bin/lessons" || ! -x "$INSTALL_ROOT/bin/lessons" ]]; then
        has_compiler "$os_name" || echo cc
        has_tool cargo || { echo cargo; has_tool curl || echo curl; }
    fi
}

# Package names for the missing apps, one per line; cargo is installed by rustup, not by a package manager.
package_names() {
    local os_name="$1" tool_name
    shift
    for tool_name in "$@"; do
        case "$os_name:$tool_name" in
            Linux:git | Darwin:git) echo git ;;
            Linux:cc) echo build-essential ;;
            Linux:curl) printf '%s\n' curl ca-certificates ;;
            Darwin:curl) echo curl ;;
            *:cargo | Darwin:cc) ;; # rustup below; on macOS the compiler comes from xcode-select --install
        esac
    done
}

run_as_root() {
    log_step "run: $*"
    case "$(id -u)" in
        0) "$@" ;;
        *) sudo "$@" ;;
    esac
}

fn_refuse() { # the message, then exit: nothing has been installed at this point
    log_step "ERROR: $*"
    exit "$EXIT_FAILURE"
}

install_missing() {
    local os_name="$1" package_name
    shift
    local -a packages=()
    case "$os_name" in
        Linux | Darwin) ;;
        *) fn_refuse "cannot install on $os_name; install these yourself, then rerun: $*" ;;
    esac
    case " $* " in
        *" cc "*) [[ "$os_name" != Darwin ]] ||
            fn_refuse "no C compiler; run xcode-select --install, then rerun install.sh" ;;
    esac
    while IFS= read -r package_name; do packages+=("$package_name"); done < <(package_names "$os_name" "$@")
    case "$os_name" in
        Linux)
            has_tool apt-get || fn_refuse "no apt-get on this Linux; install these yourself, then rerun: $*"
            if [[ ${#packages[@]} -gt 0 ]]; then
                run_as_root apt-get update # a fresh image has empty package lists
                run_as_root apt-get install -y "${packages[@]}"
            fi
            ;;
        Darwin)
            has_tool brew || fn_refuse "no Homebrew (https://brew.sh); install these yourself, then rerun: $*"
            if [[ ${#packages[@]} -gt 0 ]]; then
                log_step "run: brew install ${packages[*]}"
                brew install "${packages[@]}"
            fi
            ;;
    esac
    case " $* " in
        *" cargo "*)
            log_step "run: rustup from https://sh.rustup.rs (-y --profile minimal)"
            curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
            export PATH="$HOME/.cargo/bin:$PATH" # rustup's default install dir, for the cargo install in setup.sh
            ;;
    esac
}

main() {
    local install_depends=false argument_text tool_name os_name
    local -a setup_args=() missing=()
    for argument_text in "$@"; do
        case "$argument_text" in
            "$INSTALL_DEPENDS_FLAG") install_depends=true ;;
            *) setup_args+=("$argument_text") ;; # every other argument is setup.sh's, kept in order
        esac
    done
    os_name="$(uname -s)"
    while IFS= read -r tool_name; do missing+=("$tool_name"); done < <(missing_tools "$os_name")
    log_step "install: root $INSTALL_ROOT, os $os_name, missing: ${missing[*]:-none}, $INSTALL_DEPENDS_FLAG $install_depends"
    if [[ ${#missing[@]} -gt 0 ]]; then
        case "$install_depends" in
            true) install_missing "$os_name" "${missing[@]}" ;;
            false)
                log_step "missing: ${missing[*]}; nothing was installed. To install them, rerun:"
                log_step "  bash install.sh $INSTALL_DEPENDS_FLAG ${setup_args[*]:-}"
                exit "$EXIT_FAILURE"
                ;;
        esac
    fi
    log_step "install: handing off to scripts/setup.sh ${setup_args[*]:-}"
    exec bash "$INSTALL_ROOT/scripts/setup.sh" ${setup_args[@]+"${setup_args[@]}"} # bash, not ./: a zip may drop the exec bit
}

# The checks, run by --test: copies of this script in temp trees shaped like the two zips, run with a temp HOME and a
# PATH of fake tools that log their calls. Real git, cc and cargo are never on that PATH, so "missing" is decided by
# the case, not by this machine.
run_self_test() {
    local test_root core_bin case_bin tool_name tool_path kind_name failures=0
    local case_os="" case_uid="" case_tools="" case_xcode_status=0 first_status second_status first_output
    test_root="$(cd "$(mktemp -d "${TMPDIR:-/tmp}/lessons-install-test.XXXXXX")" && pwd)" # pwd drops the // a TMPDIR ending in / gives
    # shellcheck disable=SC2064 # expand test_root now: it is local and gone when the trap runs
    trap "rm -rf '$test_root'" RETURN
    core_bin="$test_root/core-bin"
    case_bin="$test_root/case-bin"
    mkdir -p "$core_bin" "$test_root/home"
    for tool_name in bash sh dirname cat mkdir date tee grep rm chmod tr; do # only what this script and the fakes run
        tool_path="$(command -v "$tool_name")"
        ln -s "$tool_path" "$core_bin/$tool_name"
    done
    for kind_name in binary source; do # binary: the release zip with bin/lessons; source: a tree without it
        mkdir -p "$test_root/$kind_name/scripts"
        cp "${BASH_SOURCE[0]}" "$test_root/$kind_name/install.sh"
        printf '#!/bin/sh\necho "setup $*" >>"$CALLS_LOG"\n' >"$test_root/$kind_name/scripts/setup.sh"
    done
    mkdir -p "$test_root/binary/bin"
    printf '#!/bin/sh\n' >"$test_root/binary/bin/lessons"
    chmod +x "$test_root/binary/bin/lessons"

    check_equal() {
        local check_name="$1" expected_text="$2" actual_text="$3"
        if [[ "$expected_text" == "$actual_text" ]]; then
            printf 'PASS %s\n' "$check_name"
        else
            printf 'FAIL %s\n  expected: %s\n  actual:   %s\n' "$check_name" "$expected_text" "$actual_text"
            failures=$((failures + 1))
        fi
    }
    fn_write_fake() { # tool name, then one extra shell line run after the call is logged
        printf '#!/bin/sh\necho "%s $*" >>"$CALLS_LOG"\n%s\n' "$1" "${2:-}" >"$case_bin/$1"
        chmod +x "$case_bin/$1"
    }
    fn_run_case() { # zip kind (binary|source), then install.sh's arguments; case_os, case_uid, case_tools, case_xcode_status come from the caller
        local kind_name="$1"
        shift
        rm -rf "$case_bin" "$test_root/calls.log"
        mkdir -p "$case_bin"
        printf '#!/bin/sh\necho "$FAKE_UNAME"\n' >"$case_bin/uname"
        printf '#!/bin/sh\necho "$FAKE_UID"\n' >"$case_bin/id"
        printf '#!/bin/sh\nexit "$FAKE_XCODE_STATUS"\n' >"$case_bin/xcode-select"
        chmod +x "$case_bin/uname" "$case_bin/id" "$case_bin/xcode-select"
        fn_write_fake sudo 'exec "$@"'
        for tool_name in $case_tools; do
            case "$tool_name" in
                curl) fn_write_fake curl "echo 'echo \"rustup-script \$*\" >>\"\$CALLS_LOG\"'" ;; # prints the "installer" sh runs
                *) fn_write_fake "$tool_name" ;;
            esac
        done
        env -i HOME="$test_root/home" XDG_STATE_HOME="$test_root/state" PATH="$case_bin:$core_bin" \
            CALLS_LOG="$test_root/calls.log" FAKE_UNAME="$case_os" FAKE_UID="$case_uid" FAKE_XCODE_STATUS="$case_xcode_status" \
            bash "$test_root/$kind_name/install.sh" "$@" >"$test_root/output.txt" 2>&1 && echo 0 || echo $?
    }
    fn_calls() { # the logged calls whose first word matches the pattern, joined by ';'
        grep -E "^($1) " "$test_root/calls.log" 2>/dev/null | tr '\n' ';' || true
    }
    fn_output_count() { # lines of install.sh's output holding the fixed text
        grep -cF -e "$1" "$test_root/output.txt" || true
    }
    fn_messages() { # the output without its timestamps, so two runs compare equal
        tr -d '[:digit:]' <"$test_root/output.txt"
    }

    # Without --install-depends nothing is installed.
    case_os=Linux case_uid=1000 case_tools="apt-get"
    first_status="$(fn_run_case binary --domains shared)"
    check_equal "Debian, git missing, no flag: refuses, names git and the rerun command, calls no apt-get or sudo" "1 1 1 " \
        "$first_status $(fn_output_count 'missing: git; nothing was installed') $(fn_output_count "bash install.sh $INSTALL_DEPENDS_FLAG --domains shared") $(fn_calls 'apt-get|sudo')"
    second_status="$(fn_run_case binary "$INSTALL_DEPENDS_FLAG" --domains shared)"
    check_equal "repeat after refusal: the same run with the flag installs git and hands off" "0 sudo apt-get install -y git;apt-get install -y git; setup --domains shared;" \
        "$second_status $(fn_calls 'sudo|apt-get' | sed 's/sudo apt-get update;apt-get update;//') $(fn_calls setup)"
    case_os=Darwin case_tools="brew"
    first_status="$(fn_run_case binary --domains shared)"
    check_equal "macOS, git missing, no flag: refuses with the rerun command, calls no brew" "1 1 " \
        "$first_status $(fn_output_count "bash install.sh $INSTALL_DEPENDS_FLAG --domains shared") $(fn_calls brew)"
    case_os=Linux case_tools="apt-get git cc curl"
    first_status="$(fn_run_case source --domains shared)"
    check_equal "source, no cargo, no flag: refuses, names cargo, never fetches rustup" "1 1 " \
        "$first_status $(fn_output_count 'missing: cargo; nothing was installed') $(fn_calls 'curl|rustup-script')"
    case_tools="apt-get git"
    first_status="$(fn_run_case binary --domains shared)"
    check_equal "all present, no flag: no refusal, setup.sh runs with the same arguments" "0 setup --domains shared;" \
        "$first_status $(fn_calls setup)"
    first_status="$(fn_run_case binary --domains shared --no-sync)"
    second_status="$(fn_run_case binary --domains shared --no-sync)"
    check_equal "all present, run twice (back-to-back): both hand off and neither installs anything" "0 0 " \
        "$first_status $second_status $(fn_calls 'apt-get|sudo|brew|curl|rustup-script')"
    fn_run_case binary --domains shared "$INSTALL_DEPENDS_FLAG" --no-sync >/dev/null
    check_equal "the flag is removed wherever it sits; the other arguments reach setup.sh in order" "setup --domains shared --no-sync;" \
        "$(fn_calls setup)"

    # With --install-depends, Debian.
    case_tools="apt-get" case_uid=1000
    first_status="$(fn_run_case binary "$INSTALL_DEPENDS_FLAG" --domains shared)"
    check_equal "Debian, binary zip, git missing, as a user: sudo apt-get installs git only (no curl, no build tools, no rustup)" \
        "0 sudo apt-get update;sudo apt-get install -y git; " \
        "$first_status $(fn_calls sudo) $(fn_calls 'curl|rustup-script')"
    case_tools="apt-get cargo" case_uid=0
    first_status="$(fn_run_case source "$INSTALL_DEPENDS_FLAG" --domains shared)"
    check_equal "Debian, source with cargo, git and cc missing, as root: one update, then one install of both; no sudo, no curl, no rustup" \
        "0 apt-get update;apt-get install -y git build-essential; " \
        "$first_status $(fn_calls 'apt-get|sudo') $(fn_calls 'curl|rustup-script')"
    case_tools="apt-get git cc curl"
    first_status="$(fn_run_case source "$INSTALL_DEPENDS_FLAG" --domains shared)"
    check_equal "source, no cargo: rustup is fetched once and run with -y --profile minimal" \
        "0 curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs;rustup-script -y --profile minimal;" \
        "$first_status $(fn_calls 'curl|rustup-script')"

    # With --install-depends, macOS and other systems.
    case_os=Darwin case_uid=501 case_tools="brew"
    first_status="$(fn_run_case binary "$INSTALL_DEPENDS_FLAG" --domains shared)"
    check_equal "macOS, git missing: brew install git, never sudo" "0 brew install git;" \
        "$first_status $(fn_calls 'brew|sudo')"
    case_tools="brew git cargo" case_xcode_status=2
    first_status="$(fn_run_case source "$INSTALL_DEPENDS_FLAG" --domains shared)"
    first_output="$(fn_messages)"
    second_status="$(fn_run_case source "$INSTALL_DEPENDS_FLAG" --domains shared)"
    check_equal "macOS, source, no command line tools: points at xcode-select --install, exits 1, no setup; the same twice" \
        "1 1 1 same" \
        "$first_status $second_status $(fn_output_count 'xcode-select --install')$(fn_calls setup) $([[ "$first_output" == "$(fn_messages)" ]] && echo same || echo differs)"
    case_os=FreeBSD case_tools="" case_xcode_status=0
    first_status="$(fn_run_case binary "$INSTALL_DEPENDS_FLAG" --domains shared)"
    first_output="$(fn_messages)"
    second_status="$(fn_run_case binary "$INSTALL_DEPENDS_FLAG" --domains shared)"
    check_equal "unknown OS, git missing: exits 1 naming git, installs nothing; the same twice" "1 1 1  same" \
        "$first_status $second_status $(fn_output_count 'install these yourself, then rerun: git') $(fn_calls 'apt-get|brew|sudo|curl|setup') $([[ "$first_output" == "$(fn_messages)" ]] && echo same || echo differs)"

    if [[ "$failures" -eq 0 ]]; then
        printf 'install self-test: all checks passed\n'
    else
        printf 'install self-test: %s check(s) failed\n' "$failures"
        exit "$EXIT_FAILURE"
    fi
}

case "${1:-}" in
    --test) run_self_test ;;
    *) main "$@" ;;
esac
