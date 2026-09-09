//! A single daemon-owned PTY session: the child process, the terminal
//! emulator that produces restore buffers, and the fan-out to every attached
//! client.
//!
//! The reader thread is the heart of it: it reads the PTY master, feeds the
//! `vt100` parser (for restore) and the `OscParser` (for shell/agent status),
//! and — under one lock so no client sees a gap or a duplicate — appends the
//! bytes to every attached client's outbound queue. Terminal query replies
//! (DSR/DA) are answered here only while nobody is attached, so an agent that
//! probes the terminal at startup gets a reply when headless; with a live
//! client attached, the client's real terminal answers instead (answering
//! here too would double-answer and break query fences).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::pty::ShellSession;
use crate::session::protocol::{
    Frame, SessionInfo, SessionMeta, SessionState, ShellStateSnapshot, SpawnRequest,
};
use crate::session::restore::restore_buffer;
use crate::shell_integration::parser::OscParser;
use crate::shell_integration::{ShellEvent, ShellTabState, SidecarConfig};
use crate::sidecar::sock::SockReader;

/// Depth of a client's outbound queue. A client that falls this far behind is
/// dropped (its terminal is a lost cause anyway) rather than back-pressuring
/// the PTY reader, which every other client shares.
const CLIENT_QUEUE_DEPTH: usize = 512;

/// Scrollback the daemon keeps per session for restore buffers.
const DAEMON_SCROLLBACK: usize = 5000;

/// A registered client's outbound side, held by the [`Session`] so the reader
/// can fan output to it. The client's own connection thread owns the matching
/// receiver + socket write half.
struct ClientHandle {
    id: u64,
    tx: SyncSender<Frame>,
    /// A clone of the client's socket, used only to force it closed when the
    /// daemon drops the client (so the client's blocked read unblocks).
    shutdown: UnixStream,
}

/// Output state guarded by a single lock so registration and fan-out are
/// atomic with respect to each other: a client that registers here cannot
/// miss bytes the reader is mid-fan-out, nor receive bytes already baked into
/// its restore buffer.
struct Output {
    parser: vt100::Parser,
    clients: Vec<ClientHandle>,
    next_client_id: u64,
    /// The designated answerer for terminal queries: the most recently
    /// attached client. Its `Answerback` relays reach the PTY; relays from
    /// superseded clients are dropped so the application never sees
    /// duplicates. `None` while nobody is attached (then the daemon answers
    /// queries itself — see `on_bytes`).
    answerer: Option<u64>,
}

impl Output {
    /// Queue `frame` to every client; drop any whose queue is full or gone.
    fn broadcast(&mut self, frame: &Frame) {
        self.clients.retain(|c| match c.tx.try_send(frame.clone()) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                tracing::warn!(client = c.id, "session client fell behind; dropping");
                let _ = c.shutdown.shutdown(std::net::Shutdown::Both);
                false
            }
            Err(TrySendError::Disconnected(_)) => false,
        });
        // A dropped answerer must not keep the role: hand it to the most
        // recently attached survivor (or nobody, if none remain).
        if self
            .answerer
            .is_some_and(|a| !self.clients.iter().any(|c| c.id == a))
        {
            self.answerer = self.clients.iter().map(|c| c.id).max();
        }
    }
}

/// Exit state: `None` while the child runs, `Some(code)` once it has exited.
#[derive(Default)]
struct Exit {
    code: Mutex<Option<Option<i32>>>,
    at_unix_ms: Mutex<Option<u64>>,
}

