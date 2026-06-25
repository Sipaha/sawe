#!/usr/bin/env bash
# Launch the release-fast `sawe` editor with file logging enabled.
#
# Why the redirect matters: sawe logs to stdout ONLY when stdout is a TTY
# (crates/zed/src/main.rs — `if stdout_is_a_pty()`). Run bare from a terminal,
# every log line goes to the console and NOTHING to a file. Redirecting
# stdout/stderr (the `exec` below) makes stdout a non-TTY, so the editor
# switches to its file sink and writes a clean (ansi-free), rotated, structured
# log to:
#     ~/.spk/sawe/logs/sawe.log      (+ sawe.log.old — 1 MB ring, 2 files)
# That file is the one to read / analyze.
#
# This wrapper's own log (run.log) only catches panics, pre-logger boot output,
# and the "could not open log file -> defaulting to stdout" fallback.
#
# Usage:  ./run.sh [extra sawe args...]
# Env:    SAWE_RUN_LOG  override the wrapper log path (default <repo>/run.log)
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="$DIR/target/release-fast/sawe"
RUN_LOG="${SAWE_RUN_LOG:-$DIR/run.log}"
EDITOR_LOG="$HOME/.spk/sawe/logs/sawe.log"

if [[ ! -x "$BIN" ]]; then
    echo "error: binary not found at $BIN" >&2
    echo "build it first: cargo build --bin sawe --profile release-fast" >&2
    exit 1
fi

echo "launching: $BIN $*" >&2
echo "editor log (analyze this): $EDITOR_LOG" >&2
echo "wrapper log (panics/boot): $RUN_LOG" >&2

# Redirect both streams: stdout becomes a non-TTY (so sawe enables its file
# sink) and panics / early-boot output land in RUN_LOG.
exec "$BIN" "$@" >"$RUN_LOG" 2>&1
