//! PTY proxy core: byte-transparent byte pump + hotkey interception +
//! signal handling.
//!
//! # Design: testable pump, thin TTY layer
//!
//! The pump is written over generic [`Read`]/[`Write`] streams so tests can
//! inject pipes in place of the real stdin/stdout (see [`spawn_with_io`]);
//! only [`PtyProxy::spawn`](crate::PtyProxy::spawn) touches the real TTY
//! (raw-mode termios, stdin/stdout fds, real terminal size). Synchronous
//! streams are used deliberately: `portable-pty`'s reader/writer are blocking
//! `Read`/`Write` objects, so the pump runs on a dedicated thread
//! (`tokio::task::spawn_blocking`) instead of pretending to be async.
//!
//! # Threads
//!
//! * an **input thread** pumps `stdin -> PTY master` through the hotkey
//!   chord filter;
//! * the **pump thread** proper pumps `PTY master -> stdout` verbatim, then
//!   reaps the child;
//! * two tokio tasks handle SIGWINCH (resize) and SIGINT/SIGTERM/SIGHUP
//!   (forward to the child's foreground process group).
//!
//! When the child exits, the master read returns EOF/EIO, the pump reaps the
//! child and returns its exit status semantics. The input thread may still be
//! blocked reading stdin at that point; it holds no terminal state and is
//! reaped when the process exits (the raw-mode guard lives on the pump
//! thread, so termios is restored regardless).

use std::io::{Read, Write};
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::hotkey::ChordFilter;
use crate::{HotkeyConfig, ProxyEvent};

/// Capacity of the ProxyEvent channel. Events are delivered with `try_send`;
/// a flooded consumer drops events rather than stalling the input pump.
const EVENT_QUEUE: usize = 64;

/// Fallback PTY size when the real terminal size cannot be determined
/// (e.g. stdout is a pipe, as in CI).
const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLS: u16 = 80;

/// Upper bound on how long the input thread waits for the child's first
/// output before delivering the stdin-EOF byte (VEOF). See the stdin-EOF
/// policy on [`run_pump`].
const EOF_READY_GRACE: std::time::Duration = std::time::Duration::from_millis(750);

// ---------------------------------------------------------------------------
// termios raw mode (RAII) + suspend/resume handle
// ---------------------------------------------------------------------------

/// Shared termios state behind [`TerminalGuard`] and the pump's RAII guard.
struct TermState {
    fd: RawFd,
    saved: libc::termios,
    raw: libc::termios,
    /// Shared with the proxy's input and output pumps so they can stop
    /// reading stdin / pause forwarding child output while the terminal is
    /// handed to a pager/surface.
    suspended: Arc<AtomicBool>,
    /// Serializes [`TerminalGuard::suspend`]/[`TerminalGuard::resume`]
    /// against the input pump's read step and the output pump's write step
    /// (F7): a pump holds this mutex only for a bounded, microsecond-scale
    /// critical section — re-check the suspension flag, re-poll the fd with
    /// a zero timeout, then perform the (now guaranteed non-blocking) read
    /// or write — so a suspend can only apply the cooked termios once no
    /// read is in flight (pager keystrokes cannot be stolen by an in-flight
    /// read), resume re-applies raw mode before the flag clears (the pump
    /// never reads stdin in cooked mode), and once suspend returns no
    /// further child output can reach stdout. The mutex is NEVER held
    /// across an idle wait: doing so pins it ~100% of the time and starves
    /// the other pump (and suspend/resume) — a futex-woken waiter always
    /// loses to the holder's unlock→relock fast path.
    gate: Arc<Mutex<()>>,
}

impl TermState {
    /// Put `fd` into raw mode if (and only if) it is a TTY. Returns `Ok(None)`
    /// for non-TTY inputs (e.g. piped stdin in CI), leaving them untouched.
    fn enable_if_tty(fd: RawFd) -> std::io::Result<Option<Arc<Self>>> {
        if unsafe { libc::isatty(fd) } != 1 {
            return Ok(None);
        }
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut raw = saved;
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Some(Arc::new(TermState {
            fd,
            saved,
            raw,
            suspended: Arc::new(AtomicBool::new(false)),
            gate: Arc::new(Mutex::new(())),
        })))
    }

    fn apply(&self, tio: &libc::termios) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, tio);
        }
    }
}

/// RAII guard owned by the pump: restores the original termios on drop —
/// covering normal returns, error paths and panics alike. If the terminal
/// is suspended (cooked) at drop time, restoring `saved` is a no-op
/// equivalent, so drop unconditionally restores `saved`.
pub(crate) struct RawModeGuard {
    state: Arc<TermState>,
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        self.state.apply(&self.state.saved);
    }
}

