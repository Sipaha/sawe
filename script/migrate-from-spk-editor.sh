#!/usr/bin/env bash
# One-shot, IDEMPOTENT migration of editor state from the predecessor
# `spk-editor` profile (~/.spk/spk-editor, Zed v0.235 base) into the `sawe`
# profile (~/.spk/sawe, Zed v1.7.2 base). Re-running converges to the same
# end state: nothing is duplicated, already-moved data is skipped, and row
# imports upsert (INSERT OR REPLACE) rather than erroring on conflict.
#
# WHY data-import and not a raw DB copy:
#   The two builds have divergent sqlite migration histories, so sawe's
#   migrator REJECTS spk-editor's db.sqlite wholesale and silently falls back
#   to an empty in-memory DB. We therefore let sawe create its own fresh
#   schema, then import only the DATA rows (with paths rewritten). Fork-owned
#   tables (solutions / catalog / members / agent sessions) carry over; the
#   upstream workspace state (open tabs, selections, folds) does NOT survive
#   the version gap and is intentionally left to rebuild fresh.
#
# Run with BOTH editors closed. spk-editor hosts agent sessions, so it must
# not be running while its DB is read.
#
# Usage:  script/migrate-from-spk-editor.sh [--yes]
set -euo pipefail

SRC="$HOME/.spk/spk-editor"
DST="$HOME/.spk/sawe"
SRC_DB="$SRC/data/db/0-dev/db.sqlite"            # spk-editor channel = dev
DST_DB="$DST/data/db/0-stable/db.sqlite"         # sawe channel       = stable
SRC_AGENT="$SRC/data/solution_agent/solution_agent.db"
DST_AGENT="$DST/data/solution_agent/solution_agent.db"
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SAWE_BIN="$REPO_DIR/target/release-fast/sawe"

# old base/solutions -> new base/ss, then old base -> new base; and make every
# INSERT an upsert so re-imports don't conflict on primary keys.
rewrite() {
    sed -e 's#/home/spk/\.spk/spk-editor/solutions/#/home/spk/.spk/sawe/ss/#g' \
        -e 's#/home/spk/\.spk/spk-editor/#/home/spk/.spk/sawe/#g' \
        -e 's/^INSERT INTO/INSERT OR REPLACE INTO/'
}

die() { echo "error: $*" >&2; exit 1; }

# ---- preconditions -------------------------------------------------------
[[ -x "$SAWE_BIN" ]] || die "sawe binary not found: $SAWE_BIN (build: cargo build --bin sawe --profile release-fast)"
pgrep -x spk-editor >/dev/null && die "spk-editor is running — close it first (it locks its DB)."
pgrep -x sawe       >/dev/null && die "sawe is running — close it first."
command -v sqlite3 >/dev/null || die "sqlite3 not installed."
# At least one of {source DB present, already-migrated DST} must hold.
[[ -f "$SRC_DB" || -f "$DST_DB" ]] || die "neither source DB ($SRC_DB) nor migrated DB ($DST_DB) found."

echo "Migration: $SRC -> $DST   (idempotent; safe to re-run)"
if [[ "${1:-}" != "--yes" ]]; then
    read -r -p "Proceed? [y/N] " ans
    [[ "$ans" == "y" || "$ans" == "Y" ]] || { echo "aborted."; exit 0; }
fi

# ---- 1. export DB rows (only if source DBs still present) ----------------
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
echo "[1/6] exporting DB rows (rewritten, upsert)..."
if [[ -f "$SRC_DB" ]]; then
    for t in catalog_projects solutions solution_members active_member; do
        sqlite3 "file:$SRC_DB?immutable=1" ".mode insert $t" "SELECT * FROM $t"
    done | rewrite > "$TMP/fork_rows.sql"
fi
if [[ -f "$SRC_AGENT" ]]; then
    for t in solution_sessions solution_session_background_agent solution_session_background_shell; do
        sqlite3 "file:$SRC_AGENT?immutable=1" ".mode insert $t" "SELECT * FROM $t"
    done | rewrite > "$TMP/agent_rows.sql"
fi
echo "  fork rows: $(grep -c '^INSERT' "$TMP/fork_rows.sql" 2>/dev/null || echo 0) | agent rows: $(grep -c '^INSERT' "$TMP/agent_rows.sql" 2>/dev/null || echo 0)"

