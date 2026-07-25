#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MANIFEST="$ROOT/native/distill-core/Cargo.toml"
BINARY="$ROOT/native/distill-core/target/release/distill"
TOOLCHAIN="${DISTILL_COVERAGE_TOOLCHAIN:-nightly-2026-07-19}"
TARGET="$(rustc "+$TOOLCHAIN" -vV | awk '/^host:/ { print $2 }')"
FUZZ_BINARY="$ROOT/native/distill-core/fuzz/target/$TARGET/release/fuzz_engine"
FUZZ_EVIDENCE="${DISTILL_RELEASE_FUZZ_EVIDENCE:-$ROOT/evaluation/release/evidence/fuzz-linux-x86_64.json}"
SUITE_EVIDENCE="${DISTILL_RELEASE_SUITE_EVIDENCE:-$ROOT/evaluation/release/evidence/suite-linux-x86_64.json}"
REPORT="${DISTILL_RELEASE_REPORT:-$ROOT/evaluation/release/evidence/automated-linux-x86_64.json}"
PREVALIDATION="${DISTILL_NATIVE_PREVALIDATION:-}"
PREVALIDATION_RECORD="$PREVALIDATION"

if [[ "$(uname -s)" != "Linux" || "$(uname -m)" != "x86_64" ]]; then
  echo "automated release qualification requires Linux x86_64" >&2
  exit 1
fi

native_tree="$(git -C "$ROOT" rev-parse HEAD:native/distill-core)"
if [[ -n "$PREVALIDATION" ]]; then
  if [[ "$PREVALIDATION" == "$ROOT/"* ]]; then
    PREVALIDATION_RECORD="${PREVALIDATION#"$ROOT/"}"
  fi
  jq -e \
    --arg native_tree "$native_tree" \
    '.schema_version == "distill.native-prevalidation/v1" and
     .native_tree == $native_tree and
     .status == "PASS" and
     .after.lines.percent >= 85 and
     .after.branches.percent >= 75 and
     .commands == [
       "cargo test --manifest-path native/distill-core/Cargo.toml jsonl_request_without_source_is_invalid",
       "./scripts/check-native.sh"
     ]' \
    "$PREVALIDATION" >/dev/null
else
  "$ROOT/scripts/check-native.sh"
fi
cargo build --release --manifest-path "$MANIFEST"
cargo test --release --manifest-path "$MANIFEST"
mkdir -p \
  "$(dirname "$FUZZ_EVIDENCE")" \
  "$(dirname "$SUITE_EVIDENCE")" \
  "$(dirname "$REPORT")"
git_revision="$(git -C "$ROOT" rev-parse HEAD)"
binary_sha256="$(sha256sum "$BINARY" | awk '{ print $1 }')"
completed_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
jq -n \
  --arg git_revision "$git_revision" \
  --arg binary_sha256 "$binary_sha256" \
  --arg completed_at "$completed_at" \
  --arg native_tree "$native_tree" \
  --arg prevalidation "$PREVALIDATION_RECORD" \
  '{
    schema_version: "distill.release-suite/v1",
    target: "linux-x86_64",
    profile: "release",
    result: "passed",
    git_revision: $git_revision,
    binary_sha256: $binary_sha256,
    native_prevalidation: (
      if $prevalidation == ""
      then null
      else {evidence: $prevalidation, native_tree: $native_tree}
      end
    ),
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