/// A clonable handle to the proxy's terminal mode, returned by
/// [`crate::PtyProxy::spawn_with_terminal_guard`]. While the proxy owns the
/// terminal in raw mode, an interactive surface (pager, editor) needs the
/// terminal back in cooked mode: call [`TerminalGuard::suspend`] before
/// spawning it and [`TerminalGuard::resume`] after it exits.
///
/// While suspended, the proxy's input pump stops reading stdin (polled with
/// a short timeout) so the surface process has the terminal's input to
/// itself, and the child CLI's output is captured in a bounded (1 MiB,
/// drop-oldest with a marker note) buffer instead of reaching stdout —
/// flushed back to stdout on resume — so streaming child output cannot
/// corrupt the surface's alternate-screen frame. Suspend/resume are
/// idempotent and cheap. When stdin is not a TTY the handle is inert
/// (`is_terminal()` is `false`).
#[derive(Clone)]
pub struct TerminalGuard {
    state: Option<Arc<TermState>>,
}

impl TerminalGuard {
    /// An inert handle (stdin is not a TTY); all operations are no-ops.
    pub fn none() -> Self {
        TerminalGuard { state: None }
    }

    /// Whether this handle actually controls a terminal.
    pub fn is_terminal(&self) -> bool {
        self.state.is_some()
    }

    /// Whether the terminal is currently suspended (cooked mode).
    pub fn is_suspended(&self) -> bool {
        self.state
            .as_ref()
            .map(|s| s.suspended.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    /// Temporarily restore the original (cooked) termios and pause the
    /// proxy's stdin pump. Idempotent.
    ///
    /// The gate mutex is held across the flag set + termios change, so this
    /// call blocks until any in-flight stdin read or stdout write has
    /// finished (bounded: the pumps only read/write while holding the gate
    /// when a zero-timeout poll has confirmed the operation cannot block):
    /// once `suspend` returns, the input pump is guaranteed parked and
    /// cannot steal the surface's keystrokes.
    pub fn suspend(&self) {
        if let Some(s) = &self.state {
            let _lock = s.gate.lock().unwrap();
            if !s.suspended.swap(true, Ordering::SeqCst) {
                s.apply(&s.saved);
            }
        }
    }

    /// Re-enter raw mode and resume the proxy's stdin pump. Idempotent.
    ///
    /// Raw termios are re-applied BEFORE the suspension flag is cleared
    /// (still under the gate mutex), so the input pump can never observe
    /// "not suspended" while the terminal is still in cooked mode.
    pub fn resume(&self) {
        if let Some(s) = &self.state {
            let _lock = s.gate.lock().unwrap();
            if s.suspended.load(Ordering::SeqCst) {
                s.apply(&s.raw);
                s.suspended.store(false, Ordering::SeqCst);
            }
        }
    }

    /// The suspension flag shared with the input/output pumps, if any.
    fn suspend_flag(&self) -> Option<Arc<AtomicBool>> {
        self.state.as_ref().map(|s| s.suspended.clone())
    }

    /// The gate mutex serializing suspend/resume against the input pump's
    /// read step, if any.
    fn gate(&self) -> Option<Arc<Mutex<()>>> {
        self.state.as_ref().map(|s| s.gate.clone())
    }
}

// ---------------------------------------------------------------------------
// winsize helpers
// ---------------------------------------------------------------------------

/// Query the (rows, cols) of the terminal behind `fd`; `None` if `fd` is not
/// a terminal or the size is degenerate.
fn get_winsize(fd: RawFd) -> Option<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } == 0
        && ws.ws_row > 0
        && ws.ws_col > 0
    {
        Some((ws.ws_row, ws.ws_col))
    } else {
        None
    }
}

/// Pick the fd that represents "the real terminal": the first of
/// stdout/stdin/stderr that answers `TIOCGWINSZ` with a sane size, else
/// stdout (the 80x24 fallback will be used until the first SIGWINCH).
fn real_term_fd() -> RawFd {
    for fd in [libc::STDOUT_FILENO, libc::STDIN_FILENO, libc::STDERR_FILENO] {
        if get_winsize(fd).is_some() {
            return fd;
        }
    }
    libc::STDOUT_FILENO
}

/// Read the child PTY's current VEOF control character via the master fd
/// (`tcgetattr` on the master reports the slave's termios). Returns `None`
/// when the fd is not a terminal or VEOF is disabled (`_POSIX_VDISABLE`).
fn read_veof(master_fd: RawFd) -> Option<u8> {
    let mut tio: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(master_fd, &mut tio) } != 0 {
        return None;
    }
    let veof = tio.c_cc[libc::VEOF];
    if veof == 0 {
        None
    } else {
        Some(veof)
    }
}

/// Result of a one-shot `poll(2)` for readability on a single fd.
enum PollIn {
    /// Data is available to read (POLLIN). HUP/ERR bits set alongside
    /// POLLIN still count as ready: the pending data is drained first and
    /// the following read observes the EOF/error.
    Ready,
    /// The timeout expired with nothing to read.
    Timeout,
    /// The fd is gone: HUP/ERR/NVAL without pending data, or the poll
    /// itself failed (other than EINTR, which is retried internally).
    Gone,
}

