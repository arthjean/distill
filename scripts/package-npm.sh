#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PACKAGE_SOURCE="$ROOT/npm/distill"
OUTPUT_DIRECTORY="${DISTILL_NPM_PACKAGE_OUTPUT:-$ROOT/dist/npm}"
PROTOCOL="$ROOT/evaluation/release/architecture-hardening-v6-protocol.json"
QUALIFICATION_ID="architecture-hardening-v6-20260731"
SOURCE_TREE="952711e440754621080d45f8870c55c3b3ce17c3"
PACKAGE_NAME="$(jq -er '.name' "$PACKAGE_SOURCE/package.json")"
PACKAGE_VERSION="$(jq -er '.version' "$PACKAGE_SOURCE/package.json")"
PACKAGE_BASENAME="${PACKAGE_NAME#@}"
PACKAGE_BASENAME="${PACKAGE_BASENAME//\//-}"
PACKAGE_FILENAME="$PACKAGE_BASENAME-$PACKAGE_VERSION.tgz"
PACKAGE_RECORD="$PACKAGE_BASENAME-$PACKAGE_VERSION.json"

if [[ "$PACKAGE_NAME" != "@arthjean/distill" || "$PACKAGE_VERSION" != "0.1.0" ]]; then
  echo "unexpected npm package identity: $PACKAGE_NAME@$PACKAGE_VERSION" >&2
  exit 1
fi

if [[ "$#" -ne 5 ]]; then
  echo "usage: $0 <linux-archive> <linux-receipt> <macos-archive> <macos-receipt> <aggregate-receipt>" >&2
  exit 2
fi

LINUX_ARCHIVE="$(realpath "$1")"
LINUX_RECEIPT="$(realpath "$2")"
MACOS_ARCHIVE="$(realpath "$3")"
MACOS_RECEIPT="$(realpath "$4")"
AGGREGATE_RECEIPT="$(realpath "$5")"

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
    --slurpfile protocol "$PROTOCOL" \
    --arg qualification_id "$QUALIFICATION_ID" \
    --arg target "$target" \
    --arg source_tree "$SOURCE_TREE" \
    --arg binary_sha256 "$binary_sha256" \
    '. as $receipt |
      ($protocol[0].gates | keys) as $required_gates |
      .schema_version == "distill.architecture-hardening-receipt/v1" and
      .qualification_id == $qualification_id and
      .target == $target and
      .source_tree == $source_tree and
      .source_worktree_clean == true and
      .binary_sha256 == $binary_sha256 and
      .status == "GO" and
      (.gates | keys) == $required_gates and
      all($required_gates[]; $receipt.gates[.] == "PASS")
    ' \
    "$receipt" >/dev/null || {
      echo "qualification receipt does not bind the embedded $target binary" >&2
      exit 1
    }
}

verify_aggregate() {
  local aggregate="$1"
  local linux_receipt_sha256
  local macos_receipt_sha256

  linux_receipt_sha256="$(sha256 "$LINUX_RECEIPT")"
  macos_receipt_sha256="$(sha256 "$MACOS_RECEIPT")"
  jq -e \
    --slurpfile linux "$LINUX_RECEIPT" \
    --slurpfile macos "$MACOS_RECEIPT" \
    --arg qualification_id "$QUALIFICATION_ID" \
    --arg source_tree "$SOURCE_TREE" \
    --arg linux_receipt_sha256 "$linux_receipt_sha256" \
    --arg macos_receipt_sha256 "$macos_receipt_sha256" \
    '.schema_version == "distill.architecture-hardening-aggregate/v1" and
      .qualification_id == $qualification_id and
      .source_tree == $source_tree and
      .git_revision == $linux[0].git_revision and
      .git_revision == $macos[0].git_revision and
      .status == "GO" and
      (.defects | length) == 0 and
      .decision_rule.publication_claim_permitted == true and
      .platform_receipts["linux-x86_64"].sha256 == $linux_receipt_sha256 and
      .platform_receipts["linux-x86_64"].binary_sha256 == $linux[0].binary_sha256 and
      .platform_receipts["linux-x86_64"].status == "GO" and
      .platform_receipts["macos-arm64"].sha256 == $macos_receipt_sha256 and
      .platform_receipts["macos-arm64"].binary_sha256 == $macos[0].binary_sha256 and
      .platform_receipts["macos-arm64"].status == "GO"' \
    "$aggregate" >/dev/null || {
      echo "aggregate receipt does not permit npm publication" >&2
      exit 1
    }
}