pub struct Session {
    pub id: String,
    command: String,
    args: Vec<String>,
    cwd: PathBuf,
    created_at_unix_ms: u64,
    meta: Mutex<SessionMeta>,
    size: Mutex<(u16, u16)>,
    output: Mutex<Output>,
    /// Per-session shell/agent state, shared with the FIFO reader. `None`
    /// when integration is off for this session.
    shell: Option<Arc<Mutex<ShellSession>>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    exit: Exit,
    reader: OnceLock<thread::JoinHandle<()>>,
    /// FIFO reader for the sidecar channel; its `Drop` unlinks the FIFO.
    /// `None` unless this session has one.
    cli_agent_sock: Mutex<Option<SockReader>>,
    /// How this daemon interprets its sessions' sidecar channel, if the
    /// embedding application wired one up.
    sidecar: Option<SidecarConfig>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Session {
    /// Spawn the child described by `req` and start its reader thread.
    ///
    /// The caller's environment is applied verbatim after `env_clear`, so a
    /// session spawned by either frontend behaves identically.
    pub fn spawn(
        req: &SpawnRequest,
        sidecar: Option<SidecarConfig>,
    ) -> anyhow::Result<Arc<Session>> {
        let pty = native_pty_system();
        let pair = pty.openpty(PtySize {
            rows: req.rows,
            cols: req.cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new(&req.command);
        cmd.args(&req.args);
        cmd.cwd(&req.cwd);
        cmd.env_clear();
        for (k, v) in &req.env {
            cmd.env(k, v);
        }

        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);
        let killer = child.clone_killer();

        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        let shell = req.integration_on.then(|| {
            let state = match sidecar.as_ref() {
                Some(sc) => ShellTabState::with_sidecar_factory(sc.factory.clone()),
                None => ShellTabState::new(),
            };
            Arc::new(Mutex::new(ShellSession {
                state,
                pending_events: Vec::new(),
            }))
        });

        let session = Arc::new(Session {
            id: req.id.clone(),
            command: req.command.clone(),
            args: req.args.clone(),
            cwd: req.cwd.clone(),
            created_at_unix_ms: now_ms(),
            meta: Mutex::new(req.meta.clone()),
            size: Mutex::new((req.rows, req.cols)),
            output: Mutex::new(Output {
                parser: vt100::Parser::new(req.rows, req.cols, DAEMON_SCROLLBACK),
                clients: Vec::new(),
                next_client_id: 0,
                answerer: None,
            }),
            shell,
            child: Mutex::new(child),
            killer: Mutex::new(killer),
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            exit: Exit::default(),
            reader: OnceLock::new(),
            cli_agent_sock: Mutex::new(None),
            sidecar,
        });

        // Out-of-band sidecar FIFO. Its callback fans the decoded JSON event
        // to attached clients immediately (an idle agent produces no PTY
        // bytes, so the reader thread can't do it), then clears the
        // `pending_events` the sock reader appends so it never grows. Failure
        // degrades to OSC-777-only status; it never fails the spawn.
        if let (Some(sock_path), Some(shell)) = (&req.cli_agent_sock, session.shell.as_ref()) {
            let weak = Arc::downgrade(&session);
            let cb: crate::sidecar::sock::SidecarCallback = Box::new(move |ev| {
                if let Some(session) = weak.upgrade() {
                    session.broadcast(&Frame::ShellEvent {
                        event: ShellEvent::Sidecar(ev.clone()),
                        replayed: false,
                    });
                    if let Some(shell) = &session.shell {
                        shell.lock().pending_events.clear();
                    }
                }
            });
            match crate::sidecar::sock::spawn_reader(sock_path.clone(), Arc::clone(shell), Some(cb))
            {
                Ok(r) => *session.cli_agent_sock.lock() = Some(r),
                Err(e) => {
                    tracing::warn!(error = %e, "session sidecar FIFO reader failed to start")
                }
            }
        }

        let reader_session = Arc::clone(&session);
        let handle = thread::Builder::new()
            .name(format!("session-reader({})", session.id))
            .spawn(move || reader_session.read_loop(&mut reader))?;
        let _ = session.reader.set(handle);

        Ok(session)
    }

    /// PTY reader thread body: master → parser + OSC → fan-out.
    fn read_loop(self: Arc<Self>, reader: &mut Box<dyn Read + Send>) {
        let mut buf = [0u8; 65536];
        let mut osc =
            OscParser::with_sidecar_target(self.sidecar.as_ref().map(|sc| sc.osc_target.clone()));
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => self.on_bytes(&buf[..n], &mut osc),
                Err(_) => break,
            }
        }
        self.on_child_exit();
    }

