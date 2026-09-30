//! Resize test: the child PTY is created at the "real terminal" size, and a
//! SIGWINCH delivered to the proxy propagates a `TIOCSWINSZ` with the real
//! terminal's new size to the child. The "real terminal" is faked with
//! another PTY whose master fd is injected via `SpawnIoOptions::term_fd`.

#![cfg(unix)]

use std::fs::File;
use std::io::Read;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::time::Duration;

use mdterm_pty::{spawn_with_io, HotkeyConfig, SpawnIoOptions};

/// Create a fake "real terminal" PTY at the given size; returns
/// (master File, slave File).
fn open_fake_terminal(rows: u16, cols: u16) -> (File, File) {
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &ws,
        )
    };
    assert_eq!(rc, 0, "openpty failed");
    unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) }
}

fn set_winsize(fd: RawFd, rows: u16, cols: u16) {
    let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    assert_eq!(unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) }, 0, "TIOCSWINSZ failed");
}

/// Read from `f` until `needle` appears in the accumulated output.
fn read_until(f: &File, needle: &[u8], dur: Duration) -> Vec<u8> {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut f = f.try_clone().unwrap();
    let needle = needle.to_vec();
    let needle_msg = needle.clone();
    std::thread::spawn(move || {
        let mut acc = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match f.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    acc.extend_from_slice(&buf[..n]);
                    if acc.windows(needle.len()).any(|w| *w == needle[..]) {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send(acc);
    });
    rx.recv_timeout(dur)
        .unwrap_or_else(|_| panic!("timed out waiting for {:?}", String::from_utf8_lossy(&needle_msg)))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn initial_size_and_sigwinch_propagate_to_child() {
    let (term_master, _term_slave) = open_fake_terminal(24, 80);

    let (proxy_in_read, _proxy_in_write) = {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
    };
    let (proxy_out_read, proxy_out_write) = {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
    };

    // Child prints "rows cols" (stty size), sleeps while we resize, prints again.
    let (join, _events) = spawn_with_io(
        "sh",
        &[
            "-c".to_string(),
            "stty size; sleep 2; stty size".to_string(),
        ],
        HotkeyConfig::default(),
        proxy_in_read,
        proxy_out_write,
        SpawnIoOptions {
            term_fd: Some(term_master.as_raw_fd()),
            suspend_flag: None,
        },
    )
    .expect("spawn_with_io failed");

    // 1. The child PTY must have been created at the real terminal's size.
    let out = read_until(&proxy_out_read, b"24 80", Duration::from_secs(10));
    assert!(
        out.windows(5).any(|w| w == b"24 80"),
        "child did not observe initial size 24x80, got: {:?}",
        String::from_utf8_lossy(&out)
    );

    // 2. Resize the "real terminal" and deliver SIGWINCH to the proxy.
    set_winsize(term_master.as_raw_fd(), 40, 120);
    assert_eq!(unsafe { libc::raise(libc::SIGWINCH) }, 0);

    // 3. The child must observe the new size.
    let out = read_until(&proxy_out_read, b"40 120", Duration::from_secs(10));
    assert!(
        out.windows(6).any(|w| w == b"40 120"),
        "child did not observe resized 40x120, got: {:?}",
        String::from_utf8_lossy(&out)
    );

    let code = tokio::time::timeout(Duration::from_secs(10), join)
        .await
        .expect("proxy did not return after child exit")
        .expect("pump task panicked");
    assert_eq!(code, 0);
}
