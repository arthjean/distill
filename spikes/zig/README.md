# Zig spike

Pinned toolchain: Zig 0.16.0, official archive SHA-256
`70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00`.
The executable links the system SQLite library and performs no runtime network
access.

```bash
curl -LO https://ziglang.org/download/0.16.0/zig-x86_64-linux-0.16.0.tar.xz
echo "70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00  zig-x86_64-linux-0.16.0.tar.xz" | sha256sum --check
tar -xf zig-x86_64-linux-0.16.0.tar.xz

cd spikes/zig
zig fmt --check build.zig src
zig build test
zig build -Doptimize=ReleaseFast
```
