// SPDX-License-Identifier: MIT
//! Linux without libusb: sysfs to find devices, usbfs for the cloner, and ioctl
//! numbers that are right on every architecture (MIPS, PowerPC and SPARC lay
//! them out differently). Only libc, so the headless build links statically.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::time::Duration;

type R<T> = Result<T, String>;

// (none, read, write, size bits): asm-generic/ioctl.h and the arch overrides.
#[cfg(any(
    target_arch = "mips",
    target_arch = "mips32r6",
    target_arch = "mips64",
    target_arch = "mips64r6",
    target_arch = "powerpc",
    target_arch = "powerpc64",
    target_arch = "sparc",
    target_arch = "sparc64"
))]
const IOC: (u32, u32, u32, u32) = (1, 2, 4, 13);
#[cfg(not(any(
    target_arch = "mips",
    target_arch = "mips32r6",
    target_arch = "mips64",
    target_arch = "mips64r6",
    target_arch = "powerpc",
    target_arch = "powerpc64",
    target_arch = "sparc",
    target_arch = "sparc64"
)))]
const IOC: (u32, u32, u32, u32) = (0, 2, 1, 14);

pub const IOC_NONE: u32 = IOC.0;
pub const IOC_READ: u32 = IOC.1;
pub const IOC_WRITE: u32 = IOC.2;

/// The kernel's _IOC(dir, type, nr, size).
pub const fn ioc(dir: u32, ty: u8, nr: u8, size: usize) -> u32 {
    dir << (16 + IOC.3) | (size as u32) << 16 | (ty as u32) << 8 | nr as u32
}