    fn on_bytes(&self, chunk: &[u8], osc: &mut OscParser) {
        // OSC events touch the separately-shared ShellSession, so parse and
        // apply them outside the output lock, then fan out inside it so their
        // order versus the byte stream is preserved. They are NOT pushed to
        // `pending_events` (we fan out here); only the FIFO path uses that.
        let events = if self.shell.is_some() {
            osc.feed(chunk)
        } else {
            Vec::new()
        };
        if let Some(shell) = &self.shell {
            let mut s = shell.lock();
            for ev in &events {
                s.state.apply(ev);
            }
        }

        let answerback = {
            let mut out = self.output.lock();
            out.parser.process(chunk);
            let answerback = out.parser.screen_mut().take_answerback();
            out.broadcast(&Frame::Output(chunk.to_vec()));
            for ev in events {
                out.broadcast(&Frame::ShellEvent {
                    event: ev,
                    replayed: false,
                });
            }
            // With a live client attached, the client's real terminal answers
            // queries (including ones the daemon-side parser doesn't know,
            // like OSC 10/11). Answering here too would double-answer DA1 and
            // break query fences (delta's OSC 10 + OSC 11 + DA1 probe), so
            // only reply while nobody is attached. Always drained above so a
            // suppressed reply never accumulates.
            if out.clients.is_empty() {
                answerback
            } else {
                Vec::new()
            }
        };

        if !answerback.is_empty() {
            let mut w = self.writer.lock();
            let _ = w.write_all(&answerback);
            let _ = w.flush();
        }
    }

    fn on_child_exit(&self) {
        let code = self
            .child
            .lock()
            .try_wait()
            .ok()
            .flatten()
            .map(|s| s.exit_code() as i32);
        *self.exit.code.lock() = Some(code);
        *self.exit.at_unix_ms.lock() = Some(now_ms());
        self.broadcast(&Frame::Exited { code });
        tracing::info!(session = %self.id, ?code, "session child exited");
    }

    fn broadcast(&self, frame: &Frame) {
        self.output.lock().broadcast(frame);
    }

    /// `Some(code)` once the child has exited (`code` may itself be `None` if
    /// the platform reported no status).
    pub fn exit_code(&self) -> Option<Option<i32>> {
        *self.exit.code.lock()
    }

    pub fn is_live(&self) -> bool {
        self.exit.code.lock().is_none()
    }

    /// When the session exited (ms since epoch), if it has.
    pub fn exited_at_unix_ms(&self) -> Option<u64> {
        *self.exit.at_unix_ms.lock()
    }

    pub fn meta(&self) -> SessionMeta {
        self.meta.lock().clone()
    }

    pub fn set_meta(&self, f: impl FnOnce(&mut SessionMeta)) {
        f(&mut self.meta.lock());
    }

    /// Snapshot for `List` / `Attached` replies.
    pub fn info(&self) -> SessionInfo {
        let (rows, cols) = *self.size.lock();
        let state = match self.exit_code() {
            None => SessionState::Live,
            Some(code) => SessionState::Exited { code },
        };
        SessionInfo {
            id: self.id.clone(),
            meta: self.meta(),
            cwd: self.cwd.clone(),
            command: self.command.clone(),
            args: self.args.clone(),
            state,
            integration_on: self.shell.is_some(),
            attached: self.attached_count(),
            rows,
            cols,
            created_at_unix_ms: self.created_at_unix_ms,
            exited_at_unix_ms: self.exited_at_unix_ms(),
        }
    }

    /// Current shell/agent state as a wire snapshot (empty when integration
    /// is off).
    pub fn shell_snapshot(&self) -> ShellStateSnapshot {
        self.shell
            .as_ref()
            .map(|s| s.lock().state.snapshot())
            .unwrap_or_default()
    }

