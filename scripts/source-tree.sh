#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REVISION="${1:-HEAD}"

git -C "$ROOT" ls-tree -r --full-tree "$REVISION" -- \
  Cargo.toml Cargo.lock clippy.toml rust-toolchain.toml src tests examples fuzz |
  git -C "$ROOT" hash-object --stdin
