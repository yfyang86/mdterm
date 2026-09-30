//! End-to-end regression test for the dead-keyboard bug in `mdterm wrap`:
//! drive the real `mdterm` binary with a nested PTY as its "real terminal"
//! (there may be no controlling TTY under cargo test), type at it, and
//! assert the child receives the keystrokes from the moment the session
//! starts. A `script(1)`-based variant reproduces the original reported
//! failure verbatim (Linux only; skipped when script(1) is unavailable).

#![cfg(unix)]

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Open a PTY pair standing in for the user's real terminal.
/// (`*mut`/`&mut` coerce to libc's `*const` glibc signature, so this
/// compiles on BSD/macOS and Linux alike.)
fn open_terminal() -> (File, File) {
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, 0, "openpty failed");
    unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) }
}

/// Accumulate everything readable from `f` into a shared slot; the reader
/// thread exits on EOF/EIO (child side closed).
fn collector(f: &File) -> std::sync::Arc<std::sync::Mutex<Vec<u8>>> {
    let acc = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let acc2 = std::sync::Arc::clone(&acc);
    let mut f = f.try_clone().unwrap();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match f.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => acc2.lock().unwrap().extend_from_slice(&buf[..n]),
                Err(_) => break, // EIO: the session ended
            }
        }
    });
    acc
}

fn count_occurrences(hay: &[u8], needle: &[u8]) -> usize {
    hay.windows(needle.len()).filter(|w| *w == needle).count()
}

/// Wait for `cond` up to `dur`, polling every 10 ms.
fn wait_until(dur: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + dur;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    cond()
}

/// Reap `child` within `dur`; kill it and panic on timeout.
fn wait_with_timeout(child: &mut Child, dur: Duration) -> std::process::ExitStatus {
    if wait_until(dur, || child.try_wait().unwrap().is_some()) {
        child.try_wait().unwrap().unwrap()
    } else {
        let _ = child.kill();
        let _ = child.wait();
        panic!("process did not exit within {dur:?}");
    }
}

#[test]
fn wrap_forwards_keyboard_input_from_session_start() {
    let (mut master, slave) = open_terminal();
    let slave_fd = slave.as_raw_fd();
    let stdin = unsafe { Stdio::from_raw_fd(libc::dup(slave_fd)) };
    let stdout = unsafe { Stdio::from_raw_fd(libc::dup(slave_fd)) };
    let mut child = Command::new(env!("CARGO_BIN_EXE_mdterm"))
        .args(["wrap", "--hotkeys", "off", "--", "cat"])
        .stdin(stdin)
        .stdout(stdout)
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn mdterm");
    drop(slave);
    let out = collector(&master);

    // Give mdterm a moment to set raw mode and spawn cat, then type.
    // (Typing earlier is also fine — bytes queue in the line discipline —
    // but the interesting regression window is right at session start.)
    std::thread::sleep(Duration::from_millis(300));
    master.write_all(b"hello\n").unwrap();
    master.flush().unwrap();

    // The keystrokes must reach cat and come back TWICE: once as the child
    // PTY's line-discipline echo and once as cat's own output. The dead
    // keyboard/starvation bug produced at most one copy (the outer
    // pre-raw-mode echo) and usually none.
    let ok = wait_until(Duration::from_secs(10), || {
        count_occurrences(&out.lock().unwrap(), b"hello") >= 2
    });
    assert!(
        ok,
        "child never received keyboard input: got {:?}",
        String::from_utf8_lossy(&out.lock().unwrap())
    );

    // The proxy must still be alive while the child is alive.
    assert!(
        child.try_wait().unwrap().is_none(),
        "mdterm exited while cat was still alive"
    );

    // Ctrl-D: cat sees EOF and exits 0; the proxy mirrors the status.
    master.write_all(b"\x04").unwrap();
    master.flush().unwrap();
    let status = wait_with_timeout(&mut child, Duration::from_secs(10));
    assert_eq!(status.code(), Some(0), "mdterm must mirror cat's exit 0");
}

/// The original reported repro, verbatim:
/// `printf 'hello\nworld\n' | script -qec "mdterm wrap -- cat" /dev/null`
/// must show the forwarded bytes coming back from cat (at least two
/// "hello"s), not just the initial line-discipline echo.
#[test]
fn script_session_forwards_piped_input_to_child() {
    if which("script").is_none() {
        eprintln!("skipping: script(1) not available");
        return;
    }
    // util-linux `script -qec` syntax; macOS/BSD script(1) differs, and
    // this regression was Linux-reported, so keep the test Linux-only.
    if !cfg!(target_os = "linux") {
        eprintln!("skipping: script(1) invocation is util-linux specific");
        return;
    }
    let mdterm = env!("CARGO_BIN_EXE_mdterm");
    let mut child = Command::new("script")
        .args(["-qec", &format!("{mdterm} wrap --hotkeys off -- cat"), "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn script(1)");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"hello\nworld\n")
        .unwrap();
    drop(child.stdin.take()); // EOF, like the printf pipe closing
    let status = wait_with_timeout(&mut child, Duration::from_secs(20));
    assert!(status.success(), "script session failed: {status}");
    let mut out = Vec::new();
    child
        .stdout
        .as_mut()
        .unwrap()
        .read_to_end(&mut out)
        .unwrap();
    let n = count_occurrences(&out, b"hello");
    assert!(
        n >= 2,
        "expected the forwarded input to come back from cat (>= 2 copies of \
         \"hello\": line-discipline echo + cat output); got {n} in {:?} — \
         the input path is dead",
        String::from_utf8_lossy(&out)
    );
}

/// Minimal `which(1)`.
fn which(name: &str) -> Option<std::path::PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).find_map(|dir| {
        let candidate = dir.join(name);
        candidate.is_file().then_some(candidate)
    })
}