verify_archive "$LINUX_ARCHIVE" "linux-x86_64"
verify_archive "$MACOS_ARCHIVE" "macos-arm64"

if [[ ! -f "$LINUX_RECEIPT" || ! -f "$MACOS_RECEIPT" || ! -f "$AGGREGATE_RECEIPT" ]]; then
  echo "both platform receipts and the aggregate receipt are required" >&2
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

verify_aggregate "$AGGREGATE_RECEIPT"

TZ=UTC touch -t 198001010000 \
  "$PACKAGE_DIRECTORY" \
  "$PACKAGE_DIRECTORY/package.json" \
  "$PACKAGE_DIRECTORY/README.md" \
  "$PACKAGE_DIRECTORY/LICENSE" \
  "$PACKAGE_DIRECTORY/bin" \
  "$PACKAGE_DIRECTORY/bin/distill" \
  "$PACKAGE_DIRECTORY/vendor" \
  "$PACKAGE_DIRECTORY/vendor/linux-x86_64" \
  "$PACKAGE_DIRECTORY/vendor/linux-x86_64/distill" \
  "$PACKAGE_DIRECTORY/vendor/macos-arm64" \
  "$PACKAGE_DIRECTORY/vendor/macos-arm64/distill"

TAR_PATH="$STAGING_DIRECTORY/package.tar"
if tar --version 2>/dev/null | grep -q "GNU tar"; then
  tar \
    --format=ustar \
    --owner=0 \
    --group=0 \
    --numeric-owner \
    --mtime="1980-01-01 00:00:00Z" \
    -cf "$TAR_PATH" \
    -C "$STAGING_DIRECTORY" \
    package/package.json \
    package/LICENSE \
    package/README.md \
    package/bin/distill \
    package/vendor/linux-x86_64/distill \
    package/vendor/macos-arm64/distill
else
  COPYFILE_DISABLE=1 tar \
    --format=ustar \
    --uid 0 \
    --gid 0 \
    --uname root \
    --gname root \
    -cf "$TAR_PATH" \
    -C "$STAGING_DIRECTORY" \
    package/package.json \
    package/LICENSE \
    package/README.md \
    package/bin/distill \
    package/vendor/linux-x86_64/distill \
    package/vendor/macos-arm64/distill
fi
gzip -n -9 -c "$TAR_PATH" > "$OUTPUT_DIRECTORY/$PACKAGE_FILENAME"

PACKAGE_SHA256="$(sha256 "$OUTPUT_DIRECTORY/$PACKAGE_FILENAME")"
jq -n \
  --arg package "$PACKAGE_NAME" \
  --arg version "$PACKAGE_VERSION" \
  --arg qualification_id "$QUALIFICATION_ID" \
  --arg git_revision "$LINUX_REVISION" \
  --arg source_tree "$SOURCE_TREE" \
  --arg package_sha256 "$PACKAGE_SHA256" \
  --arg aggregate_receipt_sha256 "$(sha256 "$AGGREGATE_RECEIPT")" \
  --arg linux_binary_sha256 "$(jq -r '.binary_sha256' "$LINUX_RECEIPT")" \
  --arg macos_binary_sha256 "$(jq -r '.binary_sha256' "$MACOS_RECEIPT")" \
  '{
    schema_version: "distill.native-npm-package/v1",
    package: $package,
    version: $version,
    qualification_id: $qualification_id,
    git_revision: $git_revision,
    source_tree: $source_tree,
    aggregate_receipt_sha256: $aggregate_receipt_sha256,
    package_sha256: $package_sha256,
    embedded_binaries: {
      "linux-x86_64": $linux_binary_sha256,
      "macos-arm64": $macos_binary_sha256
    }
  }' > "$OUTPUT_DIRECTORY/$PACKAGE_RECORD"
chmod 0600 "$OUTPUT_DIRECTORY/$PACKAGE_RECORD"

printf '%s\n' "$OUTPUT_DIRECTORY/$PACKAGE_FILENAME"
printf '%s\n' "$OUTPUT_DIRECTORY/$PACKAGE_RECORD"
