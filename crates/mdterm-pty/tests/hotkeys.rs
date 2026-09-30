//! Hotkey interception test: full chords produce ProxyEvents and never reach
//! the child; partial chords followed by a mismatch are flushed to the child
//! verbatim.

#![cfg(unix)]

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::io::FromRawFd;
use std::time::Duration;

use mdterm_pty::{spawn_with_io, HotkeyConfig, ProxyEvent, SpawnIoOptions};

fn pipe() -> (File, File) {
    let mut fds = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe failed");
    unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
}

fn read_exact_timeout(f: &File, n: usize, dur: Duration) -> Vec<u8> {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut f = f.try_clone().unwrap();
    std::thread::spawn(move || {
        let mut out = vec![0u8; n];
        let res = f.read_exact(&mut out);
        let _ = tx.send(res.map(|_| out));
    });
    rx.recv_timeout(dur)
        .expect("timed out reading proxy output")
        .expect("read failed")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chords_emit_events_and_partials_pass_through() {
    let (proxy_in_read, mut w) = pipe();
    let (proxy_out_read, proxy_out_write) = pipe();

    let (join, mut events) = spawn_with_io(
        env!("CARGO_BIN_EXE_ptyecho"),
        &[],
        HotkeyConfig::default(), // enabled: Ctrl-G r / Ctrl-G b
        proxy_in_read,
        proxy_out_write,
        SpawnIoOptions::default(),
    )
    .expect("spawn_with_io failed");

    // Wait until the child is in raw mode before sending anything.
    let marker = read_exact_timeout(&proxy_out_read, b"PTYECHO_READY".len(), Duration::from_secs(10));
    assert_eq!(marker, b"PTYECHO_READY");

    // 1. Full chord Ctrl-G r: event fires, chord bytes never reach the child.
    w.write_all(&[0x07, b'r']).unwrap();
    w.flush().unwrap();
    let ev = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("no ProxyEvent received")
        .expect("event channel closed");
    assert_eq!(ev, ProxyEvent::RenderTerminal);

    // Only subsequent non-chord bytes are echoed by the child.
    w.write_all(b"XY").unwrap();
    w.flush().unwrap();
    let echoed = read_exact_timeout(&proxy_out_read, 2, Duration::from_secs(5));
    assert_eq!(echoed, b"XY", "chord bytes must not reach the child");

    // 2. Partial chord (Ctrl-G) followed by a mismatch: the prefix byte DOES
    // reach the child, before the mismatch byte.
    w.write_all(&[0x07, b'z']).unwrap();
    w.flush().unwrap();
    let echoed = read_exact_timeout(&proxy_out_read, 2, Duration::from_secs(5));
    assert_eq!(echoed, vec![0x07, b'z'], "partial chord must be flushed verbatim");

    // 3. Second chord: Ctrl-G b -> RenderBrowser.
    w.write_all(&[0x07, b'b']).unwrap();
    w.flush().unwrap();
    let ev = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("no ProxyEvent received")
        .expect("event channel closed");
    assert_eq!(ev, ProxyEvent::RenderBrowser);

    // 4. A dangling partial chord is flushed to the child on stdin EOF.
    w.write_all(&[0x07]).unwrap();
    w.flush().unwrap();
    drop(w); // stdin EOF
    let echoed = read_exact_timeout(&proxy_out_read, 1, Duration::from_secs(5));
    assert_eq!(echoed, vec![0x07], "pending partial chord must flush on EOF");

    let code = tokio::time::timeout(Duration::from_secs(10), join)
        .await
        .expect("proxy did not shut down after stdin EOF")
        .expect("pump task panicked");
    assert_eq!(code, 0);
}