/// ioctl(2), retried when a signal interrupts it.
pub fn ioctl<T>(f: &File, req: u32, arg: *mut T) -> io::Result<i32> {
    loop {
        // The request is unsigned long on glibc and int on musl.
        let r = unsafe { libc::ioctl(f.as_raw_fd(), req as _, arg) };
        if r >= 0 {
            return Ok(r);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

fn attr(dir: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(name)).ok().map(|s| s.trim().to_string())
}

/// The first attached USB device with one of these ids: its sysfs directory and
/// its usbfs node.
pub fn find_usb(ids: &[(u16, u16)]) -> Option<(PathBuf, PathBuf)> {
    for e in std::fs::read_dir("/sys/bus/usb/devices").ok()?.flatten() {
        let dir = e.path();
        let id = |name| attr(&dir, name).and_then(|s| u16::from_str_radix(&s, 16).ok());
        let (Some(vid), Some(pid)) = (id("idVendor"), id("idProduct")) else { continue };
        if !ids.contains(&(vid, pid)) {
            continue;
        }
        let num = |name| attr(&dir, name).and_then(|s| s.parse::<u32>().ok());
        if let (Some(bus), Some(dev)) = (num("busnum"), num("devnum")) {
            return Some((dir, PathBuf::from(format!("/dev/bus/usb/{bus:03}/{dev:03}"))));
        }
    }
    None
}

// linux/usbdevice_fs.h
#[repr(C)]
struct CtrlTransfer {
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    length: u16,
    timeout: u32,
    data: *mut libc::c_void,
}

#[repr(C)]
struct BulkTransfer {
    ep: libc::c_uint,
    len: libc::c_uint,
    timeout: libc::c_uint,
    data: *mut libc::c_void,
}

#[repr(C)]
struct IoctlRequest {
    ifno: libc::c_int,
    code: libc::c_int,
    data: *mut libc::c_void,
}

const UINT: usize = std::mem::size_of::<libc::c_uint>();
const USBDEVFS_CONTROL: u32 = ioc(IOC_READ | IOC_WRITE, b'U', 0, std::mem::size_of::<CtrlTransfer>());
const USBDEVFS_BULK: u32 = ioc(IOC_READ | IOC_WRITE, b'U', 2, std::mem::size_of::<BulkTransfer>());
const USBDEVFS_SETCONFIGURATION: u32 = ioc(IOC_READ, b'U', 5, UINT);
const USBDEVFS_CLAIMINTERFACE: u32 = ioc(IOC_READ, b'U', 15, UINT);
const USBDEVFS_RELEASEINTERFACE: u32 = ioc(IOC_READ, b'U', 16, UINT);
const USBDEVFS_IOCTL: u32 = ioc(IOC_READ | IOC_WRITE, b'U', 18, std::mem::size_of::<IoctlRequest>());
const USBDEVFS_DISCONNECT: u32 = ioc(IOC_NONE, b'U', 22, 0);

/// usbfs needs each transfer in one piece of kernel memory; libusb splits bulk
/// transfers into 16 KiB the same way.
const BULK_PIECE: usize = 16 * 1024;

fn millis(t: Duration) -> u32 {
    t.as_millis().min(u32::MAX as u128) as u32
}

/// A USB device opened through usbfs (/dev/bus/usb), libusb-style.
pub struct UsbDevice {
    f: File,
}

impl UsbDevice {
    pub fn open(vid: u16, pid: u16) -> R<Self> {
        let (dir, node) = find_usb(&[(vid, pid)]).ok_or("device not found")?;
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&node)
            .map_err(|e| format!("{}: {e}", node.display()))?;
        let d = Self { f };
        if attr(&dir, "bConfigurationValue").as_deref() != Some("1") {
            let mut config: libc::c_uint = 1;
            let _ = ioctl(&d.f, USBDEVFS_SETCONFIGURATION, &mut config);
        }
        Ok(d)
    }

    /// Claims an interface, detaching a kernel driver from it first if one has it.
    pub fn claim(&self, iface: u8) -> R<()> {
        let mut n = iface as libc::c_uint;
        match ioctl(&self.f, USBDEVFS_CLAIMINTERFACE, &mut n) {
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {
                let mut req = IoctlRequest { ifno: iface as libc::c_int, code: USBDEVFS_DISCONNECT as libc::c_int, data: null_mut() };
                let _ = ioctl(&self.f, USBDEVFS_IOCTL, &mut req);
                ioctl(&self.f, USBDEVFS_CLAIMINTERFACE, &mut n).map(|_| ()).map_err(|e| e.to_string())
            }
            r => r.map(|_| ()).map_err(|e| e.to_string()),
        }
    }

    pub fn release(&self, iface: u8) {
        let mut n = iface as libc::c_uint;
        let _ = ioctl(&self.f, USBDEVFS_RELEASEINTERFACE, &mut n);
    }

    fn control(&self, mut t: CtrlTransfer) -> R<usize> {
        ioctl(&self.f, USBDEVFS_CONTROL, &mut t).map(|n| n as usize).map_err(|e| e.to_string())
    }

    pub fn control_out(&self, request_type: u8, request: u8, value: u16, index: u16, data: &[u8], timeout: Duration) -> R<usize> {
        let (length, data) = (data.len() as u16, data.as_ptr() as *mut libc::c_void);
        self.control(CtrlTransfer { request_type, request, value, index, length, timeout: millis(timeout), data })
    }

    pub fn control_in(&self, request_type: u8, request: u8, value: u16, index: u16, buf: &mut [u8], timeout: Duration) -> R<usize> {
        let (length, data) = (buf.len() as u16, buf.as_mut_ptr().cast());
        self.control(CtrlTransfer { request_type, request, value, index, length, timeout: millis(timeout), data })
    }

    /// One bulk transfer of at most 16 KiB; returns how much moved.
    fn bulk(&self, ep: u8, data: *mut u8, len: usize, timeout: Duration) -> R<usize> {
        let mut t = BulkTransfer { ep: ep as libc::c_uint, len: len.min(BULK_PIECE) as libc::c_uint, timeout: millis(timeout), data: data.cast() };
        ioctl(&self.f, USBDEVFS_BULK, &mut t).map(|n| n as usize).map_err(|e| e.to_string())
    }

    pub fn bulk_out(&self, ep: u8, data: &[u8], timeout: Duration) -> R<usize> {
        self.bulk(ep, data.as_ptr() as *mut u8, data.len(), timeout)
    }

    pub fn bulk_in(&self, ep: u8, buf: &mut [u8], timeout: Duration) -> R<usize> {
        self.bulk(ep, buf.as_mut_ptr(), buf.len(), timeout)
    }
}

/// The capture node of an attached camera with one of these ids (its uvcvideo
/// node with index 0; the other is for metadata), found in sysfs, so a camera
/// that has stopped answering isn't asked.
pub fn video_node(ids: &[(u16, u16)]) -> Option<PathBuf> {
    for e in std::fs::read_dir("/sys/class/video4linux").ok()?.flatten() {
        let dir = e.path();
        if attr(&dir, "index").as_deref() != Some("0") {
            continue;
        }
        // device is the USB interface; its parent is the device with the ids.
        let Some(usb) = std::fs::canonicalize(dir.join("device")).ok().and_then(|d| d.parent().map(Path::to_path_buf)) else { continue };
        let id = |name| attr(&usb, name).and_then(|s| u16::from_str_radix(&s, 16).ok());
        if let (Some(vid), Some(pid)) = (id("idVendor"), id("idProduct")) {
            if ids.contains(&(vid, pid)) {
                return Some(Path::new("/dev").join(e.file_name()));
            }
        }
    }
    None
}

/// Other processes that have this device open, as "name (pid)".
pub fn device_users(path: &str) -> Vec<String> {
    let Ok(want) = std::fs::canonicalize(path) else { return Vec::new() };
    let me = std::process::id().to_string();
    let mut users = Vec::new();
    for p in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let pid = p.file_name().to_string_lossy().into_owned();
        if pid == me || !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(p.path().join("fd")) else { continue };
        if fds.flatten().any(|fd| std::fs::read_link(fd.path()).is_ok_and(|t| t == want)) {
            let name = attr(&p.path(), "comm").unwrap_or_default();
            users.push(format!("{name} ({pid})"));
        }
    }
    users
}

