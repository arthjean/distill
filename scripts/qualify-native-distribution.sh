#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROTOCOL="$ROOT/evaluation/release/native-distribution-v1-protocol.json"
OUTPUT_ROOT="${1:-/tmp/distill-native-distribution-v1-20260724}"
EXECUTION_EVIDENCE="$OUTPUT_ROOT/native-distribution-execution-v1.json"
LOG="$OUTPUT_ROOT/qualification.log"
phase="preflight"
first_defect=""

if [[ -e "$OUTPUT_ROOT" ]]; then
  echo "qualification output already exists: $OUTPUT_ROOT" >&2
  exit 2
fi
mkdir "$OUTPUT_ROOT"
exec > >(tee -a "$LOG") 2>&1

fail() {
  first_defect="$1"
  echo "$first_defect" >&2
  exit 1
}

finish() {
  local exit_code="$?"
  set +e
  if [[ "$exit_code" -ne 0 ]]; then
    if [[ -z "$first_defect" ]]; then
      first_defect="$(tail -n 1 "$LOG")"
    fi
    jq -n \
      --arg candidate "${candidate:-}" \
      --arg native_tree "${native_tree:-}" \
      --arg phase "$phase" \
      --arg first_defect "$first_defect" \
      --arg completed_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
      --argjson exit_code "$exit_code" \
      '{
        schema_version: "distill.native-distribution-execution/v1",
        qualification_id: "native-distribution-v1-20260724",
        candidate_revision: (if $candidate == "" then null else $candidate end),
        native_tree: (if $native_tree == "" then null else $native_tree end),
        failed_phase: $phase,
        first_defect: $first_defect,
        exit_code: $exit_code,
        completed_at: $completed_at,
        status: "NO-GO"
      }' > "$EXECUTION_EVIDENCE"
  fi
}
trap finish EXIT

if [[ "$(uname -s)" != "Linux" || "$(uname -m)" != "x86_64" ]]; then
  fail "native distribution qualification requires Linux x86_64"
fi
if [[ -n "$(git -C "$ROOT" status --porcelain --untracked-files=all)" ]]; then
  fail "native distribution qualification requires a clean worktree"
fi

candidate="$(git -C "$ROOT" rev-parse HEAD)"
native_tree="$(git -C "$ROOT" rev-parse HEAD:native/distill-core)"
coverage_relative="$(jq -er '.coverage.evidence' "$PROTOCOL")"
coverage="$ROOT/$coverage_relative"
coverage_sha256="$(sha256sum "$coverage" | awk '{ print $1 }')"
attestation_ref="$(jq -er '.candidate_binding.attestation_ref' "$PROTOCOL")"
workflow_relative="$(jq -er '.macos.workflow' "$PROTOCOL")"
workflow_sha256="$(sha256sum "$ROOT/$workflow_relative" | awk '{ print $1 }')"

jq -e \
  --arg native_tree "$native_tree" \
  --arg coverage_sha256 "$coverage_sha256" \
  --arg workflow_sha256 "$workflow_sha256" \
  '.schema_version == "distill.native-distribution-protocol/v1" and
   .qualification_id == "native-distribution-v1-20260724" and
   .status == "PREREGISTERED" and
   .native_tree == $native_tree and
   .coverage.sha256 == $coverage_sha256 and
   .coverage.full_native_check_executions == 1 and
   .linux.executions == 1 and
   .linux.package_repetitions == 2 and
   .macos.executions == 1 and
   .macos.workflow_sha256 == $workflow_sha256 and
   .failure_policy.retry == false' \
  "$PROTOCOL" >/dev/null ||
  fail "preregistered qualification protocol does not match the candidate"

remote_attestation="$(git -C "$ROOT" ls-remote origin "$attestation_ref" | awk '{ print $1 }')"
if [[ "$remote_attestation" != "$candidate" ]]; then
  fail "attestation ref does not point to the clean candidate"
fi
branch="$(git -C "$ROOT" branch --show-current)"
remote_branch="$(git -C "$ROOT" ls-remote --heads origin "refs/heads/$branch" | awk '{ print $1 }')"
if [[ "$remote_branch" != "$candidate" ]]; then
  fail "remote branch does not point to the clean candidate"
