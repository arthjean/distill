#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BINARY="$ROOT/target/release/distill"
OUTPUT_DIRECTORY="${DISTILL_PACKAGE_OUTPUT:-$ROOT/dist/native}"
HOST_TRIPLE="$(rustc -vV | awk '/^host:/ { print $2 }')"

case "$HOST_TRIPLE" in
  x86_64-unknown-linux-gnu)
    ASSET_TARGET="linux-x86_64"
    ;;
  aarch64-apple-darwin)
    ASSET_TARGET="macos-arm64"
    ;;
  *)
    echo "unsupported packaging host: $HOST_TRIPLE" >&2
    echo "supported hosts: x86_64-unknown-linux-gnu, aarch64-apple-darwin" >&2
    exit 2
    ;;
esac

(
  cd "$ROOT"
  cargo build --locked --release
)

if [[ ! -x "$BINARY" ]]; then
  echo "native release binary was not produced at $BINARY" >&2
  exit 1
fi

STAGING_DIRECTORY="$(mktemp -d "${TMPDIR:-/tmp}/distill-package.XXXXXX")"
cleanup() {
  if [[ -d "$STAGING_DIRECTORY" && "$(basename "$STAGING_DIRECTORY")" == distill-package.* ]]; then
    rm -rf "$STAGING_DIRECTORY"
  fi
}
trap cleanup EXIT

ASSET_BASENAME="distill-$ASSET_TARGET"
PACKAGE_DIRECTORY="$STAGING_DIRECTORY/$ASSET_BASENAME"
ARCHIVE_NAME="$ASSET_BASENAME.tar.gz"
TAR_PATH="$STAGING_DIRECTORY/$ASSET_BASENAME.tar"

mkdir -p "$PACKAGE_DIRECTORY" "$OUTPUT_DIRECTORY"
install -m 0755 "$BINARY" "$PACKAGE_DIRECTORY/distill"
install -m 0644 "$ROOT/LICENSE" "$PACKAGE_DIRECTORY/LICENSE"
install -m 0644 \
  "$ROOT/docs/distribution/native-install.md" \
  "$PACKAGE_DIRECTORY/README.md"

touch -t 198001010000 \
  "$PACKAGE_DIRECTORY" \
  "$PACKAGE_DIRECTORY/distill" \
  "$PACKAGE_DIRECTORY/LICENSE" \
  "$PACKAGE_DIRECTORY/README.md"

if tar --version 2>/dev/null | grep -q "GNU tar"; then
  tar \
    --format=ustar \
    --owner=0 \
    --group=0 \
    --numeric-owner \
    --mtime="1980-01-01 00:00:00Z" \
    -cf "$TAR_PATH" \
    -C "$STAGING_DIRECTORY" \
    "$ASSET_BASENAME/README.md" \
    "$ASSET_BASENAME/LICENSE" \
    "$ASSET_BASENAME/distill"
else
  COPYFILE_DISABLE=1 tar \
    --format=ustar \
    --uid 0 \
    --gid 0 \
    --uname root \
    --gname root \
    -cf "$TAR_PATH" \
    -C "$STAGING_DIRECTORY" \
    "$ASSET_BASENAME/README.md" \
    "$ASSET_BASENAME/LICENSE" \
    "$ASSET_BASENAME/distill"
fi

gzip -n -9 -c "$TAR_PATH" > "$OUTPUT_DIRECTORY/$ARCHIVE_NAME"

if command -v sha256sum >/dev/null 2>&1; then
  (
    cd "$OUTPUT_DIRECTORY"
    sha256sum "$ARCHIVE_NAME" > "$ARCHIVE_NAME.sha256"
  )
else
  (
    cd "$OUTPUT_DIRECTORY"
    shasum -a 256 "$ARCHIVE_NAME" > "$ARCHIVE_NAME.sha256"
  )
fi

printf '%s\n' "$OUTPUT_DIRECTORY/$ARCHIVE_NAME"
printf '%s\n' "$OUTPUT_DIRECTORY/$ARCHIVE_NAME.sha256"
