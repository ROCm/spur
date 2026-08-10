// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! PTY primitives for interactive job I/O. These are building blocks called
//! by executor.rs (primary PTY launch) and the interactive_session handler
//! (overlap/exec-into-job). This module does NOT spawn processes.

use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use anyhow::{Context, Result};
use nix::pty::openpty;
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;

pub struct WindowSize {
    pub rows: u16,
    pub cols: u16,
    pub xpixel: u16,
    pub ypixel: u16,
}

impl WindowSize {
    pub fn to_libc(&self) -> libc::winsize {
        libc::winsize {
            ws_row: self.rows,
            ws_col: self.cols,
            ws_xpixel: self.xpixel,
            ws_ypixel: self.ypixel,
        }
    }
}

/// Open a PTY pair and optionally set the initial window size on the slave.
/// Returns `(master, slave)`.
pub fn openpty_with_winsize(ws: Option<&WindowSize>) -> Result<(OwnedFd, OwnedFd)> {
    let pty = openpty(None, None).context("openpty")?;

    if let Some(ws) = ws {
        let ret = unsafe { libc::ioctl(pty.slave.as_raw_fd(), libc::TIOCSWINSZ, &ws.to_libc()) };
        if ret < 0 {
            tracing::warn!("TIOCSWINSZ failed: {}", std::io::Error::last_os_error());
        }
    }

    Ok((pty.master, pty.slave))
}

/// Pre-exec hook that makes the PTY slave the child's controlling terminal.
///
/// Must be called inside an `unsafe { cmd.pre_exec(...) }` closure, after fork
/// but before exec.
///
/// # Safety
/// Caller must ensure `slave_fd` and `master_fd` are valid open file
/// descriptors in the child process. Only async-signal-safe operations are used.
pub unsafe fn pty_pre_exec(slave_fd: RawFd, master_fd: RawFd) -> std::io::Result<()> {
    if libc::setsid() < 0 {
        return Err(std::io::Error::last_os_error());
    }

    if libc::ioctl(slave_fd, libc::TIOCSCTTY, 0) < 0 {
        return Err(std::io::Error::last_os_error());
    }

    checked_dup2(slave_fd, libc::STDIN_FILENO)?;
    checked_dup2(slave_fd, libc::STDOUT_FILENO)?;
    checked_dup2(slave_fd, libc::STDERR_FILENO)?;

    if slave_fd > 2 {
        libc::close(slave_fd);
    }

    libc::close(master_fd);

    Ok(())
}

/// Async-signal-safe dup2 wrapper that returns an error on failure.
pub(crate) unsafe fn checked_dup2(oldfd: RawFd, newfd: RawFd) -> std::io::Result<()> {
    if libc::dup2(oldfd, newfd) < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Resize a PTY (TIOCSWINSZ on the master fd).
pub fn resize(master: RawFd, ws: &WindowSize) -> Result<()> {
    let ret = unsafe { libc::ioctl(master, libc::TIOCSWINSZ, &ws.to_libc()) };
    if ret < 0 {
        anyhow::bail!("TIOCSWINSZ failed: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

/// Send a signal to the foreground process group of the PTY, falling back
/// to signaling the child directly if `tcgetpgrp` fails.
pub fn signal_foreground(master: RawFd, child_pid: i32, sig: i32) -> Result<()> {
    let pgrp = unsafe { libc::tcgetpgrp(master) };
    if pgrp <= 0 {
        signal::kill(Pid::from_raw(child_pid), Signal::try_from(sig)?)?;
    } else {
        signal::killpg(Pid::from_raw(pgrp), Signal::try_from(sig)?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openpty_creates_valid_fds() {
        let (master, slave) = openpty_with_winsize(None).expect("openpty failed");
        assert!(master.as_raw_fd() >= 0);
        assert!(slave.as_raw_fd() >= 0);
    }

    #[test]
    fn openpty_sets_winsize() {
        let ws = WindowSize {
            rows: 40,
            cols: 120,
            xpixel: 0,
            ypixel: 0,
        };
        let (master, _slave) = openpty_with_winsize(Some(&ws)).expect("openpty failed");

        let mut got: libc::winsize = unsafe { std::mem::zeroed() };
        let ret = unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCGWINSZ, &mut got) };
        assert_eq!(ret, 0);
        assert_eq!(got.ws_row, 40);
        assert_eq!(got.ws_col, 120);
    }

    #[test]
    fn pty_pre_exec_bad_slave_returns_error() {
        // setsid() will succeed, but dup2 with an invalid fd should fail.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");

        if pid == 0 {
            let result = unsafe { pty_pre_exec(-1, -1) };
            // setsid succeeds, TIOCSCTTY on -1 fails.
            std::process::exit(if result.is_err() { 0 } else { 1 });
        }

        let mut status = 0i32;
        unsafe { libc::waitpid(pid, &mut status, 0) };
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
            "pty_pre_exec should fail with invalid slave fd"
        );
    }

    #[test]
    fn resize_updates_winsize() {
        let (master, _slave) = openpty_with_winsize(Some(&WindowSize {
            rows: 24,
            cols: 80,
            xpixel: 0,
            ypixel: 0,
        }))
        .expect("openpty failed");

        resize(
            master.as_raw_fd(),
            &WindowSize {
                rows: 50,
                cols: 200,
                xpixel: 0,
                ypixel: 0,
            },
        )
        .expect("resize failed");

        let mut got: libc::winsize = unsafe { std::mem::zeroed() };
        let ret = unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCGWINSZ, &mut got) };
        assert_eq!(ret, 0);
        assert_eq!(got.ws_row, 50);
        assert_eq!(got.ws_col, 200);
    }
}
