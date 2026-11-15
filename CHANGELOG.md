# Changelog

All notable changes to this project are documented here.
Format loosely follows Keep a Changelog.

## [Unreleased]
### Added
- `pty::SpawnOptions` builder with `PtySession::spawn_with` /
  `LocalPty::spawn_with` as the preferred spawn API; the 11-argument `spawn`
  is kept and delegates to it.
- `Error::Client` (`From<session::client::ClientError>`, transparent): `?`
  converts daemon-control errors into `crate::Error`; `ClientError::Io`
  reports its `source()`.

### Changed
- `Frame::Output` carries `bytes::Bytes`: fan-out to N attached clients
  shares one allocation instead of cloning the chunk per client, and
  encoding borrows byte payloads instead of copying them (wire format
  unchanged).

### Fixed
- Daemon PTY writes no longer block the caller: `Session` drains input and
  answerbacks through a per-session writer thread (like `LocalPty`), so a
  child that stops reading stdin can't stall attach threads or wedge the
  reader on a query reply.
- Daemon `Server` state moved from `std::sync::Mutex` to
  `parking_lot::Mutex`: a panicked worker can no longer poison a lock and
  crash the daemon on the next access.
- Daemon `Session` writer queue is byte-capped (`WRITER_QUEUE_BYTES`): input
  floods are rejected and the sending client detached instead of growing
  daemon memory without bound.
- Daemon `Spawn` rejects duplicate session ids instead of replacing the map
  entry (which orphaned the previous session — dropping its last `Arc`
  killed a live detached session's child).

## [0.2.0] - 2026-11-11
### Added
- Optional `daemon` and `sidecar` features.
- Typed `crate::Error` for public fallible APIs.
- `PIKI_SESSION_IDLE_SECS` to configure daemon idle-exit.
- GitHub Actions CI and release workflow.
- Release helper script and changelog.

### Changed
- `PtySession::write` now takes `&self`.
- `ShellTabState::sidecar_or_insert` returns `&mut dyn SidecarState`.
- `AGENTS.md` is now the principal documentation; `CLAUDE.md` Claude-only.

### Security
- Sidecar FIFO opened with `O_NOFOLLOW`.


## [0.1.0] - 2026-11-11
### Added
- Initial standalone extraction of the terminal-multiplexer engine.
