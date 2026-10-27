// SPDX-License-Identifier: MIT
//! The camera's USB serial console on Linux with libc alone, for the daemon:
//! the tty found in sysfs by the camera's USB ids, opened raw at 115200 baud
//! and exclusively (TIOCEXCL), as the GUI's serialport does.

use std::fs::File;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The tty of an attached camera with one of these ids (CDC-ACM).
pub fn find(ids: &[(u16, u16)]) -> Option<PathBuf> {
    let read = |p: &Path, n: &str| std::fs::read_to_string(p.join(n)).ok().map(|s| s.trim().to_string());
    for e in std::fs::read_dir("/sys/class/tty").ok()?.flatten() {
        // device is the USB interface; its parent is the device with the ids.
        let Some(usb) = std::fs::canonicalize(e.path().join("device")).ok().and_then(|d| d.parent().map(Path::to_path_buf)) else {
            continue;
        };
        let id = |n| read(&usb, n).and_then(|s| u16::from_str_radix(&s, 16).ok());
        if let (Some(vid), Some(pid)) = (id("idVendor"), id("idProduct")) {
            if ids.contains(&(vid, pid)) {
                return Some(Path::new("/dev").join(e.file_name()));
            }
        }
    }
    None
}

pub struct Port {
    f: File,
}

impl Port {
    pub fn open(path: &Path) -> Result<Self, String> {
        let err = |e: io::Error| format!("{}: {e}", path.display());
        let f = std::fs::OpenOptions::new().read(true).write(true).custom_flags(libc::O_NOCTTY).open(path).map_err(err)?;
        let fd = f.as_raw_fd();
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut t) } != 0 {
            return Err(err(io::Error::last_os_error()));
        }
        unsafe {
            libc::cfmakeraw(&mut t);
            libc::cfsetispeed(&mut t, libc::B115200);
            libc::cfsetospeed(&mut t, libc::B115200);
        }
        t.c_cflag |= libc::CLOCAL | libc::CREAD;
        t.c_cc[libc::VMIN] = 1;
        t.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &t) } != 0 || unsafe { libc::ioctl(fd, libc::TIOCEXCL as _) } != 0 {
            return Err(err(io::Error::last_os_error()));
        }
        Ok(Self { f })
    }

    pub fn writer(&self) -> io::Result<File> {
        self.f.try_clone()
    }

    /// What arrives within `wait`: None if nothing did; an empty read means the
    /// port went away.
    pub fn read(&mut self, buf: &mut [u8], wait: Duration) -> io::Result<Option<usize>> {
        let mut p = libc::pollfd { fd: self.f.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        match unsafe { libc::poll(&mut p, 1, wait.as_millis().min(i32::MAX as u128) as i32) } {
            0 => Ok(None),
            n if n < 0 => {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    Ok(None)
                } else {
                    Err(e)
                }
            }
            _ if p.revents & libc::POLLIN == 0 => Ok(Some(0)),
            _ => self.f.read(buf).map(Some),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::fd::{FromRawFd, OwnedFd};

    #[test]
    fn a_raw_exclusive_port_passes_bytes_both_ways() {
        let (mut master, mut slave) = (0, 0);
        assert_eq!(unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null(), std::ptr::null()) }, 0);
        let name = std::fs::read_link(format!("/proc/self/fd/{slave}")).unwrap();
        let mut master = File::from(unsafe { OwnedFd::from_raw_fd(master) });
        let _slave = unsafe { OwnedFd::from_raw_fd(slave) };
        let mut port = Port::open(&name).unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(port.read(&mut buf, Duration::from_millis(20)).unwrap(), None);
        master.write_all(b"# ").unwrap();
        assert_eq!(port.read(&mut buf, Duration::from_secs(5)).unwrap(), Some(2));
        assert_eq!(&buf[..2], b"# ");
        port.writer().unwrap().write_all(b"ls\r").unwrap();
        let mut got = [0u8; 3];
        master.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"ls\r");
        // Raw: no echo, no translation.
        assert_eq!(port.read(&mut buf, Duration::from_millis(50)).unwrap(), None);
    }
}