/// Poll `fd` for readability with `timeout_ms`. EINTR is retried.
fn poll_in(fd: RawFd, timeout_ms: libc::c_int) -> PollIn {
    loop {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if rc > 0 {
            if pfd.revents & libc::POLLIN != 0 {
                return PollIn::Ready;
            }
            if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return PollIn::Gone;
            }
            return PollIn::Timeout; // spurious wakeup; treat as no input
        }
        if rc == 0 {
            return PollIn::Timeout;
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EINTR) {
            return PollIn::Gone;
        }
    }
}

/// Bounded drop-oldest buffer for child output captured while the terminal
/// is suspended (F2: a pager/surface owns the screen). A flooding child
/// cannot grow memory without bound: past the cap the oldest bytes are
/// discarded and a marker note is emitted before the surviving bytes on
/// drain, so the user can tell output was lost.
struct SuspendedOutput {
    buf: std::collections::VecDeque<u8>,
    dropped: usize,
}

impl SuspendedOutput {
    /// 1 MiB cap, as documented for the pager-suspension policy.
    const CAP: usize = 1024 * 1024;

    fn new() -> Self {
        SuspendedOutput {
            buf: std::collections::VecDeque::new(),
            dropped: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.buf.is_empty() && self.dropped == 0
    }

    fn push(&mut self, bytes: &[u8]) {
        self.buf.extend(bytes);
        while self.buf.len() > Self::CAP {
            self.buf.pop_front();
            self.dropped += 1;
        }
    }

    /// Write the buffered bytes (and a drop marker, if any) to `out` and
    /// reset the buffer.
    fn drain<W: Write>(&mut self, out: &mut W) -> std::io::Result<()> {
        if self.dropped > 0 {
            write!(
                out,
                "\n[mdterm] {} bytes of child output dropped while the pager was open\n",
                self.dropped
            )?;
            self.dropped = 0;
        }
        if !self.buf.is_empty() {
            let (a, b) = self.buf.as_slices();
            out.write_all(a)?;
            out.write_all(b)?;
            self.buf.clear();
        }
        out.flush()
    }
}

// ---------------------------------------------------------------------------
// entry points
// ---------------------------------------------------------------------------

/// Implementation behind [`crate::PtyProxy::spawn`].
pub(crate) fn spawn(
    cmd: &str,
    args: &[String],
    hotkeys: HotkeyConfig,
) -> Result<(JoinHandle<i32>, mpsc::Receiver<ProxyEvent>)> {
    let (handle, events, _terminal) = spawn_with_terminal_guard(cmd, args, hotkeys)?;
    Ok((handle, events))
}

/// Implementation behind [`crate::PtyProxy::spawn_with_terminal_guard`]:
/// like [`spawn`] but also returns a [`TerminalGuard`] handle so the caller
/// can suspend raw mode (cooked terminal + paused stdin pump) while an
/// interactive surface such as a pager owns the terminal.
pub(crate) fn spawn_with_terminal_guard(
    cmd: &str,
    args: &[String],
    hotkeys: HotkeyConfig,
) -> Result<(JoinHandle<i32>, mpsc::Receiver<ProxyEvent>, TerminalGuard)> {
    // Raw mode first; the state is shared with the returned TerminalGuard
    // and moved into the pump task so the original termios are restored
    // when the proxy exits, whatever the exit path.
    let state = TermState::enable_if_tty(libc::STDIN_FILENO)
        .context("failed to set stdin to raw mode")?;
    let terminal = TerminalGuard {
        state: state.clone(),
    };
    let guard = state.map(|s| RawModeGuard { state: s });
    let suspend_flag = terminal.suspend_flag();
    let gate = terminal.gate();
    let (handle, events) = spawn_pumped(
        cmd,
        args,
        hotkeys,
        std::io::stdin(),
        std::io::stdout(),
        real_term_fd(),
        guard,
        suspend_flag,
        Some(libc::STDIN_FILENO),
        gate,
    )?;
    Ok((handle, events, terminal))
}

/// Options for [`spawn_with_io`].
#[doc(hidden)]
#[derive(Debug, Clone, Default)]
pub struct SpawnIoOptions {
    /// Fd treated as "the real terminal": queried with `TIOCGWINSZ` for the
    /// child PTY's initial size and re-queried on every SIGWINCH. `None`
    /// means stdout (fd 1). If the ioctl fails, an 80x24 fallback is used.
    pub term_fd: Option<RawFd>,
    /// Optional suspension flag shared with the output pump (F2): while
    /// set, child output is buffered in a bounded ring instead of being
    /// written to `output`, and flushed on resume. The real-TTY path wires
    /// this to the [`TerminalGuard`]; tests may inject their own flag.
    pub suspend_flag: Option<Arc<AtomicBool>>,
    /// Fd polled for input readiness on the suspension-aware input path
    /// (F7): when present together with `suspend_flag`, the input pump
    /// parks while suspended and re-polls this fd under `gate` before
    /// every read instead of blocking in `read`. The real-TTY path passes
    /// stdin's fd; tests wiring a fake terminal as `input` pass its slave
    /// fd. Must describe the same underlying file as `input`.
    pub input_fd: Option<RawFd>,
    /// Gate mutex serializing the input pump's read step and the output
    /// pump's write step against [`TerminalGuard::suspend`]/[`resume`]
    /// (F7). Only ever held for bounded, non-blocking critical sections
    /// (flag re-check + zero-timeout re-poll + the read/write itself), so
    /// neither pump nor suspend/resume can starve behind it.
    ///
    /// [`resume`]: TerminalGuard::resume
    pub gate: Option<Arc<Mutex<()>>>,
}

/// Like [`crate::PtyProxy::spawn`] but with injected input/output streams and
/// no termios handling. This is the testable core: feed bytes into `input`,
/// read the child's verbatim output from the writer's peer. Must be called
/// from within a Tokio runtime.
#[doc(hidden)]
pub fn spawn_with_io<R, W>(
    cmd: &str,
    args: &[String],
    hotkeys: HotkeyConfig,
    input: R,
    output: W,
    options: SpawnIoOptions,
) -> Result<(JoinHandle<i32>, mpsc::Receiver<ProxyEvent>)>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    spawn_pumped(
        cmd,
        args,
        hotkeys,
        input,
        output,
        options.term_fd.unwrap_or(libc::STDOUT_FILENO),
        None,
        options.suspend_flag,
        options.input_fd,
        options.gate,
    )
}

