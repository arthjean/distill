#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MANIFEST="$ROOT/native/distill-core/Cargo.toml"
BINARY="$ROOT/native/distill-core/target/release/distill"
TOOLCHAIN="${DISTILL_COVERAGE_TOOLCHAIN:-nightly-2026-07-19}"
TARGET="$(rustc "+$TOOLCHAIN" -vV | awk '/^host:/ { print $2 }')"
FUZZ_BINARY="$ROOT/native/distill-core/fuzz/target/$TARGET/release/fuzz_engine"
FUZZ_EVIDENCE="$ROOT/evaluation/release/evidence/fuzz-linux-x86_64.json"
SUITE_EVIDENCE="$ROOT/evaluation/release/evidence/suite-linux-x86_64.json"
REPORT="$ROOT/evaluation/release/evidence/automated-linux-x86_64.json"

if [[ "$(uname -s)" != "Linux" || "$(uname -m)" != "x86_64" ]]; then
  echo "automated release qualification requires Linux x86_64" >&2
  exit 1
fi

"$ROOT/scripts/check-native.sh"
cargo build --release --manifest-path "$MANIFEST"
cargo test --release --manifest-path "$MANIFEST"
mkdir -p "$(dirname "$SUITE_EVIDENCE")"
git_revision="$(git -C "$ROOT" rev-parse HEAD)"
binary_sha256="$(sha256sum "$BINARY" | awk '{ print $1 }')"
completed_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
jq -n \
  --arg git_revision "$git_revision" \
  --arg binary_sha256 "$binary_sha256" \
  --arg completed_at "$completed_at" \
  '{
    schema_version: "distill.release-suite/v1",
    target: "linux-x86_64",
    profile: "release",
    result: "passed",
    git_revision: $git_revision,
    binary_sha256: $binary_sha256,
    commands: [
      "./scripts/check-native.sh",
      "cargo build --release --manifest-path native/distill-core/Cargo.toml",
      "cargo test --release --manifest-path native/distill-core/Cargo.toml"
    ],
    completed_at: $completed_at
  }' > "$SUITE_EVIDENCE"
"$ROOT/scripts/run-release-fuzz.sh" "$FUZZ_EVIDENCE"
cargo run --release --manifest-path "$MANIFEST" --example release_gate -- \
  --binary "$BINARY" \
  --corpus-root "$ROOT/evaluation/corpus" \
  --fuzz-binary "$FUZZ_BINARY" \
  --fuzz-evidence "$FUZZ_EVIDENCE" \
  --suite-evidence "$SUITE_EVIDENCE" \
  --output "$REPORT"
