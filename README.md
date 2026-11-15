# piki-multiplex

A terminal multiplexer engine: PTY spawning, `vt100` terminal state, OSC
133/7 shell-integration tracking, and a headless persistent-session daemon
that survives the frontend that opened it — tabs reattach over a Unix socket
with their screen and scrollback restored.

Extracted from [agent-multi](https://github.com/juanknebel/piki-multi-ai).
This crate is domain-neutral: it has no built-in notion of Claude, Codex, or
any other agent (see the sidecar channel below).

- [`pty`](#pty) — spawn a PTY, feed a `vt100` parser, batch output for a
  render loop. Local (in-process) and remote (daemon-attached) backings share
  the same `PtySession` facade.
- [`session`](#session-persistent-daemon) — headless daemon that owns every
  PTY; thin clients attach over a Unix socket and get a restore buffer.
- [`shell_integration`](#shell_integration) — OSC 133/7 streaming parser plus
  zsh/bash/fish init scripts, turning a PTY byte stream into structured
  `ShellEvent`s.
- [`sidecar`](#sidecar-out-of-band-channel) — generic FIFO transport for a
  caller-defined out-of-band event stream (the sibling of in-band OSC 777).

Linux/macOS only (the daemon uses `fork`/`setsid`/Unix sockets/FIFOs).

## Install

```toml
[dependencies]
piki-multiplex = { git = "https://github.com/juanknebel/piki-multiplex", branch = "main" }
```

### vt100 dependency

Re-attach restore relies on two behaviors not in upstream `vt100`:
scrollback capture from a top-anchored scroll-region (needed for inline-TUI
transcripts), and `Screen::restore_formatted()` (serializing scrollback +
screen + alt-screen + cursor/attrs/input modes). Both live in a fork:
[juanknebel/piki-vt100](https://github.com/juanknebel/piki-vt100).

This crate's `Cargo.toml` patches `vt100` to that fork so it builds
standalone. **A consumer that embeds this crate in its own Cargo workspace
must apply the same `[patch.crates-io]` in its own root `Cargo.toml`** —
Cargo patches never propagate downstream, only from a build's actual root
manifest.

```toml
[patch.crates-io]
vt100 = { git = "https://github.com/juanknebel/piki-vt100", branch = "main" }
```

## Quickstart

Spawn a shell in a PTY, send input, read the emulated screen:

```rust
use piki_multiplex::pty::{PtyOutputSignal, PtySession, SpawnOptions};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let signal = PtyOutputSignal::new();
    let mut session = PtySession::spawn_with(
        SpawnOptions::new(std::env::current_dir()?.as_path(), "sh")
            .with_output_signal(signal.clone()),
    )
    .await?;

    session.write(b"echo hello\r")?;

    // Wait until the reader thread reports output, then poll the screen.
    tokio::time::timeout(std::time::Duration::from_secs(5), signal.notified()).await?;
    signal.take();
    let screen = session.parser().lock().screen().contents();
    println!("{screen}");

    session.kill()?;
    Ok(())
}
```

Run it from this repo:

```bash
cargo run --example local_pty
```

The full version (with a poll loop instead of a single read) lives in
`examples/local_pty.rs`.

## `pty`

`PtySession` is a `Local | Remote` facade with an identical public API either
way, so a frontend picks `Remote` when the daemon is reachable and falls back
to `Local` otherwise (`is_remote()` tells them apart for quit/close
semantics).

| Type | Role |
|---|---|
| `SpawnOptions` → `spawn_with` | Spawn a child in this process: `new(path, cmd)` + `with_*` setters (async ctor, sync handle afterwards). The legacy 11-arg `spawn` delegates to it. |
| `PtySession::from_attachment(...)` | Wrap a daemon `Attachment` as a remote-backed session. |
| `PtyOutputSignal` | Coalesced "some PTY produced output" wakeup shared by every session of a frontend: reader threads `raise()`, the event loop awaits `notified()` then `take()`s. |
| `ShellSession` | Per-session `{ state: ShellTabState, pending_events }`, mutated by the reader thread; consumers lock it and `drain_events()`. |

Key behaviors:

- **Non-blocking writes.** All writes to the master go through a per-session
  `pty-writer` thread; `PtySession::write(&self)` only enqueues. A direct
  blocking `write(2)` freezes once the kernel's PTY input queue (~4 KiB)
  fills, which would deadlock the reader thread mid-drain.
- **Batched reads.** The reader accumulates up to 64 KiB before locking the
  `vt100` parser, to avoid lock contention with the render thread.
- **Single answerer for terminal queries.** The vendored `vt100` queues
  DSR/DA1 replies in `Screen::take_answerback()`. Locally the reader relays
  them to the child; remotely the daemon answers only while nobody is
  attached, otherwise the attached client relays its own parser's reply as
  `Frame::Answerback` (honored only from the most recently attached client),
  so probing apps (delta, neovim, theme probes) see exactly one response.
- **Lifecycle.** `kill()` kills the child (`Local`) or asks the daemon to
  kill it (`Remote`, where the session is retained as exited). Dropping a
  `Remote` *detaches* — the session survives; dropping a `Local` kills.

## `session`: persistent daemon

The daemon owns every PTY plus one `vt100` parser per session (for restore
buffers and headless query replies). Clients attach over a Unix socket and
receive a `Restore` buffer (scrollback + screen + alt-screen + modes) that
rebuilds the terminal at the requested size.

### Embedding the daemon

The daemon never decides which binary it runs as. The embedding app re-execs
itself (a `serve` subcommand or `--serve-sessions` flag) and calls `run`
there:

```rust,no_run
use piki_multiplex::session::daemon::{DaemonPaths, run};

// Build from your app's own data-dir layout.
let paths = DaemonPaths {
    sessions_dir: data_dir.join("sessions"),
    log_dir: data_dir.join("logs"),
    lock_path: data_dir.join("sessions/daemon.lock"),
    pid_path: data_dir.join("sessions/daemon.pid"),
    socket_path: data_dir.join("sessions/daemon.sock"),
    log_path: data_dir.join("logs/session-daemon.log"),
};
// `sidecar` is supplied once here (it carries a Rust closure and can't cross
// the wire); `None` leaves the sidecar channel inert daemon-wide.
run(&paths, /* foreground: */ false, sidecar)?;
# Ok::<(), piki_multiplex::Error>(())
```

`run` double-forks (unless `foreground`), takes an exclusive `flock` so only
one daemon per data dir exists, cleans up a stale socket, restricts the
socket to mode `0600`, installs `SIGTERM`/`SIGINT` shutdown, and serves until
idle-exit or a `Shutdown` request. Long socket paths are fine: bind/connect
fall back to a short proxy when the path would overflow `sun_path`.

### Talking to the daemon

```rust,no_run
use piki_multiplex::session::client::{Daemon, ensure_daemon};

// Start on demand: `launch` re-execs your binary into daemon mode.
let daemon = ensure_daemon(&socket_path, || {
    std::process::Command::new(std::env::current_exe()?)
        .arg("--serve-sessions")
        .spawn()?;
    Ok(())
})?;

daemon.ping()?;
let sessions = daemon.list()?; // live and exited

// Attach to an existing session at the current terminal size...
let (sidecar_factory, signal) = (None, None); // or your SidecarFactory + PtyOutputSignal
let att = daemon.attach("session-id", 24, 80)?;
let session = piki_multiplex::pty::PtySession::from_attachment(
    att, /* integration_on: */ true, sidecar_factory, Some(signal),
);
// ...or spawn-and-attach in one round-trip.
let req = piki_multiplex::session::protocol::SpawnRequest {
    id: piki_multiplex::session::new_session_id(),
    command: "zsh".into(),
    cwd: std::env::current_dir()?,
    env: std::env::vars().collect(), // applied verbatim after env_clear()
    rows: 24,
    cols: 80,
    integration_on: true,
    attach: true,
    ..Default::default()
};
let att = daemon.spawn_attach(req)?;
# Ok::<(), piki_multiplex::session::client::ClientError>(())
```

Control connections are one request per connection (`ping`, `list`, `spawn`,
`kill`, `remove`, `set_meta`, `shutdown`); an attach connection becomes a
long-lived duplex stream (`Input`/`Answerback`/`Resize`/`Resync`/`Detach`/
`Kill` up, `Output`/`Restore`/`ShellState`/`ShellEvent`/`Exited`/
`Heartbeat` down). Drive it from a dedicated reader thread (see `RemotePty`).

Helpers: `new_session_id()` mints a process-unique id; `sessions_enabled(
config_path)` is a minimal `[sessions].enabled` TOML lookup (default `true`
on any failure) so frontends agree on whether persistence is on.

### Wire protocol

`session::protocol` defines the framing: `[kind u8][len u32 LE][payload]`,
capped at 16 MiB. `Input`/`Output`/`Restore`/`Answerback` carry raw terminal
bytes; everything else is JSON. Every JSON field is `#[serde(default)]` so
additive changes stay compatible — only a genuinely breaking change bumps
`PROTOCOL_VERSION` (currently v2, which added `Frame::Answerback`), and the
daemon advertises it in `Hello` for the client to check first.

Daemon-side retention: a killed session stays as *exited* for an hour so a
restarting frontend still shows its final screen; a daemon with no sessions
left exits on its own after the idle timeout (detached-but-live sessions pin
it indefinitely — they are the point of persistence).

## `shell_integration`

Init scripts (zsh, bash, fish, embedded via `include_str!`) make the user's
shell emit OSC 133 (prompt/command markers + exit code) and OSC 7 (cwd).
`install::setup_for` detects the shell family and returns the env vars +
extra args to merge into the spawn so the shell sources the bridge before the
user's real dotfiles (`ZDOTDIR` for zsh, `--rcfile` for bash, `-C 'source
...'` for fish); unsupported shells return `None` and spawn without
integration.

```rust,no_run
use piki_multiplex::shell_integration::install::setup_for;

let base = data_dir.join("shell_integration");
if let Some(setup) = setup_for("/bin/zsh", &base)? {
    let extra_env: Vec<(String, String)> = setup.env.into_iter().collect();
    // ... then SpawnOptions::new(dir, shell)
    // .with_extra_env(&extra_env)
    // .with_extra_args(&setup.extra_args)
    // .with_shell_integration(true)
}
# Ok::<(), piki_multiplex::Error>(())
```

`parser::OscParser` is a streaming, observe-only state machine over the PTY
bytes (split-safe across chunks, bounded buffers so a malformed sequence
can't wedge it). It emits `ShellEvent`s — `PromptStart`, `CommandInputStart`,
`CommandOutputStart`, `CommandEnd { exit_code, command }` (with the typed
command captured best-effort between markers), `CwdChanged`, `WindowTitle`
(OSC 0/1/2), and `Sidecar` (OSC 777, see below) — which `ShellTabState::apply`
folds into per-tab state (cwd, last command, attention marker, window title).
`snapshot()`/`from_snapshot()` round-trip that state across the daemon wire
(`Instant`s become relative durations).

## `sidecar`: out-of-band channel

A generic seam for a caller-defined structured event stream. The crate carries
no event vocabulary of its own — a consumer implements `SidecarState` and
supplies a `SidecarConfig` at spawn time (and once at daemon startup):

```rust,no_run
use std::sync::Arc;
use piki_multiplex::shell_integration::{SidecarConfig, SidecarState};

#[derive(Default)]
struct MyState { /* whatever your hooks report */ }
impl SidecarState for MyState {
    fn apply(&mut self, _event: serde_json::Value) { /* ... */ }
    fn snapshot(&self) -> serde_json::Value { serde_json::json!({}) }
    fn restore(&mut self, _snapshot: serde_json::Value) { /* ... */ }
    fn as_any(&self) -> &dyn std::any::Any { self }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any { self }
}

let config = SidecarConfig {
    osc_target: "myapp://agent".to_string(),
    factory: Arc::new(|| Box::new(MyState::default()) as Box<dyn SidecarState>),
};
```

Two transports feed the same entry point,
`ShellTabState::apply_sidecar_event`:

- **In-band OSC 777** (`\x1b]777;notify;<target>;<json>`), parsed by
  `OscParser`. Only the exact configured target is claimed; every other
  target (or no config at all) is left untouched like any unknown OSC.
- **Out-of-band FIFO** (`sidecar::sock::spawn_reader`, per-tab, mode `0600`,
  `O_RDWR | O_NONBLOCK | O_NOFOLLOW`): newline-delimited JSON for emitters
  detached from any controlling terminal that can't do an in-band OSC write.
  Used when the spawn passes both a `cli_agent_sock` path and a
  `SidecarConfig`.

Snapshots cross the daemon wire opaquely (`ShellStateSnapshot.sidecar` is a
`serde_json::Value` the daemon never interprets). `CommandEnd` clears live
sidecar state: "the shell returned to its prompt" reads as "whatever was
reporting through this shell has exited". For a real vocabulary built on this
seam (Claude Code / Antigravity hook payloads), see
[agent-multi](https://github.com/juanknebel/piki-multi-ai)'s `piki-core`
crate (`cli_agent/` module), kept out of this crate on purpose.

## Features

| Feature | Default | Contents |
|---|---|---|
| `daemon` | on | `session::daemon` (server engine + `run` launcher). |
| `sidecar` | on | `sidecar` FIFO transport. |

Disable with `default-features = false` for a PTY + parser only build:

```bash
cargo build --no-default-features
cargo build --no-default-features --features daemon
```

## Environment variables

| Variable | Default | Meaning |
|---|---|---|
| `PIKI_SESSION_LOG` | `info` | Daemon log level (`trace`, `debug`, `info`, `warn`, `error`, `off`). |
| `PIKI_SESSION_IDLE_SECS` | `60` | Seconds the daemon stays alive with no sessions left before exiting. |

## Development

```bash
cargo build
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
cargo doc --no-deps
```

Errors: public fallible APIs return `piki_multiplex::Error` (daemon control
calls return `session::client::ClientError`, which converts into `Error` via
`?`); internal code may use `anyhow::Result`. Wire-protocol changes keep new JSON fields
`#[serde(default)]` and bump `PROTOCOL_VERSION` only when breaking.