#[allow(clippy::too_many_arguments)]
fn spawn_pumped<R, W>(
    cmd: &str,
    args: &[String],
    hotkeys: HotkeyConfig,
    input: R,
    output: W,
    term_fd: RawFd,
    guard: Option<RawModeGuard>,
    // Terminal-suspension plumbing (real-TTY path only): while the flag is
    // set, the input thread stops reading `input_fd` (polled with a short
    // timeout) so a pager/surface can own the terminal's input; the gate
    // mutex serializes that read step against TerminalGuard::suspend/resume.
    suspend_flag: Option<Arc<AtomicBool>>,
    input_fd: Option<RawFd>,
    gate: Option<Arc<Mutex<()>>>,
) -> Result<(JoinHandle<i32>, mpsc::Receiver<ProxyEvent>)>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let (rows, cols) = get_winsize(term_fd).unwrap_or((DEFAULT_ROWS, DEFAULT_COLS));

    // 1. Open the child PTY at the real terminal's current size.
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
        .context("failed to open pty")?;

    // 2. Spawn the child in that PTY. portable-pty makes the child a session
    //    leader with the slave as its controlling terminal.
    let mut command = CommandBuilder::new(cmd);
    for arg in args {
        command.arg(arg);
    }
    let child = pair
        .slave
        .spawn_command(command)
        .with_context(|| format!("failed to spawn {cmd:?} in pty"))?;
    // Drop the slave side: once the child exits, reads on the master return
    // EOF (Linux: EIO), which is how the pump notices termination.
    drop(pair.slave);

    // 3. Split the master into independent read/write dup'd fds (the master
    //    object itself is kept for resize ioctls).
    let reader = pair
        .master
        .try_clone_reader()
        .context("failed to clone pty reader")?;
    let writer = pair
        .master
        .take_writer()
        .context("failed to take pty writer")?;
    let child_pid = child.process_id();
    let master = Arc::new(Mutex::new(pair.master));
    // Set by the output pump on the child's first output; the input thread
    // waits for it (with a grace cap) before synthesizing the stdin-EOF
    // byte so a canonical-mode child is past its startup window.
    let child_ready = Arc::new(AtomicBool::new(false));

    let (event_tx, event_rx) = mpsc::channel(EVENT_QUEUE);

    // 4. Signal handling tasks (SIGWINCH resize, SIGINT/SIGTERM/SIGHUP
    //    forwarding).
    let sig_handles = spawn_signal_tasks(term_fd, Arc::downgrade(&master), child_pid);

    // 5. The blocking byte pump runs on the blocking thread pool.
    let handle = tokio::task::spawn_blocking(move || {
        run_pump(
            child, child_pid, master, reader, writer, input, output, hotkeys, event_tx,
            sig_handles, guard, suspend_flag, input_fd, child_ready, gate,
        )
    });

    Ok((handle, event_rx))
}

