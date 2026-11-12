# piki-multiplex

A terminal multiplexer engine, independent of the rest of this workspace —
**must NOT depend on `piki-core`, `piki-tui`, `piki-desktop`, `piki-agent`, or
`piki-api-client`.** `piki-core` depends on this crate and re-exports its
`pty`/`session`/`shell_integration` modules unchanged (see
`crates/core/CLAUDE.md`), so downstream `piki_core::pty::*` /
`piki_core::session::*` call sites in the TUI and desktop never reference this
crate by name. This crate is domain-neutral — no built-in notion of Claude,
Codex, or any other agent; see `shell_integration::SidecarState` below.

## Modules

- `pty/` — `PtySession` wraps `portable-pty` (sync) with `spawn_blocking` for
  non-blocking reads. `vt100::Parser` accumulates terminal state. The reader
  optionally feeds bytes through `shell_integration::parser::OscParser` and
  updates a per-session `ShellSession`. The vendored vt100 (workspace-root
  `[patch.crates-io]`, `vendor/vt100`) queues terminal-query answerbacks (DSR,
  DA1) in `Screen::take_answerback()`; the reader drains that after each parse
  pass. Single-answerer policy: the daemon answers queries only while nobody
  is attached; an attached client relays its own parser's reply as
  `Frame::Answerback`, which the daemon forwards only from the most recently
  attached client — so query fences (delta, neovim, theme probes) see exactly
  one reply. **All writes to the master go through a per-session `pty-writer`
  thread** (`PtySession::write` only enqueues) — a direct `write(2)` blocks
  once the kernel's PTY input queue fills, which would freeze the caller.
  `PtySession` is a `Local | Remote` facade (`RemotePty` backs it when
  attached to the session daemon over a Unix socket); the public API is
  identical either way. `spawn`/`from_attachment` take an optional
  `SidecarConfig` alongside the existing `cli_agent_sock` FIFO path.
- `session/` — the persistent-session daemon (design: `docs/persistent-sessions.md`
  in the workspace root). `protocol.rs` is the daemon⇄client wire format:
  `[kind u8][len u32 LE][payload]` frames, JSON for control messages, raw
  bytes for `Input`/`Output`/`Restore`/`Answerback`; every JSON field is
  `#[serde(default)]` so additive changes stay compatible — bump
  `PROTOCOL_VERSION` for anything else (v2 added `Frame::Answerback`: the
  client→daemon query-relay, gated on the most recently attached client).
  `ShellStateSnapshot.sidecar: Option<serde_json::Value>` is forwarded
  opaquely — the daemon never interprets it, only whatever the caller's
  `SidecarState::snapshot()` produced. `restore.rs` wraps the vendored vt100's
  `Screen::restore_formatted()` (scrollback + screen + alt screen + modes) and
  owns its round-trip tests — keep them green when touching `vendor/vt100`.
  `daemon/launch.rs::run` takes a `DaemonPaths { sessions_dir, log_dir,
  lock_path, pid_path, socket_path, log_path }` — the embedding app builds
  this from its own paths type (`piki-core`'s `DataPaths::daemon_paths()`) —
  and an `Option<SidecarConfig>`, supplied once at daemon-process startup
  (not per spawn request: a `SidecarConfig` carries a Rust closure and can't
  cross the wire). The daemon itself never decides which binary it runs
  as — the caller re-execs itself with a `serve` subcommand /
  `--serve-sessions` flag and passes the resulting `DaemonPaths`.
  `sessions_enabled(config_path)` is a minimal `[sessions].enabled` TOML
  lookup (default `true` on any failure) — a generic file-layer helper, not
  tied to any app-specific settings merge.
- `shell_integration/` — init scripts (zsh, bash, fish) embedded via
  `include_str!`, `OscParser` (streaming OSC 133/7 state machine, plus the
  in-band half of the sidecar channel), an `install` module that detects the
  shell family and prepares env vars + extra args (`ZDOTDIR` / `--rcfile`) so
  the user's shell sources the bridge before its real dotfiles. Linux/macOS
  only. Also defines:
  - `SidecarState` — a trait a consumer implements to interpret its own
    out-of-band agent/hook event vocabulary. `apply(Value)` mutates from one
    decoded JSON event; `snapshot()`/`restore(Value)` round-trip it across
    the daemon wire protocol; `as_any()`/`as_any_mut()` let the consumer
    downcast its own concrete type back out of `ShellTabState.sidecar:
    Option<Box<dyn SidecarState>>`.
  - `SidecarConfig { osc_target, factory }` — which OSC 777 target string to
    claim (`parser.rs::parse_osc_777` only claims an exact match; every other
    target, including no target configured at all, is left untouched exactly
    like an unknown OSC) and a `SidecarFactory` (`Arc<dyn Fn() -> Box<dyn
    SidecarState>>`) that constructs a fresh state the first time an event
    arrives.
  - `ShellTabState::apply_sidecar_event(Value)` — the single entry point both
    the OSC 777 path (`ShellEvent::Sidecar`) and the FIFO transport
    (`sidecar::sock`) funnel through; also callable directly by a consumer
    that wants to synthesize an event from something that isn't either
    transport (e.g. passive status detection with no hook at all).
  - `CommandEnd` clears any live sidecar state — "the shell returned to its
    prompt" is read as "whatever was reporting through this shell has
    exited," regardless of what the sidecar actually represents.
- `sidecar/` — `sock.rs`: FIFO transport (per-tab, mode `0600`, `O_RDWR |
  O_NONBLOCK`) for the out-of-band half of the sidecar channel. Reads
  newline-delimited JSON and calls `ShellTabState::apply_sidecar_event` —
  carries no event vocabulary of its own, only moves bytes → JSON →
  `SidecarState::apply`. This exists because a detached hook subprocess (no
  controlling terminal) can't reach an in-band OSC 777 write; the FIFO is the
  fallback transport when a consumer supplies a `cli_agent_sock` path.

A consumer that wants a structured out-of-band agent-status channel
implements `SidecarState` and supplies a `SidecarConfig` at spawn/daemon-start
time — see [agent-multi](https://github.com/juanknebel/piki-multi-ai)'s
`piki-core` crate (`cli_agent/` module: `CliAgentEvent`/`CliAgentState`/
`CliAgentStatus`, hook-script installation) for a real example, kept entirely
outside this crate since it needs `DataPaths`/`ProviderManager`, which this
crate must not depend on.

## Conventions

- Error handling: public fallible APIs return [`crate::Error`](src/error.rs); internal code may use `anyhow::Result`.
- Thread safety: `parking_lot::Mutex` for shared state, no `unwrap()` on
  locks held across an `await`.
- Wire-protocol changes: keep new JSON fields `#[serde(default)]`; bump
  `session::protocol::PROTOCOL_VERSION` only for a genuinely breaking change.
