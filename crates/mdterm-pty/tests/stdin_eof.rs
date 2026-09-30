//! Regression tests for the stdin-EOF policy (F1).
//!
//! Historical bug: on stdin EOF the proxy dropped portable-pty's PTY writer,
//! whose Drop writes `"\n" + VEOF` to the master. If stdin hit EOF before a
//! raw-mode child had initialized, that `"\n\x04"` landed while the slave was
//! still in canonical+echo mode: the echoed CR/LF garbled the child's screen,
//! the line discipline consumed the VEOF, the child never saw EOF and the
//! proxy hung forever. For raw-mode TUIs the injected `\n` was also an Enter
//! the user never typed.
//!
//! Fixed policy: on stdin EOF the proxy waits for the child's first output
//! (its readiness signal) with a small grace cap, then writes ONLY the VEOF
//! byte read from the child's current termios — never a synthesized `\n` —
//! and suppresses the writer's Drop.

#![cfg(unix)]

use std::fs::File;
use std::io::Read;
use std::os::unix::io::FromRawFd;
use std::time::Duration;

use mdterm_pty::{spawn_with_io, HotkeyConfig, SpawnIoOptions};

fn pipe() -> (File, File) {
    let mut fds = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe failed");
    unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
}

/// Read `f` to EOF within `dur`, using a helper thread.
fn read_to_end_timeout(f: &File, dur: Duration) -> Vec<u8> {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut f = f.try_clone().unwrap();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let res = f.read_to_end(&mut out);
        let _ = tx.send(res.map(|_| out));
    });
    rx.recv_timeout(dur)
        .unwrap_or_else(|_| panic!("timed out reading proxy output"))
        .expect("read failed")
}

/// stdin EOF *before* the child's readiness marker: the child must still
/// terminate promptly, and the raw-mode child must receive exactly one VEOF
/// byte — no stray `\n` (an Enter the user never typed).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdin_eof_before_child_ready_terminates_without_stray_newline() {
    let (proxy_in_read, proxy_in_write) = pipe();
    let (proxy_out_read, proxy_out_write) = pipe();

    let mut hotkeys = HotkeyConfig::default();
    hotkeys.enabled = false;

    let (join, _events) = spawn_with_io(
        env!("CARGO_BIN_EXE_ptyecho"),
        &[],
        hotkeys,
        proxy_in_read,
        proxy_out_write,
        SpawnIoOptions::default(),
    )
    .expect("spawn_with_io failed");

    // Close stdin IMMEDIATELY: the EOF path races the child's startup. The
    // proxy must wait for the readiness marker internally and then deliver
    // VEOF only.
    drop(proxy_in_write);

    let code = tokio::time::timeout(Duration::from_secs(10), join)
        .await
        .expect("proxy hung after stdin EOF (child never saw VEOF)")
        .expect("pump task panicked");
    assert_eq!(code, 0, "ptyecho must exit 0 on VEOF");

    // Everything the child emitted: readiness marker + verbatim echo of the
    // single VEOF byte. A "\n" anywhere (e.g. "\r\n" from canonical echo, or
    // the old writer-Drop "\n\x04") fails this assertion.
    let out = read_to_end_timeout(&proxy_out_read, Duration::from_secs(5));
    assert_eq!(
        out,
        b"PTYECHO_READY\x04",
        "raw-mode child must receive only VEOF, no synthesized newline"
    );
}

/// A canonical-mode child (plain `cat`, never touches termios) that produces
/// no output of its own must still get a clean EOF after the grace cap and
/// terminate — without any echoed newline garbage on the output stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdin_eof_terminates_canonical_child_cleanly() {
    let (proxy_in_read, proxy_in_write) = pipe();
    let (proxy_out_read, proxy_out_write) = pipe();

    let (join, _events) = spawn_with_io(
        "cat",
        &[],
        HotkeyConfig::default(),
        proxy_in_read,
        proxy_out_write,
        SpawnIoOptions::default(),
    )
    .expect("spawn_with_io failed");

    drop(proxy_in_write); // immediate stdin EOF, child still canonical

    let code = tokio::time::timeout(Duration::from_secs(10), join)
        .await
        .expect("proxy hung: canonical child never saw EOF")
        .expect("pump task panicked");
    assert_eq!(code, 0, "cat must exit 0 on EOF");

    let out = read_to_end_timeout(&proxy_out_read, Duration::from_secs(5));
    assert!(
        !out.contains(&b'\n') && !out.contains(&b'\r'),
        "no echoed newline garbage may reach the screen: {out:?}"
    );
}