/// Spawn tokio tasks handling:
/// * SIGWINCH → re-read the real terminal size and `TIOCSWINSZ` the child PTY
///   (via portable-pty's `resize`);
/// * SIGINT / SIGTERM / SIGHUP → forward to the child's *foreground* process
///   group (`tcgetpgrp` on the master, so job control inside the child — e.g.
///   a `less` it spawned — gets the signal, exactly as with a direct
///   terminal). The proxy itself survives the signal; it exits when the child
///   does, mirroring the child's status.
///
/// Note on Ctrl-C: with our stdin in raw mode, a user's Ctrl-C arrives as the
/// byte 0x03 and reaches the child through the normal input path — identical
/// to running the CLI directly. These handlers only cover signals delivered
/// to the proxy *process* out of band.
///
/// The tasks hold only a `Weak` reference to the PTY master so they never
/// keep the child side alive after the pump is done.
fn spawn_signal_tasks(
    term_fd: RawFd,
    master: Weak<Mutex<Box<dyn MasterPty + Send>>>,
    child_pid: Option<u32>,
) -> Vec<JoinHandle<()>> {
    use tokio::signal::unix::{signal, SignalKind};

    let mut handles = Vec::new();

    if let Ok(mut winch) = signal(SignalKind::window_change()) {
        let master = master.clone();
        handles.push(tokio::spawn(async move {
            while winch.recv().await.is_some() {
                match master.upgrade() {
                    Some(master) => {
                        if let Some((rows, cols)) = get_winsize(term_fd) {
                            if let Ok(m) = master.lock() {
                                let _ = m.resize(PtySize {
                                    rows,
                                    cols,
                                    pixel_width: 0,
                                    pixel_height: 0,
                                });
                            }
                        }
                    }
                    None => break,
                }
            }
        }));
    }

    let mut forward = |kind: SignalKind, signo: libc::c_int| {
        if let Ok(mut sig) = signal(kind) {
            let master = master.clone();
            handles.push(tokio::spawn(async move {
                while sig.recv().await.is_some() {
                    // Prefer the PTY's current foreground process group;
                    // fall back to the child's own group (pgid == pid, since
                    // the child is a session leader).
                    let pgid = master
                        .upgrade()
                        .and_then(|m| m.lock().ok().and_then(|mm| mm.process_group_leader()))
                        .or_else(|| child_pid.map(|pid| pid as libc::pid_t));
                    if let Some(pgid) = pgid {
                        unsafe {
                            libc::kill(-pgid, signo);
                        }
                    }
                }
            }));
        }
    };
    forward(SignalKind::interrupt(), libc::SIGINT);
    forward(SignalKind::terminate(), libc::SIGTERM);
    forward(SignalKind::hangup(), libc::SIGHUP);

    handles
}

// ---------------------------------------------------------------------------
// the byte pump
// ---------------------------------------------------------------------------

