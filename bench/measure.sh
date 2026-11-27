#!/usr/bin/env bash
# Runs both M-1 spikes and prints a markdown comparison table.
#
# Usage:
#   bench/measure.sh
#
# Requires a display (or xvfb-run) and both spikes built in release mode:
#   cargo build --release --manifest-path apps/spike-egui/Cargo.toml
#   (cd apps/spike-tauri && pnpm build)
#   cargo build --release --manifest-path apps/spike-tauri/src-tauri/Cargo.toml
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EGUI="$ROOT/apps/spike-egui/target/release/spike-egui"
TAURI="$ROOT/apps/spike-tauri/src-tauri/target/release/spike-tauri"

if [[ -z "${DISPLAY:-}" && -z "${WAYLAND_DISPLAY:-}" ]]; then
  if command -v xvfb-run >/dev/null 2>&1; then
    exec xvfb-run -a --server-args="-screen 0 1600x1000x24" "$0" "$@"
  fi
  echo "error: no display and no xvfb-run available" >&2
  exit 1
fi

tree_rss_kb() {
  local pid="$1" total=0 rss child
  if [[ -r "/proc/$pid/status" ]]; then
    rss="$(awk '/^VmRSS:/{print $2}' "/proc/$pid/status" 2>/dev/null || true)"
    total=$(( ${rss:-0} ))
  fi
  for child in $(ps -o pid= --ppid "$pid" 2>/dev/null || true); do
    total=$(( total + $(tree_rss_kb "$child") ))
  done
  echo "$total"
}

measure() {
  local name="$1" bin="$2"
  [[ -x "$bin" ]] || { echo "missing binary: $bin" >&2; exit 1; }

  local out; out="$(mktemp)"
  "$bin" >"$out" 2>/dev/null &
  local pid=$!

  for _ in $(seq 1 600); do
    grep -q 'SPANNERS_READY' "$out" 2>/dev/null && break
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.1
  done

  local ready rss
  ready="$(awk '/SPANNERS_READY/{print $2; exit}' "$out")"
  if [[ -z "$ready" ]]; then
    echo "  ! $name never reported ready" >&2
    kill "$pid" 2>/dev/null || true
    rm -f "$out"
    return 1
  fi

  sleep 1
  rss="$(tree_rss_kb "$pid")"
  wait "$pid" 2>/dev/null || true

  local grid editor rss_mb
  grid="$(awk '/SPANNERS_FRAME_P95_GRID/{print $2; exit}' "$out")"
  editor="$(awk '/SPANNERS_FRAME_P95_EDITOR/{print $2; exit}' "$out")"
  rss_mb="$(awk -v kb="$rss" 'BEGIN{printf "%.0f", kb/1024}')"

  printf '| %s | %s | %s | %s | %s |\n' \
    "$name" "${ready} ms" "${grid:-n/a} ms" "${editor:-n/a} ms" "${rss_mb} MB"
  rm -f "$out"
}

echo
echo "| Shell | Cold start (first painted grid) | Grid p95 frame | Editor p95 frame | Peak RSS (tree) |"
echo "| --- | --- | --- | --- | --- |"
measure "Tauri + CodeMirror" "$TAURI"
measure "egui" "$EGUI"
echo
echo "Pass bar (PLAN.md §3.2): Tauri needs <=700 ms start, <=16.7 ms p95, <=200 MB."
