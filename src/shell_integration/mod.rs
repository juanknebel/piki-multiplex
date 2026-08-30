//! Shell integration: inject init scripts that emit OSC 133 (prompt/command
//! markers + exit code) and OSC 7 (cwd reporting) into the user's shell, and
//! parse those sequences out of the PTY stream as structured events.
//!
//! Architecture:
//! - [`install`] resolves the user's shell, lays out a temporary init dir and
//!   returns env vars + extra args to feed `CommandBuilder` so the shell sources
//!   our integration script *and* the user's own dotfiles.
//! - [`parser::OscParser`] is a streaming state-machine that observes PTY
//!   bytes and emits [`ShellEvent`]s without modifying the stream.
//! - [`ShellTabState`] is the per-tab record kept in sync from those events
//!   (cwd, last command exit code, attention timestamp, and an optional
//!   caller-supplied [`SidecarState`]).
//!
//! Only `AIProvider::Shell` tabs use this — provider tabs (Claude/etc.) run
//! their binary directly without a shell wrapper, so OSC 133 doesn't apply.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::session::protocol::{CommandSnapshot, ShellStateSnapshot};

pub mod install;
pub mod parser;

/// An opaque, incrementally-updated out-of-band state a caller can attach to
/// a shell tab. This crate knows nothing about its shape — it only decodes
/// JSON off the wire (OSC 777 / a FIFO — see [`crate::sidecar`]), hands it to
/// [`apply`](Self::apply), and forwards [`snapshot`](Self::snapshot) opaquely
/// over the session daemon's wire protocol. A consumer (e.g. `piki-core`)
/// implements this to interpret whatever event vocabulary its own hook
/// integration speaks; a plain terminal-multiplexer use case never
/// implements it at all.
pub trait SidecarState: Send {
    /// Apply one decoded JSON event to this state.
    fn apply(&mut self, event: serde_json::Value);
    /// Serializable snapshot for the daemon wire protocol / restore.
    fn snapshot(&self) -> serde_json::Value;
    /// Rebuild state from a previously-produced [`snapshot`](Self::snapshot).
    fn restore(&mut self, snapshot: serde_json::Value);
    /// Drop any pending-attention marker (e.g. the user focused this tab).
    fn acknowledge(&mut self) {}
    fn as_any(&self) -> &dyn std::any::Any;
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

/// Constructs a fresh, empty [`SidecarState`] — called the first time a
/// sidecar event arrives for a tab that doesn't have one yet.
pub type SidecarFactory = std::sync::Arc<dyn Fn() -> Box<dyn SidecarState> + Send + Sync>;

/// Everything a caller needs to opt a spawn into the sidecar channel: which
/// OSC 777 target string claims events, and how to interpret their JSON
/// bodies. `None` (the default for a plain multiplexer use case) means OSC
/// 777 and the FIFO transport are both inert no-ops.
#[derive(Clone)]
pub struct SidecarConfig {
    /// OSC 777 target this spawn's hooks claim (`\x1b]777;notify;<target>;<json>`).
    /// Sequences with any other target are left untouched for the downstream
    /// terminal emulator, exactly like any other unknown OSC.
    pub osc_target: String,
    pub factory: SidecarFactory,
}

/// Structured event extracted from the PTY stream by [`parser::OscParser`].
///
/// Serializable because the session daemon forwards these to attached
/// clients over the wire (`session::protocol::Frame::ShellEvent`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ShellEvent {
    /// `\x1b]133;A\x07` — prompt is about to be drawn.
    PromptStart,
    /// `\x1b]133;B\x07` — prompt is drawn, user input begins.
    CommandInputStart,
    /// `\x1b]133;C\x07` — command output begins.
    CommandOutputStart,
    /// `\x1b]133;D[;<exit_code>]\x07` — command finished. `command` is the
    /// ANSI-stripped text the user typed (captured between `B` and `C`),
    /// best-effort: terminal redraws and multi-line edits can degrade the
    /// fidelity. `None` when capture was disabled or yielded nothing.
    CommandEnd {
        exit_code: Option<i32>,
        command: Option<String>,
    },
    /// `\x1b]7;file://<host>/<path>\x07` — cwd changed.
    CwdChanged(PathBuf),
    /// `\x1b]777;notify;<target>;<json>\x07` — a decoded JSON payload whose
    /// target matched a spawn's configured [`SidecarConfig::osc_target`], or
    /// a payload delivered out-of-band via the FIFO transport
    /// (`crate::sidecar`). Uninterpreted — the caller-supplied
    /// [`SidecarState`] gives it meaning.
    Sidecar(serde_json::Value),
    /// `\x1b]0;`/`\x1b]1;`/`\x1b]2;<text>\x07` — window/icon title update.
    /// Used passively by a caller that wants to read a provider's own
    /// spinner/title convention (e.g. Codex) when there's no hook bridge.
    WindowTitle(String),
}