/// Blocking pump body. Owns the raw-mode guard; returns the child's exit
/// code semantics (exit code, or `128 + signal`).
#[allow(clippy::too_many_arguments)]
fn run_pump<R, W>(
    mut child: Box<dyn Child + Send>,
    child_pid: Option<u32>,
    // Kept to hold the master fd (and thus the PTY) alive until the child
    // is reaped; also used at stdin-EOF time to read the child's current
    // termios (for the VEOF byte) via `as_raw_fd`. Resize/signal paths use
    // their own Weak reference.
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    mut reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    mut input: R,
    mut output: W,
    hotkeys: HotkeyConfig,
    events: mpsc::Sender<ProxyEvent>,
    sig_handles: Vec<JoinHandle<()>>,
    _raw_guard: Option<RawModeGuard>,
    suspend_flag: Option<Arc<AtomicBool>>,
    input_fd: Option<RawFd>,
    child_ready: Arc<AtomicBool>,
    gate: Option<Arc<Mutex<()>>>,
) -> i32
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    // --- direction 1: stdin -> child (hotkey interception), own thread ---
    let input_ready = Arc::clone(&child_ready);
    let input_master = Arc::clone(&master);
    let input_suspend = suspend_flag.clone();
    let input_gate = gate.clone();
    let input_thread = std::thread::Builder::new()
        .name("mdterm-pty-input".into())
        .spawn(move || {
            let mut writer = writer;
            let mut filter = ChordFilter::new(&hotkeys);
            let ready = input_ready;
            let master = input_master;
            let suspend_flag = input_suspend;
            let gate = input_gate;
            let mut buf = [0u8; 8192];
            let mut write_child = |bytes: &[u8]| -> bool {
                bytes.is_empty() || writer.write_all(bytes).is_ok()
            };
            // Read the next chunk from `input`, classifying the outcome so
            // the caller can break on EOF or retry after EINTR.
            enum Chunk {
                Data(usize),
                Eof,
                Again,
            }
            let read_chunk = |input: &mut R, buf: &mut [u8]| match input.read(buf) {
                Ok(0) => Chunk::Eof, // stdin EOF
                Ok(n) => Chunk::Data(n),
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => Chunk::Again,
                Err(_) => Chunk::Eof,
            };
            'pump: loop {
                let n = if let (Some(fd), Some(flag)) = (input_fd, &suspend_flag) {
                    // Real-TTY path with suspension plumbing (F7).
                    //
                    // While the terminal is suspended (pager/surface owns
                    // it), do not read stdin: the surface gets the input to
                    // itself. This is a cheap atomic check; the gate is not
                    // needed to park.
                    if flag.load(Ordering::SeqCst) {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                        continue;
                    }
                    // Wait for input WITHOUT holding the gate. (Holding it
                    // across this 100 ms poll pinned the mutex ~100% of the
                    // idle time and starved the output pump — and vice
                    // versa — because a futex-woken waiter always loses to
                    // the holder's unlock→relock fast path: the keyboard
                    // went dead / the screen froze until the proxy had to
                    // be killed. Input must flow from the moment the child
                    // spawns, before ANY child output.)
                    match poll_in(fd, 100) {
                        PollIn::Timeout => continue, // re-check the flag
                        PollIn::Gone => break 'pump, // stdin went away → EOF policy below
                        PollIn::Ready => {}
                    }
                    // F7 critical section — bounded to microseconds so it
                    // can never starve suspend()/resume() or the output
                    // pump: re-check the flag and the fd's readiness under
                    // the gate, then read only while both hold. A suspend
                    // can therefore only apply the cooked termios when no
                    // raw-mode read is in flight, and a keystroke consumed
                    // by the surface between our two polls is never stolen
                    // (readiness is re-polled with a zero timeout, so the
                    // read below cannot block).
                    let chunk = {
                        let _gate_lock = gate.as_ref().map(|g| g.lock().unwrap());
                        if flag.load(Ordering::SeqCst) {
                            Chunk::Again // suspended meanwhile; park next round
                        } else {
                            match poll_in(fd, 0) {
                                PollIn::Timeout => Chunk::Again, // surface consumed it
                                PollIn::Gone => Chunk::Eof,
                                PollIn::Ready => read_chunk(&mut input, &mut buf),
                            }
                        }
                    };
                    match chunk {
                        Chunk::Data(n) => n,
                        Chunk::Eof => break 'pump,
                        Chunk::Again => continue 'pump,
                    }
                } else {
                    // Pipe/test path: plain blocking read, no polling.
                    match read_chunk(&mut input, &mut buf) {
                        Chunk::Data(n) => n,
                        Chunk::Eof => break 'pump,
                        Chunk::Again => continue 'pump,
                    }
                };
                // Forward the chunk through the hotkey filter. The gate is
                // NOT held here: writing to the child PTY does not touch
                // the real terminal and may legitimately block on a full
                // PTY buffer.
                let mut ok = true;
                let mut out = |bytes: &[u8]| {
                    if !write_child(bytes) {
                        ok = false;
                    }
                };
                let mut ev = |e: ProxyEvent| {
                    let _ = events.try_send(e);
                };
                for &b in &buf[..n] {
                    filter.feed(b, &mut out, &mut ev);
                }
                if !ok {
                    break 'pump; // child is gone
                }
            }
            // --- stdin EOF policy (F1) ---
            // Flush any in-progress partial chord, then deliver a clean EOF
            // to the child OURSELVES: exactly one VEOF byte read from the
            // child's *current* slave termios, and nothing else.
            //
            // We deliberately do NOT rely on portable-pty's writer Drop,
            // which writes "\n" + VEOF: the injected '\n' is an Enter the
            // user never typed (visible to raw-mode TUIs), and if our stdin
            // hits EOF before the child has switched its slave to raw mode,
            // the "\n\x04" lands in canonical+echo mode — the echoed CR/LF
            // garbles the child's screen and the line discipline consumes
            // the VEOF, so the child never sees EOF and the proxy hangs.
            //
            // To avoid firing the byte into a canonical-mode startup window
            // at all, we wait for the child's first output (its readiness
            // signal) with a grace cap; a child that never prints anything
            // gets the VEOF after the cap. After writing VEOF we LEAK the
            // writer (mem::forget) so its Drop cannot append the "\n"+VEOF
            // sequence; the fd is reclaimed at process exit. The master is
            // never closed: the child may still produce output and the pump
            // below keeps forwarding it until the child exits.
            let mut out = |bytes: &[u8]| {
                let _ = write_child(bytes);
            };
            let mut ev = |e: ProxyEvent| {
                let _ = events.try_send(e);
            };
            filter.finish(&mut out, &mut ev);
            let deadline = std::time::Instant::now() + EOF_READY_GRACE;
            while !ready.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let veof = master
                .lock()
                .ok()
                .and_then(|m| m.as_raw_fd())
                .and_then(read_veof);
            if let Some(veof) = veof {
                let _ = write_child(&[veof]);
            }
            std::mem::forget(writer); // suppress portable-pty's Drop ("\n" + VEOF)
        });
    let input_thread = match input_thread {
        Ok(t) => t,
        Err(_) => {
            // Cannot pump input at all: tear down.
            for h in &sig_handles {
                h.abort();
            }
            let _ = child.kill();
            return wait_child(child, child_pid);
        }
    };

    // --- direction 2: child -> stdout ---
    // Single poll -> read -> write -> flush: escape sequences (alternate
    // screen, OSC 8, bracketed paste, mouse reporting, ...) are never
    // buffered, batched or transformed — EXCEPT while the terminal is
    // suspended (F2): then child output must not reach stdout at all (it
    // would write into the pager's alternate-screen frame), so it is
    // captured in a bounded drop-oldest ring and flushed on resume.
    //
    // The pump polls the master fd with a short timeout before every read
    // (instead of blocking in `read`) so suspension and resume are noticed
    // promptly even when the child is silent. The poll itself never holds
    // the gate mutex (polling the child PTY does not touch the real
    // terminal); the gate is taken only around the terminal-visible steps —
    // draining the backlog and the read + write of a chunk that a
    // just-completed poll proved cannot block. Holding the gate across the
    // write is what guarantees TerminalGuard::suspend, once it returns,
    // that no further child output can reach stdout (any chunk written
    // before suspend() returns lands on the real terminal BEFORE the
    // surface is spawned, which is harmless) — while never holding it
    // across an idle wait is what keeps the input pump and
    // suspend()/resume() from starving (see the gate note on TermState).
    let child_fd = master.lock().ok().and_then(|m| m.as_raw_fd());
    let mut backlog = SuspendedOutput::new();
    let mut buf = [0u8; 16384];
    'out: loop {
        let suspended = suspend_flag
            .as_ref()
            .map(|f| f.load(Ordering::SeqCst))
            .unwrap_or(false);
        if suspended {
            // Capture phase. No gate needed: nothing here touches the real
            // terminal — output goes only into the backlog.
            match child_fd {
                Some(fd) => match poll_in(fd, 50) {
                    PollIn::Timeout => continue, // still suspended; re-check the flag
                    PollIn::Gone => break,
                    PollIn::Ready => {}
                },
                // No fd to poll (should not happen on unix): avoid a
                // blocking read so resume is still noticed.
                None => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    continue;
                }
            }
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    child_ready.store(true, Ordering::SeqCst);
                    backlog.push(&buf[..n]);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break, // EIO: child's side is gone
            }
            continue 'out;
        }
        // Not suspended: flush anything captured during suspension BEFORE
        // forwarding new output (ordering). This runs even when the child
        // is silent, so a resume drains promptly; the flag is re-checked
        // under the gate so a concurrent suspend cannot interleave with
        // the write.
        if !backlog.is_empty() {
            let _gate_lock = match (&gate, &suspend_flag) {
                (Some(g), Some(_)) => Some(g.lock().unwrap()),
                _ => None,
            };
            let still_clear = suspend_flag
                .as_ref()
                .map(|f| !f.load(Ordering::SeqCst))
                .unwrap_or(true);
            if still_clear && backlog.drain(&mut output).is_err() {
                break;
            }
        }
        // Wait for child output WITHOUT holding the gate.
        if let Some(fd) = child_fd {
            match poll_in(fd, 100) {
                PollIn::Timeout => continue, // idle; re-check the suspension flag
                PollIn::Gone => break,       // read would report EIO/EOF
                PollIn::Ready => {}
            }
        }
        // Terminal-visible step: take the gate (bounded — the poll above
        // proved the read cannot block) and re-check the flag under the
        // lock: if a suspend landed between our poll and the lock, capture
        // the chunk instead of writing it into the surface's frame.
        let _gate_lock = match (&gate, &suspend_flag) {
            (Some(g), Some(_)) => Some(g.lock().unwrap()),
            _ => None,
        };
        let suspended_now = suspend_flag
            .as_ref()
            .map(|f| f.load(Ordering::SeqCst))
            .unwrap_or(false);
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                // First child output: the child is up (past its canonical-mode
                // startup window); releases the input thread's EOF grace wait.
                child_ready.store(true, Ordering::SeqCst);
                if suspended_now {
                    backlog.push(&buf[..n]);
                    continue 'out;
                }
                if output.write_all(&buf[..n]).is_err() {
                    break;
                }
                let _ = output.flush();
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => {
                // Linux returns EIO once the child's side of the PTY is gone;
                // treat every read error as end-of-output.
                break;
            }
        }
    }
    // Child exited (possibly while suspended): deliver whatever was captured.
    let _ = backlog.drain(&mut output);

    // Child is done (or its output is). Stop signal handling and reap.
    for h in &sig_handles {
        h.abort();
    }
    // NOTE: the input thread may still be blocked reading stdin; it owns no
    // terminal state and exits on its own once stdin closes or errors.
    // `_raw_guard` drops at the end of this function, restoring the original
    // termios on every path.
    let _ = input_thread;
    wait_child(child, child_pid)
}

