//! Regression test for F2: while the terminal is suspended (a pager/surface
//! owns the real screen, e.g. after Ctrl-G r spawns `less -R`), child output
//! must NOT be written to stdout — it would land inside the surface's
//! alternate-screen frame. The output pump buffers it in a bounded
//! drop-oldest ring and flushes it to stdout on resume.

#![cfg(unix)]

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::io::FromRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use mdterm_pty::{spawn_with_io, HotkeyConfig, SpawnIoOptions};

fn pipe() -> (File, File) {
    let mut fds = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe failed");
    unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
}

/// Read exactly `n` bytes from `f` within `dur`; `None` on timeout.
fn try_read_exact(f: &File, n: usize, dur: Duration) -> Option<Vec<u8>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut f = f.try_clone().unwrap();
    std::thread::spawn(move || {
        let mut out = vec![0u8; n];
        let res = f.read_exact(&mut out);
        let _ = tx.send(res.map(|_| out));
    });
    rx.recv_timeout(dur).ok().and_then(|r| r.ok())
}

/// Assert no bytes become readable on `f` within `dur` — WITHOUT consuming
/// anything (a timed-out read thread would steal bytes arriving later).
fn assert_silent(f: &File, dur: Duration) {
    use std::os::unix::io::AsRawFd;
    let mut pfd = libc::pollfd {
        fd: f.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let rc = unsafe { libc::poll(&mut pfd, 1, dur.as_millis() as libc::c_int) };
    assert_eq!(rc, 0, "child output leaked into the surface's screen while suspended");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_output_is_buffered_while_suspended_and_flushed_on_resume() {
    let (proxy_in_read, mut proxy_in_write) = pipe();
    let (proxy_out_read, proxy_out_write) = pipe();
    let flag = Arc::new(AtomicBool::new(false));

    let mut hotkeys = HotkeyConfig::default();
    hotkeys.enabled = false;

    let (join, _events) = spawn_with_io(
        env!("CARGO_BIN_EXE_ptyecho"),
        &[],
        hotkeys,
        proxy_in_read,
        proxy_out_write,
        SpawnIoOptions {
            term_fd: None,
            suspend_flag: Some(Arc::clone(&flag)),
            ..Default::default()
        },
    )
    .expect("spawn_with_io failed");

    let marker = try_read_exact(&proxy_out_read, b"PTYECHO_READY".len(), Duration::from_secs(10))
        .expect("no readiness marker");
    assert_eq!(marker, b"PTYECHO_READY");

    // Suspend (as pager_render does before spawning less), then have the
    // child produce output. Give the pump one poll cycle to observe the
    // flag (the real-TTY path serializes this hard via the gate mutex;
    // the injected-flag path relies on the pump's 100ms poll).
    flag.store(true, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(300));
    proxy_in_write.write_all(b"while-pager-open").unwrap();
    proxy_in_write.flush().unwrap();

    // The echo must NOT reach stdout while suspended.
    assert_silent(&proxy_out_read, Duration::from_millis(500));

    // Resume: the buffered echo is flushed, then the pump is verbatim again.
    flag.store(false, Ordering::SeqCst);
    let flushed = try_read_exact(&proxy_out_read, b"while-pager-open".len(), Duration::from_secs(5))
        .expect("buffered child output was not flushed on resume");
    assert_eq!(flushed, b"while-pager-open");

    proxy_in_write.write_all(b"after-resume").unwrap();
    proxy_in_write.flush().unwrap();
    let live = try_read_exact(&proxy_out_read, b"after-resume".len(), Duration::from_secs(5))
        .expect("pump did not resume verbatim forwarding");
    assert_eq!(live, b"after-resume");

    // Clean shutdown via stdin EOF (single VEOF, see F1).
    drop(proxy_in_write);
    let code = tokio::time::timeout(Duration::from_secs(10), join)
        .await
        .expect("proxy did not shut down")
        .expect("pump task panicked");
    assert_eq!(code, 0);
}
