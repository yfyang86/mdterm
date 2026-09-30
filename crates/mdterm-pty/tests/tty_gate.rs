//! Regression tests for the F7 gate-mutex starvation bug (dead keyboard).
//!
//! Historical bug: both pump threads held the gate mutex across their idle
//! 100 ms `poll(2)`, so the gate was locked ~100% of the time. Whichever
//! pump grabbed it first at startup starved the other one permanently — a
//! futex-woken waiter always loses to the holder's unlock→relock fast
//! path. Depending on the race, keystrokes never reached the child (dead
//! keyboard; the user's interactive symptom, `kill -9` the proxy) or child
//! output never reached the screen (the `script(1)` repro: only the outer
//! line-discipline echo, session dead from the start). The pipe-injection
//! tests could not see this: they ran without `input_fd`/`gate`, i.e.
//! without the F7 plumbing entirely.
//!
//! These tests wire the full real-TTY plumbing — `input_fd` + `gate` +
//! `suspend_flag` — against a fake terminal PTY, exactly what
//! `PtyProxy::spawn` builds for an interactive session.

#![cfg(unix)]

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mdterm_pty::{spawn_with_io, HotkeyConfig, SpawnIoOptions};

fn pipe() -> (File, File) {
    let mut fds = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe failed");
    unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
}

/// Create a fake "real terminal" PTY with its slave in raw mode (what the
/// proxy's own raw-mode setup produces); returns (master File for the test
/// to type into, slave File used as the proxy's `input`, slave RawFd used
/// as `input_fd`).
fn open_fake_stdin() -> (File, File, RawFd) {
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    // See resize.rs: `*mut`/`&mut` coerce to the `*const` Linux signature,
    // so this compiles on BSD (macOS) and glibc alike.
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
    let mut raw: libc::termios = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::tcgetattr(slave, &mut raw) }, 0, "tcgetattr failed");
    unsafe { libc::cfmakeraw(&mut raw) };
    assert_eq!(unsafe { libc::tcsetattr(slave, libc::TCSANOW, &raw) }, 0, "tcsetattr failed");
    let slave_file = unsafe { File::from_raw_fd(slave) };
    (unsafe { File::from_raw_fd(master) }, slave_file, slave)
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
    rx.recv_timeout(dur).unwrap_or_else(|_| {
        panic!(
            "timed out waiting for {:?} — input not forwarded (gate starvation?)",
            String::from_utf8_lossy(&needle_msg)
        )
    })
}

/// Assert no bytes become readable on `f` within `dur` — WITHOUT consuming
/// anything (a timed-out read thread would steal bytes arriving later).
fn assert_silent(f: &File, dur: Duration) {
    let mut pfd = libc::pollfd {
        fd: f.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let rc = unsafe { libc::poll(&mut pfd, 1, dur.as_millis() as libc::c_int) };
    assert_eq!(rc, 0, "unexpected proxy output while suspended");
}

struct GateWiring {
    flag: Arc<AtomicBool>,
    options: SpawnIoOptions,
}

fn gate_wiring(input_fd: RawFd) -> GateWiring {
    let flag = Arc::new(AtomicBool::new(false));
    GateWiring {
        flag: Arc::clone(&flag),
        options: SpawnIoOptions {
            term_fd: None,
            suspend_flag: Some(flag),
            input_fd: Some(input_fd),
            gate: Some(Arc::new(Mutex::new(()))),
        },
    }
}

/// THE regression: input written before ANY child output must reach the
/// child verbatim from the moment it spawns (a TUI may wait for input
/// silently). Under the starvation bug the input thread never acquired the
/// gate and nothing was ever forwarded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_before_any_child_output_is_forwarded() {
    let (mut term_master, term_slave, slave_fd) = open_fake_stdin();
    let (proxy_out_read, proxy_out_write) = pipe();

    let mut hotkeys = HotkeyConfig::default();
    hotkeys.enabled = false;

    let wiring = gate_wiring(slave_fd);
    let (join, _events) = spawn_with_io(
        env!("CARGO_BIN_EXE_ptyecho"),
        &[],
        hotkeys,
        term_slave,
        proxy_out_write,
        wiring.options,
    )
    .expect("spawn_with_io failed");

    // Type IMMEDIATELY — before reading (or waiting for) any child output.
    term_master.write_all(b"early-line\n").unwrap();
    term_master.flush().unwrap();

    // The child must receive it and echo it back, whatever the pump
    // threads' startup interleaving. The needle includes the trailing \n:
    // only ptyecho's post-raw-mode verbatim echo contains "early-line\n"
    // (the child line discipline's own canonical echo is "early-line\r\n"),
    // so matching also proves the child is past its canonical-mode startup
    // window — the point where the stdin-EOF policy can deliver a VEOF the
    // child will read verbatim (see tests/stdin_eof.rs).
    let out = read_until(&proxy_out_read, b"early-line\n", Duration::from_secs(10));
    assert!(
        out.windows(b"early-line\n".len()).any(|w| w == b"early-line\n"),
        "input sent before child output was lost: {:?}",
        String::from_utf8_lossy(&out)
    );

    // The proxy must NOT exit while the child is alive just because our
    // stdin went momentarily quiet.
    assert!(
        !join.is_finished(),
        "proxy exited while the child was still alive"
    );

    // stdin EOF (closing the master hangs up the fake terminal) must still
    // deliver a clean VEOF: ptyecho exits 0, the proxy mirrors it.
    drop(term_master);
    let code = tokio::time::timeout(Duration::from_secs(10), join)
        .await
        .expect("proxy hung after stdin EOF")
        .expect("pump task panicked");
    assert_eq!(code, 0, "ptyecho must exit 0 on VEOF");
}

