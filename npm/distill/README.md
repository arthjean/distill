# @arthjean/distill

Bounded, recoverable context projection for coding agents.

## Install

```bash
npm install --global @arthjean/distill
distill --help
```

The package embeds the native Distill executable and performs no download or
build during installation. It supports Linux x86_64 with GNU libc and macOS
arm64. Other platforms are rejected during npm installation or by the launcher.

The executable performs no runtime network requests. It captures source bytes
before returning an omitting projection, stores them with restricted local
permissions, and emits versioned receipts for projected output.

See the [Distill repository](https://github.com/arthjean/distill) for setup,
security, platform dependencies, and the complete product contract.