#[cfg(test)]
mod suspended_output_tests {
    use super::SuspendedOutput;

    #[test]
    fn buffers_and_drains_in_order() {
        let mut b = SuspendedOutput::new();
        b.push(b"hello ");
        b.push(b"world");
        assert!(!b.is_empty());
        let mut out = Vec::new();
        b.drain(&mut out).unwrap();
        assert_eq!(out, b"hello world");
        assert!(b.is_empty());
    }

    #[test]
    fn drop_oldest_past_cap_with_marker() {
        let mut b = SuspendedOutput::new();
        // 1.5x the cap: the oldest half must be dropped.
        let payload = vec![b'x'; SuspendedOutput::CAP + SuspendedOutput::CAP / 2];
        b.push(&payload);
        assert_eq!(b.buf.len(), SuspendedOutput::CAP);
        assert_eq!(b.dropped, SuspendedOutput::CAP / 2);
        let mut out = Vec::new();
        b.drain(&mut out).unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains(&format!(
                "{} bytes of child output dropped",
                SuspendedOutput::CAP / 2
            )),
            "drop marker missing: {:.100}",
            text
        );
        assert_eq!(out.len(), SuspendedOutput::CAP + text.find('x').unwrap());
    }

    #[test]
    fn marker_only_when_dropped() {
        let mut b = SuspendedOutput::new();
        b.push(b"tiny");
        let mut out = Vec::new();
        b.drain(&mut out).unwrap();
        assert_eq!(out, b"tiny", "no marker when nothing was dropped");
    }
}

