#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MANIFEST="$ROOT/native/distill-core/fuzz/Cargo.toml"
OUTPUT="${1:-$ROOT/evaluation/release/evidence/fuzz-linux-x86_64.json}"
TOOLCHAIN="${DISTILL_COVERAGE_TOOLCHAIN:-nightly-2026-07-19}"
WORKERS="${DISTILL_FUZZ_WORKERS:-16}"
WALL_SECONDS="${DISTILL_FUZZ_WALL_SECONDS:-300}"
TARGET="$(rustc "+$TOOLCHAIN" -vV | awk '/^host:/ { print $2 }')"
FUZZ_BINARY="$ROOT/native/distill-core/fuzz/target/$TARGET/release/fuzz_engine"
RUN_DIRECTORY="$(mktemp -d)"
TIMING="$RUN_DIRECTORY/timing.txt"
LOG="$RUN_DIRECTORY/fuzzer.log"
CORPUS="$RUN_DIRECTORY/corpus"

cleanup() {
  if [[ -d "$RUN_DIRECTORY" && "$RUN_DIRECTORY" == /tmp/* ]]; then
    rm -r "$RUN_DIRECTORY"
  fi
}
trap cleanup EXIT

mkdir -p "$(dirname "$OUTPUT")"
mkdir -p "$CORPUS"
(
  cd "$ROOT/native/distill-core"
  cargo "+$TOOLCHAIN" fuzz build fuzz_engine
)
if [[ ! -x "$FUZZ_BINARY" ]]; then
  echo "fuzz binary was not produced at $FUZZ_BINARY" >&2
  exit 1
fi
started_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
set +e
(
  cd "$RUN_DIRECTORY"
  /usr/bin/time -f '%U %S %e %x' -o "$TIMING" \
    "$FUZZ_BINARY" "$CORPUS" \
    "-jobs=$WORKERS" \
    "-workers=$WORKERS" \
    "-max_total_time=$WALL_SECONDS" \
    -max_len=65536 \
    -timeout=5 > "$LOG" 2>&1
)
process_status="$?"
set -e
read -r user_seconds system_seconds wall_seconds timed_status < "$TIMING"
completed_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
cpu_seconds="$(awk -v user="$user_seconds" -v sys="$system_seconds" 'BEGIN { printf "%.2f", user + sys }')"
git_revision="$(git -C "$ROOT" rev-parse HEAD)"
binary_sha256="$(sha256sum "$FUZZ_BINARY" | awk '{ print $1 }')"
result="failed"
if [[ "$process_status" -eq 0 && "$timed_status" -eq 0 ]] && ! rg -qi 'crash|deadly signal|timeout unit|invariant' "$LOG"; then
  result="clean"
fi

jq -n \
  --arg schema_version "distill.release-fuzz/v1" \
  --arg target "fuzz_engine" \
  --arg mode "release" \
  --arg result "$result" \
  --arg git_revision "$git_revision" \
  --arg binary_sha256 "$binary_sha256" \
  --arg started_at "$started_at" \
  --arg completed_at "$completed_at" \
  --argjson cpu_seconds "$cpu_seconds" \
  --argjson wall_seconds "$wall_seconds" \
  --argjson timeout_seconds 5 \
  --argjson workers "$WORKERS" \
  '{
    schema_version: $schema_version,
    target: $target,
    mode: $mode,
    result: $result,
    git_revision: $git_revision,
    binary_sha256: $binary_sha256,
    cpu_seconds: $cpu_seconds,
    wall_seconds: $wall_seconds,
    timeout_seconds: $timeout_seconds,
    workers: $workers,
    started_at: $started_at,
    completed_at: $completed_at
  }' > "$OUTPUT"

if [[ "$result" != "clean" ]] || ! awk -v cpu="$cpu_seconds" 'BEGIN { exit !(cpu >= 3600) }'; then
  echo "release fuzz gate failed: result=$result cpu_seconds=$cpu_seconds" >&2
  exit 1
fi
echo "$OUTPUT: clean (${cpu_seconds} CPU seconds)"