/// One executed command, captured between [`ShellEvent::CommandOutputStart`]
/// and [`ShellEvent::CommandEnd`].
#[derive(Debug, Clone)]
pub struct CommandRecord {
    pub started_at: Instant,
    pub duration: Duration,
    pub exit_code: Option<i32>,
}

impl CommandRecord {
    pub fn ok(&self) -> bool {
        matches!(self.exit_code, Some(0))
    }
}

/// Per-tab state derived from [`ShellEvent`] stream. UIs render badges, status
/// bars, and "needs attention" markers from this.
#[derive(Default)]
pub struct ShellTabState {
    pub cwd: Option<PathBuf>,
    pub last_command: Option<CommandRecord>,
    /// Set when a command finishes; UIs treat this as "user should look at
    /// this tab". Cleared when the user focuses the tab/workspace.
    pub last_attention_at: Option<Instant>,
    /// Wall-clock start of the in-flight command, if any. Set on
    /// `CommandOutputStart`, consumed on `CommandEnd`.
    in_flight_started_at: Option<Instant>,
    /// Caller-defined out-of-band agent state, populated from
    /// [`ShellEvent::Sidecar`] events. `None` until both a
    /// [`SidecarConfig`] was supplied at spawn time *and* the first event
    /// arrives (shell-only tabs with no sidecar configured never set it).
    pub sidecar: Option<Box<dyn SidecarState>>,
    sidecar_factory: Option<SidecarFactory>,
    /// Latest window/icon title reported via OSC 0/1/2, if any.
    pub window_title: Option<String>,
}

impl std::fmt::Debug for ShellTabState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellTabState")
            .field("cwd", &self.cwd)
            .field("last_command", &self.last_command)
            .field("last_attention_at", &self.last_attention_at)
            .field("in_flight_started_at", &self.in_flight_started_at)
            .field("sidecar", &self.sidecar.is_some())
            .field("window_title", &self.window_title)
            .finish()
    }
}

