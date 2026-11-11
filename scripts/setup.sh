#!/usr/bin/env bash
# Installs `lessons`, creates the chosen DB files, sets up their sync, and registers the MCP server with coding agents.
#
#   scripts/setup.sh --domains work,shared [--agents opencode,claude] [--dry-run]
#                    [--remote URL --ssh-key PATH [--update live|on-demand] [--pull-every-minutes N] | --no-sync]
#   scripts/setup.sh --test        # run this script's own checks; touches only temp dirs
#
# --domains is required: only the files this machine should hold are created, so a work laptop
# never gets a personal.db by accident. In a terminal, without --remote or --no-sync, it asks per
# domain for the sync repo URL (Enter = do not sync it), the SSH key, and the update mode. The
# answers are APPENDED to $LESSONS_HOME/config.toml; an existing [sync.<domain>] is never edited.
# Each step is logged to stderr and to $LOG_FILE. Written for bash 3.2 too (the macOS default).
set -euo pipefail

unset GREP_OPTIONS GREP_COLOR GREP_COLORS
grep() { command grep --color=never "$@"; }

# --test must touch nothing of the user's, and log_step appends to a log under XDG_STATE_HOME: point it at a temp dir.
if [[ "${1:-}" == --test ]]; then
    test_state_dir="$(mktemp -d "${TMPDIR:-/tmp}/lessons-setup-state.XXXXXX")"
    # shellcheck disable=SC2064 # expand test_state_dir now: it is gone when the trap runs
    trap "rm -rf '$test_state_dir'" EXIT
    export XDG_STATE_HOME="$test_state_dir"
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly REPO_ROOT
readonly KNOWN_DOMAINS="work personal shared"
readonly KNOWN_AGENTS="opencode claude"
readonly MCP_SERVER_NAME="lessons"
readonly EXIT_USAGE=2
readonly EXIT_FAILURE=1
readonly KNOWN_UPDATE_MODES="live on-demand"
readonly DEFAULT_UPDATE_MODE="live"
readonly DEFAULT_PULL_EVERY_MINUTES=10
# 18 digits always fit a bash integer (signed 64-bit) and the config's u64.
readonly MAX_MINUTES_DIGITS=18
# The usage text is the comment block at the top of this file, lines 2 to 11.
readonly USAGE_FIRST_LINE=2
readonly USAGE_LAST_LINE=11
# A per-user log directory: a fixed name in a shared /tmp could be pre-planted as a symlink.
readonly LOG_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/lessons"
readonly LOG_FILE="$LOG_DIR/setup.log"

dry_run=false

log_step() {
    mkdir -p "$LOG_DIR"
    printf '[%s] %s\n' "$(date -u +%H:%M:%SZ)" "$*" | tee -a "$LOG_FILE" >&2
}

usage() {
    sed -n "${USAGE_FIRST_LINE},${USAGE_LAST_LINE}p" "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' >&2
    exit "$EXIT_USAGE"
}

# --- Sync configuration (plan step S1b) -------------------------------------------------------

# True when the script may ask questions: stdin is a terminal. --test sets LESSONS_SETUP_ASSUME_TTY=1
# to drive the questions with piped answers.
can_prompt() {
    [[ -t 0 || "${LESSONS_SETUP_ASSUME_TTY:-}" == 1 ]]
}

# Asks `question` on stderr and prints the answer, or `default_answer` when the answer is empty.
ask() {
    local question="$1" default_answer="$2" typed_answer=""
    if [[ -n "$default_answer" ]]; then
        printf '%s [%s]: ' "$question" "$default_answer" >&2
    else
        printf '%s: ' "$question" >&2
    fi
    IFS= read -r typed_answer || true # end of input counts as an empty answer
    printf '%s\n' "${typed_answer:-$default_answer}"
}

