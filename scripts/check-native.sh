#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MANIFEST="$ROOT/native/distill-core/Cargo.toml"
COVERAGE_JSON="$ROOT/native/distill-core/target/llvm-cov-summary.json"
COVERAGE_TOOLCHAIN="${DISTILL_COVERAGE_TOOLCHAIN:-nightly-2026-07-19}"

cargo fmt --manifest-path "$MANIFEST" --check
cargo clippy --manifest-path "$MANIFEST" --all-targets -- -D warnings
cargo "+$COVERAGE_TOOLCHAIN" llvm-cov \
  --manifest-path "$MANIFEST" \
  --all-targets \
  --branch \
  --json \
  --summary-only \
  --output-path "$COVERAGE_JSON" \
  --fail-under-lines 85
jq -e '.data[0].totals.branches.percent >= 75' "$COVERAGE_JSON" >/dev/null || {
  branch_coverage="$(jq -r '.data[0].totals.branches.percent' "$COVERAGE_JSON")"
  echo "branch coverage ${branch_coverage}% is below the 75% floor" >&2
  exit 1
}
(
  cd "$ROOT/native/distill-core"
  cargo "+$COVERAGE_TOOLCHAIN" fuzz build fuzz_engine
)
