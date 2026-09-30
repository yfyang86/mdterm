//! mdterm-pty — byte-transparent PTY proxy with hotkey interception.
//!
//! Implements the `mdterm-pty` section of SPEC.md: a child CLI runs inside a
//! pseudo-terminal; every byte the child writes is forwarded to our stdout
//! verbatim and every stdin byte is forwarded to the child, except configured
//! hotkey chords which are consumed and reported as [`ProxyEvent`]s.
//!
//! # Architecture (why tests can run headless)
//!
//! The byte pump is generic over [`std::io::Read`]/[`std::io::Write`]
//! streams: [`spawn_with_io`] injects any input/output pair (pipes in tests),
//! while [`PtyProxy::spawn`] is a thin layer that adds real-TTY handling —
//! raw mode on our stdin (RAII-restored on every exit path) and the real
//! terminal's size for the initial PTY and SIGWINCH propagation. There may
//! be no controlling TTY under `cargo test`, so all integration tests drive
//! [`spawn_with_io`]; a small helper binary (`src/bin/ptyecho.rs`) plays the
//! role of a real TUI child (sets its slave side raw, echoes bytes, exits on
//! Ctrl-D).
//!
//! # Hotkey state machine and flush policy
//!
//! Input bytes are matched against the configured chords with a trie (see
//! `hotkey.rs`). Bytes that form a *potential* chord prefix are withheld —
//! at most `max(chord_len) - 1` bytes — and are resolved deterministically:
//!
//! * the chord completes → the bytes are consumed, a [`ProxyEvent`] is
//!   emitted, nothing reaches the child;
//! * a non-matching byte arrives → the withheld prefix is flushed to the
//!   child verbatim **before** the new byte is re-examined (so overlapping
//!   chords like the shared Ctrl-G prefix can never swallow input);
//! * stdin reaches EOF with a prefix pending → it is flushed to the child.
//!
//! Deliberate choice (SPEC allows either): there is **no idle timeout** — a
//! dangling prefix (e.g. a bare Ctrl-G) is only released by the next byte or
//! by EOF, never by a clock. Rationale: byte transparency must be
//! deterministic; a timer would make delivery timing-dependent and could
//! reorder a chord split across the timeout boundary. The trade-off is that
//! a bare prefix key is delivered one keystroke "late", which matches how
//! e.g. tmux prefix keys behave.
//!
//! # stdin EOF semantics
//!
//! When *our* stdin hits EOF (e.g. a pipe closes), any pending chord prefix
//! is flushed and the proxy delivers EOF to the child itself: after waiting
//! for the child's first output (readiness) with a short grace cap, it
//! writes exactly one VEOF byte read from the child's *current* slave
//! termios — and nothing else. We deliberately do NOT rely on portable-pty's
//! writer Drop (`"\n" + VEOF`): the synthesized `\n` is an Enter the user
//! never typed for raw-mode TUIs, and if EOF arrives before the child has
//! switched its slave to raw mode, the `"\n\x04"` lands in canonical+echo
//! mode (echoed CR/LF garbles the screen, the VEOF is consumed by the line
//! discipline, the child never sees EOF → hang). The master stays open and
//! child output keeps flowing until the child exits; the proxy never kills
//! the child just because stdin ended.
//!
//! # Exit status semantics
//!
//! The returned `JoinHandle<i32>` resolves to the child's exit code, or
//! `128 + signal` when the child died on a signal (shell convention), reaped
//! via `waitpid` directly because portable-pty's `ExitStatus` loses the
//! signal number.

#[cfg(unix)]
mod hotkey;
#[cfg(unix)]
mod proxy;

#[cfg(unix)]
pub use proxy::{spawn_with_io, SpawnIoOptions, TerminalGuard};

/// Hotkey chord configuration: chords are raw byte sequences matched against
/// the stdin stream, e.g. `[0x07, b'r']` for Ctrl-G then `r`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotkeyConfig {
    /// Chord that triggers a terminal (pager) render.
    pub render_terminal: Vec<u8>,
    /// Chord that triggers a browser render.
    pub render_browser: Vec<u8>,
    /// Master switch: when `false`, every input byte passes through untouched.
    pub enabled: bool,
}

impl Default for HotkeyConfig {
    /// Ctrl-G r / Ctrl-G b, enabled.
    fn default() -> Self {
        HotkeyConfig {
            render_terminal: vec![0x07, b'r'],
            render_browser: vec![0x07, b'b'],
            enabled: true,
        }
    }
}

/// Emitted on the proxy's event channel when a hotkey chord is fully matched.
/// The chord bytes are consumed and never reach the child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyEvent {
    RenderTerminal,
    RenderBrowser,
}

/// The PTY proxy handle. Use [`PtyProxy::spawn`].
pub struct PtyProxy;

impl PtyProxy {
    /// Spawn `cmd args` in a new PTY sized to the real terminal; put our
    /// stdin in raw mode (skipped gracefully when stdin is not a TTY); pump
    /// bytes both ways; intercept hotkey chords and send [`ProxyEvent`]s on
    /// the returned receiver instead of forwarding them.
    ///
    /// The returned `JoinHandle<i32>` resolves when the child exits, yielding
    /// the child's exit code (`128 + signal` when the child dies on a
    /// signal). Terminal state is restored on ALL exit paths (RAII guard).
    ///
    /// Must be called from within a Tokio runtime.
    #[cfg(unix)]
    pub fn spawn(
        cmd: &str,
        args: &[String],
        hotkeys: HotkeyConfig,
    ) -> anyhow::Result<(
        tokio::task::JoinHandle<i32>,
        tokio::sync::mpsc::Receiver<ProxyEvent>,
    )> {
        proxy::spawn(cmd, args, hotkeys)
    }

    /// Like [`PtyProxy::spawn`] but additionally returns a
    /// [`TerminalGuard`] handle. The guard lets the caller temporarily
    /// suspend the proxy's raw mode (restoring the cooked termios and
    /// pausing the stdin pump) while an interactive surface — e.g. a pager
    /// rendering markdown — owns the real terminal, then resume afterwards.
    /// Additive in Sprint 2; [`PtyProxy::spawn`] is unchanged.
    #[cfg(unix)]
    pub fn spawn_with_terminal_guard(
        cmd: &str,
        args: &[String],
        hotkeys: HotkeyConfig,
    ) -> anyhow::Result<(
        tokio::task::JoinHandle<i32>,
        tokio::sync::mpsc::Receiver<ProxyEvent>,
        TerminalGuard,
    )> {
        proxy::spawn_with_terminal_guard(cmd, args, hotkeys)
    }
}
