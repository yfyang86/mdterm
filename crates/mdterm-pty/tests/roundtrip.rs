//! Byte round-trip test: with hotkeys disabled, every byte fed into the
//! proxy must come back bit-identical — including raw escape sequences and
//! control bytes. The child is the `ptyecho` helper binary, which sets its
//! PTY slave to raw mode (like a real TUI app) and echoes bytes verbatim; a
//! plain `/bin/cat` would run in canonical mode where the line discipline
//! itself echoes/erases/translates bytes, which is not what we are testing.

#![cfg(unix)]

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::io::FromRawFd;
use std::time::Duration;

use mdterm_pty::{spawn_with_io, HotkeyConfig, SpawnIoOptions};

fn pipe() -> (File, File) {
    let mut fds = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe failed");
    unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
}

/// Read exactly `n` bytes from `f` within `dur`, using a helper thread.
fn read_exact_timeout(f: &File, n: usize, dur: Duration) -> Vec<u8> {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut f = f.try_clone().unwrap();
    std::thread::spawn(move || {
        let mut out = vec![0u8; n];
        let res = f.read_exact(&mut out);
        let _ = tx.send(res.map(|_| out));
    });
    rx.recv_timeout(dur)
        .unwrap_or_else(|_| panic!("timed out reading proxy output (want {n} bytes)"))
        .expect("read failed")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn byte_roundtrip_including_escape_sequences() {
    let (proxy_in_read, mut proxy_in_write) = pipe();
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

    // Wait until the child is in raw mode before sending anything.
    let marker = read_exact_timeout(&proxy_out_read, b"PTYECHO_READY".len(), Duration::from_secs(10));
    assert_eq!(marker, b"PTYECHO_READY");

    // Alternate screen, SGR color, reset, NUL, SOH, DEL, CR/LF: all must
    // survive the round trip bit-identical.
    let payload: &[u8] = b"\x1b[?1049h\x1b[31mhi\x1b[0m\x00\x01\x7fplain\r\n";
    proxy_in_write.write_all(payload).unwrap();
    proxy_in_write.flush().unwrap();

    let echoed = read_exact_timeout(&proxy_out_read, payload.len(), Duration::from_secs(10));
    assert_eq!(echoed, payload, "bytes must round-trip verbatim");

    // Closing our stdin makes the proxy deliver a single VEOF byte (the
    // child's current Ctrl-D, no synthesized newline): ptyecho exits 0.
    // (Regression: portable-pty's writer Drop used to inject "\n" + VEOF —
    // an Enter the user never typed — see tests/stdin_eof.rs.)
    drop(proxy_in_write);
    let code = tokio::time::timeout(Duration::from_secs(10), join)
        .await
        .expect("proxy did not shut down after stdin EOF")
        .expect("pump task panicked");
    assert_eq!(code, 0, "cat must exit 0 on EOF");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_exit_code_is_returned() {
    let (proxy_in_read, _proxy_in_write) = pipe();
    let (proxy_out_read, proxy_out_write) = pipe();
    drop(proxy_out_read);

    let (join, _events) = spawn_with_io(
        "sh",
        &["-c".to_string(), "exit 42".to_string()],
        HotkeyConfig::default(),
        proxy_in_read,
        proxy_out_write,
        SpawnIoOptions::default(),
    )
    .expect("spawn_with_io failed");

    let code = tokio::time::timeout(Duration::from_secs(10), join)
        .await
        .expect("proxy did not return after child exit")
        .expect("pump task panicked");
    assert_eq!(code, 42, "child exit code must be propagated");
}
