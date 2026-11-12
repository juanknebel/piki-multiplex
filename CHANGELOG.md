# Changelog

All notable changes to this project are documented here.
Format loosely follows Keep a Changelog.

## [Unreleased]

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
