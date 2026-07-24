# Distill language spikes

EP-002 compares Zig and Rust on one deliberately narrow vertical slice. Both
binaries implement the versioned JSONL protocol in
[`shared/protocol.schema.json`](shared/protocol.schema.json), link the host
SQLite library, use WAL with `synchronous=FULL`, and are evaluated by the same
runner.

The spike is evidence, not the production engine. It supports inline capture,
artifact recovery, and one argv-only process source. Token budgets deliberately
return `token_profile_unsupported`; no candidate substitutes byte estimates for
the named tokenizer.

## Reproduction

Install the pinned toolchains:

```bash
curl -LO https://ziglang.org/download/0.16.0/zig-x86_64-linux-0.16.0.tar.xz
echo "70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00  zig-x86_64-linux-0.16.0.tar.xz" | sha256sum --check
tar -xf zig-x86_64-linux-0.16.0.tar.xz
rustup toolchain install 1.97.1
```

From the repository root:

```bash
cd spikes/zig
zig fmt --check build.zig src
zig build test
zig build -Doptimize=ReleaseFast
cd ../..

cargo fmt --manifest-path spikes/rust/Cargo.toml --check
cargo clippy --manifest-path spikes/rust/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path spikes/rust/Cargo.toml
cargo build --manifest-path spikes/rust/Cargo.toml --release

bun spikes/shared/evaluate.mjs conformance zig spikes/zig/zig-out/bin/distill-spike-zig
bun spikes/shared/evaluate.mjs conformance rust spikes/rust/target/release/distill-spike-rust
bun spikes/shared/evaluate.mjs benchmark zig spikes/zig/zig-out/bin/distill-spike-zig
bun spikes/shared/evaluate.mjs benchmark rust spikes/rust/target/release/distill-spike-rust
bun spikes/shared/evaluate.mjs fuzz zig spikes/zig/zig-out/bin/distill-spike-zig --cpu-seconds 3600 --workers 8
bun spikes/shared/evaluate.mjs fuzz rust spikes/rust/target/release/distill-spike-rust --cpu-seconds 3600 --workers 8
```

The runner writes raw, machine-identified evidence beneath `spikes/evidence/`.
On Linux, the fuzz runner reads child user and system CPU ticks from `/proc` and
does not stop until their sum reaches 3,600 seconds. Wall time is recorded
separately.
