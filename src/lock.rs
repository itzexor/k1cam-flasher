// SPDX-License-Identifier: MIT
//! One k1cam-flasher at a time drives the camera on a computer: the command
//! line, the GUI and the daemon each hold this lock while they use it. It is
//! an abstract Unix socket, so it works across users (a daemon run as root and
//! a GUI run as a user), leaves no file behind, and goes away with the process
//! that held it. Whoever asks while it is held is told who holds it.
//!
//! It keeps cooperating flashers apart; other programs are checked separately
//! (linux::device_users), and nothing stops one from opening the camera later.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

const NAME: &str = "k1cam-flasher";

pub struct Lock {
    who: Arc<Mutex<String>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// Takes the lock, saying `who` holds it (e.g. "k1cam-flasher backup (pid
/// 812)"); or says who holds it now.
pub fn acquire(who: impl Into<String>) -> Result<Lock, String> {
    acquire_named(NAME, who.into())
}

fn address(name: &str) -> (libc::sockaddr_un, libc::socklen_t) {
    let mut a: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    a.sun_family = libc::AF_UNIX as libc::sa_family_t;
    // Abstract: a NUL, then the name.
    for (i, b) in name.bytes().take(a.sun_path.len() - 1).enumerate() {
        a.sun_path[i + 1] = b as libc::c_char;
    }
    let len = std::mem::size_of::<libc::sa_family_t>() + 1 + name.len().min(a.sun_path.len() - 1);
    (a, len as libc::socklen_t)
}

fn socket() -> Result<OwnedFd, String> {
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(format!("lock: {}", std::io::Error::last_os_error()));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub(crate) fn acquire_named(name: &str, who: String) -> Result<Lock, String> {
    let fd = socket()?;
    let (a, len) = address(name);
    if unsafe { libc::bind(fd.as_raw_fd(), &a as *const _ as *const libc::sockaddr, len) } != 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EADDRINUSE) {
            return Err(format!("the camera is in use by {}", holder(name)));
        }
        return Err(format!("lock: {e}"));
    }
    if unsafe { libc::listen(fd.as_raw_fd(), 4) } != 0 {
        return Err(format!("lock: {}", std::io::Error::last_os_error()));
    }
    let who = Arc::new(Mutex::new(who));
    let stop = Arc::new(AtomicBool::new(false));
    let (who2, stop2) = (who.clone(), stop.clone());
    let thread = std::thread::spawn(move || answer(fd, who2, stop2));
    Ok(Lock { who, stop, thread: Some(thread) })
}

/// Tells each asker who holds the lock, until `stop`; then lets go of it.
fn answer(fd: OwnedFd, who: Arc<Mutex<String>>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        let mut p = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut p, 1, 100) } <= 0 {
            continue;
        }
        let c = unsafe { libc::accept4(fd.as_raw_fd(), std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC) };
        if c < 0 {
            continue;
        }
        let mut s = UnixStream::from(unsafe { OwnedFd::from_raw_fd(c) });
        let line = format!("{}\n", who.lock().unwrap());
        let _ = s.set_write_timeout(Some(Duration::from_millis(200)));
        let _ = s.write_all(line.as_bytes());
    }
}

/// Who holds the lock, as it says.
fn holder(name: &str) -> String {
    let unknown = "another k1cam-flasher".to_string();
    let Ok(fd) = socket() else { return unknown };
    let (a, len) = address(name);
    if unsafe { libc::connect(fd.as_raw_fd(), &a as *const _ as *const libc::sockaddr, len) } != 0 {
        return unknown;
    }
    let s = UnixStream::from(fd);
    let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
    let mut buf = Vec::new();
    let _ = s.take(512).read_to_end(&mut buf);
    let said = String::from_utf8_lossy(&buf).trim().to_string();
    if said.is_empty() {
        unknown
    } else {
        said
    }
}

impl Lock {
    /// Changes what askers are told.
    pub fn set_who(&self, who: impl Into<String>) {
        *self.who.lock().unwrap() = who.into();
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_holder_at_a_time_and_askers_learn_who() {
        let name = format!("k1cam-flasher-test-{}", std::process::id());
        let held = acquire_named(&name, "k1cam-flasher daemon (pid 1)".into()).unwrap();
        let e = acquire_named(&name, "cli".into()).err().unwrap();
        assert_eq!(e, "the camera is in use by k1cam-flasher daemon (pid 1)");
        held.set_who("k1cam-flasher daemon (pid 1), controlled from studio-pc");
        assert!(acquire_named(&name, "cli".into()).err().unwrap().ends_with("controlled from studio-pc"));
        drop(held);
        let again = acquire_named(&name, "cli".into());
        assert!(again.is_ok(), "a dropped lock is free again");
    }
}
