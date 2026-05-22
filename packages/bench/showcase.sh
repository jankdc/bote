#!/usr/bin/env bash
# Cold-cache JSON.parse vs bote showdown.
#
# For each (approach, operation) cell, drops the OS page cache via
# `sudo purge` and then runs a fresh node process so V8, the bote NAPI
# binding, and the bote chunk cache all start cold. Renders a table at
# the end.
#
# Environment variables:
#   BYTES         Target fixture size in bytes (default: 524288000 ≈ 500 MiB).
#   SKIP_PURGE    Set to any non-empty value to skip OS-cache drops.
#                 Useful for quick iteration; not representative of cold-cache.

set -euo pipefail

BYTES="${BYTES:-524288000}"
SKIP_PURGE="${SKIP_PURGE:-}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NODE=(yarn node --experimental-strip-types --no-warnings=ExperimentalWarning)
TS="$SCRIPT_DIR/showcase.ts"

# Prime sudo once up front so subsequent `sudo purge` calls don't block
# on a password prompt mid-run. A background keepalive then refreshes the
# credential timestamp every minute until the script exits.
SUDO_KEEPALIVE_PID=""
if [[ -z "$SKIP_PURGE" ]]; then
  if ! command -v purge >/dev/null 2>&1; then
    echo "error: 'purge' not found in PATH (needed to drop the OS page cache)" >&2
    echo "       set SKIP_PURGE=1 to run anyway, with a warm OS cache." >&2
    exit 1
  fi
  echo "[sudo] priming credentials (you may be prompted once)…" >&2
  sudo -v
  ( while true; do sudo -n true; sleep 60; done ) 2>/dev/null &
  SUDO_KEEPALIVE_PID=$!
fi

cleanup() {
  if [[ -n "$SUDO_KEEPALIVE_PID" ]]; then
    kill "$SUDO_KEEPALIVE_PID" 2>/dev/null || true
  fi
  if [[ -n "${RESULTS_JSONL:-}" && -f "$RESULTS_JSONL" ]]; then
    rm -f "$RESULTS_JSONL"
  fi
}
trap cleanup EXIT INT TERM

# Step 1: ensure fixture exists. This is *not* a measured step; it's also
# done before priming the page cache for the first run so generation I/O
# doesn't tarnish the first measurement.
fixture_json=$("${NODE[@]}" "$TS" fixture --bytes "$BYTES")
file_path=$(printf '%s' "$fixture_json" | "${NODE[@]}" -e 'let s=""; process.stdin.on("data",d=>s+=d).on("end",()=>process.stdout.write(JSON.parse(s).filePath))')
count=$(printf '%s' "$fixture_json"   | "${NODE[@]}" -e 'let s=""; process.stdin.on("data",d=>s+=d).on("end",()=>process.stdout.write(String(JSON.parse(s).count)))')
mid_index=$((count / 2))
last_index=$((count - 1))

echo "fixture: $file_path  ($(printf '%s items' "$count"))" >&2

RESULTS_JSONL="$(mktemp -t bote-showcase-results.XXXXXX)"

drop_caches() {
  if [[ -n "$SKIP_PURGE" ]]; then
    echo "[skip-purge] OS page cache left warm" >&2
    return
  fi
  echo "[purge] dropping OS page cache…" >&2
  sudo -n purge
}

run_cell() {
  local op_label="$1"
  local approach="$2"
  local index="$3"
  drop_caches
  echo "[run] op='$op_label' approach=$approach index=$index" >&2
  "${NODE[@]}" "$TS" run \
    --op "$op_label" \
    --approach "$approach" \
    --file "$file_path" \
    --index "$index" \
    >> "$RESULTS_JSONL"
}

run_cell "first item"                    json-parse 0
run_cell "first item"                    bote       0
run_cell "middle item (arr[$mid_index])" json-parse "$mid_index"
run_cell "middle item (arr[$mid_index])" bote       "$mid_index"
run_cell "last item (arr[$last_index])"  json-parse "$last_index"
run_cell "last item (arr[$last_index])"  bote       "$last_index"

"${NODE[@]}" "$TS" render "$RESULTS_JSONL"
