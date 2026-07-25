#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MANIFEST="$ROOT/native/distill-core/Cargo.toml"
BINARY="$ROOT/native/distill-core/target/release/distill"
OUTPUT="${1:-$ROOT/evaluation/release/evidence/macos-arm64.json}"
RUN_DIRECTORY="$(mktemp -d "${TMPDIR:-/tmp}/distill-macos.XXXXXX")"
source_worktree_clean=false

cleanup() {
  if [[ -d "$RUN_DIRECTORY" && "$(basename "$RUN_DIRECTORY")" == distill-macos.* ]]; then
    rm -r "$RUN_DIRECTORY"
  fi
}

finish() {
  local exit_code="$?"
  set +e
  if [[ "$exit_code" -ne 0 ]]; then
    mkdir -p "$(dirname "$OUTPUT")"
    binary_sha256=""
    if [[ -f "$BINARY" ]]; then
      binary_sha256="$(shasum -a 256 "$BINARY" | awk '{ print $1 }')"
    fi
    jq -n \
      --arg git_revision "${GITHUB_SHA:-$(git -C "$ROOT" rev-parse HEAD)}" \
      --arg architecture "$(uname -m)" \
      --arg binary_sha256 "$binary_sha256" \
      --arg workflow_run_url "${GITHUB_SERVER_URL:-https://github.com}/${GITHUB_REPOSITORY:-unknown}/actions/runs/${GITHUB_RUN_ID:-unknown}" \
      --arg completed_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
      --argjson source_worktree_clean "$source_worktree_clean" \
      --argjson exit_code "$exit_code" \
      '{
        schema_version: "distill.macos-qualification/v1",
        target: "macos-arm64",
        git_revision: $git_revision,
        source_worktree_clean: $source_worktree_clean,
        binary_sha256: (if $binary_sha256 == "" then null else $binary_sha256 end),
        machine: { architecture: $architecture },
        failure_exit_code: $exit_code,
        workflow_run_url: $workflow_run_url,
        completed_at: $completed_at,
        status: "NO-GO"
      }' > "$OUTPUT"
  fi
  cleanup
}
trap finish EXIT

if [[ "$(uname -s)" != "Darwin" || "$(uname -m)" != "arm64" ]]; then
  echo "macOS qualification requires a Darwin arm64 runner" >&2
  exit 1
fi
if [[ -n "$(git -C "$ROOT" status --porcelain --untracked-files=all)" ]]; then
  echo "macOS qualification requires a clean source worktree" >&2
  exit 1
fi
source_worktree_clean=true

cargo fmt --manifest-path "$MANIFEST" --check
cargo clippy --manifest-path "$MANIFEST" --all-targets -- -D warnings
cargo test --release --manifest-path "$MANIFEST"
cargo build --release --manifest-path "$MANIFEST"

assert_action() {
  local receipt="$1"
  local expected="$2"
  jq -e --arg expected "$expected" '.action == $expected' "$receipt" >/dev/null
}

codex_config="$RUN_DIRECTORY/codex/hooks.json"
claude_config="$RUN_DIRECTORY/claude/settings.json"
store="$RUN_DIRECTORY/store/artifacts.sqlite"
workspace="$RUN_DIRECTORY/workspace"
mkdir -p "$workspace"

"$BINARY" setup codex \
  --config "$codex_config" \
  --command "$BINARY" \
  --store "$store" > "$RUN_DIRECTORY/codex-install.json"
"$BINARY" setup codex \
  --config "$codex_config" \
  --command "$BINARY" \
  --store "$store" > "$RUN_DIRECTORY/codex-repeat.json"
"$BINARY" setup codex \
  --config "$codex_config" \
  --restore > "$RUN_DIRECTORY/codex-uninstall.json"
assert_action "$RUN_DIRECTORY/codex-install.json" "installed"
assert_action "$RUN_DIRECTORY/codex-repeat.json" "unchanged"
assert_action "$RUN_DIRECTORY/codex-uninstall.json" "restored_absent"
[[ ! -e "$codex_config" ]]

"$BINARY" setup claude \
  --config "$claude_config" \
  --command "$BINARY" \
  --root "workspace=$workspace" > "$RUN_DIRECTORY/claude-install.json"
