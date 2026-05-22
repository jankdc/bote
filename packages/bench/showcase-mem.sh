#!/usr/bin/env bash
# Cold-cache memory showdown for JSON.parse vs bote.
#
# For each (approach, operation) cell, drops the OS page cache via
# `sudo purge` and spawns a fresh node process so V8 and the bote chunk
# cache start cold. Each process samples baseline `heapUsed` at the top,
# performs the work, then reports:
#
#   - JS heap Δ : peak `process.memoryUsage().heapUsed` − baseline
#   - Rust peak : for the bote path only, peak live bytes reported by
#                 the native crate's heap profiler between
#                 heapProfileStart and heapProfileStop, attributing peak
#                 live bytes to the bote crate's allocator activity.
#
# RSS is intentionally not reported: it's a process-wide high-water mark
# influenced by node's V8 arena sizing decisions, not by what's actually
# held live.
#
# Requires the native crate built with `--features heap-profile`. This
# orchestrator rebuilds it for you and reminds you to restore the
# release build (`yarn build`) afterwards, since the profiling allocator
# carries non-trivial overhead.
#
# Environment variables:
#   BYTES         Fixture size in bytes (default: 524288000 ≈ 500 MiB).
#   SKIP_PURGE    Set to skip OS-cache drops (warm OS cache; useful for
#                 iteration but not representative).
#   SKIP_BUILD    Set if the native is already built with --features
#                 heap-profile and you want to skip the rebuild step.

set -euo pipefail

BYTES="${BYTES:-524288000}"
SKIP_PURGE="${SKIP_PURGE:-}"
SKIP_BUILD="${SKIP_BUILD:-}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NATIVE_DIR="$SCRIPT_DIR/../native"
NODE=(yarn node --experimental-strip-types --no-warnings=ExperimentalWarning)
TS="$SCRIPT_DIR/showcase.ts"

if [[ -z "$SKIP_BUILD" ]]; then
  echo "[build] rebuilding @bote/native with --features heap-profile…" >&2
  ( cd "$NATIVE_DIR" && yarn napi build --platform --release -- --features heap-profile >&2 )
fi

# Prime sudo once for the run; background keepalive refreshes the
# credential timestamp every minute until exit so `sudo purge` between
# cells never re-prompts.
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

fixture_json=$("${NODE[@]}" "$TS" fixture --bytes "$BYTES")
file_path=$(printf '%s' "$fixture_json" | "${NODE[@]}" -e 'let s=""; process.stdin.on("data",d=>s+=d).on("end",()=>process.stdout.write(JSON.parse(s).filePath))')
count=$(printf '%s' "$fixture_json"   | "${NODE[@]}" -e 'let s=""; process.stdin.on("data",d=>s+=d).on("end",()=>process.stdout.write(String(JSON.parse(s).count)))')
mid_index=$((count / 2))
last_index=$((count - 1))

echo "fixture: $file_path  ($count items)" >&2

RESULTS_JSONL="$(mktemp -t bote-showcase-mem-results.XXXXXX)"
HEAP_PROFILE_DIR="$(mktemp -d -t bote-showcase-mem-heap.XXXXXX)"

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
  local heap_profile_arg=()
  if [[ "$approach" == "bote" ]]; then
    local slug
    slug=$(printf '%s' "$op_label" | tr -c 'A-Za-z0-9' '_')
    heap_profile_arg=(--heap-profile-out "$HEAP_PROFILE_DIR/heap-${approach}-${slug}.json")
  fi
  drop_caches
  echo "[run] op='$op_label' approach=$approach index=$index" >&2
  "${NODE[@]}" "$TS" mem \
    --op "$op_label" \
    --approach "$approach" \
    --file "$file_path" \
    --index "$index" \
    ${heap_profile_arg[@]+"${heap_profile_arg[@]}"} \
    >> "$RESULTS_JSONL"
}

run_cell "first item"                    json-parse 0
run_cell "first item"                    bote       0
run_cell "middle item (arr[$mid_index])" json-parse "$mid_index"
run_cell "middle item (arr[$mid_index])" bote       "$mid_index"
run_cell "last item (arr[$last_index])"  json-parse "$last_index"
run_cell "last item (arr[$last_index])"  bote       "$last_index"

"${NODE[@]}" "$TS" render-mem "$RESULTS_JSONL"

echo "" >&2
echo "heap profiles written to: $HEAP_PROFILE_DIR" >&2
echo "  the @bote/native heap-profile feature currently emits dhat-rs JSON;" >&2
echo "  open any of these at https://nnethercote.github.io/dh_view.html for per-call-stack attribution." >&2
echo "" >&2
echo "note: the @bote/native build is currently using --features heap-profile." >&2
echo "      run \`yarn build\` from the repo root to restore the normal release build." >&2
