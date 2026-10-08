use anyhow::{Context, Result};
use libc::{TIOCSWINSZ, winsize};
use nix::fcntl::OFlag;
use nix::pty::{grantpt, posix_openpt, ptsname_r, unlockpt};
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

pub struct Pty {
    pub master: File,
    pub slave: File,
}

/// Open a PTY with `O_CLOEXEC` on both ends at creation time so a concurrent
/// `fork`/`exec` cannot inherit a session master belonging to another user.
pub fn open_cloexec(cols: u16, rows: u16) -> Result<Pty> {
    let master =
        posix_openpt(OFlag::O_RDWR | OFlag::O_NOCTTY | OFlag::O_CLOEXEC).context("posix_openpt")?;
    grantpt(&master).context("grantpt")?;
    unlockpt(&master).context("unlockpt")?;
    let name = ptsname_r(&master).context("ptsname")?;
    let slave = nix::fcntl::open(
        name.as_str(),
        OFlag::O_RDWR | OFlag::O_NOCTTY | OFlag::O_CLOEXEC,
        nix::sys::stat::Mode::empty(),
    )
    .context("open slave")?;
    let ws = winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe { libc::ioctl(master.as_raw_fd(), TIOCSWINSZ, &ws) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(Pty {
        master: File::from(OwnedFd::from(master)),
        slave: File::from(slave),
    })
}

/// Duplicate `fd` for Command stdio **keeping** `FD_CLOEXEC`. The child must
/// `dup2` onto 0–2 (which clears CLOEXEC on those final fds). Inheritable
/// copies must never sit in the parent across another session's spawn.
pub fn dup_cloexec(fd: i32) -> Result<OwnedFd> {
    let n = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if n < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(n) })
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn fd_is_cloexec(fd: i32) -> Result<bool> {
    let fl = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if fl < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(fl & libc::FD_CLOEXEC != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pty_master_and_slave_are_cloexec() {
        let pty = open_cloexec(80, 24).expect("pty");
        assert!(fd_is_cloexec(pty.master.as_raw_fd()).unwrap());
        assert!(fd_is_cloexec(pty.slave.as_raw_fd()).unwrap());
        let dup = dup_cloexec(pty.slave.as_raw_fd()).unwrap();
        assert!(fd_is_cloexec(dup.as_raw_fd()).unwrap());
    }
}
