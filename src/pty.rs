//! POSIX pseudo-terminal (PTY) allocation and child process management.

use nix::fcntl::{FcntlArg, FdFlag, OFlag, fcntl};
use nix::pty::{Winsize, openpty};
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitPidFlag, waitpid};
use nix::unistd::{Pid, setsid};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

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
    /// Returns an [`io::Error`] if PTY allocation, process setup, or command execution fails.
    pub fn spawn(command: Option<&[&str]>, cols: u16, rows: u16) -> io::Result<Self> {
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

        // Neither PTY descriptor should leak into an executed child beyond its stdio.
        for fd in [&master, &slave] {
            fcntl(fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
        }

        // Configure non-blocking reads on master fd
        let flags = fcntl(master.as_fd(), FcntlArg::F_GETFL)
            .map_err(|e| io::Error::from_raw_os_error(e as i32))?;
        fcntl(
            master.as_fd(),
            FcntlArg::F_SETFL(OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK),
        )
        .map_err(|e| io::Error::from_raw_os_error(e as i32))?;

        let default_shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into());
        let mut child = match command.and_then(|args| args.split_first()) {
            Some((program, args)) => {
                let mut child = Command::new(program);
                child.args(args);
                child
            }
            None => Command::new(default_shell),
        };
        child
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("TERM_PROGRAM", "ftty")
            .env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"))
            .env_remove("WEZTERM_EXECUTABLE")
            .env_remove("WEZTERM_PANE")
            .env_remove("WEZTERM_UNIX_SOCKET")
            .env_remove("KITTY_WINDOW_ID")
            .env_remove("KITTY_PID")
            .env_remove("ALACRITTY_WINDOW_ID")
            .env_remove("ALACRITTY_LOG")
            .env_remove("KONSOLE_VERSION")
            .env_remove("FOOT_TERMINAL")
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));

        // SAFETY: Command prepares arguments, environment, and stdio before this hook.
        // The child hook only performs system calls and constructs OS errors; it does
        // not allocate, acquire Rust locks, or run destructors between fork and exec.
        unsafe {
            child.pre_exec(|| {
                setsid()?;
                // Command has already connected stdin to the live PTY slave.
                // TIOCSCTTY takes an integer argument, not a pointer.
                if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = child.spawn()?;
        Ok(Self {
            master,
            child_pid: Pid::from_raw(child.id() as i32),
        })
    }

    /// Resizes the PTY terminal window (`TIOCSWINSZ`), including the pixel geometry
    /// image clients read through `TIOCGWINSZ` to compute their cell size.
    ///
    /// # Errors
    /// Returns an [`io::Error`] if the ioctl system call fails.
    pub fn resize(
        &self,
        cols: u16,
        rows: u16,
        pixel_width: u16,
        pixel_height: u16,
    ) -> io::Result<()> {
        let ws = Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: pixel_width,
            ws_ypixel: pixel_height,
        };
        // SAFETY: master owns a live PTY descriptor, and ws is an initialized Winsize
        // with the layout required by TIOCSWINSZ; the ioctl does not retain its pointer.
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

impl AsFd for Pty {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.master.as_fd()
    }
}

impl Read for Pty {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        nix::unistd::read(&self.master, buf).map_err(Into::into)
    }
}

impl Write for Pty {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        nix::unistd::write(&self.master, buf).map_err(Into::into)
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

/// Releases free glibc arena memory pages back to the operating system kernel.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[inline]
pub fn trim_memory() {
    // SAFETY: malloc_trim(0) is thread-safe on GNU/Linux libc and requests the allocator
    // to return free arena pages to the OS kernel via madvise(MADV_DONTNEED).
    unsafe {
        libc::malloc_trim(0);
    }
}

/// No-op fallback on platforms or libc variants that do not support glibc malloc_trim.
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
#[inline]
pub fn trim_memory() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_has_terminal_stdio_environment_and_a_controlling_tty() {
        use nix::poll::{PollFd, PollFlags, poll};
        use std::time::{Duration, Instant};

        let parent_term = std::env::var_os("TERM");
        let parent_colorterm = std::env::var_os("COLORTERM");
        let mut pty = Pty::spawn(
            Some(&[
                "/bin/sh",
                "-c",
                "test -t 0 && test -t 1 && test -t 2 && stty size </dev/tty && printf '%s|%s\\n' \"$TERM\" \"$COLORTERM\"",
            ]),
            80,
            24,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut output = Vec::new();
        let mut closed = false;
        while Instant::now() < deadline {
            let mut buf = [0; 1024];
            match pty.read(&mut buf) {
                Ok(0) => {
                    closed = true;
                    break;
                }
                Ok(n) => output.extend_from_slice(&buf[..n]),
                Err(err) if err.raw_os_error() == Some(libc::EIO) => {
                    closed = true;
                    break;
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    let mut fds = [PollFd::new(pty.as_fd(), PollFlags::POLLIN)];
                    poll(&mut fds, 100u16).unwrap();
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => panic!("PTY read failed: {err}"),
            }
        }
        let output = String::from_utf8(output).unwrap();
        assert!(
            closed,
            "PTY did not close after the command exited: {output:?}"
        );
        assert!(
            output.contains("24 80"),
            "missing controlling TTY: {output:?}"
        );
        assert!(output.contains("xterm-256color|truecolor"), "{output:?}");
        assert_eq!(std::env::var_os("TERM"), parent_term);
        assert_eq!(std::env::var_os("COLORTERM"), parent_colorterm);
    }

    #[test]
    fn nonexistent_command_reports_an_exec_error() {
        let error = Pty::spawn(Some(&["/ftty-test-nonexistent-command"]), 80, 24).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn test_pty_spawn_and_resize() {
        let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24);
        assert!(pty.is_ok(), "Failed to spawn PTY: {:?}", pty.err());
        let pty = pty.unwrap();
        assert!(pty.as_raw_fd() >= 0);
        assert!(pty.resize(120, 40, 1200, 800).is_ok());
        assert!(pty.is_alive());

        let pty_with_args = Pty::spawn(Some(&["/bin/sh", "-c", "exit 0"]), 80, 24);
        assert!(pty_with_args.is_ok());
    }
}
