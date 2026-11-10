#!/usr/bin/env bash
# Installs `lessons`, creates the chosen DB files, and registers its MCP server with coding agents.
#
#   scripts/setup.sh --domains work,shared [--agents opencode,claude] [--dry-run]
#   scripts/setup.sh --test        # run this script's own checks; changes nothing
#
# --domains is required: only the files this machine should hold are created, so a work laptop
# never gets a personal.db by accident. Each step is logged to stderr and to $LOG_FILE.
# Written for bash 3.2 too (the macOS default): no mapfile, and empty arrays are guarded.
set -euo pipefail

unset GREP_OPTIONS GREP_COLOR GREP_COLORS
grep() { command grep --color=never "$@"; }

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly REPO_ROOT
readonly KNOWN_DOMAINS="work personal shared"
readonly KNOWN_AGENTS="opencode claude"
readonly MCP_SERVER_NAME="lessons"
readonly EXIT_USAGE=2
readonly EXIT_FAILURE=1
# A per-user log directory: a fixed name in a shared /tmp could be pre-planted as a symlink.
readonly LOG_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/lessons"
readonly LOG_FILE="$LOG_DIR/setup.log"

dry_run=false

log_step() {
    mkdir -p "$LOG_DIR"
    printf '[%s] %s\n' "$(date -u +%H:%M:%SZ)" "$*" | tee -a "$LOG_FILE" >&2
}

usage() {
    sed -n '2,8p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' >&2
    exit "$EXIT_USAGE"
}

# Prints each word of the comma list `csv_text` on its own line, failing on a word that is not
# in `known_words`. `list_name` only labels the error.
validated_list() {
    local list_name="$1" csv_text="$2" known_words="$3"
    local requested_word
    local -a requested_words=()
    IFS=',' read -r -a requested_words <<< "$csv_text"
    for requested_word in ${requested_words[@]+"${requested_words[@]}"}; do
        case " $known_words " in
            *" $requested_word "*) printf '%s\n' "$requested_word" ;;
            *)
                printf 'setup: unknown %s "%s"; expected one of: %s\n' "$list_name" "$requested_word" "$known_words" >&2
                return 1
                ;;
        esac
    done
}

# Prints the command that registers the MCP server with `agent_name`, one word per line, so the
# same words are both run and checked by --test.
agent_register_command() {
    local agent_name="$1" lessons_binary="$2"
    case "$agent_name" in
        opencode) printf '%s\n' opencode mcp add --global "$MCP_SERVER_NAME" -- "$lessons_binary" mcp ;;
        claude) printf '%s\n' claude mcp add --scope user "$MCP_SERVER_NAME" -- "$lessons_binary" mcp ;;
        *)
            printf 'setup: no register command for agent "%s"\n' "$agent_name" >&2
            return 1
            ;;
    esac
}

# Runs a command, or only logs it under --dry-run.
run_step() {
    log_step "run: $*"
    if [[ "$dry_run" == true ]]; then
        return 0
    fi
    "$@"
}

register_agent() {
    local agent_name="$1" lessons_binary="$2"
    local command_word
    local -a command_words=()
    if ! command -v "$agent_name" >/dev/null 2>&1; then
        log_step "skip $agent_name: not installed"
        return 0
    fi
    while IFS= read -r command_word; do
        command_words+=("$command_word")
    done < <(agent_register_command "$agent_name" "$lessons_binary")
    # A failed registration (already registered, an older CLI) must not undo the steps before it.
    if ! run_step "${command_words[@]}"; then
        log_step "WARNING: registering with $agent_name failed; see the README to add it by hand"
    fi
}

