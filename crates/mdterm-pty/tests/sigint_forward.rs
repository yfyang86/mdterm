//! Signal-forwarding test: SIGINT delivered to the proxy process must be
//! forwarded to the child's process group, not kill the proxy outright.
//! (Own test file: raising process-wide signals must not disturb other tests
//! running in the same process.)

#![cfg(unix)]

use std::fs::File;
use std::os::unix::io::FromRawFd;
use std::time::Duration;

use mdterm_pty::{spawn_with_io, HotkeyConfig, SpawnIoOptions};

fn pipe() -> (File, File) {
    let mut fds = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe failed");
    unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigint_is_forwarded_to_child_process_group() {
    let (proxy_in_read, _proxy_in_write) = pipe();
    let (proxy_out_read, proxy_out_write) = pipe();
    drop(proxy_out_read);

    let (join, _events) = spawn_with_io(
        "sleep",
        &["30".to_string()],
        HotkeyConfig::default(),
        proxy_in_read,
        proxy_out_write,
        SpawnIoOptions::default(),
    )
    .expect("spawn_with_io failed");

    // Give the child time to exec and the signal tasks to register.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // SIGINT to the proxy: must be forwarded to the child's process group.
    assert_eq!(unsafe { libc::raise(libc::SIGINT) }, 0);

    let code = tokio::time::timeout(Duration::from_secs(10), join)
        .await
        .expect("proxy survived SIGINT but child did not exit (forwarding broken?)")
        .expect("pump task panicked");
    assert_eq!(
        code,
        128 + libc::SIGINT,
        "child must die from the forwarded SIGINT (exit semantics 128+sig)"
    );
}