# Prints the keys found in `ssh_dir`, one per line: the private key when it sits beside its .pub,
# else the .pub itself (a key whose private half only an agent holds, such as 1Password).
list_ssh_keys() {
    local ssh_dir="$1" public_key_path
    for public_key_path in "$ssh_dir"/*.pub; do
        [[ -e "$public_key_path" ]] || continue # the glob matched nothing
        if [[ -e "${public_key_path%.pub}" ]]; then
            printf '%s\n' "${public_key_path%.pub}"
        else
            printf '%s\n' "$public_key_path"
        fi
    done
}

# Asks for a key: a number from the list of `ssh_dir` keys, or a path typed out. Prints the path.
ask_ssh_key() {
    local domain_name="$1" ssh_dir="$2" key_index=0 key_path chosen_key
    local -a found_keys=()
    while IFS= read -r key_path; do found_keys+=("$key_path"); done < <(list_ssh_keys "$ssh_dir")
    for key_path in ${found_keys[@]+"${found_keys[@]}"}; do
        key_index=$((key_index + 1))
        printf '  %s) %s\n' "$key_index" "$key_path" >&2
    done
    chosen_key="$(ask "SSH key for $domain_name (a number above, or a path)" "")"
    case "$chosen_key" in
        '' ) printf 'setup: an SSH key is required to sync %s\n' "$domain_name" >&2; return 1 ;;
        *[!0-9]* ) printf '%s\n' "$chosen_key" ;; # a path
        * )
            if [[ "$chosen_key" -ge 1 && "$chosen_key" -le "${#found_keys[@]}" ]]; then
                printf '%s\n' "${found_keys[$((chosen_key - 1))]}"
            else
                printf 'setup: no key number %s\n' "$chosen_key" >&2
                return 1
            fi
            ;;
    esac
}

# A value as a TOML basic string: backslashes and double quotes escaped.
toml_string() {
    local raw_value="$1"
    raw_value="${raw_value//\\/\\\\}"
    printf '"%s"' "${raw_value//\"/\\\"}"
}

# Prints one [sync.<domain>] section.
sync_section_text() {
    local domain_name="$1" remote_url="$2" ssh_key="$3" update_mode="$4" pull_minutes="$5"
    printf '\n[sync.%s]\nremote = %s\nssh_key = %s\nupdate = %s\npull_every_minutes = %s\n' \
        "$domain_name" "$(toml_string "$remote_url")" "$(toml_string "$ssh_key")" "$(toml_string "$update_mode")" "$pull_minutes"
}

lower_text() {
    printf '%s' "$1" | tr '[:upper:]' '[:lower:]'
}

# `user@host` or `host`, with only the host part lower-cased: a user name is case-sensitive.
lower_host() {
    local authority="$1"
    case "$authority" in
        *@*) printf '%s@%s' "${authority%@*}" "$(lower_text "${authority##*@}")" ;;
        *) lower_text "$authority" ;;
    esac
}

trim_trailing_slashes() {
    local trimmed_text="$1"
    while [[ "$trimmed_text" == */ ]]; do trimmed_text="${trimmed_text%/}"; done
    printf '%s' "$trimmed_text"
}