    /// Feed `data` to the child's stdin.
    pub fn write_input(&self, data: &[u8]) -> std::io::Result<()> {
        let mut w = self.writer.lock();
        w.write_all(data)?;
        w.flush()
    }

    /// Forward a client's terminal-query relay to the child's stdin — but
    /// only while `client_id` is the designated answerer (the most recently
    /// attached client). Relays from superseded clients are silently dropped
    /// so the application sees each query answered exactly once. User
    /// keystrokes ([`write_input`](Self::write_input)) are never gated.
    pub fn write_answerback(&self, client_id: u64, data: &[u8]) -> std::io::Result<()> {
        if self.output.lock().answerer != Some(client_id) {
            return Ok(());
        }
        let mut w = self.writer.lock();
        w.write_all(data)?;
        w.flush()
    }

    /// Resize the PTY (and the restore parser) to `rows`×`cols`.
    pub fn resize(&self, rows: u16, cols: u16) {
        *self.size.lock() = (rows, cols);
        let _ = self.master.lock().resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        self.output.lock().parser.screen_mut().set_size(rows, cols);
    }

    /// The restore buffer at the current size (for a `Resync`).
    pub fn restore(&self) -> Vec<u8> {
        restore_buffer(&self.output.lock().parser)
    }

    /// SIGHUP the child; SIGKILL if it is still alive after a short grace.
    /// A no-op if it has already exited.
    pub fn kill(&self) {
        if !self.is_live() {
            return;
        }
        let _ = self.killer.lock().kill();
        let pid = self.child.lock().process_id();
        // Give the child up to 500ms to die on SIGHUP before escalating.
        for _ in 0..10 {
            if !self.is_live() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        #[cfg(unix)]
        if let Some(pid) = pid {
            // SAFETY: kill(2) with a real pid and a real signal; a stale pid
            // just returns ESRCH.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }

    /// Register a newly attached client. Resizes to the client's size,
    /// captures the restore buffer and enqueues it — all under the output
    /// lock so no live byte is lost or double-sent — then returns the client
    /// id, a sender the connection thread can use for `Resync`, and the
    /// receiver its writer thread drains.
    #[allow(clippy::type_complexity)]
    pub fn attach(
        &self,
        rows: u16,
        cols: u16,
        shutdown: UnixStream,
    ) -> (u64, SyncSender<Frame>, Receiver<Frame>) {
        *self.size.lock() = (rows, cols);
        let _ = self.master.lock().resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        let (tx, rx) = std::sync::mpsc::sync_channel::<Frame>(CLIENT_QUEUE_DEPTH);
        let mut out = self.output.lock();
        out.parser.screen_mut().set_size(rows, cols);
        let restore = restore_buffer(&out.parser);
        let id = out.next_client_id;
        out.next_client_id += 1;
        // Enqueue restore before registering so it is the first frame the
        // writer sends, ahead of any live output that arrives next.
        let _ = tx.try_send(Frame::Restore(restore));
        if let Some(code) = self.exit_code() {
            let _ = tx.try_send(Frame::Exited { code });
        }
        out.clients.push(ClientHandle {
            id,
            tx: tx.clone(),
            shutdown,
        });
        // The newcomer takes over answering terminal queries; every other
        // attached client stays silent so the application sees one reply.
        out.answerer = Some(id);
        (id, tx, rx)
    }

    /// Deregister a client (on detach / disconnect). When the designated
    /// answerer leaves, the most recently attached survivor takes over; when
    /// nobody remains, the daemon answers queries itself again.
    pub fn detach(&self, client_id: u64) {
        let mut out = self.output.lock();
        out.clients.retain(|c| c.id != client_id);
        if out.answerer == Some(client_id) {
            out.answerer = out.clients.iter().map(|c| c.id).max();
        }
    }

    pub fn attached_count(&self) -> usize {
        self.output.lock().clients.len()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.killer.lock().kill();
        // The reader exits on its own when the master closes; the FIFO reader
        // unlinks its socket on drop.
        let _ = self.cli_agent_sock.lock().take();
    }
}
