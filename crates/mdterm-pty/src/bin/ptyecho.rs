//! Test helper child for mdterm-pty's integration tests (not part of the
//! library API; only built because tests need a real child process).
//!
//! It mimics what a real TUI child (Claude Code, Kimi CLI) does:
//!
//! * puts its own stdin — the slave side of the proxy's PTY — into raw mode,
//!   so the PTY line discipline does not echo, buffer or transform any byte
//!   (this is what makes bit-exact round-trip assertions meaningful: a plain
//!   `/bin/cat` in a fresh PTY runs in canonical mode and would echo/erase/
//!   CR-translate input);
//! * echoes every byte it reads back verbatim;
//! * exits with status 0 when it reads VEOF (0x04), the byte a real terminal
//!   sends for Ctrl-D — the proxy injects it when *its* stdin reaches EOF
//!   (via `portable-pty`'s writer-drop EOF sequence).

use std::io::{Read, Write};

const VEOF: u8 = 0x04;

fn main() {
    // Raw mode on the slave side, exactly like a real curses/TUI app would.
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(libc::STDIN_FILENO, &mut t) == 0 {
            libc::cfmakeraw(&mut t);
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &t);
        }
    }

    let mut stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    // Tell the driver we are in raw mode and listening; until this marker is
    // seen, bytes sent to us could still hit the PTY's default canonical
    // mode (kernel-side echo/input buffering), which would corrupt
    // byte-exact assertions.
    let _ = out.write_all(b"PTYECHO_READY");
    let _ = out.flush();

    let mut buf = [0u8; 4096];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if out.write_all(&buf[..n]).is_err() {
                    break;
                }
                let _ = out.flush();
                if buf[..n].contains(&VEOF) {
                    break; // Ctrl-D: exit like a shell would.
                }
            }
            Err(_) => break,
        }
    }
}
