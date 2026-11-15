# Persistent sessions — daemon design

The `session` module's design record: why the daemon exists, the wire
protocol, attach/restore semantics, retention, backpressure, and failure
modes. Code references are to `src/session/` unless noted.

## Goal

Tabs survive the frontend that opened them. A headless daemon process owns
every PTY (plus one `vt100` parser per session); frontends are thin clients
that attach over a Unix socket and receive a restore buffer rebuilding
screen + scrollback. One daemon per data directory; detached-but-live
sessions pin it indefinitely.

## Process lifecycle (`daemon/launch.rs`)

The daemon never decides which binary it runs as: the embedding app re-execs
itself (a `serve` subcommand or `--serve-sessions` flag), builds
`DaemonPaths` from its own layout, and calls `run(paths, foreground,
sidecar)` there. `sidecar` is supplied once at startup — it carries a Rust
closure and can't cross the wire.

Startup order:

1. Create `sessions_dir` / `log_dir`. Unless `foreground`, double-fork +
   `setsid`, redirect stdio to `/dev/null`.
2. Init logging (file when daemonized, stderr when foreground;
   `PIKI_SESSION_LOG`, default `info`; `try_init` so tests aren't clobbered).
3. Take an exclusive `flock` on `lock_path`. A second daemon for the same
   data dir sees `Busy` and exits quietly. Write the pid file.
4. Probe the socket: if a file exists but nothing listens, remove it (crashed
   predecessor); if a live daemon answers, leave it (our bind then rightly
   fails). Bind (see `uds.rs` for over-long paths), then `chmod 0600` the
   socket and `0700` its parent.
5. Serve until idle-exit, a `Shutdown` request, or `SIGTERM`/`SIGINT` (the
   handler only flips the shutdown atomic — async-signal-safe).
6. On exit: `kill_all` sessions, remove socket + pid file.

## Threading model

- Accept loop (non-blocking accept + 50 ms sleep) + `session-reaper` thread.
- Per connection: one `session-conn` thread; per attach, plus a socket-writer
  thread draining that client's outbound queue (heartbeat every 5 s idle).
- Per session: one `session-reader` thread (master → parser + OSC → fan-out),
  one `session-writer` thread draining the input queue, and optionally one
  `sidecar-sock` FIFO reader.

## Wire format (`protocol.rs`)

Every message is one frame: `[kind u8][len u32 LE][payload]`, payload capped
at `MAX_FRAME_LEN` (16 MiB). `Input`, `Output`, `Restore`, `Answerback` carry
raw terminal bytes; every other payload is JSON. All JSON fields are
`#[serde(default)]` so additive changes stay compatible; only a genuinely
breaking change bumps `PROTOCOL_VERSION` (v2 added `Frame::Answerback`),
which the daemon advertises in `Hello` for the client to check first.

Connection lifecycle:

1. Daemon → client: `Hello { protocol, version, pid }`.
2. Client → daemon: exactly one `Request`; daemon → client: one `Reply`.
3. `Attach` / `Spawn { attach: true }` turn the connection into a duplex
   attach stream (frames below) until detach/EOF.

Attach-stream frames (`↑` client→daemon, `↓` daemon→client):

| Frame | Dir | Meaning |
|---|---|---|
| `Input` | ↑ | Keystrokes for the PTY. Always forwarded. |
| `Answerback` | ↑ | Terminal-query relay; gated on the answerer. |
| `Resize` | ↑ | Resize PTY + restore parser. |
| `Resync` | ↑ | Re-send `Restore` at the current size. |
| `Detach` | ↑ | Leave the session running. |
| `Kill` | ↑ | Kill this session's child. |
| `Output` | ↓ | Live PTY output (≤ 64 KiB per chunk). |
| `Restore` | ↓ | Full rebuild buffer (see below). |
| `ShellState` | ↓ | Reserved: full snapshot wholesale (rides in `Attached` today; clients handle both). |
| `ShellEvent` | ↓ | One shell-integration event. |
| `Exited` | ↓ | The child exited (code may be unknown). |
| `Heartbeat` | ↓ | Liveness probe when the queue is quiet. |
| `Detached` | ↓ | Reserved: closing notice (conveyed by EOF today; clients handle both). |

Control connections carry a 10 s read timeout client-side; attach
connections clear it (quiet periods are normal).

## Requests

| Request | Reply | Notes |
|---|---|---|
| `Ping` | `Pong` | Liveness + protocol check. |
| `List` | `Sessions` | Live and exited sessions. |
| `Spawn(req)` | `Spawned` / `Error` | Without `attach`, control-only. |
| `Attach` | `Attached` / `Error` | Carries `info` + shell snapshot. |
| `Kill` | `Ok` / `Error` | SIGHUP, SIGKILL after 500 ms; retained as exited. |
| `Remove` | `Ok` / `Error` | Kills first if still live; drops everything. |
| `SetMeta` | `Ok` / `Error` | Partial metadata update (`set_title` clears). |
| `Shutdown` | `Ok` / `Error` | Refused while live sessions exist unless `force`. |

`SpawnRequest` carries everything: client-chosen id, command/args/cwd, the
**complete** env (the daemon `env_clear()`s, so spawns behave identically no
matter who started the daemon), size (default 24×80), `integration_on`,
optional `cli_agent_sock` FIFO path, tab `meta`, and `attach`.

Duplicate ids are rejected (`session 'x' already exists`) *before*
spawning: replacing the map entry would orphan the previous session, and
dropping its last `Arc` kills a live detached session's child. The sessions
lock is held across check + spawn + insert so two concurrent spawns can't
both pass (spawns are rare and fast).

Registration happens *before* the reply is sent, so the connection is an
attach stream with no gap: no live byte between reply and registration is
lost, and `info.attached` already counts the newcomer.

## Sessions (`daemon/session.rs`)

One reader thread per session reads the master (64 KiB buffer), feeds the
`vt100` parser (scrollback 5000, kept purely for restore + headless query
replies) and the `OscParser`, then — under one lock so no client sees a gap
or a duplicate — appends the bytes plus any shell events to every attached
client's outbound queue.

- `attach(rows, cols)` resizes the PTY and the parser to the newcomer's
  size, captures the restore buffer, and enqueues `Restore` (+ `Exited` if
  already dead) *before* registering — all under the output lock. (With
  several clients at different sizes, the last attacher wins.)
- `detach(id)` deregisters; the answerer role hands over (below).
- `kill()` is SIGHUP via the child's killer, a 500 ms grace poll, then
  SIGKILL; a no-op once exited. The exit code is captured when the reader
  sees EOF and broadcast as `Exited`.
- Dropping the last `Arc<Session>` kills the child; the reader exits when
  the master closes and the FIFO reader unlinks its socket.

## Restore semantics (`restore.rs`)

`Screen::restore_formatted()` serializes scrollback rows as plain lines, the
primary screen, the alternate screen if the app is in it, then
cursor/attribute/input modes. The client replays it into a *fresh* parser of
the attach size, so both the visible screen and the scrollback (mouse wheel,
search) match. `Resync` re-sends the buffer at the current size. Exited
sessions stay attachable: newcomers get the final screen plus `Exited`.

## Terminal-query single answerer

The daemon-side parser drains `take_answerback()` after every pass but
forwards the reply to the PTY only while *nobody* is attached — so an agent
probing the terminal at startup gets an answer when headless. With a live
client, the client's real terminal answers instead (including queries the
daemon parser doesn't know, like OSC 10/11): each client relays its own
parser's reply as `Answerback`, and the daemon forwards only the most
recently attached client's. Answering in both places would double-answer DA1
and break query fences (delta's OSC 10 + OSC 11 + DA1 probe is the canonical
victim). When the answerer detaches or is dropped, the most recently
attached survivor takes over; with nobody left, the daemon answers again.

## Shell / sidecar state sync

For `integration_on` sessions the reader applies OSC events to a shared
`ShellTabState` and broadcasts them (`replayed: false` — the daemon never
buffers while detached; the attach snapshot covers the gap). `Attached`
carries a `ShellStateSnapshot` (cwd, last command, attention flag,
in-flight age, window title); the client rebuilds via `from_snapshot`, and
`RemotePty` then applies live events exactly like a local tab. The
`sidecar` snapshot is opaque JSON the daemon never interprets. `CommandEnd`
clears live sidecar state ("back at the prompt" ⇒ "the reporter exited").

## Retention and reaping

- Exited sessions are kept one hour (`EXITED_TTL`) so a restarting frontend
  still shows their final screen and code; the reaper drops them (sweep
  every 30 s, shutdown polled every 200 ms).
- Idle means the map is *empty*: a live detached session pins the daemon
  indefinitely (persistence is the point), as does a retained exited one
  until reaped. After `IDLE_EXIT` (60 s, overridable via
  `PIKI_SESSION_IDLE_SECS`) with nothing left, the daemon stops itself.
- Any population change (spawn/attach/detach/remove) resets the idle timer.
- Clean shutdown kills every child (`kill_all`) — persistence covers the
  *frontend* dying, not the daemon.

## Backpressure and quotas

- Output fan-out: each client owns a 512-frame (`CLIENT_QUEUE_DEPTH`)
  bounded queue. A client that falls behind is dropped — warned, socket shut
  down — rather than stalling the reader every other client shares.
- Input: the per-session writer thread's queue is byte-capped
  (`WRITER_QUEUE_BYTES`, 16 MiB). Past it, writes fail with `QuotaExceeded`
  and `run_attach` detaches the sender; other write errors (child gone) are
  ignored so the client keeps its final screen.
- `MAX_FRAME_LEN` (16 MiB) rejects corrupt/giant frames on both ends.

## Failure modes

- **Protocol mismatch.** The daemon always says `Hello`; the *client*
  decides — `Incompatible` surfaces to the caller, which falls back to an
  in-process PTY or asks for a daemon restart.
- **Daemon not running.** `ensure_daemon` launches one and pings with
  backoff (~5 s) before giving up.
- **Stale socket / second daemon.** Startup probes and removes a dead
  socket; a live rival fails the bind, and the `flock` makes a second daemon
  exit quietly without touching anything.
- **Client crash mid-attach.** EOF ends the attach loop; the session keeps
  running detached and the answerer role hands over.
- **Daemon crash.** The session map is in-memory only, so it is lost;
  children are reparented and keep running orphaned (only a *clean* exit
  kills them). The socket file goes stale and is cleaned on next start; FIFO
  nodes are unlinked only on clean drop, so a crash may leak them. A fresh
  daemon starts on demand with no sessions.
- **Slow/dead consumers.** Covered above: lagging output clients are
  dropped, flooding input clients detached, dead sockets noticed within a
  heartbeat interval.
- **Unknown ids / refused ops.** `Kill`/`Remove`/`Attach` on a missing id,
  duplicate `Spawn` ids, and non-forced `Shutdown` with live sessions all
  return `Reply::Error`; nothing is half-applied.

## Security model

Local socket, same user only: every connection's peer credential
(`SO_PEERCRED` on Linux, `getpeereid` on macOS) must equal the daemon's uid
or the connection is dropped before `Hello`. Socket `0600`, socket dir
`0700`, sidecar FIFOs `0600` + `O_NOFOLLOW`. Within that trust domain the
client is fully privileged — it chooses the command and the entire
environment — by design: the daemon is a persistence mechanism, not a
sandbox.

## Configuration and files

`DaemonPaths`, supplied by the embedding app:

| Path | Purpose |
|---|---|
| `sessions_dir` | Reserved for session state. |
| `log_dir` | Holds `log_path`. |
| `lock_path` | Single-instance `flock`. |
| `pid_path` | Daemon pid (removed on exit). |
| `socket_path` | Unix socket (removed on exit). |
| `log_path` | Daemon log when backgrounded. |

| Variable | Default | Meaning |
|---|---|---|
| `PIKI_SESSION_LOG` | `info` | Log level (`trace`…`off`). |
| `PIKI_SESSION_IDLE_SECS` | `60` | Idle-exit timeout, positive ints only. |

Over-long socket paths are addressed through a short proxy (`uds.rs`) so
the socket always lives at its configured path.