"$BINARY" setup claude \
  --config "$claude_config" \
  --command "$BINARY" \
  --root "workspace=$workspace" > "$RUN_DIRECTORY/claude-repeat.json"
"$BINARY" setup claude \
  --config "$claude_config" \
  --restore > "$RUN_DIRECTORY/claude-uninstall.json"
assert_action "$RUN_DIRECTORY/claude-install.json" "installed"
assert_action "$RUN_DIRECTORY/claude-repeat.json" "unchanged"
assert_action "$RUN_DIRECTORY/claude-uninstall.json" "restored_absent"
[[ ! -e "$claude_config" ]]

for index in {1..256}; do
  printf 'macOS arm64 projection and restore proof line %03d\n' "$index"
done > "$RUN_DIRECTORY/expected.txt"
"$BINARY" \
  --store "$store" \
  project \
  --budget 128 \
  --json < "$RUN_DIRECTORY/expected.txt" > "$RUN_DIRECTORY/project.json"
jq -e \
  '.ok == true and
   .result.receipt.fidelity == "extractive" and
   (.result.artifact.id | type == "string")' \
  "$RUN_DIRECTORY/project.json" >/dev/null
artifact_id="$(jq -er '.result.artifact.id' "$RUN_DIRECTORY/project.json")"
"$BINARY" \
  --store "$store" \
  artifact get "$artifact_id" > "$RUN_DIRECTORY/restored.txt"
cmp "$RUN_DIRECTORY/expected.txt" "$RUN_DIRECTORY/restored.txt"
binary_sha256="$(shasum -a 256 "$BINARY" | awk '{ print $1 }')"

mkdir -p "$(dirname "$OUTPUT")"
jq -n \
  --arg git_revision "${GITHUB_SHA:-$(git -C "$ROOT" rev-parse HEAD)}" \
  --arg os_version "$(sw_vers -productVersion)" \
  --arg kernel "$(uname -r)" \
  --arg architecture "$(uname -m)" \
  --arg rustc "$(rustc --version)" \
  --arg binary_sha256 "$binary_sha256" \
  --arg workflow_run_url "${GITHUB_SERVER_URL:-https://github.com}/${GITHUB_REPOSITORY:-unknown}/actions/runs/${GITHUB_RUN_ID:-unknown}" \
  --arg completed_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  --slurpfile codex_install "$RUN_DIRECTORY/codex-install.json" \
  --slurpfile codex_repeat "$RUN_DIRECTORY/codex-repeat.json" \
  --slurpfile codex_uninstall "$RUN_DIRECTORY/codex-uninstall.json" \
  --slurpfile claude_install "$RUN_DIRECTORY/claude-install.json" \
  --slurpfile claude_repeat "$RUN_DIRECTORY/claude-repeat.json" \
  --slurpfile claude_uninstall "$RUN_DIRECTORY/claude-uninstall.json" \
  --slurpfile project "$RUN_DIRECTORY/project.json" \
  '{
    schema_version: "distill.macos-qualification/v1",
    target: "macos-arm64",
    git_revision: $git_revision,
    source_worktree_clean: true,
    binary_sha256: $binary_sha256,
    machine: {
      os_version: $os_version,
      kernel: $kernel,
      architecture: $architecture,
      rustc: $rustc
    },
    release_suite: {
      format: "passed",
      clippy: "passed",
      contract_and_corpus_tests: "passed",
      build: "passed"
    },
    surfaces: {
      codex: {
        install: $codex_install[0].action,
        repeat: $codex_repeat[0].action,
        uninstall: $codex_uninstall[0].action
      },
      claude: {
        install: $claude_install[0].action,
        repeat: $claude_repeat[0].action,
        uninstall: $claude_uninstall[0].action
      },
      project: {
        fidelity: $project[0].result.receipt.fidelity,
        artifact_id: $project[0].result.artifact.id,
        byte_exact_restore: true
      }
    },
    workflow_run_url: $workflow_run_url,
    completed_at: $completed_at,
    status: "GO"
  }' > "$OUTPUT"

echo "$OUTPUT: go"
