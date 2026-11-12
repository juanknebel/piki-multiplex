//! Out-of-band per-tab transport for a caller-defined [`SidecarState`](crate::shell_integration::SidecarState) event
//! stream: a FIFO reader ([`sock`]) that decodes newline-delimited JSON and
//! applies it to a [`ShellTabState`](crate::shell_integration::ShellTabState),
//! the same way [`OscParser`](crate::shell_integration::parser::OscParser)
//! does for in-band OSC 777 sequences.
//!
//! This module carries no event vocabulary of its own — it only moves JSON
//! from a FIFO into [`ShellTabState::apply_sidecar_event`](crate::shell_integration::ShellTabState::apply_sidecar_event). The shape of that
//! JSON, and what it means, is entirely up to whatever
//! [`SidecarState`](crate::shell_integration::SidecarState) a caller supplied
//! at spawn time via a [`SidecarConfig`](crate::shell_integration::SidecarConfig).

#[cfg(unix)]
pub mod sock;