run_setup() {
    local domains_csv="$1" agents_csv="$2"
    local domain_name agent_name
    local -a domain_names=() agent_names=()
    # Validate first, in the current shell: inside the process substitutions below a failure would
    # be invisible to set -e.
    validated_list domain "$domains_csv" "$KNOWN_DOMAINS" >/dev/null || exit "$EXIT_USAGE"
    validated_list agent "$agents_csv" "$KNOWN_AGENTS" >/dev/null || exit "$EXIT_USAGE"
    while IFS= read -r domain_name; do domain_names+=("$domain_name"); done < <(validated_list domain "$domains_csv" "$KNOWN_DOMAINS")
    while IFS= read -r agent_name; do agent_names+=("$agent_name"); done < <(validated_list agent "$agents_csv" "$KNOWN_AGENTS")
    if [[ ${#domain_names[@]} -eq 0 ]]; then
        printf 'setup: --domains needs at least one domain (%s)\n' "$KNOWN_DOMAINS" >&2
        exit "$EXIT_USAGE"
    fi

    log_step "setup: repo $REPO_ROOT, domains ${domain_names[*]}, agents ${agent_names[*]:-none}, log $LOG_FILE"
    if ! command -v cargo >/dev/null 2>&1; then
        # A dry run only shows the steps, so it goes on without cargo.
        if [[ "$dry_run" != true ]]; then
            log_step "ERROR: cargo not found; install Rust from https://rustup.rs"
            exit "$EXIT_FAILURE"
        fi
        log_step "note: cargo not found; a real run needs it (https://rustup.rs)"
    fi
    run_step cargo install --locked --path "$REPO_ROOT"
    # Where `cargo install` puts the binary: CARGO_INSTALL_ROOT, else CARGO_HOME, else ~/.cargo.
    local lessons_binary="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}/bin/lessons"
    for domain_name in "${domain_names[@]}"; do
        run_step "$lessons_binary" init --domain "$domain_name"
    done
    for agent_name in ${agent_names[@]+"${agent_names[@]}"}; do
        register_agent "$agent_name" "$lessons_binary"
    done
    log_step "setup: done"
}

# --test: checks the pure parts (argument validation, the registration commands). Runs nothing.
run_self_test() {
    local failures=0
    check_equal() {
        local check_name="$1" expected_text="$2" actual_text="$3"
        if [[ "$expected_text" == "$actual_text" ]]; then
            printf 'PASS %s\n' "$check_name"
        else
            printf 'FAIL %s\n  expected: %s\n  actual:   %s\n' "$check_name" "$expected_text" "$actual_text"
            failures=$((failures + 1))
        fi
    }
    check_equal "domains split in order" "work shared" "$(validated_list domain work,shared "$KNOWN_DOMAINS" | tr '\n' ' ' | sed 's/ $//')"
    check_equal "unknown domain refused" "refused" "$(validated_list domain work,home "$KNOWN_DOMAINS" >/dev/null 2>&1 && echo accepted || echo refused)"
    check_equal "unknown agent refused" "refused" "$(validated_list agent cursor "$KNOWN_AGENTS" >/dev/null 2>&1 && echo accepted || echo refused)"
    check_equal "empty agent list is allowed" "" "$(validated_list agent "" "$KNOWN_AGENTS")"
    check_equal "opencode registers globally" "opencode mcp add --global lessons -- /bin/lessons mcp" \
        "$(agent_register_command opencode /bin/lessons | tr '\n' ' ' | sed 's/ $//')"
    check_equal "claude registers for the user" "claude mcp add --scope user lessons -- /bin/lessons mcp" \
        "$(agent_register_command claude /bin/lessons | tr '\n' ' ' | sed 's/ $//')"
    if [[ "$failures" -eq 0 ]]; then
        printf 'setup self-test: all checks passed\n'
    else
        printf 'setup self-test: %s check(s) failed\n' "$failures"
        exit "$EXIT_FAILURE"
    fi
}

main() {
    local domains_csv="" agents_csv=""
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --domains) domains_csv="${2:-}"; shift 2 || usage ;;
            --agents) agents_csv="${2:-}"; shift 2 || usage ;;
            --dry-run) dry_run=true; shift ;;
            --test) run_self_test; return 0 ;;
            -h | --help) usage ;;
            *) printf 'setup: unknown option %s\n' "$1" >&2; usage ;;
        esac
    done
    [[ -n "$domains_csv" ]] || usage
    run_setup "$domains_csv" "$agents_csv"
}

main "$@"