#[cfg(test)]
mod tests {
    use super::*;

    // The values the kernel headers give, per architecture.
    #[test]
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    fn ioctl_numbers_64bit() {
        assert_eq!(USBDEVFS_CONTROL, 0xc018_5500);
        assert_eq!(USBDEVFS_BULK, 0xc018_5502);
        assert_eq!(USBDEVFS_SETCONFIGURATION, 0x8004_5505);
        assert_eq!(USBDEVFS_CLAIMINTERFACE, 0x8004_550f);
        assert_eq!(USBDEVFS_RELEASEINTERFACE, 0x8004_5510);
        assert_eq!(USBDEVFS_IOCTL, 0xc010_5512);
        assert_eq!(USBDEVFS_DISCONNECT, 0x0000_5516);
        assert_eq!(crate::xu::UVCIOC_CTRL_QUERY, 0xc010_7521);
    }

    #[test]
    #[cfg(target_arch = "arm")]
    fn ioctl_numbers_arm() {
        assert_eq!(USBDEVFS_CONTROL, 0xc010_5500);
        assert_eq!(USBDEVFS_CLAIMINTERFACE, 0x8004_550f);
        assert_eq!(USBDEVFS_IOCTL, 0xc00c_5512);
        assert_eq!(crate::xu::UVCIOC_CTRL_QUERY, 0xc00c_7521);
    }

    #[test]
    #[cfg(target_arch = "mips")]
    fn ioctl_numbers_mips() {
        assert_eq!(USBDEVFS_CONTROL, 0xc010_5500);
        assert_eq!(USBDEVFS_CLAIMINTERFACE, 0x4004_550f);
        assert_eq!(USBDEVFS_DISCONNECT, 0x2000_5516);
        assert_eq!(crate::xu::UVCIOC_CTRL_QUERY, 0xc00c_7521);
    }
}
