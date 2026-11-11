#!/usr/bin/env bash
# Runs the whole test suite on Debian (the primary target) in a Docker container.
#
#   scripts/test-debian.sh [repo-path]      # default: the repo this script lives in
#   DEBIAN_RUST_IMAGE=rust:1-bookworm scripts/test-debian.sh    # another Debian release
#
# The repo is mounted read-only and copied (without target/) into the container, so nothing on the host changes and
# the host's build output is never reused. Two tests run `git ls-files`, so a NEW file must be `git add`ed first.
set -euo pipefail

readonly DEFAULT_IMAGE="rust:1" # the official Rust image, built on the current Debian stable
image_name="${DEBIAN_RUST_IMAGE:-$DEFAULT_IMAGE}"
repo_root="$(cd "${1:-$(dirname "${BASH_SOURCE[0]}")/..}" && pwd)"

if ! command -v docker >/dev/null 2>&1; then
    printf 'test-debian: docker is not installed or not on the PATH\n' >&2
    exit 1
fi

printf 'test-debian: %s on %s\n' "$repo_root" "$image_name" >&2
docker run --rm \
    -v "$repo_root":/src:ro \
    -e CARGO_TARGET_DIR=/tmp/target \
    "$image_name" \
    bash -c '
        set -euo pipefail
        mkdir /work
        tar -C /src --exclude=./target -cf - . | tar -C /work -xf -
        cd /work
        git config --global --add safe.directory /work
        grep PRETTY_NAME /etc/os-release
        git --version
        cargo test -q
    '