# A remote in the form the lessons config compares remotes in (src/config.rs normalize_remote):
# host lower-cased, a trailing / and .git removed, a portless ssh:// URL written in the scp form.
normalized_remote() {
    local remote_url="$1" scheme_part after_scheme authority path_part
    remote_url="${remote_url#"${remote_url%%[![:space:]]*}"}" # trim leading space
    remote_url="${remote_url%"${remote_url##*[![:space:]]}"}" # trim trailing space
    case "$remote_url" in
        *://*)
            scheme_part="$(lower_text "${remote_url%%://*}")"
            after_scheme="${remote_url#*://}"
            authority="${after_scheme%%/*}"
            path_part="${after_scheme#"$authority"}"
            path_part="${path_part#/}"
            if [[ "$scheme_part" == ssh && "$authority" != *:* ]]; then
                remote_url="$(lower_host "$authority"):$path_part"
            else
                remote_url="$scheme_part://$(lower_host "$authority")/$path_part"
            fi
            ;;
        *:*) # scp-like user@host:path when no / comes before the first :, else a local path, left as it is
            [[ "${remote_url%%:*}" == */* ]] || remote_url="$(lower_host "${remote_url%%:*}"):${remote_url#*:}"
            ;;
    esac
    remote_url="$(trim_trailing_slashes "$remote_url")"
    trim_trailing_slashes "${remote_url%.git}"
    printf '\n'
}

# Prints the lines of [sync.<domain>] in `config_path`, header first, CR line endings removed; nothing
# when it is missing. A header still matches with surrounding spaces or a trailing # comment.
sync_section_lines() {
    local config_path="$1" domain_name="$2"
    [[ -f "$config_path" ]] || return 0
    awk -v section="[sync.$domain_name]" '
        { sub(/\r$/, "") }
        /^[ \t]*\[/ {
            header = $0
            sub(/[ \t]*#.*$/, "", header); sub(/^[ \t]+/, "", header); sub(/[ \t]+$/, "", header)
            in_section = (header == section)
        }
        in_section { print }' "$config_path"
}

# Prints `key`'s value from the [sync.<domain>] section of `config_path`, or nothing when the file, the section
# or the key is missing.
existing_sync_value() {
    local config_path="$1" domain_name="$2" key_name="$3"
    # Reads a "basic" string (only \\ and \" decoded; any other escape prints nothing) or a 'literal'
    # one; whatever follows the value, such as a comment, is ignored. Anything else prints nothing.
    # It reads to the end instead of `exit`: closing the pipe early could fail the writer under pipefail.
    sync_section_lines "$config_path" "$domain_name" | awk -v key="$key_name" -v squote="'" '
        !value_done && NR > 1 && match($0, "^[ \t]*" key "[ \t]*=[ \t]*") { # NR 1 is the header line
            rest = substr($0, RLENGTH + 1)
            if (match(rest, "^" squote "[^" squote "]*" squote)) { print substr(rest, 2, RLENGTH - 2); value_done = 1; next }
            if (!match(rest, /^"([^"\\]|\\.)*"/)) { value_done = 1; next }
            decoded = ""
            for (char_pos = 2; char_pos < RLENGTH; char_pos++) {
                current_char = substr(rest, char_pos, 1)
                if (current_char == "\\") {
                    char_pos++
                    current_char = substr(rest, char_pos, 1)
                    if (current_char != "\\" && current_char != "\"") { value_done = 1; next }
                }
                decoded = decoded current_char
            }
            print decoded
            value_done = 1
        }'
}

# True when `config_path` has a [sync.<domain>] header, even one whose values cannot be read.
has_sync_section() {
    local config_path="$1" domain_name="$2"
    [[ -n "$(sync_section_lines "$config_path" "$domain_name")" ]]
}

# True for a whole number of minutes, 1 or more, short enough for a bash and a u64 integer.
is_valid_minutes() {
    [[ "$1" =~ ^[1-9][0-9]*$ && ${#1} -le $MAX_MINUTES_DIGITS ]]
}

is_known_update_mode() {
    case " $KNOWN_UPDATE_MODES " in
        *" $1 "*) return 0 ;;
        *) return 1 ;;
    esac
}

# Checks the sync flags before anything runs, so a bad combination has no side effects.
validate_sync_flags() {
    local flag_remote="$1" flag_key="$2" flag_mode="$3" flag_minutes="$4"
    if [[ -n "$flag_remote" && -z "$flag_key" ]]; then
        printf 'setup: --remote needs --ssh-key\n' >&2
        return 1
    fi
    if [[ -n "$flag_mode" ]] && ! is_known_update_mode "$flag_mode"; then
        printf 'setup: unknown --update "%s"; expected one of: %s\n' "$flag_mode" "$KNOWN_UPDATE_MODES" >&2
        return 1
    fi
    if [[ -n "$flag_minutes" ]] && ! is_valid_minutes "$flag_minutes"; then
        printf 'setup: --pull-every-minutes must be a whole number of 1 or more (at most %s digits), not "%s"\n' "$MAX_MINUTES_DIGITS" "$flag_minutes" >&2
        return 1
    fi
}

# Decides the [sync] sections for `domain_names`, from flags or, in a terminal, from answers, and
# prints them. Domains given the same remote get the same key, because one clone has one key. A
# domain that already has a section in `config_path` is skipped without a question, and its remote
# and key are remembered, so a later domain on that remote gets the same key.
# Arguments: config path, remote, key, mode, minutes (empty when not given as flags), then the domains.
decide_sync_sections() {
    local config_path="$1" flag_remote="$2" flag_key="$3" flag_mode="$4" flag_minutes="$5"
    shift 5
    local domain_name remote_url ssh_key update_mode pull_minutes previous_remote="" remote_index existing_remote
    local -a seen_remotes=() seen_keys=()
    local ssh_dir="${LESSONS_SETUP_SSH_DIR:-$HOME/.ssh}"
    for domain_name in "$@"; do
        if has_sync_section "$config_path" "$domain_name"; then
            existing_remote="$(existing_sync_value "$config_path" "$domain_name" remote)"
            log_step "sync: $config_path already has [sync.$domain_name] for ${existing_remote:-a remote this script cannot read}; it is kept as it is"
            if [[ -n "$existing_remote" ]]; then # an unreadable remote lends nothing: a later domain is asked for its key
                seen_remotes+=("$(normalized_remote "$existing_remote")")
                seen_keys+=("$(existing_sync_value "$config_path" "$domain_name" ssh_key)")
                previous_remote="$existing_remote"
            fi
            continue
        fi
        if [[ -n "$flag_remote" ]]; then
            remote_url="$flag_remote"
        elif can_prompt; then
            if [[ -n "$previous_remote" ]]; then
                remote_url="$(ask "Sync repo URL for $domain_name (Enter = the same repo, - = do not sync $domain_name)" "$previous_remote")"
            else
                remote_url="$(ask "Sync repo URL for $domain_name (Enter = do not sync $domain_name)" "")"
            fi
        else
            remote_url=""
        fi
        if [[ -z "$remote_url" || "$remote_url" == - ]]; then
            log_step "sync: $domain_name not synced on this machine"
            continue
        fi
        ssh_key=""
        for remote_index in ${seen_remotes[@]+"${!seen_remotes[@]}"}; do
            [[ "${seen_remotes[$remote_index]}" == "$(normalized_remote "$remote_url")" ]] && ssh_key="${seen_keys[$remote_index]}"
        done
        if [[ -z "$ssh_key" ]]; then
            if [[ -n "$flag_key" ]]; then
                ssh_key="$flag_key"
            else
                ssh_key="$(ask_ssh_key "$domain_name" "$ssh_dir")" || return 1
            fi
        fi
        if [[ -n "$flag_remote" ]]; then
            update_mode="${flag_mode:-$DEFAULT_UPDATE_MODE}"
            pull_minutes="${flag_minutes:-$DEFAULT_PULL_EVERY_MINUTES}"
        else
            # Asked again until valid; at end of input `ask` gives the default, which is valid, so this ends.
            update_mode="$(ask "Update mode for $domain_name ($KNOWN_UPDATE_MODES)" "${flag_mode:-$DEFAULT_UPDATE_MODE}")"
            until is_known_update_mode "$update_mode"; do
                printf 'setup: unknown update mode "%s"; expected one of: %s\n' "$update_mode" "$KNOWN_UPDATE_MODES" >&2
                update_mode="$(ask "Update mode for $domain_name ($KNOWN_UPDATE_MODES)" "${flag_mode:-$DEFAULT_UPDATE_MODE}")"
            done
            pull_minutes="$(ask "Pull at most every N minutes for $domain_name" "${flag_minutes:-$DEFAULT_PULL_EVERY_MINUTES}")"
            until is_valid_minutes "$pull_minutes"; do
                printf 'setup: pull every N minutes must be a whole number of 1 or more (at most %s digits), not "%s"\n' "$MAX_MINUTES_DIGITS" "$pull_minutes" >&2
                pull_minutes="$(ask "Pull at most every N minutes for $domain_name" "${flag_minutes:-$DEFAULT_PULL_EVERY_MINUTES}")"
            done
        fi
        seen_remotes+=("$(normalized_remote "$remote_url")")
        seen_keys+=("$ssh_key")
        previous_remote="$remote_url"
        sync_section_text "$domain_name" "$remote_url" "$ssh_key" "$update_mode" "$pull_minutes"
    done
}

# Appends `sections_text` to `config_path`, skipping any [sync.<domain>] the file already has (that
# one is shown, never edited). An existing file is backed up first; its path is printed on stdout
# ("none" when there was no file). Under --dry-run it only logs what it would write.
append_sync_sections() {
    local config_path="$1" sections_text="$2" backup_path="none" section_name section_domain
    local new_text="" current_section="" section_line kept_or_added=""
    while IFS= read -r section_line; do
        case "$section_line" in
            '[sync.'*']')
                section_name="$section_line"
                section_domain="${section_name#\[sync.}" # [sync.work] -> work
                section_domain="${section_domain%\]}"
                if has_sync_section "$config_path" "$section_domain"; then
                    kept_or_added="kept"
                    log_step "sync: $config_path already has $section_name; it is left as it is:"
                    sync_section_lines "$config_path" "$section_domain" | tee -a "$LOG_FILE" >&2
                else
                    kept_or_added="added"
                fi
                current_section="$section_line"
                [[ "$kept_or_added" == added ]] && new_text+=$'\n'"$section_line"$'\n'
                ;;
            '') ;;
            *) [[ -n "$current_section" && "$kept_or_added" == added ]] && new_text+="$section_line"$'\n' ;;
        esac
    done <<< "$sections_text"
    if [[ -z "$new_text" ]]; then
        log_step "sync: nothing to add to $config_path"
        printf '%s\n' "$backup_path"
        return 0
    fi
    if [[ "$dry_run" == true ]]; then
        log_step "sync: would append to $config_path:$new_text"
        printf '%s\n' "$backup_path"
        return 0
    fi
    mkdir -p "$(dirname "$config_path")"
    if [[ -f "$config_path" ]]; then
        backup_path="$config_path.bak-$(date -u +%Y%m%dT%H%M%SZ)"
        cp -p "$config_path" "$backup_path"
        log_step "sync: backed up $config_path to $backup_path"
    else
        printf '# lessons settings; see docs/config.sample.toml for every key.\n' > "$config_path"
    fi
    printf '%s' "$new_text" >> "$config_path"
    log_step "sync: appended to $config_path:$new_text"
    printf '%s\n' "$backup_path"
}

# Decides, appends and validates the [sync] sections for the given domains.
# Arguments: lessons binary, LESSONS_HOME, remote, key, mode, minutes, then the domains.
configure_sync() {
    local lessons_binary="$1" lessons_home="$2" flag_remote="$3" flag_key="$4" flag_mode="$5" flag_minutes="$6"
    shift 6
    local config_path="$lessons_home/config.toml" sections_text backup_path
    if [[ -z "$flag_remote" ]] && ! can_prompt; then
        log_step "sync: not a terminal and no --remote given; sync left unconfigured (see docs/config.sample.toml)"
        return 0
    fi
    validate_sync_flags "$flag_remote" "$flag_key" "$flag_mode" "$flag_minutes" || return 1
    sections_text="$(decide_sync_sections "$config_path" "$flag_remote" "$flag_key" "$flag_mode" "$flag_minutes" "$@")" || return 1
    if [[ -z "$sections_text" ]]; then
        log_step "sync: no domain to sync"
        return 0
    fi
    backup_path="$(append_sync_sections "$config_path" "$sections_text")" || return 1
    if [[ "$dry_run" == true ]]; then
        return 0
    fi
    validate_config "$lessons_binary" "$lessons_home" "$config_path" "$backup_path"
}

# Loads the config through lessons itself, so a misspelled key fails now, not at the first sync.
# `lessons search` without --in reads config.toml (for default_search), and with it every [sync]
# rule; `list` does not read the config at all. On failure the file is put back the way it was.
validate_config() {
    local lessons_binary="$1" lessons_home="$2" config_path="$3" backup_path="$4"
    if LESSONS_HOME="$lessons_home" "$lessons_binary" search setup-config-check --limit 1 >/dev/null; then
        log_step "sync: $config_path loads"
        return 0
    fi
    if [[ "$backup_path" == none ]]; then
        rm -f "$config_path"
        log_step "ERROR: $config_path did not load; it was new, so it was removed"
    else
        cp -p "$backup_path" "$config_path"
        log_step "ERROR: $config_path did not load; restored from $backup_path"
    fi
    return 1
}

# Names the platform from `uname -s`, `uname -r` and $WSL_DISTRO_NAME: "unix" (Linux, macOS), "wsl2", "wsl1" or "windows"
# (Git Bash, MSYS, Cygwin). Only unix and wsl2 are supported: on Windows `lessons` is built and run inside WSL2, as a
# Linux program; a native Windows build is not supported yet.
platform_kind() {
    local kernel_name="$1" kernel_release="$2" wsl_distro="$3"
    case "$kernel_name" in
        MINGW* | MSYS* | CYGWIN*) printf 'windows\n' ;;
        *)
            # WSL1 reports a kernel with a capital M, `4.4.0-19041-Microsoft`; WSL2 kernels say `microsoft-standard-WSL2` in
            # lower case, and a custom one may drop the `WSL2`. A WSL distro with any other kernel is a WSL2 as well.
            case "$kernel_release" in
                *WSL2* | *microsoft*) printf 'wsl2\n' ;;
                *Microsoft*) printf 'wsl1\n' ;;
                *)
                    case "$wsl_distro" in
                        '') printf 'unix\n' ;;
                        *) printf 'wsl2\n' ;;
                    esac
                    ;;
            esac
            ;;
    esac
}

# Refuses a platform `lessons` is not built for, and warns when the data would sit on the Windows drive from WSL2.
check_platform() {
    local kernel_name="$1" kernel_release="$2" wsl_distro="$3" lessons_home="$4" kind
    kind="$(platform_kind "$kernel_name" "$kernel_release" "$wsl_distro")"
    case "$kind" in
        windows)
            printf 'setup: native Windows is not supported; install WSL2 and run this script inside it.\n' >&2
            return 1
            ;;
        wsl1)
            printf 'setup: this is WSL1; lessons needs WSL2 (run `wsl --set-version <distro> 2` from Windows).\n' >&2
            return 1
            ;;
        wsl2)
            case "$lessons_home" in
                /mnt/[a-z] | /mnt/[a-z]/*) log_step "WARNING: $lessons_home is on the Windows drive; keep LESSONS_HOME inside the Linux filesystem (file modes and SQLite locking are unreliable there)" ;;
                *) ;;
            esac
            ;;
        *) ;;
    esac
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
    local domains_csv="$1" agents_csv="$2" flag_remote="$3" flag_key="$4" flag_mode="$5" flag_minutes="$6" skip_sync="$7"
    local domain_name agent_name
    local -a domain_names=() agent_names=()
    check_platform "$(uname -s)" "$(uname -r)" "${WSL_DISTRO_NAME:-}" "${LESSONS_HOME:-$HOME/.lessons}" || exit "$EXIT_USAGE"
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

    # Bad sync flags are refused before anything is installed or created.
    if [[ "$skip_sync" != true ]]; then
        validate_sync_flags "$flag_remote" "$flag_key" "$flag_mode" "$flag_minutes" || exit "$EXIT_USAGE"
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
    if [[ "$skip_sync" != true ]]; then
        configure_sync "$lessons_binary" "${LESSONS_HOME:-$HOME/.lessons}" "$flag_remote" "$flag_key" "$flag_mode" "$flag_minutes" \
            "${domain_names[@]}" || exit "$EXIT_FAILURE"
    fi
    for agent_name in ${agent_names[@]+"${agent_names[@]}"}; do
        register_agent "$agent_name" "$lessons_binary"
    done
    log_step "setup: done"
}

# The sync-configuration checks, run by --test with piped answers in throwaway LESSONS_HOME dirs.
# Called from run_self_test, whose check_equal and failure count it uses.
run_sync_self_test() {
    local test_root lessons_binary="${LESSONS_BIN:-$REPO_ROOT/target/debug/lessons}" config_text prompt_text
    test_root="$(mktemp -d "${TMPDIR:-/tmp}/lessons-setup-test.XXXXXX")"
    # shellcheck disable=SC2064 # expand test_root now: it is local and gone when the trap runs
    trap "rm -rf '$test_root'" RETURN
    readonly SYNC_TEST_REMOTE="git@github.com:owner/lessons-repo.git"
    if [[ ! -x "$lessons_binary" ]] && command -v cargo >/dev/null 2>&1; then
        (cd "$REPO_ROOT" && cargo build -q) # the validation checks need a real lessons binary
    fi
    fn_configure() { # LESSONS_HOME, then configure_sync's flag arguments and domains; answers on stdin
        local lessons_home="$1"
        shift
        LESSONS_SETUP_SSH_DIR="$test_root/no-ssh-dir" configure_sync "$lessons_binary" "$lessons_home" "$@" 2>/dev/null || true # a failure must show as a FAIL below, not end --test under set -e
    }

    mkdir -p "$test_root/created"
    printf '%s\n' "$SYNC_TEST_REMOTE" /keys/main '' '' | LESSONS_SETUP_ASSUME_TTY=1 fn_configure "$test_root/created" "" "" "" "" work >/dev/null
    config_text="$(cat "$test_root/created/config.toml" 2>/dev/null)"
    check_equal "a missing config is created with the answers" "1 1" \
        "$(grep -cxF '[sync.work]' <<< "$config_text") $(grep -cxF 'ssh_key = "/keys/main"' <<< "$config_text")"

    mkdir -p "$test_root/shared-key"
    printf '%s\n' "$SYNC_TEST_REMOTE" /keys/main '' '' '' '' '' '' '' | LESSONS_SETUP_ASSUME_TTY=1 \
        fn_configure "$test_root/shared-key" "" "" "" "" work personal shared >/dev/null
    check_equal "one URL for all three domains gives one shared key" "3" \
        "$(grep -cxF 'ssh_key = "/keys/main"' "$test_root/shared-key/config.toml" 2>/dev/null)"

    mkdir -p "$test_root/existing"
    printf '[sync.work]\nremote = "git@github.com:owner/kept.git"\nssh_key = "/keys/kept"\n' > "$test_root/existing/config.toml"
    printf '%s\n' "$SYNC_TEST_REMOTE" /keys/main '' '' '' '' '' | LESSONS_SETUP_ASSUME_TTY=1 \
        fn_configure "$test_root/existing" "" "" "" "" work shared >/dev/null
    config_text="$(cat "$test_root/existing/config.toml")"
    check_equal "an existing section is untouched and a missing one is appended" "1 1 1" \
        "$(grep -cxF 'remote = "git@github.com:owner/kept.git"' <<< "$config_text") $(grep -cxF '[sync.shared]' <<< "$config_text") $(grep -cxF '[sync.work]' <<< "$config_text")"
    check_equal "an existing config is backed up before the append" "1" \
        "$(find "$test_root/existing" -name 'config.toml.bak-*' | grep -c .)"

    mkdir -p "$test_root/skipped"
    printf '\n' | LESSONS_SETUP_ASSUME_TTY=1 fn_configure "$test_root/skipped" "" "" "" "" work >/dev/null
    check_equal "Enter on the first URL question syncs nothing" "no file" \
        "$([[ -e "$test_root/skipped/config.toml" ]] && echo file || echo 'no file')"

    mkdir -p "$test_root/dash"
    printf '%s\n' "$SYNC_TEST_REMOTE" /keys/main '' '' - | LESSONS_SETUP_ASSUME_TTY=1 fn_configure "$test_root/dash" "" "" "" "" work shared >/dev/null
    check_equal "- on a later URL question skips that domain" "1 0" \
        "$(grep -cxF '[sync.work]' "$test_root/dash/config.toml") $(grep -cxF '[sync.shared]' "$test_root/dash/config.toml")"

    mkdir -p "$test_root/flags"
    prompt_text="$(LESSONS_SETUP_ASSUME_TTY=1 LESSONS_SETUP_SSH_DIR="$test_root/no-ssh-dir" configure_sync "$lessons_binary" "$test_root/flags" \
        "$SYNC_TEST_REMOTE" /keys/main on-demand "" work shared </dev/null 2>&1 >/dev/null || true)"
    check_equal "flags given: nothing is asked" "0 2" \
        "$(grep -c 'Sync repo URL\|SSH key for\|Update mode for' <<< "$prompt_text") $(grep -cxF 'update = "on-demand"' "$test_root/flags/config.toml")"

    mkdir -p "$test_root/no-tty"
    printf '%s\n' "$SYNC_TEST_REMOTE" /keys/main | fn_configure "$test_root/no-tty" "" "" "" "" work >/dev/null
    check_equal "no terminal and no --remote: nothing is asked or written" "no file" \
        "$([[ -e "$test_root/no-tty/config.toml" ]] && echo file || echo 'no file')"

    check_equal "remotes normalize like src/config.rs" \
        "git@github.com:Owner/Repo git@github.com:o/r https://github.com/o/r ./a/b:c" \
        "$(normalized_remote ' git@GitHub.com:Owner/Repo.git/ ') $(normalized_remote 'ssh://git@GITHUB.com/o/r.git') $(normalized_remote 'HTTPS://GitHub.COM/o/r//') $(normalized_remote './a/b:c')"

    mkdir -p "$test_root/same-remote"
    printf '[sync.work]\nremote = "git@github.com:owner/lessons-repo"\nssh_key = "/keys/kept"\n' > "$test_root/same-remote/config.toml"
    prompt_text="$(printf '%s\n' "$SYNC_TEST_REMOTE" '' '' | LESSONS_SETUP_ASSUME_TTY=1 LESSONS_SETUP_SSH_DIR="$test_root/no-ssh-dir" \
        configure_sync "$lessons_binary" "$test_root/same-remote" "" "" "" "" work shared 2>&1 >/dev/null || true)"
    check_equal "an existing section is not asked again and lends its key to the same remote" "0 2" \
        "$(grep -c 'URL for work\|SSH key for' <<< "$prompt_text") $(grep -cxF 'ssh_key = "/keys/kept"' "$test_root/same-remote/config.toml")"

    mkdir -p "$test_root/by-number/ssh"
    touch "$test_root/by-number/ssh/id_first" "$test_root/by-number/ssh/id_first.pub" "$test_root/by-number/ssh/id_second.pub"
    printf '%s\n' "$SYNC_TEST_REMOTE" 2 '' '' | LESSONS_SETUP_ASSUME_TTY=1 LESSONS_SETUP_SSH_DIR="$test_root/by-number/ssh" \
        configure_sync "$lessons_binary" "$test_root/by-number" "" "" "" "" work >/dev/null 2>&1 || true
    check_equal "a key chosen by number; an agent-only key keeps its .pub path" "1" \
        "$(grep -cxF "ssh_key = \"$test_root/by-number/ssh/id_second.pub\"" "$test_root/by-number/config.toml" 2>/dev/null)"

    check_equal "TOML strings escape backslashes and quotes" '"C:\\keys\\a\"b"' "$(toml_string 'C:\keys\a"b')"

    mkdir -p "$test_root/dry-run"
    dry_run=true
    fn_configure "$test_root/dry-run" "$SYNC_TEST_REMOTE" /keys/main "" "" work </dev/null >/dev/null
    dry_run=false
    check_equal "--dry-run writes no config" "no file" \
        "$([[ -e "$test_root/dry-run/config.toml" ]] && echo file || echo 'no file')"

    mkdir -p "$test_root/refused-state" "$test_root/refused-home"
    fn_refused() { # expected error text, then setup.sh arguments; "refused" only for exit 2 with that text
        local expected_error="$1" exit_code=0 error_text
        shift
        error_text="$(HOME="$test_root/refused-home" XDG_STATE_HOME="$test_root/refused-state" LESSONS_HOME="$test_root/refused-home" \
            bash "${BASH_SOURCE[0]}" --domains work --dry-run "$@" 2>&1 >/dev/null </dev/null)" || exit_code=$?
        [[ "$exit_code" -eq "$EXIT_USAGE" && "$error_text" == *"$expected_error"* ]] && printf 'refused' || printf 'ran'
    }
    check_equal "bad sync flags are refused before any step runs" "refused refused refused refused refused" \
        "$(fn_refused 'needs --ssh-key' --remote "$SYNC_TEST_REMOTE") $(fn_refused 'unknown --update' --remote "$SYNC_TEST_REMOTE" --ssh-key /k --update hourly) $(fn_refused 'whole number' --remote "$SYNC_TEST_REMOTE" --ssh-key /k --pull-every-minutes 0) $(fn_refused 'whole number' --remote "$SYNC_TEST_REMOTE" --ssh-key /k --pull-every-minutes ten) $(fn_refused 'whole number' --remote "$SYNC_TEST_REMOTE" --ssh-key /k --pull-every-minutes 9999999999999999999)"

    mkdir -p "$test_root/bad-answers"
    prompt_text="$(printf '%s\n' "$SYNC_TEST_REMOTE" /keys/main hourly on-demand 0 7 | LESSONS_SETUP_ASSUME_TTY=1 LESSONS_SETUP_SSH_DIR="$test_root/no-ssh-dir" \
        configure_sync "$lessons_binary" "$test_root/bad-answers" "" "" "" "" work 2>&1 >/dev/null || true)"
    check_equal "a bad typed mode or minutes is asked again" "1 1 1 1" \
        "$(grep -c 'unknown update mode "hourly"' <<< "$prompt_text") $(grep -c 'not "0"' <<< "$prompt_text") $(grep -cxF 'update = "on-demand"' "$test_root/bad-answers/config.toml" 2>/dev/null) $(grep -cxF 'pull_every_minutes = 7' "$test_root/bad-answers/config.toml" 2>/dev/null)"

    printf '%s\n' '[sync.work]' 'remote="git@github.com:o/tight"' "ssh_key = 'C:\\lit'  # 'note'" '[sync.shared]' \
        'remote = "git@github.com:o/r" # "note"' 'ssh_key = "C:\\keys\\a\"b"' '[sync.personal]' 'ssh_key = "a\tb"' > "$test_root/values.toml"
    check_equal "existing values: any spacing, comments ignored, literal and basic strings decoded, other escapes unread" \
        'git@github.com:o/tight|C:\lit|git@github.com:o/r|C:\keys\a"b||' \
        "$(existing_sync_value "$test_root/values.toml" work remote)|$(existing_sync_value "$test_root/values.toml" work ssh_key)|$(existing_sync_value "$test_root/values.toml" shared remote)|$(existing_sync_value "$test_root/values.toml" shared ssh_key)|$(existing_sync_value "$test_root/values.toml" personal ssh_key)|$(existing_sync_value "$test_root/values.toml" personal remote)"

    printf '[sync.workx]\nremote = "x"\n[sync.work]   \nremote = "spaced"\nremote = "duplicate"\n  [sync.shared] # note\nremote = "commented"\r\n[sync.personal]\r\nremote = "crlf"\r\n' > "$test_root/headers.toml"
    fn_section_state() { # domain; prints "<found|missing>:<remote>"
        has_sync_section "$test_root/headers.toml" "$1" && printf 'found' || printf 'missing'
        printf ':%s' "$(existing_sync_value "$test_root/headers.toml" "$1" remote)"
    }
    check_equal "headers match with trailing spaces, a comment or CRLF, and never by prefix" \
        "found:spaced found:commented found:crlf" "$(fn_section_state work) $(fn_section_state shared) $(fn_section_state personal)"

    if [[ -x "$lessons_binary" ]]; then
        mkdir -p "$test_root/restored"
        printf 'default_search = ["work"]\n' > "$test_root/restored/config.toml"
        cp -p "$test_root/restored/config.toml" "$test_root/restored/original.toml"
        printf '\n[sync.work]\nremtoe = "typo"\n' >> "$test_root/restored/config.toml"
        validate_config "$lessons_binary" "$test_root/restored" "$test_root/restored/config.toml" "$test_root/restored/original.toml" 2>/dev/null || true
        check_equal "a failed validation restores the backup" "default_search = [\"work\"]" "$(cat "$test_root/restored/config.toml")"

        mkdir -p "$test_root/invalid"
        printf '[sync.work]\nremote = "git@github.com:owner/r.git"\nssh_key = "/keys/main"\nremtoe = "typo"\n' > "$test_root/invalid/config.toml"
        check_equal "a misspelled key fails validation and a new file is removed" "refused no file" \
            "$(validate_config "$lessons_binary" "$test_root/invalid" "$test_root/invalid/config.toml" none 2>/dev/null && echo loaded || echo refused) $([[ -e "$test_root/invalid/config.toml" ]] && echo file || echo 'no file')"
        check_equal "a valid appended config passes validation" "loaded" \
            "$(validate_config "$lessons_binary" "$test_root/created" "$test_root/created/config.toml" none 2>/dev/null && echo loaded || echo refused)"
    else
        printf 'SKIP validation checks: no lessons binary at %s and no cargo to build it\n' "$lessons_binary"
    fi
}

# --test: checks the pure parts (argument validation, the registration commands) and the sync
# configuration in throwaway dirs. Touches nothing of the user's.
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
    check_equal "--test logs into a temp directory, not the user's state directory" "yes" \
        "$([[ "$LOG_FILE" == "${test_state_dir:-unset}"/* ]] && echo yes || echo no)"
    check_equal "platform: Linux and macOS are unix" "unix unix" \
        "$(platform_kind Linux 6.1.0-18-amd64 '') $(platform_kind Darwin 25.1.0 '')"
    check_equal "platform: WSL2 by its kernel or a custom kernel in a WSL distro; a Microsoft kernel without WSL2 is WSL1" "wsl2 wsl2 wsl1 wsl1" \
        "$(platform_kind Linux 5.15.153.1-microsoft-standard-WSL2 '') $(platform_kind Linux 6.6.0-custom Debian) $(platform_kind Linux 4.4.0-19041-Microsoft Debian) $(platform_kind Linux 4.4.0-19041-Microsoft '')"
    check_equal "platform: a distro name that mentions Microsoft or WSL2 does not change the verdict" "wsl2 wsl2 unix" \
        "$(platform_kind Linux 6.6.0-custom Microsoft-Debian) $(platform_kind Linux 6.6.0-custom WSL2-Ubuntu) $(platform_kind Linux 6.1.0 '')"
    check_equal "platform: a custom WSL2 kernel that says microsoft but not WSL2 is still WSL2" "wsl2 wsl2" \
        "$(platform_kind Linux 6.6.0-microsoft-custom Debian) $(platform_kind Linux 6.6.0-microsoft-custom '')"
    check_equal "platform: Git Bash, MSYS and Cygwin are native Windows" "windows windows windows" \
        "$(platform_kind MINGW64_NT-10.0-22631 3.5.4 '') $(platform_kind MSYS_NT-10.0 3.5.4 '') $(platform_kind CYGWIN_NT-10.0 3.5.4 '')"
    check_equal "platform: Windows and WSL1 are refused with a pointer to WSL2, WSL2 and Unix pass" "refused refused ok ok" \
        "$(check_platform MINGW64_NT '' '' /x >/dev/null 2>&1 && echo ok || echo refused) $(check_platform Linux 4.4.0-Microsoft Debian /x >/dev/null 2>&1 && echo ok || echo refused) $(check_platform Linux 5.15-microsoft-standard-WSL2 Debian /home/u/.lessons >/dev/null 2>&1 && echo ok || echo refused) $(check_platform Darwin 25.1.0 '' /Users/u/.lessons >/dev/null 2>&1 && echo ok || echo refused)"
    check_equal "platform: a WSL2 LESSONS_HOME on the Windows drive is warned about, also the drive root itself" "1 1 0" \
        "$(check_platform Linux 5.15-microsoft-standard-WSL2 Debian /mnt/c/Users/u/.lessons 2>&1 | grep -c 'Windows drive') $(check_platform Linux 5.15-microsoft-standard-WSL2 Debian /mnt/c 2>&1 | grep -c 'Windows drive') $(check_platform Linux 5.15-microsoft-standard-WSL2 Debian /home/u/.lessons 2>&1 | grep -c 'Windows drive')"
    run_sync_self_test
    if [[ "$failures" -eq 0 ]]; then
        printf 'setup self-test: all checks passed\n'
    else
        printf 'setup self-test: %s check(s) failed\n' "$failures"
        exit "$EXIT_FAILURE"
    fi
}

main() {
    local domains_csv="" agents_csv="" flag_remote="" flag_key="" flag_mode="" flag_minutes="" skip_sync=false
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --domains) domains_csv="${2:-}"; shift 2 || usage ;;
            --agents) agents_csv="${2:-}"; shift 2 || usage ;;
            --remote) flag_remote="${2:-}"; shift 2 || usage ;;
            --ssh-key) flag_key="${2:-}"; shift 2 || usage ;;
            --update) flag_mode="${2:-}"; shift 2 || usage ;;
            --pull-every-minutes) flag_minutes="${2:-}"; shift 2 || usage ;;
            --no-sync) skip_sync=true; shift ;;
            --dry-run) dry_run=true; shift ;;
            --test) run_self_test; return 0 ;;
            -h | --help) usage ;;
            *) printf 'setup: unknown option %s\n' "$1" >&2; usage ;;
        esac
    done
    [[ -n "$domains_csv" ]] || usage
    run_setup "$domains_csv" "$agents_csv" "$flag_remote" "$flag_key" "$flag_mode" "$flag_minutes" "$skip_sync"
}

main "$@"
