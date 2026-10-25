// SPDX-License-Identifier: GPL-3.0-or-later

//! Checked signal/termination helpers for spawned children, plus the bounded
//! log tail their failures are reported with.
//!
//! Rust's `Child` can only SIGKILL, so the SIGINT that lets GSR and FFmpeg
//! finish their files goes through `libc::kill` here. Windows has no SIGINT
//! for another process: `wr-capture` finishes when its stdin closes, which
//! the recorder does before terminating it.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// How much of a child's log a failure report carries.
pub const LOG_TAIL_BYTES: u64 = 8 * 1024;

/// The last `LOG_TAIL_BYTES` of a log file, lossily decoded; empty when the
/// file cannot be read.
pub fn read_log_tail(path: &Path) -> String {
    let Ok(mut file) = File::open(path) else {
        return String::new();
    };
    let Ok(size) = file.metadata().map(|meta| meta.len()) else {
        return String::new();
    };
    if file
        .seek(SeekFrom::Start(size.saturating_sub(LOG_TAIL_BYTES)))
        .is_err()
    {
        return String::new();
    }
    let mut tail = Vec::new();
    let _ = file.read_to_end(&mut tail);
    String::from_utf8_lossy(&tail).into_owned()
}

/// A `Command` that opens no console window when the GUI app starts a
/// console program on Windows.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// Send `signal` to a live child. Rejects a missing/zero PID and converts the
/// OS failure into the underlying error.
#[cfg(unix)]
pub fn send_signal(child: &Child, signal: i32) -> io::Result<()> {
    let pid = child.id();
    if pid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "child has no pid",
        ));
    }
    let pid = i32::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "pid out of range"))?;
    if unsafe { libc::kill(pid, signal) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Wait for the child to exit within `timeout`, polling `try_wait`. Returns
/// true when it exited (and was reaped), false on timeout.
pub fn wait_with_timeout(child: &mut Child, timeout: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Graceful stop: SIGINT (Unix only), wait up to `grace`, then kill and reap.
pub fn terminate(child: &mut Child, grace: Duration) -> io::Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    #[cfg(unix)]
    let _ = send_signal(child, libc::SIGINT);
    if wait_with_timeout(child, grace)? {
        return Ok(());
    }
    child.kill()?;
    child.wait()?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn signal_and_terminate_control_a_real_child() {
        let mut child = Command::new("sleep")
            .arg("30")
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        // A benign signal succeeds against a live child.
        send_signal(&child, 0).expect("signal 0");
        terminate(&mut child, Duration::from_millis(500)).expect("terminate");
        // The child is reaped: signalling now fails.
        assert!(send_signal(&child, 0).is_err() || child.try_wait().unwrap().is_some());
    }
}
