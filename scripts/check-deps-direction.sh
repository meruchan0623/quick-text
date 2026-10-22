#!/usr/bin/env bash
# Dependency-direction check (development plan, CI job `deps`; B-51).
#
#   quicker-domain   must not depend on tauri*, windows-sys, windows
#   quicker-engine   same rules as domain (checked only once crates/engine exists, 0.5+)
#   quicker-platform must not depend on tauri*
#
# Why: domain/engine must stay portable and testable on Linux (`rust-portable` job), and the
# platform adapter must stay free of the UI shell so it can be tested without a window.
#
# Only normal and build edges are inspected: dev-dependencies such as proptest legitimately
# pull windows-sys in through tempfile on Windows and never reach the shipped binary. All
# targets and all features are considered, so a Windows-only or feature-gated edge cannot
# slip through.
#
# Usage: bash scripts/check-deps-direction.sh   (needs cargo; Linux, macOS or Git Bash)
# This file must keep LF line endings (see .gitattributes).
set -euo pipefail

cd "$(dirname "$0")/.."

status=0

# check_crate <package> <forbidden-name-regex> <human readable list>
check_crate() {
  local package="$1" forbidden="$2" human="$3"
  local names count hits
  # stdout only: cargo's own warnings on stderr must not be parsed as crate names.
  if ! names="$(cargo tree --locked -p "$package" --all-features --target all \
        -e normal,build --prefix none --format '{p}' | awk '{ print $1 }' | sort -u)"; then
    echo "error: 'cargo tree -p $package' failed" >&2
    status=1
    return
  fi
  count="$(printf '%s\n' "$names" | grep -c . || true)"
  hits="$(printf '%s\n' "$names" | grep -E "^(${forbidden})\$" || true)"
  if [ -n "$hits" ]; then
    echo "FAIL: $package depends on forbidden crate(s) ($human):" >&2
    printf '%s\n' "$hits" | sed 's/^/  /' >&2
    echo "  find the path with: cargo tree -p $package -e normal,build --target all -i <crate>" >&2
    status=1
  else
    echo "ok:   $package ($count crates in its production tree) has no dependency on $human"
  fi
}

check_crate quicker-domain 'tauri(-.*)?|windows-sys|windows' 'tauri*, windows-sys, windows'

if [ -f crates/engine/Cargo.toml ]; then
  check_crate quicker-engine 'tauri(-.*)?|windows-sys|windows' 'tauri*, windows-sys, windows'
else
  echo "skip: crates/engine does not exist yet (planned for 0.5, B-49)"
fi

check_crate quicker-platform 'tauri(-.*)?' 'tauri*'

exit "$status"
