#!/usr/bin/env bash
# Launch the release-fast `sawe` editor so it logs to its own file.
#
# sawe logs to stdout ONLY when stdout is a TTY (main.rs: `stdout_is_a_pty()`).
# Launched bare from a terminal, every log line goes to the console and nothing
# to a file. Sending stdout to /dev/null makes it a non-TTY, so the editor uses
# its standard file sink — a clean (ansi-free), rotated log at:
#     ~/.spk/sawe/logs/sawe.log      (+ sawe.log.old — 1 MB ring)
# Read / analyze THAT file (`tail -F`). The /dev/null redirect only flips the
# TTY check; we deliberately do NOT capture stdout (it's ansi-colored — the
# file sink is the clean source of truth).
#
# Usage:  ./run.sh [extra sawe args...]
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="$DIR/target/release-fast/sawe"
EDITOR_LOG="$HOME/.spk/sawe/logs/sawe.log"

if [[ ! -x "$BIN" ]]; then
    echo "error: binary not found at $BIN" >&2
    echo "build it first: cargo build --bin sawe --profile release-fast" >&2
    exit 1
fi

echo "launching: $BIN $*" >&2
echo "log -> $EDITOR_LOG  (tail -F to follow)" >&2

# Redirect to /dev/null ONLY so stdout is not a TTY -> editor enables its file
# sink. The logs themselves go to $EDITOR_LOG via the editor's own mechanism.
exec "$BIN" "$@" >/dev/null 2>&1
