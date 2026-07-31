#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PACKAGE_SOURCE="$ROOT/npm/distill"
OUTPUT_DIRECTORY="${DISTILL_NPM_PACKAGE_OUTPUT:-$ROOT/dist/npm}"
PACKAGE_FILENAME="arthjean-distill-0.1.0.tgz"
PACKAGE_RECORD="arthjean-distill-0.1.0.json"
QUALIFICATION_ID="architecture-hardening-v3-20260731"
SOURCE_TREE="61a582a3c2af991c8c88ddc5563cc6ca062b24f8"

if [[ "$#" -ne 4 ]]; then
  echo "usage: $0 <linux-archive> <linux-receipt> <macos-archive> <macos-receipt>" >&2
  exit 2
fi

LINUX_ARCHIVE="$(realpath "$1")"
LINUX_RECEIPT="$(realpath "$2")"
MACOS_ARCHIVE="$(realpath "$3")"
MACOS_RECEIPT="$(realpath "$4")"

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

verify_archive() {
  local archive="$1"
  local target="$2"
  local checksum="$archive.sha256"
  local expected
  local actual

  if [[ ! -f "$archive" || ! -f "$checksum" ]]; then
    echo "archive and adjacent checksum are required: $archive" >&2
    exit 1
  fi

  expected="$(awk 'NF { print $1; exit }' "$checksum")"
  actual="$(sha256 "$archive")"
  if [[ ! "$expected" =~ ^[0-9a-f]{64}$ || "$actual" != "$expected" ]]; then
    echo "archive checksum verification failed: $archive" >&2
    exit 1
  fi

  case "$target:$(file -b "$archive" 2>/dev/null || true)" in
    linux-x86_64:*gzip*) ;;
    macos-arm64:*gzip*) ;;
    *)
      echo "native archive is not gzip data: $archive" >&2
      exit 1
      ;;
  esac
}

verify_binary() {
  local binary="$1"
  local target="$2"
  local identity

  identity="$(file -b "$binary")"
  case "$target:$identity" in
    linux-x86_64:*ELF\ 64-bit*x86-64*) ;;
    macos-arm64:*Mach-O\ 64-bit\ arm64*) ;;
    *)
      echo "native binary does not match $target: $identity" >&2
      exit 1
      ;;
  esac
}

verify_receipt() {
  local receipt="$1"
  local target="$2"
  local binary="$3"
  local binary_sha256

  binary_sha256="$(sha256 "$binary")"
  jq -e \
    --arg qualification_id "$QUALIFICATION_ID" \
    --arg target "$target" \
    --arg source_tree "$SOURCE_TREE" \
    --arg binary_sha256 "$binary_sha256" \
    '
      .schema_version == "distill.architecture-hardening-receipt/v1" and
      .qualification_id == $qualification_id and
      .target == $target and
      .source_tree == $source_tree and
      .source_worktree_clean == true and
      .binary_sha256 == $binary_sha256 and
      .status == "GO"
    ' \
    "$receipt" >/dev/null || {
      echo "qualification receipt does not bind the embedded $target binary" >&2
      exit 1
    }
}

verify_archive "$LINUX_ARCHIVE" "linux-x86_64"
verify_archive "$MACOS_ARCHIVE" "macos-arm64"

if [[ ! -f "$LINUX_RECEIPT" || ! -f "$MACOS_RECEIPT" ]]; then
  echo "both qualification receipts are required" >&2
  exit 1
fi

STAGING_DIRECTORY="$(mktemp -d "${TMPDIR:-/tmp}/distill-npm.XXXXXX")"
cleanup() {
  if [[ -d "$STAGING_DIRECTORY" && "$(basename "$STAGING_DIRECTORY")" == distill-npm.* ]]; then
    rm -rf "$STAGING_DIRECTORY"
  fi
}
trap cleanup EXIT