#[cfg(test)]
mod terminal_guard_tests {
    use super::{TermState, TerminalGuard};
    use std::os::unix::io::RawFd;
    use std::sync::Arc;

    #[test]
    fn inert_guard_is_noop() {
        let g = TerminalGuard::none();
        assert!(!g.is_terminal());
        assert!(!g.is_suspended());
        g.suspend();
        assert!(!g.is_suspended());
        g.resume();
        assert!(!g.is_suspended());
    }

    /// Open a real PTY and return the slave fd (a TTY usable for termios).
    fn open_pty_slave() -> RawFd {
        unsafe {
            let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            assert!(master >= 0, "posix_openpt failed");
            assert_eq!(libc::grantpt(master), 0, "grantpt failed");
            assert_eq!(libc::unlockpt(master), 0, "unlockpt failed");
            let name = libc::ptsname(master);
            assert!(!name.is_null(), "ptsname failed");
            let slave = libc::open(name, libc::O_RDWR | libc::O_NOCTTY);
            assert!(slave >= 0, "open slave failed");
            slave
        }
    }

    fn current_termios(fd: RawFd) -> libc::termios {
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::tcgetattr(fd, &mut t) }, 0);
        t
    }

    /// F7(a): suspend must block until the input pump's read step (which
    /// holds the gate mutex across poll/read) has parked — only then may
    /// the cooked termios be applied.
    #[test]
    fn suspend_waits_for_in_flight_read_step() {
        let slave = open_pty_slave();
        let state = TermState::enable_if_tty(slave).unwrap().unwrap();
        let guard = TerminalGuard { state: Some(Arc::clone(&state)) };

        // Simulate the input pump inside its poll/read critical section.
        let lock = state.gate.lock().unwrap();

        let g2 = guard.clone();
        let t = std::thread::spawn(move || g2.suspend());
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(
            !guard.is_suspended(),
            "suspend must block while a read is in flight"
        );
        drop(lock); // reader parked
        t.join().unwrap();
        assert!(guard.is_suspended());
        let now = current_termios(slave);
        assert_eq!(
            now.c_lflag, state.saved.c_lflag,
            "cooked termios applied once the reader is parked"
        );
        unsafe { libc::close(slave) };
    }

    /// F7(b): resume re-applies raw mode BEFORE the suspension flag clears,
    /// and (still F7(a)) blocks behind an in-flight read step.
    #[test]
    fn resume_applies_raw_before_clearing_flag() {
        let slave = open_pty_slave();
        let state = TermState::enable_if_tty(slave).unwrap().unwrap();
        let guard = TerminalGuard { state: Some(Arc::clone(&state)) };

        guard.suspend();
        assert!(guard.is_suspended());

        // Hold the gate: resume must wait, flag must stay set.
        let lock = state.gate.lock().unwrap();
        let g2 = guard.clone();
        let t = std::thread::spawn(move || g2.resume());
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(guard.is_suspended(), "resume must block behind the gate");
        drop(lock);
        t.join().unwrap();

        // After resume returns, raw mode is in effect and the flag is clear.
        assert!(!guard.is_suspended());
        let now = current_termios(slave);
        assert_eq!(now.c_lflag, state.raw.c_lflag, "raw termios restored");
        unsafe { libc::close(slave) };
    }
}

/// Reap the child and return shell-style exit semantics: the exit code, or
/// `128 + signal` when the child was killed by a signal.
///
/// We `waitpid` ourselves instead of using `Child::wait` because
/// portable-pty's `ExitStatus` does not expose the terminating signal
/// number, which `128 + signal` requires.
fn wait_child(mut child: Box<dyn Child + Send>, pid: Option<u32>) -> i32 {
    if let Some(pid) = pid {
        let pid = pid as libc::pid_t;
        let mut status: libc::c_int = 0;
        loop {
            let rc = unsafe { libc::waitpid(pid, &mut status, 0) };
            if rc == pid {
                if libc::WIFEXITED(status) {
                    return libc::WEXITSTATUS(status);
                }
                if libc::WIFSIGNALED(status) {
                    return 128 + libc::WTERMSIG(status);
                }
                continue;
            }
            if rc < 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                break; // e.g. ECHILD: fall back to portable-pty's wait
            }
        }
    }
    match child.wait() {
        Ok(status) => status.exit_code() as i32,
        Err(_) => -1,
    }
}