impl ShellTabState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Same as [`new`](Self::new), but wires a [`SidecarFactory`] so
    /// [`ShellEvent::Sidecar`] events materialize state on first arrival.
    pub fn with_sidecar_factory(factory: SidecarFactory) -> Self {
        Self {
            sidecar_factory: Some(factory),
            ..Self::default()
        }
    }

    /// Mutate the state in response to a parsed event.
    pub fn apply(&mut self, event: &ShellEvent) {
        match event {
            ShellEvent::CwdChanged(path) => {
                self.cwd = Some(path.clone());
            }
            ShellEvent::CommandOutputStart => {
                self.in_flight_started_at = Some(Instant::now());
            }
            ShellEvent::CommandEnd { exit_code, .. } => {
                let now = Instant::now();
                let started = self.in_flight_started_at.take().unwrap_or(now);
                self.last_command = Some(CommandRecord {
                    started_at: started,
                    duration: now.saturating_duration_since(started),
                    exit_code: *exit_code,
                });
                self.last_attention_at = Some(now);
                // A foreground command just returned to the shell prompt. If a
                // sidecar (e.g. a manually-run agent CLI) was reporting through
                // this shell, it has now exited — clear its state so it drops
                // off whatever rollup the caller renders. Dedicated agent tabs
                // run the agent directly (no shell integration), never see
                // CommandEnd, and keep their state.
                self.sidecar = None;
            }
            ShellEvent::Sidecar(json) => {
                self.apply_sidecar_event(json.clone());
            }
            ShellEvent::WindowTitle(t) => {
                self.window_title = Some(t.clone());
            }
            ShellEvent::PromptStart | ShellEvent::CommandInputStart => {
                // No-op — markers we keep for future UX (e.g. "scroll to
                // previous prompt"), no state change today.
            }
        }
    }

    /// Feed a decoded JSON event straight into the sidecar state, whether it
    /// arrived via [`ShellEvent::Sidecar`] or was synthesized by the caller
    /// out-of-band (e.g. passive status detection with no hook bridge at
    /// all). Requires [`with_sidecar_factory`](Self::with_sidecar_factory) —
    /// a no-op otherwise.
    /// Materialize the sidecar state (via the configured factory) if it
    /// doesn't exist yet, and return it for direct mutation — for a caller
    /// that observes status out-of-band with no JSON event to hand
    /// [`apply_sidecar_event`](Self::apply_sidecar_event) (e.g. passive
    /// status detection reading a provider's own title/screen text). `None`
    /// without a factory configured.
    pub fn sidecar_or_insert(&mut self) -> Option<&mut Box<dyn SidecarState>> {
        let factory = self.sidecar_factory.as_ref()?;
        Some(self.sidecar.get_or_insert_with(|| factory()))
    }

    pub fn apply_sidecar_event(&mut self, json: serde_json::Value) {
        let Some(factory) = self.sidecar_factory.as_ref() else {
            return;
        };
        self.sidecar.get_or_insert_with(|| factory()).apply(json);
    }

    /// Drop the attention marker (e.g. when the user focuses this tab).
    pub fn acknowledge(&mut self) {
        self.last_attention_at = None;
        if let Some(sidecar) = self.sidecar.as_mut() {
            sidecar.acknowledge();
        }
    }

    /// Wire form for the session daemon → client handoff. `Instant`s become
    /// durations relative to now; [`from_snapshot`](Self::from_snapshot)
    /// turns them back.
    pub fn snapshot(&self) -> ShellStateSnapshot {
        let now = Instant::now();
        ShellStateSnapshot {
            cwd: self.cwd.clone(),
            last_command: self.last_command.as_ref().map(|c| CommandSnapshot {
                exit_code: c.exit_code,
                duration_ms: c.duration.as_millis() as u64,
                finished_ago_ms: now
                    .saturating_duration_since(c.started_at + c.duration)
                    .as_millis() as u64,
            }),
            attention_pending: self.last_attention_at.is_some(),
            in_flight_for_ms: self
                .in_flight_started_at
                .map(|t| now.saturating_duration_since(t).as_millis() as u64),
            sidecar: self.sidecar.as_ref().map(|a| a.snapshot()),
            window_title: self.window_title.clone(),
        }
    }

    /// Rebuild the state a daemon-side tab had at snapshot time. Attention
    /// markers that were pending are re-armed as of now, which is what the
    /// UIs need (they only test `is_some()`). `sidecar_factory`, when given,
    /// is used to reconstruct the sidecar state via
    /// [`SidecarState::restore`]; without one the sidecar snapshot (if any)
    /// is dropped rather than guessed at.
    pub fn from_snapshot(s: &ShellStateSnapshot, sidecar_factory: Option<&SidecarFactory>) -> Self {
        let now = Instant::now();
        let last_command = s.last_command.as_ref().map(|c| {
            let duration = Duration::from_millis(c.duration_ms);
            let finished_at = now
                .checked_sub(Duration::from_millis(c.finished_ago_ms))
                .unwrap_or(now);
            CommandRecord {
                started_at: finished_at.checked_sub(duration).unwrap_or(finished_at),
                duration,
                exit_code: c.exit_code,
            }
        });
        let sidecar = match (&s.sidecar, sidecar_factory) {
            (Some(snap), Some(factory)) => {
                let mut state = factory();
                state.restore(snap.clone());
                Some(state)
            }
            _ => None,
        };
        Self {
            cwd: s.cwd.clone(),
            last_command,
            last_attention_at: s.attention_pending.then_some(now),
            in_flight_started_at: s
                .in_flight_for_ms
                .map(|ms| now.checked_sub(Duration::from_millis(ms)).unwrap_or(now)),
            sidecar,
            sidecar_factory: sidecar_factory.cloned(),
            window_title: s.window_title.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Minimal test double: last event wins, tracks an "attention" flag.
    #[derive(Default, Clone)]
    struct FakeSidecar {
        last_event: Option<serde_json::Value>,
        attention: bool,
    }

    impl SidecarState for FakeSidecar {
        fn apply(&mut self, event: serde_json::Value) {
            self.attention = event
                .get("attention")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            self.last_event = Some(event);
        }
        fn snapshot(&self) -> serde_json::Value {
            serde_json::json!({
                "last_event": self.last_event,
                "attention": self.attention,
            })
        }
        fn restore(&mut self, snapshot: serde_json::Value) {
            self.last_event = snapshot.get("last_event").cloned();
            self.attention = snapshot
                .get("attention")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
        }
        fn acknowledge(&mut self) {
            self.attention = false;
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
    }

    fn factory() -> SidecarFactory {
        Arc::new(|| Box::new(FakeSidecar::default()) as Box<dyn SidecarState>)
    }

    #[test]
    fn apply_command_lifecycle_records_exit_code_and_duration() {
        let mut s = ShellTabState::new();
        s.apply(&ShellEvent::CommandOutputStart);
        std::thread::sleep(Duration::from_millis(5));
        s.apply(&ShellEvent::CommandEnd {
            exit_code: Some(0),
            command: None,
        });
        let cmd = s.last_command.expect("command recorded");
        assert!(cmd.ok());
        assert!(cmd.duration >= Duration::from_millis(5));
        assert!(s.last_attention_at.is_some());
    }

    #[test]
    fn apply_cwd_changed_updates_cwd() {
        let mut s = ShellTabState::new();
        s.apply(&ShellEvent::CwdChanged(PathBuf::from("/tmp/foo")));
        assert_eq!(s.cwd, Some(PathBuf::from("/tmp/foo")));
    }

    #[test]
    fn acknowledge_clears_attention() {
        let mut s = ShellTabState::new();
        s.apply(&ShellEvent::CommandOutputStart);
        s.apply(&ShellEvent::CommandEnd {
            exit_code: Some(1),
            command: None,
        });
        assert!(s.last_attention_at.is_some());
        s.acknowledge();
        assert!(s.last_attention_at.is_none());
    }

    #[test]
    fn command_end_without_start_still_records() {
        let mut s = ShellTabState::new();
        s.apply(&ShellEvent::CommandEnd {
            exit_code: Some(2),
            command: None,
        });
        let cmd = s.last_command.expect("command recorded");
        assert_eq!(cmd.exit_code, Some(2));
        assert_eq!(cmd.duration, Duration::ZERO);
    }

    #[test]
    fn sidecar_event_is_inert_without_a_factory() {
        let mut s = ShellTabState::new();
        s.apply(&ShellEvent::Sidecar(serde_json::json!({"attention": true})));
        assert!(s.sidecar.is_none());
    }

    #[test]
    fn sidecar_event_materializes_state_via_factory() {
        let mut s = ShellTabState::with_sidecar_factory(factory());
        s.apply(&ShellEvent::Sidecar(serde_json::json!({"attention": true})));
        let sidecar = s.sidecar.as_ref().expect("sidecar materialized");
        let fake = sidecar.as_any().downcast_ref::<FakeSidecar>().unwrap();
        assert!(fake.attention);
    }

    #[test]
    fn snapshot_round_trips_everything_the_ui_reads() {
        let mut s = ShellTabState::with_sidecar_factory(factory());
        s.apply(&ShellEvent::CwdChanged(PathBuf::from("/work/here")));
        s.apply(&ShellEvent::WindowTitle("codex ⠋".to_string()));
        s.apply(&ShellEvent::CommandOutputStart);
        std::thread::sleep(Duration::from_millis(5));
        s.apply(&ShellEvent::CommandEnd {
            exit_code: Some(7),
            command: Some("make".to_string()),
        });
        s.apply(&ShellEvent::Sidecar(
            serde_json::json!({"attention": true, "kind": "permission_request"}),
        ));

        let snap = s.snapshot();
        assert_eq!(snap.cwd, Some(PathBuf::from("/work/here")));
        assert_eq!(snap.window_title.as_deref(), Some("codex ⠋"));
        let cmd = snap.last_command.as_ref().unwrap();
        assert_eq!(cmd.exit_code, Some(7));
        assert!(cmd.duration_ms >= 5);
        assert!(snap.attention_pending);
        assert!(snap.in_flight_for_ms.is_none());
        assert!(snap.sidecar.is_some());

        let back = ShellTabState::from_snapshot(&snap, Some(&factory()));
        assert_eq!(back.cwd, s.cwd);
        assert_eq!(back.window_title, s.window_title);
        let rec = back.last_command.unwrap();
        assert_eq!(rec.exit_code, Some(7));
        assert_eq!(rec.duration.as_millis() as u64, cmd.duration_ms);
        assert!(back.last_attention_at.is_some());
        let sidecar = back.sidecar.unwrap();
        let fake = sidecar.as_any().downcast_ref::<FakeSidecar>().unwrap();
        assert!(fake.attention);

        // And a quiet tab stays quiet.
        let quiet = ShellTabState::from_snapshot(&ShellTabState::new().snapshot(), None);
        assert!(quiet.last_attention_at.is_none());
        assert!(quiet.sidecar.is_none());
        assert!(quiet.last_command.is_none());
    }

    #[test]
    fn in_flight_command_survives_the_snapshot() {
        let mut s = ShellTabState::new();
        s.apply(&ShellEvent::CommandOutputStart);
        std::thread::sleep(Duration::from_millis(5));
        let snap = s.snapshot();
        assert!(snap.in_flight_for_ms.unwrap() >= 5);
        let mut back = ShellTabState::from_snapshot(&snap, None);
        back.apply(&ShellEvent::CommandEnd {
            exit_code: Some(0),
            command: None,
        });
        assert!(back.last_command.unwrap().duration >= Duration::from_millis(5));
    }

    #[test]
    fn command_end_clears_sidecar_state() {
        // A manually-run agent reports through a shell tab...
        let mut s = ShellTabState::with_sidecar_factory(factory());
        s.apply(&ShellEvent::Sidecar(
            serde_json::json!({"event": "session_start"}),
        ));
        assert!(s.sidecar.is_some(), "sidecar state set by the event");

        // ...and drops off when the CLI exits and the shell returns to its
        // prompt (OSC 133 command-end).
        s.apply(&ShellEvent::CommandEnd {
            exit_code: Some(0),
            command: None,
        });
        assert!(
            s.sidecar.is_none(),
            "sidecar state cleared once the process exits"
        );
    }
}