/// Interleaved input/output over a session, plus suspend/resume cycling:
/// the gate machinery must never deadlock or starve the input path when
/// NOT suspended, and must park input while suspended.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interleaved_io_and_suspend_resume_do_not_starve_input() {
    let (mut term_master, term_slave, slave_fd) = open_fake_stdin();
    let (proxy_out_read, proxy_out_write) = pipe();

    let mut hotkeys = HotkeyConfig::default();
    hotkeys.enabled = false;

    let wiring = gate_wiring(slave_fd);
    let flag = wiring.flag;
    let (join, _events) = spawn_with_io(
        env!("CARGO_BIN_EXE_ptyecho"),
        &[],
        hotkeys,
        term_slave,
        proxy_out_write,
        wiring.options,
    )
    .expect("spawn_with_io failed");

    // Wait for the child's readiness marker, then ping-pong a few rounds.
    read_until(&proxy_out_read, b"PTYECHO_READY", Duration::from_secs(10));
    for i in 0..5 {
        let payload = format!("ping{i}\n");
        term_master.write_all(payload.as_bytes()).unwrap();
        term_master.flush().unwrap();
        read_until(&proxy_out_read, format!("ping{i}").as_bytes(), Duration::from_secs(10));
    }

    // Suspend (pager owns the terminal): typed input must be PARKED, not
    // consumed by the proxy, and no child output may reach the screen.
    flag.store(true, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(300));
    term_master.write_all(b"parked-line\n").unwrap();
    term_master.flush().unwrap();
    assert_silent(&proxy_out_read, Duration::from_millis(400));

    // Resume: the parked input is delivered and echoed.
    flag.store(false, Ordering::SeqCst);
    read_until(&proxy_out_read, b"parked-line", Duration::from_secs(10));

    // Hammer suspend/resume while continuing to type: no deadlock, no
    // starvation, no lost input.
    for i in 0..10 {
        flag.store(true, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(5));
        flag.store(false, Ordering::SeqCst);
        let payload = format!("x{i}\n");
        term_master.write_all(payload.as_bytes()).unwrap();
        term_master.flush().unwrap();
        read_until(&proxy_out_read, format!("x{i}").as_bytes(), Duration::from_secs(10));
    }

    assert!(
        !join.is_finished(),
        "proxy exited while the child was still alive"
    );

    drop(term_master);
    let code = tokio::time::timeout(Duration::from_secs(10), join)
        .await
        .expect("proxy hung after stdin EOF")
        .expect("pump task panicked");
    assert_eq!(code, 0);
}
