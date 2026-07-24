# Rust spike

Pinned toolchain: Rust 1.97.1. The crate links the system SQLite library through
`rusqlite` and performs no runtime network access.

```bash
rustup toolchain install 1.97.1

cargo fmt --manifest-path spikes/rust/Cargo.toml --check
cargo clippy --manifest-path spikes/rust/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path spikes/rust/Cargo.toml
cargo build --manifest-path spikes/rust/Cargo.toml --release
```