PACKAGE_DIRECTORY="$STAGING_DIRECTORY/package"
mkdir -p \
  "$PACKAGE_DIRECTORY/bin" \
  "$PACKAGE_DIRECTORY/vendor/linux-x86_64" \
  "$PACKAGE_DIRECTORY/vendor/macos-arm64" \
  "$OUTPUT_DIRECTORY"

install -m 0644 "$PACKAGE_SOURCE/package.json" "$PACKAGE_DIRECTORY/package.json"
install -m 0644 "$PACKAGE_SOURCE/README.md" "$PACKAGE_DIRECTORY/README.md"
install -m 0644 "$ROOT/LICENSE" "$PACKAGE_DIRECTORY/LICENSE"
install -m 0755 "$PACKAGE_SOURCE/bin/distill" "$PACKAGE_DIRECTORY/bin/distill"

tar -xOf "$LINUX_ARCHIVE" \
  "distill-linux-x86_64/distill" \
  > "$PACKAGE_DIRECTORY/vendor/linux-x86_64/distill"
tar -xOf "$MACOS_ARCHIVE" \
  "distill-macos-arm64/distill" \
  > "$PACKAGE_DIRECTORY/vendor/macos-arm64/distill"
chmod 0755 \
  "$PACKAGE_DIRECTORY/vendor/linux-x86_64/distill" \
  "$PACKAGE_DIRECTORY/vendor/macos-arm64/distill"

verify_binary "$PACKAGE_DIRECTORY/vendor/linux-x86_64/distill" "linux-x86_64"
verify_binary "$PACKAGE_DIRECTORY/vendor/macos-arm64/distill" "macos-arm64"
verify_receipt \
  "$LINUX_RECEIPT" \
  "linux-x86_64" \
  "$PACKAGE_DIRECTORY/vendor/linux-x86_64/distill"
verify_receipt \
  "$MACOS_RECEIPT" \
  "macos-arm64" \
  "$PACKAGE_DIRECTORY/vendor/macos-arm64/distill"

LINUX_REVISION="$(jq -r '.git_revision' "$LINUX_RECEIPT")"
MACOS_REVISION="$(jq -r '.git_revision' "$MACOS_RECEIPT")"
if [[ ! "$LINUX_REVISION" =~ ^[0-9a-f]{40}$ || "$LINUX_REVISION" != "$MACOS_REVISION" ]]; then
  echo "qualification receipts do not bind the same source revision" >&2
  exit 1
fi

(
  cd "$PACKAGE_DIRECTORY"
  bun pm pack \
    --destination "$OUTPUT_DIRECTORY" \
    --filename "$PACKAGE_FILENAME"
)

PACKAGE_SHA256="$(sha256 "$OUTPUT_DIRECTORY/$PACKAGE_FILENAME")"
jq -n \
  --arg qualification_id "$QUALIFICATION_ID" \
  --arg git_revision "$LINUX_REVISION" \
  --arg source_tree "$SOURCE_TREE" \
  --arg package_sha256 "$PACKAGE_SHA256" \
  --arg linux_binary_sha256 "$(jq -r '.binary_sha256' "$LINUX_RECEIPT")" \
  --arg macos_binary_sha256 "$(jq -r '.binary_sha256' "$MACOS_RECEIPT")" \
  '{
    schema_version: "distill.native-npm-package/v1",
    package: "@arthjean/distill",
    version: "0.1.0",
    qualification_id: $qualification_id,
    git_revision: $git_revision,
    source_tree: $source_tree,
    package_sha256: $package_sha256,
    embedded_binaries: {
      "linux-x86_64": $linux_binary_sha256,
      "macos-arm64": $macos_binary_sha256
    }
  }' > "$OUTPUT_DIRECTORY/$PACKAGE_RECORD"
chmod 0600 "$OUTPUT_DIRECTORY/$PACKAGE_RECORD"

printf '%s\n' "$OUTPUT_DIRECTORY/$PACKAGE_FILENAME"
printf '%s\n' "$OUTPUT_DIRECTORY/$PACKAGE_RECORD"
