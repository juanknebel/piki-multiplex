# piki-multiplex

A terminal multiplexer engine: PTY spawning, `vt100` terminal state, OSC 133/7
shell-integration tracking, and a headless persistent-session daemon that
survives the frontend that opened it — tabs reattach over a Unix socket with
their screen and scrollback restored.

Extracted from [agent-multi](https://github.com/juanknebel/piki-multi-ai),
where the design doc for the persistent-session daemon (wire protocol,
restore semantics, failure modes) still lives at `docs/persistent-sessions.md`
— read that for the full daemon design before touching `session/`.

## What's here

- **`pty`** — `PtySession` wraps [`portable-pty`](https://docs.rs/portable-pty)
  (sync) with `spawn_blocking` for non-blocking reads. A `vt100::Parser`
  accumulates terminal state; the reader optionally feeds bytes through
  `shell_integration::parser::OscParser`. `PtySession` is a `Local | Remote`
  facade — `RemotePty` backs it when attached to the session daemon over a
  Unix socket — with an identical public API either way. All writes to the
  PTY master go through a per-session writer thread; `PtySession::write` only
  enqueues, since a direct blocking `write(2)` can freeze the caller once the
  kernel's PTY input queue fills.
- **`session`** — the persistent-session daemon. `protocol.rs` is the
  daemon⇄client wire format (length-prefixed frames, JSON control messages,
  raw bytes for input/output/restore; every JSON field is `#[serde(default)]`
  so additive changes stay compatible). `restore.rs` wraps the vendored
  `vt100`'s `Screen::restore_formatted()` to round-trip scrollback + screen +
  alt-screen + modes across a re-attach. `daemon::launch::run` takes a
  `DaemonPaths { sessions_dir, log_dir, lock_path, pid_path, socket_path,
  log_path }` supplied by the embedding app — the daemon never decides which
  binary it runs as; the caller re-execs itself into daemon mode and passes
  the paths.
- **`shell_integration`** — OSC 133 (prompt/command markers) and OSC 7 (cwd)
  streaming parser, plus install scripts (zsh/bash/fish, Linux/macOS only)
  that make a shell source the integration bridge before its own dotfiles.
  Also defines the generic out-of-band seam: [`SidecarState`] (a trait a
  caller implements to interpret its own hook/agent event vocabulary — this
  crate carries none of its own) and [`SidecarConfig`] (which OSC 777 target
  string to claim, and a factory that constructs a fresh `SidecarState`).
- **`sidecar`** — the transport for that seam: `sock::spawn_reader` reads a
  per-tab FIFO of newline-delimited JSON and feeds it to
  `ShellTabState::apply_sidecar_event`, the out-of-band sibling of the
  in-band OSC 777 path in `shell_integration::parser`. Carries no event
  vocabulary itself — it only moves JSON. Both the FIFO and OSC 777 paths are
  inert no-ops unless a caller supplies a `SidecarConfig` at spawn time (and,
  for the FIFO, a `cli_agent_sock` path).

This crate is domain-neutral: it has no built-in notion of Claude, Codex, or
any other agent. A consumer that wants a structured out-of-band agent-status
channel implements `SidecarState` itself — see
[agent-multi](https://github.com/juanknebel/piki-multi-ai)'s `piki-core`
crate (`cli_agent/` module) for a real example (Claude Code / Antigravity
hook payloads, decoded from the same generic JSON this crate hands it).

## vt100 dependency

The persistent-session daemon's re-attach restore relies on two behaviors not
in upstream `vt100`: scrollback capture from a top-anchored scroll-region
(needed for inline-TUI transcripts like Codex), and `Screen::restore_formatted()`
(serializing scrollback + screen + alt-screen + cursor/attrs/input modes so a
re-attaching client can rebuild the terminal). Both live in a fork:
[juanknebel/piki-vt100](https://github.com/juanknebel/piki-vt100).

This crate's `Cargo.toml` patches `vt100` to that fork so it builds standalone.
**A consumer that embeds this crate in its own Cargo workspace must apply the
same `[patch.crates-io]` in its own root `Cargo.toml`** — Cargo patches never
propagate downstream through a dependency graph, only from a build's actual
root manifest.

```toml
[patch.crates-io]
vt100 = { git = "https://github.com/juanknebel/piki-vt100", branch = "main" }
```

## Using it

```toml
[dependencies]
piki-multiplex = { git = "https://github.com/juanknebel/piki-multiplex", branch = "main" }
```

Linux/macOS only (the daemon uses `fork`/`setsid`/Unix sockets/FIFOs).
