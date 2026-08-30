//! `piki-multiplex`: a terminal multiplexer engine.
//!
//! - [`pty`] — spawn a PTY, feed a `vt100::Parser`, batch output for a render
//!   loop. Local (in-process) and remote (daemon-attached) variants share the
//!   same [`pty::PtySession`] facade.
//! - [`session`] — a headless daemon that owns every PTY so tabs survive the
//!   frontend that opened them; thin clients attach over a Unix socket and
//!   receive a restore buffer that rebuilds the screen + scrollback. See
//!   `docs/persistent-sessions.md` in the workspace root for the design.
//! - [`shell_integration`] — OSC 133 (prompt/command markers) and OSC 7 (cwd)
//!   parsing, turning a PTY byte stream into structured [`shell_integration::ShellEvent`]s.
//!   Also defines [`shell_integration::SidecarState`], the generic out-of-band
//!   event seam a caller implements to interpret its own hook integration
//!   (Claude, Codex, or anything else) — this crate carries no such
//!   vocabulary itself.
//! - [`sidecar`] — the generic transport for that seam: a FIFO reader that
//!   decodes JSON and feeds it to a spawn's [`shell_integration::SidecarState`],
//!   the out-of-band sibling of the in-band OSC 777 path in
//!   [`shell_integration::parser`]. Inert unless a caller opts in by
//!   supplying a [`shell_integration::SidecarConfig`] at spawn time.

pub mod pty;
pub mod session;
pub mod shell_integration;
pub mod sidecar;
