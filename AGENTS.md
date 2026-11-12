# AGENTS.md — Instructions for AI agents

## Mandatory workflow

### Documentation updates

Update `README.md` and `CLAUDE.md` (and this file) in the same task as any code change.
Run before every commit:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

## What is this?

`piki-multiplex` is a terminal multiplexer engine (Rust): PTY spawning, `vt100`
terminal state, OSC 133/7/777 shell integration, and a headless persistent-session
daemon with Unix-socket reattach. Linux/macOS only. Independent of `piki-core`.

## Build & Test

```bash
cargo build
cargo build --no-default-features
cargo build --no-default-features --features daemon
cargo build --no-default-features --features sidecar
cargo test
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```

Features: `daemon` (default), `sidecar` (default).

## Environment

- `PIKI_SESSION_LOG` — daemon log level (default `info`).
- `PIKI_SESSION_IDLE_SECS` — idle-exit seconds (default `60`).

## Architecture

See `CLAUDE.md` for module layout and conventions. Key points:

- `PtySession = Local | Remote` facade with identical API.
- Daemon single-instance via lock; live detached sessions pin it.
- Wire protocol `PROTOCOL_VERSION = 2`; additive JSON fields `#[serde(default)]`.
- vt100 is patched to `juanknebel/piki-vt100`; consumers must apply the same
  `[patch.crates-io]` in their root manifest.
- Public fallible APIs return `crate::Error`; internal code may use `anyhow`.

## Quality gate

Zero warnings: `cargo clippy --all-targets --all-features -- -D warnings`.