fi

phase="linux-release"
set +e
DISTILL_NATIVE_PREVALIDATION="$coverage" \
DISTILL_RELEASE_REPORT="$OUTPUT_ROOT/automated-linux-x86_64-v2.json" \
DISTILL_RELEASE_SUITE_EVIDENCE="$OUTPUT_ROOT/suite-linux-x86_64-v2.json" \
DISTILL_RELEASE_FUZZ_EVIDENCE="$OUTPUT_ROOT/fuzz-linux-x86_64-v2.json" \
  "$ROOT/scripts/qualify-release.sh"
linux_status="$?"
set -e
if [[ "$linux_status" -ne 0 ]]; then
  fail "Linux release qualification failed: $(tail -n 1 "$LOG")"
fi
jq -e \
  --arg candidate "$candidate" \
  '.status == "GO" and .git_revision == $candidate' \
  "$OUTPUT_ROOT/automated-linux-x86_64-v2.json" >/dev/null ||
  fail "Linux release qualification did not emit GO for the candidate"

phase="package-a"
set +e
DISTILL_PACKAGE_OUTPUT="$OUTPUT_ROOT/package-a" "$ROOT/scripts/package-native.sh"
package_a_status="$?"
set -e
if [[ "$package_a_status" -ne 0 ]]; then
  fail "first Linux package build failed: $(tail -n 1 "$LOG")"
fi

phase="package-b"
set +e
DISTILL_PACKAGE_OUTPUT="$OUTPUT_ROOT/package-b" "$ROOT/scripts/package-native.sh"
package_b_status="$?"
set -e
if [[ "$package_b_status" -ne 0 ]]; then
  fail "second Linux package build failed: $(tail -n 1 "$LOG")"
fi

archive_a="$OUTPUT_ROOT/package-a/distill-linux-x86_64.tar.gz"
archive_b="$OUTPUT_ROOT/package-b/distill-linux-x86_64.tar.gz"
checksum_a="$archive_a.sha256"
checksum_b="$archive_b.sha256"
(
  cd "$(dirname "$archive_a")"
  sha256sum -c "$(basename "$checksum_a")"
)
(
  cd "$(dirname "$archive_b")"
  sha256sum -c "$(basename "$checksum_b")"
)
cmp "$archive_a" "$archive_b" ||
  fail "Linux package archives are not byte-identical"
archive_sha256="$(sha256sum "$archive_a" | awk '{ print $1 }')"
checksum_a_sha256="$(sha256sum "$checksum_a" | awk '{ print $1 }')"
checksum_b_sha256="$(sha256sum "$checksum_b" | awk '{ print $1 }')"

jq -n \
  --arg candidate "$candidate" \
  --arg native_tree "$native_tree" \
  --arg archive_sha256 "$archive_sha256" \
  --arg checksum_a_sha256 "$checksum_a_sha256" \
  --arg checksum_b_sha256 "$checksum_b_sha256" \
  --arg completed_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  '{
    schema_version: "distill.native-package-qualification/v1",
    target: "linux-x86_64",
    candidate_revision: $candidate,
    native_tree: $native_tree,
    repetitions: 2,
    archive: "distill-linux-x86_64.tar.gz",
    archive_sha256: $archive_sha256,
    checksum_sidecars: {
      first_sha256: $checksum_a_sha256,
      second_sha256: $checksum_b_sha256,
      independently_validated: true
    },
    byte_identical: true,
    completed_at: $completed_at,
    status: "GO"
  }' > "$OUTPUT_ROOT/package-linux-x86_64-v1.json"

phase="complete"
jq -n \
  --arg candidate "$candidate" \
  --arg native_tree "$native_tree" \
  --arg completed_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  '{
    schema_version: "distill.native-distribution-execution/v1",
    qualification_id: "native-distribution-v1-20260724",
    candidate_revision: $candidate,
    native_tree: $native_tree,
    linux_executions: 1,
    package_repetitions: 2,
    full_native_check_executions: 1,
    first_defect: null,
    completed_at: $completed_at,
    status: "GO"
  }' > "$EXECUTION_EVIDENCE"

trap - EXIT
echo "$OUTPUT_ROOT: GO"