# ---- 2. move working trees + caches + jsonl (each step skips if done) ----
echo "[2/6] moving working trees and caches..."
mkdir -p "$DST/config" "$DST/data/solution_agent"
if [[ -d "$SRC/solutions" && ! -e "$DST/ss" ]]; then
    mv "$SRC/solutions" "$DST/ss"
elif [[ -d "$SRC/solutions" && -e "$DST/ss" ]]; then
    echo "  note: both $SRC/solutions and $DST/ss exist — leaving both untouched (resolve by hand)."
fi
for d in languages extensions node prettier prompts external_agents debug_adapters; do
    if [[ -d "$SRC/data/$d" && ! -e "$DST/data/$d" ]]; then mv "$SRC/data/$d" "$DST/data/$d"; fi
done
# Move agent jsonl/artifacts but NEVER the donor .db (wrong migration history);
# sawe creates its own DST_AGENT in step 4 and we import rows in step 5.
if [[ -d "$SRC/data/solution_agent" ]]; then
    find "$SRC/data/solution_agent" -maxdepth 1 -mindepth 1 \
        ! -name '*.db' ! -name '*.db-shm' ! -name '*.db-wal' \
        -exec mv -t "$DST/data/solution_agent/" {} + 2>/dev/null || true
fi

# ---- 3. copy config (file-based, path-independent) -----------------------
echo "[3/6] copying config..."
for f in settings.json keymap.json themes snippets tasks.json debug.json icons; do
    [[ -e "$SRC/config/$f" ]] && cp -r "$SRC/config/$f" "$DST/config/" || true
done

# ---- 4. ensure sawe's fresh v1.7.2 schema exists (boot once if needed) ----
schema_ready() {
    [[ -f "$DST_DB" ]]    && sqlite3 "$DST_DB"    "SELECT 1 FROM solutions LIMIT 1"         >/dev/null 2>&1 && \
    [[ -f "$DST_AGENT" ]] && sqlite3 "$DST_AGENT" "SELECT 1 FROM solution_sessions LIMIT 1" >/dev/null 2>&1
}
if schema_ready; then
    echo "[4/6] sawe schema already present — skipping boot."
else
    echo "[4/6] booting sawe once to create its schema..."
    # Runtime state lives in state/ (an old build put it in config/ — the
    # editor sweeps that itself at startup, but a stale lock there would still
    # be gone by the time this boot happens, so clear both).
    rm -f "$DST/state/mcp.sock" "$DST/state/mcp.lock" \
          "$DST/config/mcp.sock" "$DST/config/mcp.lock"
    "$SAWE_BIN" --headless >/dev/null 2>&1 &
    boot_pid=$!
    for _ in $(seq 1 60); do schema_ready && break; sleep 1; done
    kill "$boot_pid" 2>/dev/null || true
    sleep 2
    schema_ready || die "sawe did not create its schema ($DST_DB / $DST_AGENT)"
fi
rm -f "$DST_DB"-wal "$DST_DB"-shm "$DST_AGENT"-wal "$DST_AGENT"-shm

# ---- 5. import rows (upsert — safe to repeat) ----------------------------
echo "[5/6] importing rows..."
[[ -f "$TMP/fork_rows.sql"  ]] && sqlite3 "$DST_DB"    < "$TMP/fork_rows.sql"
[[ -f "$TMP/agent_rows.sql" ]] && sqlite3 "$DST_AGENT" < "$TMP/agent_rows.sql"

# ---- 6. report -----------------------------------------------------------
echo "[6/6] done."
echo "  solutions: $(sqlite3 "$DST_DB" 'SELECT count(*) FROM solutions')"
echo "  members:   $(sqlite3 "$DST_DB" 'SELECT count(*) FROM solution_members')"
echo "  catalog:   $(sqlite3 "$DST_DB" 'SELECT count(*) FROM catalog_projects')"
echo "  sessions:  $(sqlite3 "$DST_AGENT" 'SELECT count(*) FROM solution_sessions' 2>/dev/null || echo 0)"
echo
echo "Launch sawe now. Old profile leftovers (config, empty data/db) remain at $SRC — remove when satisfied."
