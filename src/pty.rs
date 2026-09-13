//! POSIX pseudo-terminal (PTY) allocation and child process management.

use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::pty::{Winsize, openpty};
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitPidFlag, waitpid};
use nix::unistd::{ForkResult, Pid, execvp, fork, setsid};
use std::ffi::CString;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};

/// Manages a PTY master file descriptor and its associated child process.
#[derive(Debug)]
pub struct Pty {
    master: OwnedFd,
    child_pid: Pid,
}

impl Pty {
    /// Spawns a shell or specific command inside a new PTY session.
    ///
    /// # Errors
    /// Returns an [`io::Error`] if PTY allocation, forking, or file descriptor manipulation fails.
    pub fn spawn(command: Option<&str>, cols: u16, rows: u16) -> io::Result<Self> {
        let winsize = Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        let pty_res =
            openpty(Some(&winsize), None).map_err(|e| io::Error::from_raw_os_error(e as i32))?;

        let master = pty_res.master;
        let slave = pty_res.slave;

        // Configure non-blocking reads on master fd
        let flags = fcntl(master.as_fd(), FcntlArg::F_GETFL)
            .map_err(|e| io::Error::from_raw_os_error(e as i32))?;
        fcntl(
            master.as_fd(),
            FcntlArg::F_SETFL(OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK),
        )
        .map_err(|e| io::Error::from_raw_os_error(e as i32))?;

        match unsafe { fork() } {
            Ok(ForkResult::Parent { child }) => {
                // In parent: close slave
                drop(slave);
                Ok(Self {
                    master,
                    child_pid: child,
                })
            }
            Ok(ForkResult::Child) => {
                // In child process
                drop(master);

                // Create new session
                if setsid().is_err() {
                    unsafe { libc::_exit(1) };
                }

                // Acquire controlling terminal
                unsafe {
                    libc::ioctl(slave.as_raw_fd(), libc::TIOCSCTTY as _, 0);
                }

                // Redirect stdin, stdout, stderr to slave
                let slave_raw = slave.as_raw_fd();
                unsafe {
                    if libc::dup2(slave_raw, 0) < 0
                        || libc::dup2(slave_raw, 1) < 0
                        || libc::dup2(slave_raw, 2) < 0
                    {
                        libc::_exit(1);
                    }
                }

                if slave_raw > 2 {
                    drop(slave);
                }

                // Terminal identification environment variables
                unsafe {
                    std::env::set_var("TERM", "xterm-256color");
                    std::env::set_var("COLORTERM", "truecolor");
                }

                let shell = command
                    .map(str::to_string)
                    .or_else(|| std::env::var("SHELL").ok())
                    .unwrap_or_else(|| "/bin/sh".to_string());

                let shell_c = CString::new(shell.clone()).unwrap_or_default();
                let args = [shell_c.as_c_str()];

                let _ = execvp(&shell_c, &args);
                unsafe { libc::_exit(127) };
            }
            Err(e) => Err(io::Error::from_raw_os_error(e as i32)),
        }
    }

    /// Resizes the PTY terminal window size (`TIOCSWINSZ`).
    ///
    /// # Errors
    /// Returns an [`io::Error`] if the ioctl system call fails.
    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let ws = Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let res = unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
        if res < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Returns the raw file descriptor of the master PTY end.
    #[must_use]
    pub fn as_raw_fd(&self) -> RawFd {
        self.master.as_raw_fd()
    }

    /// Returns a duplicate of the master PTY file descriptor.
    ///
    /// # Errors
    /// Returns an [`io::Error`] if duplicating the file descriptor fails.
    pub fn try_clone_master(&self) -> io::Result<OwnedFd> {
        self.master.try_clone()
    }

    /// Returns the child process PID.
    #[must_use]
    pub fn child_pid(&self) -> Pid {
        self.child_pid
    }

    /// Checks if the child process is still running.
    pub fn is_alive(&self) -> bool {
        matches!(
            waitpid(self.child_pid, Some(WaitPidFlag::WNOHANG)),
            Ok(nix::sys::wait::WaitStatus::StillAlive)
        )
    }
}

impl Read for Pty {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let res =
            unsafe { libc::read(self.master.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if res < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(res as usize)
        }
    }
}

impl Write for Pty {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let res = unsafe { libc::write(self.master.as_raw_fd(), buf.as_ptr().cast(), buf.len()) };
        if res < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(res as usize)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        if self.is_alive() {
            let _ = kill(self.child_pid, Signal::SIGHUP);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pty_spawn_and_resize() {
        let pty = Pty::spawn(Some("/bin/sh"), 80, 24);
        assert!(pty.is_ok(), "Failed to spawn PTY: {:?}", pty.err());
        let pty = pty.unwrap();
        assert!(pty.as_raw_fd() >= 0);
        assert!(pty.resize(120, 40).is_ok());
        assert!(pty.is_alive());
    }
}
